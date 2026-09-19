//! `to_char` side of the numeric formatting engine (v0.26).
//!
//! Ports `numeric_to_char` / `float8_to_char` input preparation plus
//! `NUM_processor_to_char` from PostgreSQL's formatting.c.

use super::numfmt::{Node, NumFmt, NumFmtError};
use super::storage::{Numeric, NumericSpecial};

/// Convert an integer to Roman numerals (upper case, unpadded).
/// Out of range (not 1..=3999) yields 15 `#`s, like PG's `int_to_roman`.
fn int_to_roman(number: i64) -> String {
    if !(1..=3999).contains(&number) {
        return "#".repeat(15);
    }
    const RM100: [&str; 10] = ["", "C", "CC", "CCC", "CD", "D", "DC", "DCC", "DCCC", "CM"];
    const RM10: [&str; 10] = ["", "X", "XX", "XXX", "XL", "L", "LX", "LXX", "LXXX", "XC"];
    const RM1: [&str; 10] = ["", "I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX"];
    let mut out = String::new();
    let n = number as usize;
    for _ in 0..n / 1000 {
        out.push('M');
    }
    out.push_str(RM100[(n / 100) % 10]);
    out.push_str(RM10[(n / 10) % 10]);
    out.push_str(RM1[n % 10]);
    out
}

/// Ordinal suffix for the trailing digits of `num` (`get_th`):
/// 1st/2nd/3rd (unless a teen), else th.
fn get_th(num: &str, upper: bool) -> &'static str {
    let bytes = num.as_bytes();
    let mut last = *bytes.last().unwrap_or(&b'0');
    if bytes.len() > 1 && bytes[bytes.len() - 2] == b'1' {
        last = 0; // teens -> th
    }
    match (last, upper) {
        (b'1', true) => "ST",
        (b'1', false) => "st",
        (b'2', true) => "ND",
        (b'2', false) => "nd",
        (b'3', true) => "RD",
        (b'3', false) => "rd",
        (_, true) => "TH",
        (_, false) => "th",
    }
}

/// `get_last_relevant_decnum`: index of the last non-`0` char at or
/// after the decimal point (the `.` itself if all zeros).
fn last_relevant_decnum(input: &[char]) -> Option<usize> {
    let dot = input.iter().position(|&c| c == '.')?;
    let mut result = dot;
    for (i, &c) in input.iter().enumerate().skip(dot + 1) {
        if c != '0' {
            result = i;
        }
    }
    Some(result)
}

/// Round a decimal digit string to `post` fractional digits
/// (round-half-away-from-zero, like PG's `numeric_round`).
///
/// Input: `(int_digits, frac_digits)` with no sign and no point.
/// Returns the rounded `(int_digits, frac_digits)` with exactly
/// `post` fractional digits.
fn round_digits(int_d: &str, frac_d: &str, post: i32) -> (String, String) {
    let post = post.max(0) as usize;
    let mut frac: Vec<char> = frac_d.chars().collect();
    if frac.len() > post {
        let round_up = frac.get(post).is_some_and(|&c| c >= '5');
        frac.truncate(post);
        if round_up {
            // Add one at the last kept position, carrying left.
            let mut int_c: Vec<char> = int_d.chars().collect();
            let mut i = frac.len();
            loop {
                if i == 0 {
                    // Carry into the integer part.
                    let mut j = int_c.len();
                    loop {
                        if j == 0 {
                            int_c.insert(0, '1');
                            break;
                        }
                        j -= 1;
                        if int_c[j] == '9' {
                            int_c[j] = '0';
                        } else {
                            int_c[j] = ((int_c[j] as u8) + 1) as char;
                            break;
                        }
                    }
                    break;
                }
                i -= 1;
                if frac[i] == '9' {
                    frac[i] = '0';
                } else {
                    frac[i] = ((frac[i] as u8) + 1) as char;
                    break;
                }
            }
            while frac.len() < post {
                frac.push('0');
            }
            return (int_c.iter().collect(), frac.iter().collect());
        }
    }
    while frac.len() < post {
        frac.push('0');
    }
    (int_d.to_string(), frac.iter().collect())
}

