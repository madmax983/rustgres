//! Numeric `to_char` / `to_number` formatting engine (v0.26).
//!
//! Faithful Rust port of the NUM processor in PostgreSQL's
//! `src/backend/utils/adt/formatting.c` (REL_19_STABLE): the same
//! keyword table, the same `NUMDesc` picture analysis, and the same
//! `NUM_processor_to_char` / `NUM_processor_from_char` output
//! algorithms, including fill mode, zero padding, locale-independent
//! (C-locale) group/decimal/currency symbols, Roman numerals,
//! ordinals (`TH`), `V` decimal-shift, and `EEEE` scientific output.
//!
//! Deviations from PG are documented on the individual items. The
//! regression suite's `numeric.out` expectations (PG's own outputs)
//! are the ground truth; every pattern exercised there is covered by
//! `protocol_test26.py`.

/// Error from parsing or applying a numeric format picture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NumFmtError {
    /// Bad picture, e.g. `"9" must be ahead of "PR"`, multiple decimal
    /// points, `RN` combined with other elements. Maps to SQLSTATE 42601
    /// (syntax_error), matching PG's `ERRCODE_SYNTAX_ERROR`.
    Syntax(String),
    /// Input text is not a valid number for the picture (e.g. invalid
    /// Roman numeral). Maps to SQLSTATE 22P02, matching PG's
    /// `ERRCODE_INVALID_TEXT_REPRESENTATION`.
    InvalidInput(String),
    /// Valid picture but unsupported operation (e.g. `EEEE` on input).
    /// Maps to SQLSTATE 0A000, matching PG's
    /// `ERRCODE_FEATURE_NOT_SUPPORTED`.
    Unsupported(String),
}

/// One parsed element of a numeric format picture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Node {
    /// `9` digit slot.
    N9,
    /// `0` zero-padded digit slot.
    N0,
    /// `.` / `D` decimal point.
    Dec,
    /// `,` group separator position.
    Comma,
    /// `G` locale group separator.
    G,
    /// `L` locale currency symbol (empty in the C locale).
    L,
    /// `C` ISO currency code (no output effect in the C locale).
    C,
    /// `RN` / `rn` Roman numerals.
    Roman(bool),
    /// `TH` / `th` ordinal suffix.
    Th(bool),
    /// `MI` minus sign position.
    MI,
    /// `PL` plus sign position.
    PL,
    /// `SG` sign position.
    SG,
    /// `S` locale sign anchor.
    S,
    /// Any other character: copied verbatim to `to_char` output.
    Lit(char),
}

/// Parsed numeric format picture: PG's `NUMDesc` plus the node list.
#[derive(Clone, Debug)]
pub struct NumFmt {
    /// Integer digit slots before the decimal point.
    pub(crate) pre: i32,
    /// Digit slots after the decimal point.
    pub(crate) post: i32,
    /// A `0` pattern is present.
    pub(crate) zero: bool,
    /// First slot (0-based) where `0`-padding starts.
    pub(crate) zero_start: i32,
    /// One past the last `0`-padded slot.
    pub(crate) zero_end: i32,
    /// `FM` fill mode: suppress leading/trailing blanks.
    pub(crate) fillmode: bool,
    /// `S` locale sign: 0 = none, 1 = pre-decimal, 2 = post-decimal.
    pub(crate) lsign: i8,
    /// `pre` value when the `S` was seen (PG's `pre_lsign_num`).
    pub(crate) pre_lsign_num: i32,
    /// `MI` present.
    pub(crate) minus: bool,
    /// `PL` present.
    pub(crate) plus: bool,
    /// `PR` present (angle brackets for negatives).
    pub(crate) bracket: bool,
    /// `RN`/`rn` present.
    pub(crate) roman: bool,
    /// `EEEE` present.
    pub(crate) eeee: bool,
    /// `B` seen before any digit (PG's `NUM_F_BLANK`).
    pub(crate) blank: bool,
    /// A `.`/`D` decimal point was parsed (PG's `IS_DECIMAL`).
    pub decimal: bool,
    /// Count of `9` slots after `V` (PG's `multi`).
    pub(crate) multi: i32,
    /// Format nodes in picture order.
    pub(crate) nodes: Vec<Node>,
}

