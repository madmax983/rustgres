//! v2.44: minimal geometric types (PG19 point / box / circle),
//! carried as canonical-text `Value::Text` (see storage::ColType).
//! Input parsing follows PG's point_in / box_in / circle_in
//! (geo_ops.c): points are `(x, y)`, boxes are two points in either
//! order (stored high-corner first), circles are `<(x, y), r>` or
//! `(x, y), r` / a bare point (radius 0). Output rendering uses
//! PG's float8out discipline via Rust's shortest-round-trip f64
//! Display, which agrees with PG on the values the regression
//! suites exercise (integral and short-decimal coordinates).
use super::*;

/// PG float8 rendering for geometry output (geo_ops.c uses
/// float8out_internal with extra_float_digits handling; the
/// canonical stored text only ever carries values this renders).
pub(crate) fn geo_f64(v: f64) -> String {
    if v == 0.0 {
        return "0".to_string();
    }
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    format!("{}", v)
}

fn parse_f64(s: &str) -> Option<f64> {
    let t = s.trim();
    match t {
        "Infinity" | "infinity" | "+Infinity" => Some(f64::INFINITY),
        "-Infinity" => Some(f64::NEG_INFINITY),
        "NaN" | "nan" => Some(f64::NAN),
        _ => t.parse::<f64>().ok(),
    }
}

/// Parse one point body `x, y` (no surrounding parens required).
fn parse_point_body(s: &str) -> Option<(f64, f64)> {
    let mut it = s.split(',');
    let x = parse_f64(it.next()?)?;
    let y = parse_f64(it.next()?)?;
    if it.next().is_some() {
        return None;
    }
    Some((x, y))
}

pub(crate) fn point_to_text(x: f64, y: f64) -> String {
    format!("({},{})", geo_f64(x), geo_f64(y))
}

/// PG point_in: `(x, y)` or `x, y`.
pub(crate) fn parse_point(s: &str) -> Option<(f64, f64)> {
    let t = s.trim();
    let inner = if t.starts_with('(') && t.ends_with(')') {
        &t[1..t.len() - 1]
    } else {
        t
    };
    parse_point_body(inner)
}

pub(crate) fn canonical_point(s: &str) -> Option<String> {
    let (x, y) = parse_point(s)?;
    Some(point_to_text(x, y))
}

/// A box as (high, low) corners, PG's stored order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GeoBox {
    pub hx: f64,
    pub hy: f64,
    pub lx: f64,
    pub ly: f64,
}

/// PG box_in: `(x1, y1), (x2, y2)` or `(x1, y1, x2, y2)` (a single
/// parenthesized list of four numbers), normalized so the first
/// corner is the high one.
pub(crate) fn parse_box(s: &str) -> Option<GeoBox> {
    let t = s.trim();
    // Point extraction: any parenthesized groups are points (this
    // covers "((x,y),(x,y))" and the canonical "(x,y),(x,y)"); a
    // single 4-number list (optionally parenthesized) is two
    // points back to back.
    let pts: Vec<(f64, f64)> = if t.contains('(') && t.matches('(').count() >= 2 {
        let mut out = Vec::new();
        let mut rest = t;
        // Tolerate one outer wrapper pair.
        if rest.starts_with("((") && rest.ends_with("))") {
            rest = &rest[1..rest.len() - 1];
        }
        loop {
            let rest_t = rest.trim_start();
            if rest_t.is_empty() {
                break;
            }
            if !rest_t.starts_with('(') {
                return None;
            }
            let close = rest_t.find(')')?;
            out.push(parse_point_body(&rest_t[1..close])?);
            rest = &rest_t[close + 1..];
            let rest_t2 = rest.trim_start();
            if let Some(r2) = rest_t2.strip_prefix(',') {
                rest = r2;
            } else if !rest_t2.is_empty() {
                return None;
            } else {
                rest = rest_t2;
            }
        }
        out
    } else if t.starts_with('(') && t.ends_with(')') && t.matches(',').count() == 3 {
        let inner = &t[1..t.len() - 1];
        let nums: Vec<&str> = inner.split(',').collect();
        vec![
            (parse_f64(nums[0])?, parse_f64(nums[1])?),
            (parse_f64(nums[2])?, parse_f64(nums[3])?),
        ]
    } else {
        // Bare `x1, y1, x2, y2` or two points separated by comma at
        // top level with parens handled above; try the 4-number form.
        let nums: Vec<&str> = t.split(',').collect();
        if nums.len() != 4 {
            return None;
        }
        vec![
            (parse_f64(nums[0])?, parse_f64(nums[1])?),
            (parse_f64(nums[2])?, parse_f64(nums[3])?),
        ]
    };
    if pts.len() != 2 {
        return None;
    }
    let (ax, ay) = pts[0];
    let (bx, by) = pts[1];
    Some(GeoBox {
        hx: ax.max(bx),
        hy: ay.max(by),
        lx: ax.min(bx),
        ly: ay.min(by),
    })
}

pub(crate) fn box_to_text(b: &GeoBox) -> String {
    // PG box_out: "(high),(low)" with no outer wrapper.
    format!(
        "{},{}",
        point_to_text(b.hx, b.hy),
        point_to_text(b.lx, b.ly)
    )
}