/// Split a finite `Numeric` into `(negative, int_digits, frac_digits)`,
/// expanding scientific notation from huge negative scales.
fn numeric_digits(n: &Numeric) -> (bool, String, String) {
    let neg = n.unscaled < 0;
    let mut digits = n.unscaled.unsigned_abs().to_string();
    let scale = n.scale;
    if scale < 0 {
        digits.push_str(&"0".repeat((-scale) as usize));
        return (neg, digits, String::new());
    }
    let scale = scale as usize;
    if digits.len() > scale {
        let at = digits.len() - scale;
        let frac = digits.split_off(at);
        (neg, digits, frac)
    } else {
        let mut frac = "0".repeat(scale - digits.len());
        frac.push_str(&digits);
        (neg, "0".to_string(), frac)
    }
}

/// `numeric_out_sci` equivalent: `d.dddde±XX` with `post` fractional
/// digits and at least two exponent digits (`%se%+03d`).
fn numeric_out_sci(n: &Numeric, post: i32) -> String {
    let post = post.max(0) as usize;
    let (neg, int_d, frac_d) = numeric_digits(n);
    let mut all: Vec<char> = format!("{int_d}{frac_d}").chars().collect();
    // Strip leading zeros to find the first significant digit.
    let first = all.iter().position(|&c| c != '0');
    let (sig, exp, overflow): (Vec<char>, i32, bool) = match first {
        None => {
            let mut sig = vec!['0'];
            while sig.len() < 1 + post {
                sig.push('0');
            }
            (sig, 0, false)
        }
        Some(f) => {
            let exp = (int_d.len() as i32 - 1) - f as i32;
            let mut sig: Vec<char> = all.drain(f..).collect();
            // Round significand to 1 + post digits (half away from zero).
            let mut overflow = false;
            if sig.len() > 1 + post {
                let round_up = sig.get(1 + post).is_some_and(|&c| c >= '5');
                sig.truncate(1 + post);
                if round_up {
                    let mut i = sig.len();
                    loop {
                        if i == 0 {
                            sig.insert(0, '1');
                            overflow = true;
                            break;
                        }
                        i -= 1;
                        if sig[i] == '9' {
                            sig[i] = '0';
                        } else {
                            sig[i] = ((sig[i] as u8) + 1) as char;
                            break;
                        }
                    }
                }
            }
            while sig.len() < 1 + post {
                sig.push('0');
            }
            // PG does not renormalize after a rounding overflow: the
            // significand prints as "10.00" with the exponent unchanged.
            (sig, exp, overflow)
        }
    };
    let mut s = String::new();
    if neg {
        s.push('-');
    }
    s.push(sig[0]);
    if overflow {
        s.push(sig[1]);
    }
    if post > 0 {
        s.push('.');
        if overflow {
            s.extend(sig.iter().skip(2).take(post));
        } else {
            s.extend(sig.iter().skip(1).take(post));
        }
    }
    s.push('e');
    if exp < 0 {
        s.push('-');
    } else {
        s.push('+');
    }
    let e = exp.unsigned_abs().to_string();
    if e.len() < 2 {
        s.push('0');
    }
    s.push_str(&e);
    s
}

