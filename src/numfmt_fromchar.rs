//! `to_number` side of the numeric formatting engine (v0.26).
//!
//! Ports `numeric_to_number` + `NUM_processor_from_char` from
//! PostgreSQL's formatting.c. The processor scans the input text
//! according to the format picture and builds a standard decimal
//! string that is then parsed as a numeric.

use super::numfmt::{Node, NumFmt, NumFmtError};
use super::storage::{Numeric, NumericSpecial};

/// Parse a Roman numeral at `input[pos..]`; returns (value, chars
/// consumed) or None if invalid. Mirrors PG's `roman_to_int` exactly:
/// skips leading whitespace, consumes up to 15 valid numerals
/// (case-insensitive), and enforces the standard-form rules.
fn roman_to_int(input: &[char], pos: usize) -> Option<(i64, usize)> {
    // Skip leading whitespace.
    let mut p = pos;
    while p < input.len() && input[p].is_whitespace() {
        p += 1;
    }
    // Collect and decode valid roman numerals, at most 15.
    let mut chars: Vec<char> = Vec::new();
    let mut vals: Vec<i64> = Vec::new();
    while chars.len() < 15 && p < input.len() {
        let v = match input[p].to_ascii_uppercase() {
            'I' => 1,
            'V' => 5,
            'X' => 10,
            'L' => 50,
            'C' => 100,
            'D' => 500,
            'M' => 1000,
            _ => break,
        };
        chars.push(input[p].to_ascii_uppercase());
        vals.push(v);
        p += 1;
    }
    if chars.is_empty() {
        return None;
    }
    let consumed = p - pos;

    let mut result = 0i64;
    let mut repeat_count = 1i32;
    let mut v_count = 0i32;
    let mut l_count = 0i32;
    let mut d_count = 0i32;
    let mut subtraction_encountered = false;
    let mut last_subtracted_value = 0i64;

    let n = vals.len();
    let mut i = 0;
    while i < n {
        let curr_char = chars[i];
        let curr_value = vals[i];

        // No numeral >= the subtracted numeral may follow a subtraction.
        if subtraction_encountered && curr_value >= last_subtracted_value {
            return None;
        }
        // V, L, D must not be repeated nor followed by a larger numeral.
        if (v_count > 0 && curr_value >= 5)
            || (l_count > 0 && curr_value >= 50)
            || (d_count > 0 && curr_value >= 500)
        {
            return None;
        }
        match curr_char {
            'V' => v_count += 1,
            'L' => l_count += 1,
            'D' => d_count += 1,
            _ => {}
        }

        if i < n - 1 {
            let next_char = chars[i + 1];
            let next_value = vals[i + 1];
            if curr_value < next_value {
                // Subtraction: must be a valid subtractive pair.
                let valid = matches!(
                    (curr_char, next_char),
                    ('I', 'V') | ('I', 'X') | ('X', 'L') | ('X', 'C') | ('C', 'D') | ('C', 'M')
                );
                if !valid {
                    return None;
                }
                // Reject repeats with subtraction (e.g. 'MCCM').
                if repeat_count > 1 {
                    return None;
                }
                // V/L/D checks for the skipped numeral.
                if (v_count > 0 && next_value >= 5)
                    || (l_count > 0 && next_value >= 50)
                    || (d_count > 0 && next_value >= 500)
                {
                    return None;
                }
                match next_char {
                    'V' => v_count += 1,
                    'L' => l_count += 1,
                    'D' => d_count += 1,
                    _ => {}
                }
                i += 1; // skip the next numeral
                repeat_count = 1;
                subtraction_encountered = true;
                last_subtracted_value = curr_value;
                result += next_value - curr_value;
            } else {
                if curr_char == next_char {
                    repeat_count += 1;
                    if repeat_count > 3 {
                        return None;
                    }
                } else {
                    repeat_count = 1;
                }
                result += curr_value;
            }
        } else {
            result += curr_value;
        }
        i += 1;
    }
    Some((result, consumed))
}