pub(crate) fn canonical_box(s: &str) -> Option<String> {
    Some(box_to_text(&parse_box(s)?))
}

/// A circle: center + radius.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GeoCircle {
    pub cx: f64,
    pub cy: f64,
    pub r: f64,
}

/// PG circle_in: `<(x, y), r>`, `((x, y), r)`, `(x, y), r`, or a
/// bare point / `(x, y)` (radius 0).
pub(crate) fn parse_circle(s: &str) -> Option<GeoCircle> {
    let mut t = s.trim();
    if t.starts_with('<') && t.ends_with('>') {
        t = &t[1..t.len() - 1];
    }
    // Find the point part: it ends at the first ')' that closes it.
    if t.starts_with('(') {
        let close = t.find(')')?;
        let (cx, cy) = parse_point_body(&t[1..close])?;
        let rest = t[close + 1..].trim();
        if rest.is_empty() {
            return Some(GeoCircle { cx, cy, r: 0.0 });
        }
        let rest = rest.strip_prefix(',')?.trim();
        // rest may itself be parenthesized one more level: `((x,y),r)`.
        let rest = rest.strip_suffix(')').unwrap_or(rest);
        let r = parse_f64(rest)?;
        if r < 0.0 {
            return None;
        }
        return Some(GeoCircle { cx, cy, r });
    }
    // Bare point without parens: `x, y` or `x, y, r`.
    let parts: Vec<&str> = t.split(',').collect();
    match parts.len() {
        2 => {
            let (cx, cy) = parse_point_body(t)?;
            Some(GeoCircle { cx, cy, r: 0.0 })
        }
        3 => {
            let cx = parse_f64(parts[0])?;
            let cy = parse_f64(parts[1])?;
            let r = parse_f64(parts[2])?;
            if r < 0.0 {
                return None;
            }
            Some(GeoCircle { cx, cy, r })
        }
        _ => None,
    }
}

pub(crate) fn circle_to_text(c: &GeoCircle) -> String {
    format!("<{},{}>", point_to_text(c.cx, c.cy), geo_f64(c.r))
}

pub(crate) fn canonical_circle(s: &str) -> Option<String> {
    Some(circle_to_text(&parse_circle(s)?))
}

/// Validate + canonicalize a text input for a geometric column /
/// cast target (PG's *_in functions). Error text follows geo_ops.c.
pub(crate) fn geo_input(ty: ColType, s: &str) -> Result<String, ExecError> {
    // PG19 float8in: a numeric token that overflows float8 is
    // 22003 ("<tok>" is out of range for type double precision),
    // distinct from a syntax error; literal infinity is accepted.
    for tok in s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '+' || c == '-')) {
        if tok.is_empty() {
            continue;
        }
        if let Ok(v) = tok.parse::<f64>() {
            let lt = tok.to_ascii_lowercase();
            if v.is_infinite()
                && !matches!(
                    lt.as_str(),
                    "inf" | "infinity" | "+inf" | "-inf" | "+infinity" | "-infinity"
                )
            {
                return Err(exec_err(
                    "22003",
                    format!("\"{}\" is out of range for type double precision", tok),
                ));
            }
        }
    }
    let (name, canon) = match ty {
        ColType::Point => ("point", canonical_point(s)),
        ColType::BoxGeo => ("box", canonical_box(s)),
        ColType::Circle => ("circle", canonical_circle(s)),
        _ => return Ok(s.to_string()),
    };
    canon.ok_or_else(|| {
        exec_err(
            "22P02",
            format!("invalid input syntax for type {}: \"{}\"", name, s),
        )
    })
}

fn f64_arg(v: &Value) -> Option<f64> {
    match v {
        Value::Float(f) => Some(*f),
        Value::Float4(f) => Some(*f as f64),
        Value::Int(i) | Value::BigInt(i) => Some(*i as f64),
        Value::SmallInt(i) => Some(*i as f64),
        Value::Numeric(n) => Some(n.to_f64()),
        _ => None,
    }
}