/// Prepare the digit string for the plain (non-Roman, non-EEEE)
/// numeric path: apply `V` shift, round to `post` decimals (keeping
/// all `post` digits, like `numeric_out`), split off the sign.
/// Returns `(sign, digit_chars, Ok(out_pre_spaces) | Err(overflow))`.
fn prepare_plain(
    desc: &mut NumFmt,
    int_d: &str,
    frac_d: &str,
    neg: bool,
) -> (char, Vec<char>, Result<i32, ()>) {
    // V: shift the decimal point right by `multi` (multiply by 10^multi).
    let (mut int_d, mut frac_d) = (int_d.to_string(), frac_d.to_string());
    for _ in 0..desc.multi {
        let c = if frac_d.is_empty() {
            '0'
        } else {
            frac_d.remove(0)
        };
        int_d.push(c);
    }
    desc.pre += desc.multi;

    // Round to `post` decimals; numeric_out keeps every post digit.
    let (int_d, frac_d) = round_digits(&int_d, &frac_d, desc.post);
    let int_trimmed = int_d.trim_start_matches('0');
    let int_part = if int_trimmed.is_empty() {
        "0"
    } else {
        int_trimmed
    };
    let mut numstr = int_part.to_string();
    if desc.post > 0 {
        numstr.push('.');
        numstr.push_str(&frac_d);
    }
    let sign = if neg { '-' } else { '+' };
    let pre_len = int_part.len() as i32;
    let layout = if pre_len < desc.pre {
        Ok(desc.pre - pre_len)
    } else if pre_len > desc.pre {
        Err(())
    } else {
        Ok(0)
    };
    (sign, numstr.chars().collect(), layout)
}

/// Shared layout step for the plain path: measure the integer-digit
/// count and compute `out_pre_spaces`, or the overflow `#` fill.
fn plain_layout(desc: &NumFmt, numstr: &[char]) -> (Vec<char>, i32) {
    let pre_len = numstr
        .iter()
        .position(|&c| c == '.')
        .unwrap_or(numstr.len()) as i32;
    if pre_len < desc.pre {
        (numstr.to_vec(), desc.pre - pre_len)
    } else if pre_len > desc.pre {
        (overflow_fill(desc.pre, desc.post), 0)
    } else {
        (numstr.to_vec(), 0)
    }
}

/// `NUM_processor` prologue + node loop for `to_char`.
///
/// `input` is the prepared digit string (sign split off), `sign` is
/// `'+'`/`'-'`, `out_pre_spaces` comes from the caller.
fn run_processor(mut desc: NumFmt, input: Vec<char>, sign: char, out_pre_spaces: i32) -> String {
    if desc.eeee {
        // PG regurgitates the prepared string as-is.
        return input.iter().collect();
    }

    // `if (Np->Num->zero_start) --Np->Num->zero_start;`
    if desc.zero_start > 0 {
        desc.zero_start -= 1;
    }

    // Sign prologue.
    let sign_wrote = if desc.plus || desc.minus {
        // MI/SG: the picture writes the sign itself (sign_wrote=true).
        // PL: if sign is '+', PL node wrote '+', done. If '-', PL node wrote
        // ' ' placeholder, numpart must write '-'.
        if desc.plus && !desc.minus {
            sign == '+'
        } else {
            true
        }
    } else {
        if sign != '-' && desc.fillmode {
            desc.bracket = false;
        }
        // PG: sign_wrote=true ("needn't sign") ONLY for '+' in fillmode
        // with no locale sign; otherwise numpart must write the sign.
        let sw = sign == '+' && desc.fillmode && desc.lsign == 0;
        if desc.lsign == 1 && desc.pre == desc.pre_lsign_num {
            desc.lsign = 2;
        }
        sw
    };

    // Count.
    let mut num_count = desc.post + desc.pre - 1;

    // Last relevant digit (fill mode + decimal only).
    let mut last_relevant = None;
    if desc.fillmode && desc.decimal {
        last_relevant = last_relevant_decnum(&input);
        // If any '0' specifiers are present, make sure we don't strip
        // those digits.
        if let Some(lr) = last_relevant {
            if desc.zero_end > out_pre_spaces {
                let last_zero_pos =
                    ((input.len() as i32 - 1).min(desc.zero_end - out_pre_spaces)) as usize;
                if lr < last_zero_pos {
                    last_relevant = Some(last_zero_pos);
                }
            }
        }
    }

    if !sign_wrote && out_pre_spaces == 0 {
        num_count += 1;
    }

    let tc = ToChar {
        desc,
        input,
        input_p: 0,
        out: String::new(),
        out_pre_spaces,
        num_count,
        num_curr: 0,
        num_in: false,
        sign,
        sign_wrote,
        last_relevant,
    };
    tc.run()
}