/// Processor state for `NUM_processor_from_char`.
struct FromChar<'a> {
    desc: &'a NumFmt,
    input: Vec<char>,
    input_p: usize,
    /// Output being built; `out[0]` is the sign slot (' ', '+' or '-').
    out: Vec<char>,
    read_pre: i32,
    read_post: i32,
    read_dec: bool,
}

impl<'a> FromChar<'a> {
    fn overloaded(&self) -> bool {
        self.input_p >= self.input.len()
    }

    /// Skip up to `n` input chars that are not numeric data
    /// (`NUM_eat_non_data_chars`).
    fn eat_non_data(&mut self, n: usize) {
        let mut left = n;
        while left > 0 && !self.overloaded() {
            let c = self.input[self.input_p];
            if matches!(c, '0'..='9' | '.' | ',' | '+' | '-') {
                break;
            }
            self.input_p += 1;
            left -= 1;
        }
    }

    /// `NUM_numpart_from_char` for `9` / `0` / `.` / `D`.
    /// Returns true if the caller should do the trailing `input_p++`.
    fn numpart(&mut self, id: &Node) -> bool {
        if self.overloaded() {
            return false;
        }
        if self.input[self.input_p] == ' ' {
            self.input_p += 1;
        }
        if self.overloaded() {
            return false;
        }

        // Read sign before the number.
        let is_digit_slot = matches!(id, Node::N0 | Node::N9);
        if self.out[0] == ' ' && is_digit_slot && self.read_pre + self.read_post == 0 {
            if self.desc.lsign == 1 {
                // Locale pre-sign (C locale: '-' / '+').
                if !self.overloaded() {
                    let c = self.input[self.input_p];
                    if c == '-' || c == '+' {
                        self.out[0] = c;
                        self.input_p += 1;
                    }
                }
            } else {
                let c = self.input[self.input_p];
                if c == '-' || (self.desc.bracket && c == '<') {
                    self.out[0] = '-';
                    self.input_p += 1;
                } else if c == '+' {
                    self.out[0] = '+';
                    self.input_p += 1;
                }
            }
        }
        if self.overloaded() {
            return false;
        }

        // Read a digit or the decimal point.
        let mut isread = false;
        let c = self.input[self.input_p];
        if c.is_ascii_digit() {
            if !(self.read_dec && self.read_post == self.desc.post) {
                self.out.push(c);
                if self.read_dec {
                    self.read_post += 1;
                } else {
                    self.read_pre += 1;
                }
                isread = true;
            }
        } else if self.desc.decimal && !self.read_dec && c == '.' {
            self.out.push('.');
            self.read_dec = true;
            isread = true;
        }
        if self.overloaded() {
            return false;
        }

        // Read sign behind the last number.
        if self.out[0] == ' ' && self.read_pre + self.read_post > 0 {
            if self.desc.lsign != 0
                && isread
                && self.input_p + 1 < self.input.len()
                && !self.input[self.input_p + 1].is_ascii_digit()
            {
                // Locale post-sign (C locale: '-' / '+').
                let nc = self.input[self.input_p + 1];
                if nc == '-' || nc == '+' {
                    self.out[0] = nc;
                    self.input_p += 1; // caller adds one more
                }
            } else if !isread && self.desc.lsign == 0 && (self.desc.plus || self.desc.minus) {
                let c = self.input[self.input_p];
                if c == '-' || c == '+' {
                    self.out[0] = c;
                    // Caller does input_p++.
                }
            }
        }
        true
    }

