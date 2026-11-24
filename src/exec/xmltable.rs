//! v2.42: XMLTABLE evaluation (PG19 gram.y XmlTable; executor
//! semantics from src/backend/utils/adt/xml.c XmlTable* +
//! nodeAgg's table-function scan). The row path is evaluated with
//! the document node as context; each result node becomes a row and
//! each column path is evaluated with that row node as context.

use crate::exec::cast::eval_cast;
use crate::exec::{ExecError, Q, QCol, QRow, Scope, eval_expr, exec_err};
use crate::sql::{XmlTableSpec, XtColumn};
use crate::storage::{ColType, Row, Value};

use super::xml::{
    Doc, Xp, XpValue, compile_xpath, eval_xpath_at_ns, parse_xml_strict_ns, xpath_number_text,
};

/// Static output schema of an XMLTABLE item (Describe and execution
/// share it, mirroring jt_schema).
/// PG19: a DEFAULT namespace in XMLTABLE's XMLNAMESPACES is an
/// explicit error (0A000), checked at parse analysis.
pub(crate) fn check_no_default_ns(spec: &XmlTableSpec) -> Result<(), ExecError> {
    if spec.namespaces.iter().any(|(p, _)| p.is_none()) {
        return Err(ExecError {
            code: "0A000",
            message: "DEFAULT namespace is not supported".to_string(),
            detail: None,
            hint: None,
        });
    }
    Ok(())
}