/// Build the overflow `#` fill: `pre + post + 1` `#`s with `.` at
/// position `pre` (PG's `fill_str` + dot logic).
fn overflow_fill(pre: i32, post: i32) -> Vec<char> {
    let total = (pre + post + 1).max(1) as usize;
    let mut v = vec!['#'; total];
    if pre >= 0 && (pre as usize) < total {
        v[pre as usize] = '.';
    }
    v
}

/// Processor state for `NUM_processor_to_char`.
struct ToChar {
    desc: NumFmt,
    /// Digit string without sign (may contain one `.`).
    input: Vec<char>,
    input_p: usize,
    out: String,
    out_pre_spaces: i32,
    num_count: i32,
    num_curr: i32,
    num_in: bool,
    sign: char, // '+' or '-'
    sign_wrote: bool,
    last_relevant: Option<usize>,
}

impl ToChar {
    /// `IS_PREDEC_SPACE`: at the very first digit and the value is `0.xxx`.
    fn is_predec_space(&self) -> bool {
        !self.desc.zero
            && self.input_p == 0
            && self.input.first() == Some(&'0')
            && self.desc.post != 0
    }

    fn emit(&mut self, c: char) {
        self.out.push(c);
    }

    fn emit_str(&mut self, s: &str) {
        self.out.push_str(s);
    }

    /// `NUM_numpart_to_char` for `9` / `0` / `.` / `D`.
    /// Returns false when PG would write `'\0'` into the output, i.e.
    /// the digit string is exhausted and all further output is
    /// invisible; the caller then stops processing nodes.
    fn numpart(&mut self, id: &Node) -> bool {
        if self.desc.roman {
            return true;
        }
        self.num_in = false;

        // Write the sign ahead of the first real output position.
        let zero_start_hit = self.desc.zero && self.desc.zero_start == self.num_curr;
        if !self.sign_wrote
            && (self.num_curr >= self.out_pre_spaces || zero_start_hit)
            && (!self.is_predec_space()
                || self.last_relevant.is_some_and(|lr| self.input[lr] == '.'))
        {
            if self.desc.lsign == 1 {
                // C locale sign (PRE): emit here.
                self.emit(if self.sign == '-' { '-' } else { '+' });
                self.sign_wrote = true;
            } else if self.desc.lsign == 2 {
                // Locale sign POST: emitted at end, not here. Do not fall
                // through to the '-' branch below.
            } else if self.desc.bracket {
                self.emit(if self.sign == '+' { ' ' } else { '<' });
                self.sign_wrote = true;
            } else if self.sign == '+' {
                if !self.desc.fillmode {
                    self.emit(' ');
                }
                self.sign_wrote = true;
            } else if self.sign == '-' {
                self.emit('-');
                self.sign_wrote = true;
            }
        }

        if matches!(id, Node::N9 | Node::N0 | Node::Dec) {
            if self.num_curr < self.out_pre_spaces
                && (self.desc.zero_start > self.num_curr || !self.desc.zero)
            {
                // Leading blank.
                if !self.desc.fillmode {
                    self.emit(' ');
                }
            } else if self.desc.zero
                && self.num_curr < self.out_pre_spaces
                && self.desc.zero_start <= self.num_curr
            {
                // Zero padding.
                self.emit('0');
                self.num_in = true;
            } else if self.input.get(self.input_p) == Some(&'.') {
                let at_dot = self.last_relevant.is_none_or(|lr| self.input[lr] != '.');
                let fm_dot = self.desc.fillmode
                    && self.last_relevant.is_some_and(|lr| self.input[lr] == '.');
                if at_dot || fm_dot {
                    self.emit('.');
                }
                self.input_p += 1;
            } else {
                // Digit slot.
                let past_relevant = self.last_relevant.is_some_and(|lr| self.input_p > lr);
                if past_relevant && *id != Node::N0 {
                    // Skip: beyond last relevant digit in fill mode.
                } else if self.is_predec_space() {
                    if !self.desc.fillmode {
                        self.emit(' ');
                    } else if self.last_relevant.is_some_and(|lr| self.input[lr] == '.') {
                        self.emit('0');
                    }
                } else if self.input_p < self.input.len() {
                    self.emit(self.input[self.input_p]);
                    self.num_in = true;
                } else {
                    // PG writes '\0': terminate all further output.
                    return false;
                }
                if self.input_p < self.input.len() {
                    self.input_p += 1;
                }
            }

            let mut end =
                self.num_count + i32::from(self.out_pre_spaces != 0) + i32::from(self.desc.decimal);
            if self.last_relevant.is_some_and(|lr| lr == self.input_p) {
                end = self.num_curr;
            }
            if self.num_curr + 1 == end {
                if self.sign_wrote && self.desc.bracket {
                    self.emit(if self.sign == '+' { ' ' } else { '>' });
                } else if self.desc.lsign == 2 {
                    self.emit(if self.sign == '-' { '-' } else { '+' });
                }
            }
        }

        self.num_curr += 1;
        true
    }