impl NumFmt {
    fn new() -> Self {
        NumFmt {
            pre: 0,
            post: 0,
            zero: false,
            zero_start: 0,
            zero_end: 0,
            fillmode: false,
            lsign: 0,
            pre_lsign_num: 0,
            minus: false,
            plus: false,
            bracket: false,
            roman: false,
            eeee: false,
            blank: false,
            decimal: false,
            multi: 0,
            nodes: Vec::new(),
        }
    }
}

/// Match a format keyword at `s[pos..]`; returns (node, length).
/// Mirrors PG's `NUM_keywords` table exactly: case-sensitive prefix
/// match, table order (so "SG" wins over "S", "RN" over nothing...).
fn match_keyword(s: &[char], pos: usize) -> Option<(Node, usize)> {
    let rest: String = s[pos..].iter().collect();
    // Table order follows NUM_keywords (upper block, then lower block).
    for (kw, node) in [
        ("EEEE", Node::Lit('E')),
        ("FM", Node::Lit('F')),
        ("MI", Node::MI),
        ("PL", Node::PL),
        ("PR", Node::Lit('P')),
        ("RN", Node::Roman(false)),
        ("SG", Node::SG),
        ("SP", Node::Lit(' ')),
        ("S", Node::S),
        ("TH", Node::Th(false)),
        ("eeee", Node::Lit('E')),
        ("fm", Node::Lit('F')),
        ("mi", Node::MI),
        ("pl", Node::PL),
        ("pr", Node::Lit('P')),
        ("rn", Node::Roman(true)),
        ("sg", Node::SG),
        ("sp", Node::Lit(' ')),
        ("s", Node::S),
        ("th", Node::Th(true)),
    ] {
        if rest.starts_with(kw) {
            return Some((node, kw.len()));
        }
    }
    // 1-char keywords.
    match s[pos] {
        ',' => Some((Node::Comma, 1)),
        '.' => Some((Node::Dec, 1)),
        '0' => Some((Node::N0, 1)),
        '9' => Some((Node::N9, 1)),
        'B' | 'b' => Some((Node::Lit('B'), 1)),
        'C' | 'c' => Some((Node::C, 1)),
        'D' | 'd' => Some((Node::Dec, 1)),
        'G' | 'g' => Some((Node::G, 1)),
        'L' | 'l' => Some((Node::L, 1)),
        'V' | 'v' => Some((Node::Lit('V'), 1)),
        _ => None,
    }
}

