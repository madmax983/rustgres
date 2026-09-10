//! v0.10: COPY text/CSV data formatting and parsing.
//!
//! This module handles the byte-level COPY format (delimiter-separated
//! text and CSV). SQL-level COPY execution lives in `exec.rs`; the wire
//! protocol (CopyInResponse/CopyData/...) lives in `server.rs`.

use crate::sql::{CopyFormat, CopyOptions};

/// A parsed COPY field: text or NULL.
#[derive(Clone, Debug, PartialEq)]
pub enum CopyField {
    Text(String),
    Null,
}

/// A COPY parse failure with a 1-based line number.
#[derive(Clone, Debug)]
pub struct CopyParseError {
    pub line: usize,
    pub message: String,
}

impl CopyParseError {
    fn at(line: usize, message: impl Into<String>) -> Self {
        CopyParseError {
            line,
            message: message.into(),
        }
    }
}

/// Format one row (fields + null flags) into `out`.
pub fn format_row(
    fields: &[String],
    is_null: &[bool],
    out: &mut Vec<u8>,
    options: &CopyOptions,
) {
    match options.format {
        CopyFormat::Text => format_text_row(fields, is_null, out, options),
        CopyFormat::Csv => format_csv_row(fields, is_null, out, options),
    }
}

/// v0.10: Postgres text-format escaping.
fn escape_text(s: &str, out: &mut Vec<u8>, options: &CopyOptions) {
    for b in s.bytes() {
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\x08' => out.extend_from_slice(b"\\b"),
            b'\x0c' => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\x0b' => out.extend_from_slice(b"\\v"),
            d if d == options.delimiter => {
                out.push(b'\\');
                out.push(d);
            }
            _ => out.push(b),
        }
    }
}

fn format_text_row(
    fields: &[String],
    is_null: &[bool],
    out: &mut Vec<u8>,
    options: &CopyOptions,
) {
    for (i, (f, &null)) in fields.iter().zip(is_null.iter()).enumerate() {
        if i > 0 {
            out.push(options.delimiter);
        }
        if null {
            out.extend_from_slice(options.null.as_bytes());
        } else {
            escape_text(f, out, options);
        }
    }
    out.push(b'\n');
}

fn format_csv_row(
    fields: &[String],
    is_null: &[bool],
    out: &mut Vec<u8>,
    options: &CopyOptions,
) {
    for (i, (f, &null)) in fields.iter().zip(is_null.iter()).enumerate() {
        if i > 0 {
            out.push(options.delimiter);
        }
        if null {
            out.extend_from_slice(options.null.as_bytes());
        } else {
            // Quote when the field contains delimiter, quote, newline,
            // or matches the null string (like Postgres).
            let needs_quote = f.bytes().any(|b| {
                b == options.delimiter
                    || b == options.quote
                    || b == b'\n'
                    || b == b'\r'
            }) || *f == options.null;
            if needs_quote {
                out.push(options.quote);
                for b in f.bytes() {
                    if b == options.quote {
                        out.push(options.escape);
                    }
                    out.push(b);
                }
                out.push(options.quote);
            } else {
                out.extend_from_slice(f.as_bytes());
            }
        }
    }
    out.push(b'\n');
}

/// Parse COPY input bytes into rows of fields. `ncols` is the expected
/// column count. Returns rows of `CopyField`.
pub fn parse_rows(
    data: &[u8],
    options: &CopyOptions,
    ncols: usize,
) -> Result<Vec<Vec<CopyField>>, CopyParseError> {
    match options.format {
        CopyFormat::Text => parse_text_rows(data, options, ncols),
        CopyFormat::Csv => parse_csv_rows(data, options, ncols),
    }
}

/// Split into lines (handling \r\n and \n).
fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' {
            let mut end = i;
            if end > start && data[end - 1] == b'\r' {
                end -= 1;
            }
            lines.push(&data[start..end]);
            start = i + 1;
        }
    }
    // Trailing data without a newline is an error (except empty input).
    if start < data.len() {
        lines.push(&data[start..]);
    }
    lines
}

fn parse_text_rows(
    data: &[u8],
    options: &CopyOptions,
    ncols: usize,
) -> Result<Vec<Vec<CopyField>>, CopyParseError> {
    let mut rows = Vec::new();
    let lines = split_lines(data);
    let skip = if options.header { 1 } else { 0 };
    for (li, line) in lines.iter().enumerate() {
        let lineno = li + 1;
        if li < skip {
            continue;
        }
        // An empty line (from a trailing newline) is skipped; a truly
        // empty input line would be a row with one empty field, but
        // COPY data always ends lines with \n.
        if line.is_empty() && li == lines.len() - 1 && data.ends_with(b"\n") {
            continue;
        }
        let fields = split_text_line(line, options.delimiter);
        if fields.len() != ncols {
            return Err(CopyParseError::at(
                lineno,
                format!(
                    "expected {} column(s) but found {}",
                    ncols,
                    fields.len()
                ),
            ));
        }
        let mut row = Vec::with_capacity(ncols);
        for f in fields {
            // NULL check on the raw field (before unescaping).
            if f == options.null.as_bytes() {
                row.push(CopyField::Null);
            } else {
                let s = unescape_text(f, lineno)?;
                row.push(CopyField::Text(s));
            }
        }
        rows.push(row);
    }
    Ok(rows)
}