pub(crate) fn xt_schema(
    spec: &XmlTableSpec,
    alias: &Option<String>,
    col_aliases: &[String],
) -> Result<Vec<QCol>, ExecError> {
    check_no_default_ns(spec)?;
    let qual = alias.clone().unwrap_or_else(|| "xmltable".to_string());
    if col_aliases.len() > spec.columns.len() {
        return Err(exec_err(
            "42601",
            format!(
                "XMLTABLE function has {} columns available but {} columns specified",
                spec.columns.len(),
                col_aliases.len()
            ),
        ));
    }
    let mut out = Vec::with_capacity(spec.columns.len());
    for (i, c) in spec.columns.iter().enumerate() {
        let (name, ty) = match c {
            XtColumn::Ordinality { name } => (name.clone(), ColType::BigInt),
            XtColumn::Regular { name, ty, .. } => (name.clone(), *ty),
        };
        let name = col_aliases.get(i).cloned().unwrap_or(name);
        out.push(QCol {
            qual: qual.clone(),
            name,
            ty,
            hidden: false,
            src_ord: i as u32,
        });
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn eval_xml_table(
    q: &mut Q,
    scopes: &[Scope],
    spec: &XmlTableSpec,
    arg_vals: &[Value],
    alias: &Option<String>,
    col_aliases: &[String],
) -> Result<(Vec<QCol>, Vec<QRow>), ExecError> {
    let schema = xt_schema(spec, alias, col_aliases)?;
    // Row path: a text expression (PG requires it to be a string).
    let rowpath_text = match &arg_vals[0] {
        Value::Null => {
            return Err(exec_err(
                "22004",
                "row path expression must not be null".to_string(),
            ));
        }
        Value::Text(t) | Value::BpChar(t) => t.to_string(),
        other => {
            return Err(exec_err(
                "42804",
                format!(
                    "row path expression must be of type text, not {}",
                    other.type_name()
                ),
            ));
        }
    };
    // PASSING document: null yields zero rows in PG.
    if arg_vals[1] == Value::Null {
        return Ok((schema, Vec::new()));
    }
    let doc_text = match &arg_vals[1] {
        Value::Xml(t) => t.to_string(),
        Value::Text(t) | Value::BpChar(t) => {
            // PG casts the value to xml (document semantics inside
            // XMLTABLE regardless of xmloption).
            parse_xml_strict_ns(t, true).map_err(|e| e.to_exec())?;
            t.to_string()
        }
        other => {
            return Err(exec_err(
                "42804",
                format!(
                    "passing expression must be of type xml, not {}",
                    other.type_name()
                ),
            ));
        }
    };
    let doc: Doc = parse_xml_strict_ns(&doc_text, true).map_err(|e| e.to_exec())?;
    let mut nsmap: Vec<(String, String)> = Vec::new();
    let mut default_ns: Option<String> = None;
    for (prefix, uri) in &spec.namespaces {
        match prefix {
            Some(p) => nsmap.push((p.clone(), uri.clone())),
            None => default_ns = Some(uri.clone()),
        }
    }
    let row_expr = compile_xpath(&rowpath_text)?;
    let xp = Xp {
        forest: &doc.nodes,
        nsmap: &nsmap,
        default_ns: default_ns.as_deref(),
    };
    let row_val = xp.eval_root(&row_expr).map_err(|e| ExecError {
        code: "10608",
        message: "invalid XPath expression".to_string(),
        detail: Some(e.0),
        hint: None,
    })?;
    let row_nodes = match row_val {
        XpValue::Nodes(ns) => ns,
        _ => {
            return Err(exec_err(
                "10608",
                "row path expression must return a node set".to_string(),
            ));
        }
    };
    // Pre-compile column paths.
    let mut col_exprs: Vec<Option<crate::exec::xml::XExpr>> = Vec::new();
    for c in &spec.columns {
        match c {
            XtColumn::Ordinality { .. } => col_exprs.push(None),
            XtColumn::Regular { name, path, .. } => {
                let ptext = match path {
                    Some(pe) => {
                        let v = eval_expr(q, scopes, pe)?;
                        match v {
                            Value::Text(t) | Value::BpChar(t) => t.to_string(),
                            Value::Null => {
                                return Err(exec_err(
                                    "22004",
                                    "column path expression must not be null".to_string(),
                                ));
                            }
                            other => {
                                return Err(exec_err(
                                    "42804",
                                    format!(
                                        "column path expression must be of type text, not {}",
                                        other.type_name()
                                    ),
                                ));
                            }
                        }
                    }
                    None => name.clone(),
                };
                col_exprs.push(Some(compile_xpath(&ptext)?));
            }
        }
    }
    let mut rows: Vec<QRow> = Vec::new();
    for (row_idx, row_node) in row_nodes.iter().enumerate() {
        let ctx_path = match row_node {
            super::xml::XNode::Path(p) => p.clone(),
            _ => {
                return Err(exec_err(
                    "10608",
                    "row path expression must return element nodes".to_string(),
                ));
            }
        };
        let mut cells: Vec<Value> = Vec::with_capacity(spec.columns.len());
        for (ci, c) in spec.columns.iter().enumerate() {
            match c {
                XtColumn::Ordinality { .. } => {
                    cells.push(Value::BigInt((row_idx + 1) as i64));
                }
                XtColumn::Regular {
                    name,
                    ty,
                    exists,
                    default,
                    not_null,
                    ..
                } => {
                    let expr = col_exprs[ci].as_ref().unwrap();
                    let v = eval_xpath_at_ns(&doc, expr, &nsmap, &ctx_path, default_ns.as_deref())?;
                    let cell = column_value(q, scopes, &doc, &nsmap, *ty, *exists, v)?;
                    let cell = match cell {
                        None => match default {
                            Some(d) => {
                                let dv = eval_expr(q, scopes, d)?;
                                if dv == Value::Null {
                                    None
                                } else {
                                    Some(eval_cast(&dv, *ty)?)
                                }
                            }
                            None => None,
                        },
                        some => some,
                    };
                    match cell {
                        None => {
                            if *not_null {
                                return Err(exec_err(
                                    "22004",
                                    format!("null is not allowed in column \"{}\"", name),
                                ));
                            }
                            cells.push(Value::Null);
                        }
                        Some(v) => cells.push(v),
                    }
                }
            }
        }
        rows.push(QRow {
            cells: Row::new(cells),
            prov: Vec::new(),
        });
    }
    Ok((schema, rows))
}

/// Convert one column XPath result to the column type. `None` = no
/// value (empty node-set), which DEFAULT / NULL handling consumes.
/// Mirrors PG19 xml.c XmlTableGetValue.
fn column_value(
    _q: &mut Q,
    _scopes: &[Scope],
    doc: &Doc,
    nsmap: &[(String, String)],
    ty: ColType,
    exists: bool,
    v: XpValue,
) -> Result<Option<Value>, ExecError> {
    let xp = Xp {
        forest: &doc.nodes,
        nsmap,
        default_ns: None,
    };
    if exists {
        let b = xp.to_bool(&v);
        return Ok(Some(eval_cast(&Value::Bool(b), ty)?));
    }
    match v {
        XpValue::Nodes(ns) => {
            if ns.is_empty() {
                return Ok(None);
            }
            if ty == ColType::Xml {
                // PG concatenates the serialized nodes.
                let mut out = String::new();
                for n in &ns {
                    out.push_str(&xp.node_result_text(n));
                }
                return Ok(Some(Value::xml(out)));
            }
            if ns.len() > 1 {
                return Err(exec_err(
                    "21000",
                    "more than one value returned by column XPath expression".to_string(),
                ));
            }
            let s = xp.node_string_value(&ns[0]);
            Ok(Some(eval_cast(&Value::text(s), ty)?))
        }
        XpValue::Str(s) => {
            if ty == ColType::Xml {
                // PG escapes string results destined for xml columns.
                let mut out = String::new();
                super::xml::escape_text_into(&s, &mut out);
                return Ok(Some(Value::xml(out)));
            }
            Ok(Some(eval_cast(&Value::text(s), ty)?))
        }
        XpValue::Num(n) => {
            let s = xpath_number_text(n);
            if ty == ColType::Xml {
                let mut out = String::new();
                super::xml::escape_text_into(&s, &mut out);
                return Ok(Some(Value::xml(out)));
            }
            Ok(Some(eval_cast(&Value::text(s), ty)?))
        }
        XpValue::Bool(b) => Ok(Some(eval_cast(&Value::Bool(b), ty)?)),
    }
}