    fn run(mut self) -> (String, i32) {
        // output starts as " " (sign slot)
        let nodes = self.desc.nodes.clone();
        'outer: for node in &nodes {
            if self.overloaded() {
                break;
            }
            match node {
                Node::N9 | Node::N0 | Node::Dec => {
                    if !self.numpart(node) {
                        break 'outer;
                    }
                }
                Node::Comma => {
                    // num_in is never set in from_char; with fill mode
                    // the comma is skipped, otherwise it must match.
                    if !self.desc.fillmode {
                        if self.input[self.input_p] != ',' {
                            continue;
                        }
                    } else {
                        continue;
                    }
                }
                Node::G => {
                    if !self.desc.fillmode {
                        // thousands_sep is "," in the C locale.
                        if self.input[self.input_p] == ',' {
                            // consume below
                        } else {
                            continue;
                        }
                    } else {
                        continue;
                    }
                }
                Node::L => {
                    // C locale currency symbol is " " (one char):
                    // eat one non-data char.
                    self.eat_non_data(1);
                    continue;
                }
                Node::Roman(_) => {
                    match roman_to_int(&self.input, self.input_p) {
                        Some((v, used)) => {
                            let s = v.to_string();
                            self.out.extend(s.chars());
                            self.input_p += used;
                        }
                        None => {
                            // PG raises 22P02 invalid Roman numeral.
                            // Mark via a sentinel the caller maps.
                            self.out = vec!['\u{0}'];
                            break 'outer;
                        }
                    }
                    continue;
                }
                Node::Th(_) => {
                    // Suppressed for Roman or decimal pictures (in
                    // from_char the sign slot is never '#' or '-').
                    if self.desc.roman || self.desc.decimal {
                        continue;
                    }
                    // All variants of 'th' occupy 2 characters.
                    self.eat_non_data(2);
                    continue;
                }
                Node::MI => {
                    if self.input[self.input_p] == '-' {
                        self.out[0] = '-';
                    } else {
                        self.eat_non_data(1);
                        continue;
                    }
                }
                Node::PL => {
                    if self.input[self.input_p] == '+' {
                        self.out[0] = '+';
                    } else {
                        self.eat_non_data(1);
                        continue;
                    }
                }
                Node::SG => {
                    let c = self.input[self.input_p];
                    if c == '-' {
                        self.out[0] = '-';
                    } else if c == '+' {
                        self.out[0] = '+';
                    } else {
                        self.eat_non_data(1);
                        continue;
                    }
                }
                // S, FM, PR, V, EEEE, B, C, SP: default -> skip node.
                _ => {
                    // Literals: skip exactly one input char.
                    if matches!(node, Node::Lit(_)) {
                        self.input_p += 1;
                    }
                    continue;
                }
            }
            self.input_p += 1;
        }

        let mut s: String = self.out.iter().collect();
        if s == "\0" {
            return (s, 0);
        }
        // Truncate any final '.'.
        if s.ends_with('.') {
            s.pop();
        }
        // Correction: post = actually-read post digits.
        let post = self.read_post;
        (s, post)
    }
}

/// Parse text with a numeric format picture: PG's `numeric_to_number`.
///
/// Returns the parsed `Numeric`. `V` (multi) shifts the decimal point
/// left by `multi` (multiply by 10^-multi), like PG.
pub fn to_number(s: &str, fmt: &NumFmt) -> Result<Numeric, NumFmtError> {
    if fmt.eeee {
        return Err(NumFmtError::Unsupported(
            "\"EEEE\" not supported for input".to_string(),
        ));
    }
    let fc = FromChar {
        desc: fmt,
        input: s.chars().collect(),
        input_p: 0,
        out: vec![' '],
        read_pre: 0,
        read_post: 0,
        read_dec: false,
    };
    let (mut numstr, _post) = fc.run();
    if numstr.starts_with('\0') {
        return Err(NumFmtError::InvalidInput(
            "invalid Roman numeral".to_string(),
        ));
    }
    // The leading sign slot: ' ' means positive.
    if numstr.starts_with(' ') {
        numstr.remove(0);
    }
    let mut n = parse_std_numeric(&numstr)?;
    if fmt.multi != 0 {
        // PG: result = value * numeric_power(10, -multi). The power is
        // computed with dscale 16+multi (rscale = NUMERIC_MIN_SIG_DIGITS
        // - f, f = -multi for base 10), and mul_var adds dscales without
        // stripping. Emulate: shift the decimal left, then widen the
        // scale by 16.
        n = shift_decimal(&n, -(fmt.multi as i32))
            .ok_or_else(|| NumFmtError::InvalidInput("value out of range".to_string()))?;
        n = scale_up(&n, 16)
            .ok_or_else(|| NumFmtError::InvalidInput("value out of range".to_string()))?;
    }
    Ok(n)
}