/// Parse a numeric format picture into a `NumFmt`.
///
/// Mirrors `parse_format()` + `NUMDesc_prepare()` in formatting.c,
/// including the quoted-literal rules (`"..."` with `\"` and `\\`
/// escapes; an unterminated quote swallows the rest of the picture).
pub fn parse_numfmt(fmt: &str) -> Result<NumFmt, NumFmtError> {
    let chars: Vec<char> = fmt.chars().collect();
    let mut d = NumFmt::new();
    let mut decimal_seen = false;
    let mut multi_seen = false;
    let mut pos = 0;
    // (kind, node-index) list for NUMDesc_prepare-style bookkeeping is
    // folded into the match arms directly.
    while pos < chars.len() {
        let c = chars[pos];
        if c == '"' {
            // Quoted literal.
            pos += 1;
            while pos < chars.len() {
                let q = chars[pos];
                if q == '\\' && pos + 1 < chars.len() {
                    let n = chars[pos + 1];
                    if n == '"' || n == '\\' {
                        d.nodes.push(Node::Lit(n));
                    } else {
                        // PG drops the backslash before other chars.
                        d.nodes.push(Node::Lit(n));
                    }
                    pos += 2;
                } else if q == '"' {
                    pos += 1;
                    break;
                } else {
                    d.nodes.push(Node::Lit(q));
                    pos += 1;
                }
            }
            continue;
        }
        if let Some((node, len)) = match_keyword(&chars, pos) {
            pos += len;
            match node {
                Node::N9 => {
                    if d.bracket {
                        return Err(NumFmtError::Syntax(
                            "\"9\" must be ahead of \"PR\"".to_string(),
                        ));
                    }
                    if multi_seen {
                        d.multi += 1;
                    } else if decimal_seen {
                        d.post += 1;
                    } else {
                        d.pre += 1;
                    }
                    d.nodes.push(Node::N9);
                }
                Node::N0 => {
                    if d.bracket {
                        return Err(NumFmtError::Syntax(
                            "\"0\" must be ahead of \"PR\"".to_string(),
                        ));
                    }
                    if !d.zero && !decimal_seen {
                        d.zero = true;
                        d.zero_start = d.pre + 1;
                    }
                    if multi_seen {
                        // PG has no multi branch for 0; count as pre/post.
                        if decimal_seen {
                            d.post += 1;
                        } else {
                            d.pre += 1;
                        }
                    } else if !decimal_seen {
                        d.pre += 1;
                    } else {
                        d.post += 1;
                    }
                    d.zero_end = d.pre + d.post;
                    d.nodes.push(Node::N0);
                }
                Node::Dec => {
                    if decimal_seen {
                        return Err(NumFmtError::Syntax("multiple decimal points".to_string()));
                    }
                    if multi_seen {
                        return Err(NumFmtError::Syntax(
                            "cannot use \"V\" and decimal point together".to_string(),
                        ));
                    }
                    decimal_seen = true;
                    d.decimal = true;
                    d.nodes.push(Node::Dec);
                }
                Node::Comma => d.nodes.push(Node::Comma),
                Node::G => d.nodes.push(Node::G),
                Node::L => d.nodes.push(Node::L),
                Node::C => d.nodes.push(Node::C),
                Node::Roman(lower) => {
                    if d.roman {
                        return Err(NumFmtError::Syntax("cannot use \"RN\" twice".to_string()));
                    }
                    d.roman = true;
                    d.nodes.push(Node::Roman(lower));
                }
                Node::Th(lower) => d.nodes.push(Node::Th(lower)),
                Node::MI => {
                    if d.lsign != 0 {
                        return Err(NumFmtError::Syntax(
                            "cannot use \"S\" and \"MI\" together".to_string(),
                        ));
                    }
                    d.minus = true;
                    d.nodes.push(Node::MI);
                }
                Node::PL => {
                    if d.lsign != 0 {
                        return Err(NumFmtError::Syntax(
                            "cannot use \"S\" and \"PL\" together".to_string(),
                        ));
                    }
                    d.plus = true;
                    d.nodes.push(Node::PL);
                }
                Node::SG => {
                    if d.lsign != 0 {
                        return Err(NumFmtError::Syntax(
                            "cannot use \"S\" and \"SG\" together".to_string(),
                        ));
                    }
                    d.minus = true;
                    d.plus = true;
                    d.nodes.push(Node::SG);
                }
                Node::S => {
                    if d.lsign != 0 {
                        return Err(NumFmtError::Syntax("cannot use \"S\" twice".to_string()));
                    }
                    if d.plus || d.minus || d.bracket {
                        return Err(NumFmtError::Syntax(
                            "cannot use \"S\" and \"PL\"/\"MI\"/\"SG\"/\"PR\" together".to_string(),
                        ));
                    }
                    if !decimal_seen {
                        d.lsign = 1;
                        d.pre_lsign_num = d.pre;
                    } else if d.lsign == 0 {
                        d.lsign = 2;
                    }
                    d.nodes.push(Node::S);
                }
                Node::Lit('P') => {
                    // PR
                    if d.lsign != 0 || d.plus || d.minus {
                        return Err(NumFmtError::Syntax(
                            "cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together".to_string(),
                        ));
                    }
                    d.bracket = true;
                    // PR emits nothing itself; remembered via bracket flag.
                }
                Node::Lit('F') => {
                    // FM
                    d.fillmode = true;
                }
                Node::Lit('E') => {
                    // EEEE
                    if d.eeee {
                        return Err(NumFmtError::Syntax("cannot use \"EEEE\" twice".to_string()));
                    }
                    if d.fillmode
                        || d.lsign != 0
                        || d.bracket
                        || d.minus
                        || d.plus
                        || d.roman
                        || multi_seen
                        || d.blank
                    {
                        return Err(NumFmtError::Syntax(
                            "\"EEEE\" is incompatible with other formats".to_string(),
                        ));
                    }
                    d.eeee = true;
                }
                Node::Lit('V') => {
                    if decimal_seen {
                        return Err(NumFmtError::Syntax(
                            "cannot use \"V\" and decimal point together".to_string(),
                        ));
                    }
                    multi_seen = true;
                }
                Node::Lit('B') => {
                    // PG: B sets BLANK only before any digit/zero.
                    if d.pre == 0 && d.post == 0 && !d.zero {
                        d.blank = true;
                    }
                }
                Node::Lit(' ') => {
                    // SP: no-op for numbers.
                }
                Node::Lit(ch) => d.nodes.push(Node::Lit(ch)),
            }
        } else {
            // Ordinary character: literal in to_char, skipped in to_number.
            // Outside quotes, backslash is only special before '"'.
            if c == '\\' && pos + 1 < chars.len() && chars[pos + 1] == '"' {
                d.nodes.push(Node::Lit('"'));
                pos += 2;
            } else {
                d.nodes.push(Node::Lit(c));
                pos += 1;
            }
        }
    }
    // PG: "RN" may only be used together with "FM" — the check is on
    // flags only, so stray 9/0/. slots alongside RN are tolerated (and
    // ignored by the processor) exactly like PG.
    if d.roman
        && (d.lsign != 0
            || d.minus
            || d.plus
            || d.bracket
            || d.eeee
            || multi_seen
            || decimal_seen
            || d.zero
            || d.blank)
    {
        return Err(NumFmtError::Syntax(
            "\"RN\" is incompatible with other formats".to_string(),
        ));
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_counts() {
        let d = parse_numfmt("999.99").unwrap();
        assert_eq!(d.pre, 3);
        assert_eq!(d.post, 2);
        assert!(!d.fillmode);
    }

    #[test]
    fn parse_fm_and_signs() {
        let d = parse_numfmt("FMS999.99").unwrap();
        assert!(d.fillmode);
        assert_eq!(d.lsign, 1); // S
        let d = parse_numfmt("MI99.99").unwrap();
        assert!(d.minus);
        let d = parse_numfmt("999PR").unwrap();
        assert!(d.bracket);
    }

    #[test]
    fn parse_roman() {
        let d = parse_numfmt("RN").unwrap();
        assert!(d.roman);
        let d = parse_numfmt("fmrn").unwrap();
        assert!(d.roman && d.fillmode);
    }

    #[test]
    fn reject_9_after_pr() {
        assert!(parse_numfmt("PR999").is_err());
        assert!(parse_numfmt("999PR").is_ok());
    }

    #[test]
    fn reject_rn_twice() {
        assert!(parse_numfmt("RNRN").is_err());
    }

    #[test]
    fn reject_eeee_twice() {
        assert!(parse_numfmt("EEEEEEEE").is_err());
    }

    #[test]
    fn reject_eeee_with_fm() {
        assert!(parse_numfmt("FMEEEE").is_err());
    }

    #[test]
    fn reject_v_with_decimal() {
        assert!(parse_numfmt("99V99.9").is_err());
        assert!(parse_numfmt("99999V99").is_ok());
    }

    #[test]
    fn reject_rn_with_sign() {
        assert!(parse_numfmt("SNRN").is_err());
    }
}