    /// Node loop only; the `NUM_processor` prologue (zero_start,
    /// sign, count, last-relevant) is done by `run_processor`.
    fn run(mut self) -> String {
        self.num_in = false;
        self.num_curr = 0;

        let nodes = self.desc.nodes.clone();
        for node in &nodes {
            match node {
                Node::N9 | Node::N0 | Node::Dec => {
                    if !self.numpart(node) {
                        break;
                    }
                }
                Node::Comma => {
                    if !self.num_in {
                        if !self.desc.fillmode {
                            self.emit(' ');
                        }
                    } else {
                        self.emit(',');
                    }
                }
                Node::G => {
                    if !self.num_in {
                        if !self.desc.fillmode {
                            // Width of the group separator.
                            self.emit(' ');
                        }
                    } else {
                        self.emit(',');
                    }
                }
                Node::L | Node::C => {
                    // C locale fallback: the currency symbol is one
                    // space (PG sets L_currency_symbol = " " when the
                    // locale provides none).
                    self.emit(' ');
                }
                Node::Roman(lower) => {
                    let mut s: String = self.input.iter().collect();
                    if *lower {
                        s = s.to_lowercase();
                    }
                    if self.desc.fillmode {
                        self.emit_str(&s);
                    } else {
                        self.emit_str(&format!("{s:>15}"));
                    }
                }
                Node::Th(lower) => {
                    let input_s: String = self.input.iter().collect();
                    if !(self.desc.roman
                        || input_s.starts_with('#')
                        || self.sign == '-'
                        || self.desc.decimal)
                    {
                        self.emit_str(get_th(&input_s, !lower));
                    }
                }
                Node::MI => {
                    if self.sign == '-' {
                        self.emit('-');
                    } else if !self.desc.fillmode {
                        self.emit(' ');
                    }
                }
                Node::PL => {
                    if self.sign == '+' {
                        self.emit('+');
                    } else if !self.desc.fillmode {
                        self.emit(' ');
                    }
                }
                Node::SG => {
                    self.emit(self.sign);
                }
                Node::S => {
                    // The locale sign is emitted from numpart(); nothing here.
                }
                Node::Lit(c) => self.emit(*c),
            }
        }
        self.out
    }
}