/// v2.44: geometric builtins on evaluated values (PG19 geo_ops.c):
/// circle_center(circle), center(box|circle), point(float8, float8),
/// box(point, point). Returns None for non-geometric names.
pub(crate) fn try_eval_geo_func(name: &str, vals: &[Value]) -> Option<Result<Value, ExecError>> {
    let text_of = |v: &Value| -> Option<String> {
        match v {
            Value::Text(s) | Value::BpChar(s) => Some(s.to_string()),
            _ => None,
        }
    };
    match name {
        "circle_center" if vals.len() == 1 => {
            if matches!(vals[0], Value::Null) {
                return Some(Ok(Value::Null));
            }
            let s = text_of(&vals[0])?;
            Some(match parse_circle(&s) {
                Some(c) => Ok(Value::text(point_to_text(c.cx, c.cy))),
                None => Err(geo_input(ColType::Circle, &s).unwrap_err()),
            })
        }
        "center" if vals.len() == 1 => {
            if matches!(vals[0], Value::Null) {
                return Some(Ok(Value::Null));
            }
            let s = text_of(&vals[0])?;
            if let Some(c) = parse_circle(&s) {
                return Some(Ok(Value::text(point_to_text(c.cx, c.cy))));
            }
            Some(match parse_box(&s) {
                Some(b) => Ok(Value::text(point_to_text(
                    (b.hx + b.lx) / 2.0,
                    (b.hy + b.ly) / 2.0,
                ))),
                None => Err(geo_input(ColType::BoxGeo, &s).unwrap_err()),
            })
        }
        "point" if vals.len() == 2 => {
            if vals.iter().any(|v| matches!(v, Value::Null)) {
                return Some(Ok(Value::Null));
            }
            let x = f64_arg(&vals[0])?;
            let y = f64_arg(&vals[1])?;
            Some(Ok(Value::text(point_to_text(x, y))))
        }
        "box" if vals.len() == 2 => {
            if vals.iter().any(|v| matches!(v, Value::Null)) {
                return Some(Ok(Value::Null));
            }
            let a = parse_point(&text_of(&vals[0])?)?;
            let b = parse_point(&text_of(&vals[1])?)?;
            let bx = GeoBox {
                hx: a.0.max(b.0),
                hy: a.1.max(b.1),
                lx: a.0.min(b.0),
                ly: a.1.min(b.1),
            };
            Some(Ok(Value::text(box_to_text(&bx))))
        }
        // PG19 circle(point, float8) constructor (geo_ops.c
        // circle_construct): a negative radius is 22023.
        "circle" if vals.len() == 2 => {
            if vals.iter().any(|v| matches!(v, Value::Null)) {
                return Some(Ok(Value::Null));
            }
            let p = parse_point(&text_of(&vals[0])?)?;
            let r = f64_arg(&vals[1])?;
            if r < 0.0 {
                return Some(Err(exec_err(
                    "22023",
                    "circle radius must not be negative".to_string(),
                )));
            }
            Some(Ok(Value::text(circle_to_text(&GeoCircle {
                cx: p.0,
                cy: p.1,
                r,
            }))))
        }
        _ => None,
    }
}

/// PG circle `&&` (circle_overlap): centers within r1 + r2.
pub(crate) fn circles_overlap(a: &GeoCircle, b: &GeoCircle) -> bool {
    let dx = a.cx - b.cx;
    let dy = a.cy - b.cy;
    (dx * dx + dy * dy).sqrt() <= a.r + b.r
}

/// PG box `&&` (box_overlap).
pub(crate) fn boxes_overlap(a: &GeoBox, b: &GeoBox) -> bool {
    a.lx <= b.hx && b.lx <= a.hx && a.ly <= b.hy && b.ly <= a.hy
}

/// PG box `<@` (box_contained): a entirely within b.
pub(crate) fn box_contained(a: &GeoBox, b: &GeoBox) -> bool {
    a.lx >= b.lx && a.hx <= b.hx && a.ly >= b.ly && a.hy <= b.hy
}

/// v2.44: `__point_subscript_assign(base, path, newval)` — PG19
/// subscripting for point treats it as a fixed-length float8[2]:
/// index 0 is x, 1 is y; any other index is 2202E. A NULL base is
/// a no-op (stays NULL); a NULL assigned value is ignored (the
/// base is returned unchanged); a NULL subscript is 22004.
pub(crate) fn eval_point_subscript_assign(vals: &[Value]) -> Result<Value, ExecError> {
    let (_, path) =
        match crate::exec::json::json_path_list_arg("__point_subscript_assign", &vals[1])? {
            None => return Ok(Value::Null),
            Some(p) => p,
        };
    if path.iter().any(|e| e.is_none()) {
        return Err(exec_err(
            "22004",
            "array subscript in assignment must not be null",
        ));
    }
    if matches!(vals[0], Value::Null) {
        return Ok(Value::Null);
    }
    let idx: i64 = path
        .first()
        .and_then(|s| s.as_ref())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(-1);
    if path.len() != 1 || !(0..=1).contains(&idx) {
        return Err(exec_err("2202E", "array subscript out of range"));
    }
    if matches!(vals[2], Value::Null) {
        return Ok(vals[0].clone());
    }
    let base = match &vals[0] {
        Value::Text(s) | Value::BpChar(s) => s.clone(),
        _ => return Ok(vals[0].clone()),
    };
    let Some(mut pt) = parse_point(&base) else {
        return Ok(vals[0].clone());
    };
    let nv = match &vals[2] {
        Value::Float(f) => *f,
        Value::Float4(f) => *f as f64,
        Value::Int(i) | Value::BigInt(i) => *i as f64,
        Value::SmallInt(i) => *i as f64,
        Value::Numeric(n) => n.to_f64(),
        Value::Text(s) | Value::BpChar(s) => match s.parse::<f64>() {
            Ok(f) => f,
            Err(_) => return Ok(vals[0].clone()),
        },
        _ => return Ok(vals[0].clone()),
    };
    if idx == 0 {
        pt.0 = nv;
    } else {
        pt.1 = nv;
    }
    Ok(Value::text(point_to_text(pt.0, pt.1)))
}