/// Split a text-format line on the delimiter, respecting backslash
/// escapes (the delimiter can be escaped).
fn split_text_line<'a>(line: &'a [u8], delimiter: u8) -> Vec<&'a [u8]> {
    let mut fields = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < line.len() {
        if line[i] == b'\\' && i + 1 < line.len() {
            i += 2;
            continue;
        }
        if line[i] == delimiter {
            fields.push(&line[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    fields.push(&line[start..]);
    fields
}

/// Unescape a text-format field.
fn unescape_text(field: &[u8], lineno: usize) -> Result<String, CopyParseError> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        if field[i] == b'\\' {
            i += 1;
            if i >= field.len() {
                return Err(CopyParseError::at(
                    lineno,
                    "unexpected backslash at end of field",
                ));
            }
            match field[i] {
                b'b' => out.push(0x08),
                b'f' => out.push(0x0c),
                b'n' => out.push(b'\n'),
                b'r' => out.push(b'\r'),
                b't' => out.push(b'\t'),
                b'v' => out.push(0x0b),
                b'\\' => out.push(b'\\'),
                c => out.push(c), // \<char> -> <char> (e.g. \<delim>)
            }
            i += 1;
        } else {
            out.push(field[i]);
            i += 1;
        }
    }
    String::from_utf8(out)
        .map_err(|_| CopyParseError::at(lineno, "invalid UTF-8 in COPY data"))
}

fn parse_csv_rows(
    data: &[u8],
    options: &CopyOptions,
    ncols: usize,
) -> Result<Vec<Vec<CopyField>>, CopyParseError> {
    let mut rows = Vec::new();
    let mut row: Vec<CopyField> = Vec::new();
    let mut field = Vec::new();
    let mut in_quotes = false;
    let mut lineno = 1;
    let mut i = 0;

    // Whether the current field was quoted (for NULL detection).
    let mut field_quoted = false;

    macro_rules! end_field {
        () => {{
            // `replace` both reads the flag and resets it for the next
            // field (a plain assignment trips `unused_assignments` on the
            // final iteration).
            let was_quoted = std::mem::replace(&mut field_quoted, false);
            let f = if !was_quoted && field == options.null.as_bytes() {
                CopyField::Null
            } else {
                let s = String::from_utf8(std::mem::take(&mut field)).map_err(|_| {
                    CopyParseError::at(lineno, "invalid UTF-8 in COPY data")
                })?;
                CopyField::Text(s)
            };
            row.push(f);
        }};
    }

    macro_rules! end_row {
        () => {{
            end_field!();
            if row.len() != ncols {
                return Err(CopyParseError::at(
                    lineno,
                    format!("expected {} column(s) but found {}", ncols, row.len()),
                ));
            }
            rows.push(std::mem::take(&mut row));
        }};
    }

    let skip_header = options.header;
    let mut rows_skipped = 0;

    while i < data.len() {
        let b = data[i];
        if in_quotes {
            if b == options.escape && i + 1 < data.len() && data[i + 1] == options.quote {
                // Escaped quote ("").
                field.push(options.quote);
                i += 2;
                continue;
            }
            if b == options.quote {
                in_quotes = false;
                i += 1;
                continue;
            }
            field.push(b);
            i += 1;
        } else if b == options.quote && field.is_empty() {
            in_quotes = true;
            field_quoted = true;
            i += 1;
        } else if b == options.delimiter {
            end_field!();
            i += 1;
        } else if b == b'\n' {
            end_row!();
            if skip_header && rows_skipped == 0 && !rows.is_empty() {
                rows.pop();
                rows_skipped += 1;
            }
            lineno += 1;
            i += 1;
        } else if b == b'\r' {
            // Allow \r\n; a bare \r is data.
            if i + 1 < data.len() && data[i + 1] == b'\n' {
                i += 1; // the \n arm handles it
            } else {
                field.push(b);
                i += 1;
            }
        } else {
            field.push(b);
            i += 1;
        }
    }
    if in_quotes {
        return Err(CopyParseError::at(
            lineno,
            "unterminated quoted field",
        ));
    }
    // Trailing field/row without a newline.
    if !field.is_empty() || !row.is_empty() || field_quoted {
        end_row!();
        if skip_header && rows_skipped == 0 {
            rows.pop();
        }
    }
    Ok(rows)
}