/// Format a `Numeric` with a numeric format picture: PG's
/// `numeric_to_char`.
pub fn numeric_to_char(n: &Numeric, fmt: &NumFmt) -> Result<String, NumFmtError> {
    let mut desc = fmt.clone();

    // Roman path: round to int first (PG: numeric_int4_safe).
    if desc.roman {
        let iv = match n.special {
            NumericSpecial::Finite => {
                // Round half away from zero, saturate to int32 range
                // (PG uses PG_INT32_MAX on overflow).
                let (neg, int_d, frac_d) = numeric_digits(n);
                let (ri, _) = round_digits(&int_d, &frac_d, 0);
                let mut v: i128 = ri.parse().unwrap_or(i128::MAX);
                if neg {
                    v = -v;
                }
                v.clamp(i32::MIN as i128, i32::MAX as i128) as i64
            }
            _ => i64::from(i32::MAX),
        };
        let input: Vec<char> = int_to_roman(iv).chars().collect();
        return Ok(run_processor(desc, input, '+', 0));
    }

    // EEEE path.
    if desc.eeee {
        let input: Vec<char> = match n.special {
            NumericSpecial::NaN | NumericSpecial::PosInf | NumericSpecial::NegInf => {
                // '#' fill: pre+post+6 wide, ' ' at 0, '.' at pre+1.
                let total = (desc.pre + desc.post + 6).max(2) as usize;
                let mut v = vec!['#'; total];
                v[0] = ' ';
                let dot_at = desc.pre as usize + 1;
                if dot_at < total {
                    v[dot_at] = '.';
                }
                v
            }
            NumericSpecial::Finite => {
                let mut s = numeric_out_sci(n, desc.post);
                if !s.starts_with('-') {
                    s.insert(0, ' ');
                }
                s.chars().collect()
            }
        };
        return Ok(run_processor(desc, input, '+', 0));
    }

    // Plain path.
    let (neg, int_d, frac_d) = match n.special {
        NumericSpecial::NaN => (false, "NaN".to_string(), String::new()),
        NumericSpecial::PosInf => (false, "Infinity".to_string(), String::new()),
        NumericSpecial::NegInf => (true, "Infinity".to_string(), String::new()),
        NumericSpecial::Finite => numeric_digits(n),
    };
    // NaN/Infinity flow through the same layout (numeric_out emits
    // "NaN"/"Infinity"), overflowing to '#' unless the picture fits.
    let (sign, numstr, out_pre_spaces) = if matches!(
        n.special,
        NumericSpecial::NaN | NumericSpecial::PosInf | NumericSpecial::NegInf
    ) {
        let chars: Vec<char> = int_d.chars().collect();
        let (numstr, ops) = plain_layout(&desc, &chars);
        (if neg { '-' } else { '+' }, numstr, ops)
    } else {
        let (sign, numstr, layout) = prepare_plain(&mut desc, &int_d, &frac_d, neg);
        let (numstr, ops) = match layout {
            Ok(s) => (numstr, s),
            Err(()) => (overflow_fill(desc.pre, desc.post), 0),
        };
        (sign, numstr, ops)
    };
    Ok(run_processor(desc, numstr, sign, out_pre_spaces))
}

/// Format an `f64` with a numeric format picture: PG's `float8_to_char`.
pub fn float8_to_char(v: f64, fmt: &NumFmt) -> Result<String, NumFmtError> {
    let mut desc = fmt.clone();

    if desc.roman {
        // PG: rint(value); overflow/NaN -> PG_INT32_MAX.
        let iv = if v.is_nan() || v.is_infinite() || v.abs() > i32::MAX as f64 {
            i64::from(i32::MAX)
        } else {
            v.round_ties_even() as i64
        };
        let input: Vec<char> = int_to_roman(iv).chars().collect();
        return Ok(run_processor(desc, input, '+', 0));
    }

    if desc.eeee {
        let input: Vec<char> = if v.is_nan() || v.is_infinite() {
            let total = (desc.pre + desc.post + 6).max(2) as usize;
            let mut vv = vec!['#'; total];
            vv[0] = ' ';
            let dot_at = desc.pre as usize + 1;
            if dot_at < total {
                vv[dot_at] = '.';
            }
            vv
        } else {
            // "%+.*e" with a leading '+' swapped for ' '.
            let s = format!("{v:+.prec$e}", prec = desc.post.max(0) as usize);
            s.replacen('+', " ", 1).chars().collect()
        };
        return Ok(run_processor(desc, input, '+', 0));
    }

    // Plain path.
    if v.is_nan() || v.is_infinite() {
        // C printf renders these as "nan"/"inf"/"-inf".
        let (neg, word) = if v.is_nan() {
            (false, "nan")
        } else if v.is_sign_negative() {
            (true, "inf")
        } else {
            (false, "inf")
        };
        let chars: Vec<char> = word.chars().collect();
        let (numstr, out_pre_spaces) = plain_layout(&desc, &chars);
        return Ok(run_processor(
            desc,
            numstr,
            if neg { '-' } else { '+' },
            out_pre_spaces,
        ));
    }

    // V (multi) shift first, like PG.
    let mut val = v;
    if desc.multi != 0 {
        val *= 10f64.powi(desc.multi);
        desc.pre += desc.multi;
    }
    // "%.0f" of |v| to count integer digits, then limit post to
    // DBL_DIG (15) significant digits.
    let int_probe = format!("{:.0}", val.abs());
    let pre_len = int_probe.len() as i32;
    if pre_len >= 15 {
        desc.post = 0;
    } else if pre_len + desc.post > 15 {
        desc.post = 15 - pre_len;
    }
    let orgnum = format!("{:.prec$}", val, prec = desc.post.max(0) as usize);
    let (sign, digits) = match orgnum.strip_prefix('-') {
        Some(rest) => ('-', rest.to_string()),
        None => ('+', orgnum),
    };
    let numstr: Vec<char> = digits.chars().collect();
    let (numstr, out_pre_spaces) = plain_layout(&desc, &numstr);
    Ok(run_processor(desc, numstr, sign, out_pre_spaces))
}