/// Parse a standard decimal string into a `Numeric` (sign, digits,
/// optional fraction and exponent).
fn parse_std_numeric(s: &str) -> Result<Numeric, NumFmtError> {
    let t = s.trim();
    if t.is_empty() {
        return Err(NumFmtError::InvalidInput("empty input".to_string()));
    }
    // Reuse the storage-layer parser via a finite Numeric text round
    // trip: build (unscaled, scale) manually.
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    // Split exponent.
    let (mant, exp): (&str, i32) = match rest.find(|c| c == 'e' || c == 'E') {
        Some(i) => {
            let e: i32 = rest[i + 1..]
                .parse()
                .map_err(|_| NumFmtError::InvalidInput("bad exponent".to_string()))?;
            (&rest[..i], e)
        }
        None => (rest, 0),
    };
    let (int_d, frac_d) = match mant.find('.') {
        Some(i) => (&mant[..i], &mant[i + 1..]),
        None => (mant, ""),
    };
    if !int_d.chars().all(|c| c.is_ascii_digit())
        || !frac_d.chars().all(|c| c.is_ascii_digit())
        || (int_d.is_empty() && frac_d.is_empty())
    {
        return Err(NumFmtError::InvalidInput(format!("invalid number \"{s}\"")));
    }
    let mut digits = format!("{int_d}{frac_d}");
    // Strip leading zeros but keep at least one digit.
    let stripped = digits.trim_start_matches('0');
    digits = if stripped.is_empty() {
        "0".to_string()
    } else {
        stripped.to_string()
    };
    let leading_zeros = int_d.len() as i32 - (int_d.trim_start_matches('0').len() as i32);
    let mut scale = frac_d.len() as i32 - exp;
    // Adjust for stripped leading zeros of the integer part.
    let _ = leading_zeros;
    let mut unscaled: i128 = digits
        .parse()
        .map_err(|_| NumFmtError::InvalidInput("value out of range".to_string()))?;
    if neg {
        unscaled = -unscaled;
    }
    // Normalize: strip trailing zeros while scale > 0.
    while scale > 0 && unscaled != 0 && unscaled % 10 == 0 {
        unscaled /= 10;
        scale -= 1;
    }
    Ok(Numeric::new(unscaled, scale))
}

/// Shift the decimal point by `places` (negative = left).
fn shift_decimal(n: &Numeric, places: i32) -> Option<Numeric> {
    if n.special != NumericSpecial::Finite {
        return Some(n.clone());
    }
    Some(Numeric {
        unscaled: n.unscaled,
        scale: n.scale - places,
        // v0.61: shifting the point preserves the declared display scale.
        dscale: n.dscale,
        special: NumericSpecial::Finite,
    })
}

/// Multiply the unscaled value by 10^`places`, widening the scale by
/// the same amount (value unchanged; more fractional digits shown).
/// Uses raw construction: `Numeric::new` would strip the added zeros.
fn scale_up(n: &Numeric, places: i32) -> Option<Numeric> {
    if n.special != NumericSpecial::Finite {
        return Some(n.clone());
    }
    let factor = 10i128.checked_pow(places as u32)?;
    Some(Numeric {
        unscaled: n.unscaled.checked_mul(factor)?,
        scale: n.scale + places,
        // v0.61: widening shows more fractional digits.
        dscale: n.dscale + places,
        special: NumericSpecial::Finite,
    })
}