#[cfg(test)]
mod tests {
    use super::super::numfmt::parse_numfmt;
    use super::super::storage::Numeric;
    use super::*;

    fn fmt_to_char(val: &str, scale: i32, picture: &str) -> String {
        // Raw construction: Numeric::new would strip trailing zeros and
        // change the displayed scale.
        let n = Numeric {
            unscaled: val.parse::<i128>().unwrap(),
            scale,
            // v0.61: the test wants the full declared scale displayed.
            dscale: scale.max(0),
            special: super::super::storage::NumericSpecial::Finite,
        };
        let d = parse_numfmt(picture).unwrap();
        numeric_to_char(&n, &d).unwrap()
    }

    #[test]
    fn basic_fixed_width() {
        assert_eq!(fmt_to_char("10000", 2, "999.99"), " 100.00");
        assert_eq!(fmt_to_char("-10000", 2, "999.99"), "-100.00");
    }

    #[test]
    fn fill_mode() {
        assert_eq!(fmt_to_char("1000", 1, "FM999.9"), "100.");
        assert_eq!(fmt_to_char("100", 0, "FM999"), "100");
    }

    #[test]
    fn roman_output() {
        assert_eq!(fmt_to_char("1234", 0, "rn"), "       mccxxxiv");
        assert_eq!(fmt_to_char("4", 0, "RN"), "             IV");
    }

    #[test]
    fn v_multiplies() {
        // 1234.56 with '99999V99' -> digits shift left 2 -> "123456".
        assert_eq!(fmt_to_char("123456", 2, "99999V99"), "  123456");
    }

    #[test]
    fn eeee_scientific() {
        assert_eq!(fmt_to_char("0", 0, "9.999EEEE"), " 0.000e+00");
        assert_eq!(fmt_to_char("-34338492", 0, "9.999EEEE"), "-3.434e+07");
    }

    #[test]
    fn overflow_fill() {
        assert_eq!(fmt_to_char("12345", 0, "999"), " ###");
        assert_eq!(fmt_to_char("-12345", 0, "999"), "-###");
    }

    #[test]
    fn sign_styles() {
        assert_eq!(fmt_to_char("-420", 2, "MI99.99"), "- 4.20");
        assert_eq!(fmt_to_char("420", 2, "PL99.99"), "+ 4.20");
        assert_eq!(fmt_to_char("-123", 0, "999PR"), "<123>");
        assert_eq!(fmt_to_char("123", 0, "999PR"), " 123 ");
    }

    #[test]
    fn ordinal_suffix() {
        assert_eq!(fmt_to_char("1", 0, "99th"), "  1st");
        assert_eq!(fmt_to_char("23", 0, "99th"), " 23rd");
        assert_eq!(fmt_to_char("13", 0, "99th"), " 13th");
    }
}
