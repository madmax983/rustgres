//! Date / timestamp support for rustgres v0.7.
//!
//! `DATE` is stored as an `i32` count of days since 1970-01-01.
//! `TIMESTAMP` and `TIMESTAMPTZ` are stored as `i64` microseconds since
//! 1970-01-01 00:00:00 UTC. `TIMESTAMPTZ` input with an explicit offset is
//! normalized to UTC on the way in; formatting always renders `+00`.
//! There is no session `TimeZone` setting and no `INTERVAL` type (v0.7
//! limitations, documented in the README).
//!
//! The civil-calendar conversions are Howard Hinnant's algorithms
//! (days-from-civil / civil-from-days), implemented from the published
//! formulas.

use std::time::{SystemTime, UNIX_EPOCH};

/// Days from civil date to days since 1970-01-01. `m` is 1-12, `d` is 1-31.
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y as i64 - 1 } else { y as i64 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m as i64 + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Inverse: days since 1970-01-01 -> (year, month, day).
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y } as i32, m as u32, d as u32)
}

pub fn is_leap_year(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn parse_u32(s: &str, what: &str) -> Result<u32, String> {
    s.parse::<u32>()
        .map_err(|_| format!("invalid {} {:?}", what, s))
}

fn parse_i32(s: &str, what: &str) -> Result<i32, String> {
    s.parse::<i32>()
        .map_err(|_| format!("invalid {} {:?}", what, s))
}

/// Parse `YYYY-MM-DD` -> days since 1970-01-01. Leading/trailing
/// whitespace is ignored. Years may be 1-4 digits (PG accepts 1-4).
pub fn parse_date(s: &str) -> Result<i32, String> {
    let s = s.trim();
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return Err(format!("invalid date {:?}", s));
    }
    let y = parse_i32(parts[0], "year")?;
    let m = parse_u32(parts[1], "month")?;
    let d = parse_u32(parts[2], "day")?;
    if !(1..=12).contains(&m) {
        return Err(format!("month out of range in date {:?}", s));
    }
    if d == 0 || d > days_in_month(y, m) {
        return Err(format!("day out of range in date {:?}", s));
    }
    let days = days_from_civil(y, m, d);
    if days < i32::MIN as i64 || days > i32::MAX as i64 {
        return Err(format!("date out of range {:?}", s));
    }
    Ok(days as i32)
}

/// Split micros-since-epoch into (days, micros-of-day), Euclidean.
pub fn split_micros(micros: i64) -> (i64, i64) {
    const PER_DAY: i64 = 86_400_000_000;
    let mut days = micros / PER_DAY;
    let mut tod = micros % PER_DAY;
    if tod < 0 {
        tod += PER_DAY;
        days -= 1;
    }
    (days, tod)
}

/// Parse a timestamp literal into microseconds since 1970-01-01 00:00:00
/// UTC. Accepted forms (a pragmatic ISO subset):
///
/// ```text
/// YYYY-MM-DD
/// YYYY-MM-DD HH:MM:SS[.ffffff]
/// YYYY-MM-DDTHH:MM:SS[.ffffff]
/// ... each optionally suffixed with Z or +HH[:MM] / -HH[:MM] / +HHMM
/// ```
///
/// A missing time means midnight; a missing offset means UTC (documented
/// deviation: real Postgres assumes the session TimeZone).
pub fn parse_timestamp(s: &str) -> Result<i64, String> {
    let s = s.trim();
    // Split off a trailing zone designator: Z, +HH[:MM], -HH[:MM], +HHMM.
    let (core, offset_micros) = split_zone(s)?;
    // Split date from time on ' ' or 'T'.
    let (date_part, time_part) = match core.find([' ', 'T']) {
        Some(i) => (&core[..i], Some(&core[i + 1..])),
        None => (core, None),
    };
    let days = parse_date(date_part)? as i64;
    let tod_micros: i64 = match time_part {
        None | Some("") => 0,
        Some(t) => parse_time(t)?,
    };
    Ok(days * 86_400_000_000 + tod_micros - offset_micros)
}

/// `parse_timestamp`, but a missing offset is still UTC (timestamptz has
/// no session timezone to fall back on in v0.7).
pub fn parse_timestamptz(s: &str) -> Result<i64, String> {
    parse_timestamp(s)
}

/// Split "core[Z|+HH:MM|-HHMM]" into (core, offset micros). A positive
/// offset means local = UTC + offset, so UTC = local - offset.
fn split_zone(s: &str) -> Result<(&str, i64), String> {
    if let Some(core) = s.strip_suffix(['Z', 'z']) {
        return Ok((core, 0));
    }
    // A zone offset only makes sense after a time, so only scan the part
    // after the date/time separator. Without a separator the string is
    // date-only and its dashes are not zones.
    let time_start = match s.find([' ', 'T']) {
        Some(i) => i + 1,
        None => return Ok((s, 0)),
    };
    // Find the last '+' or '-' that starts a zone (not part of the date).
    let bytes = s.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        // Stop scanning at the start of the time part: anything
        // earlier is the date part.
        if i < time_start {
            break;
        }
        let c = bytes[i];
        if c == b'+' || c == b'-' {
            // Must be preceded by a time-ish char (digit, '.', or zone
            // digits), i.e. this is a zone, not the date separator. The
            // date's dashes sit before any 'T'/space; a zone sign always
            // comes after the time's seconds.
            let before_ok = i > 0 && (bytes[i - 1].is_ascii_digit() || bytes[i - 1] == b'.');
            let after_ok = i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit();
            if before_ok && after_ok {
                let zone = &s[i..];
                let digits: String = zone[1..].chars().filter(|c| *c != ':').collect();
                if digits.len() != 2 && digits.len() != 4 {
                    return Err(format!("invalid time zone {:?}", zone));
                }
                let hh: i64 = digits[..2]
                    .parse()
                    .map_err(|_| format!("invalid time zone {:?}", zone))?;
                let mm: i64 = if digits.len() == 4 {
                    digits[2..]
                        .parse()
                        .map_err(|_| format!("invalid time zone {:?}", zone))?
                } else {
                    0
                };
                if hh > 23 || mm > 59 {
                    return Err(format!("time zone out of range {:?}", zone));
                }
                let off = (hh * 3600 + mm * 60) * 1_000_000;
                return Ok((&s[..i], if bytes[i] == b'+' { off } else { -off }));
            }
        }
    }
    Ok((s, 0))
}

/// Parse `HH:MM:SS[.ffffff]` -> micros since midnight.
fn parse_time(t: &str) -> Result<i64, String> {
    let parts: Vec<&str> = t.split(':').collect();
    if parts.len() != 3 {
        return Err(format!("invalid time {:?}", t));
    }
    let hh = parse_u32(parts[0], "hour")?;
    let mm = parse_u32(parts[1], "minute")?;
    let (ss_part, frac_part) = match parts[2].find('.') {
        Some(i) => (&parts[2][..i], &parts[2][i + 1..]),
        None => (parts[2], ""),
    };
    let ss = parse_u32(ss_part, "second")?;
    if hh > 23 || mm > 59 || ss > 60 {
        return Err(format!("time out of range {:?}", t));
    }
    if ss == 60 {
        // Leap second: clamp to 59 (documented simplification).
        return parse_time(&format!("{:02}:{:02}:59.{}", hh, mm, frac_part));
    }
    if frac_part.len() > 6 || !frac_part.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("invalid fractional seconds {:?}", t));
    }
    let mut frac = frac_part.to_string();
    while frac.len() < 6 {
        frac.push('0');
    }
    let us: i64 = if frac.is_empty() {
        0
    } else {
        frac.parse().map_err(|_| format!("invalid time {:?}", t))?
    };
    Ok(((hh as i64 * 60 + mm as i64) * 60 + ss as i64) * 1_000_000 + us)
}

/// `YYYY-MM-DD`.
pub fn format_date(days: i32) -> String {
    let (y, m, d) = civil_from_days(days as i64);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// `YYYY-MM-DD HH:MM:SS[.ffffff]` (fraction trimmed, like Postgres).
pub fn format_timestamp(micros: i64) -> String {
    let (days, tod) = split_micros(micros);
    let (y, m, d) = civil_from_days(days);
    let hh = tod / 3_600_000_000;
    let mm = (tod / 60_000_000) % 60;
    let ss = (tod / 1_000_000) % 60;
    let us = tod % 1_000_000;
    let mut out = format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, m, d, hh, mm, ss);
    if us != 0 {
        let mut frac = format!("{:06}", us);
        while frac.ends_with('0') {
            frac.pop();
        }
        out.push('.');
        out.push_str(&frac);
    }
    out
}

/// Timestamptz renders in UTC with an explicit `+00` suffix.
pub fn format_timestamptz(micros: i64) -> String {
    format!("{}+00", format_timestamp(micros))
}

/// Current time as micros since the Unix epoch.
pub fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// Current UTC date as days since the Unix epoch.
pub fn today_days() -> i32 {
    (now_micros() / 86_400_000_000) as i32
}

/// Postgres day-of-week numbering: Sunday = 0 .. Saturday = 6.
/// 1970-01-01 was a Thursday.
pub fn day_of_week(days: i64) -> u32 {
    (days.rem_euclid(7) + 4) as u32 % 7
}

/// Day of year, 1-366.
pub fn day_of_year(y: i32, m: u32, d: u32) -> u32 {
    days_from_civil(y, m, d) as u32 - days_from_civil(y, 1, 1) as u32 + 1
}

/// ISO 8601 week-numbering year and week (1-53).
pub fn iso_year_week(days: i64) -> (i32, u32) {
    let (y, m, d) = civil_from_days(days);
    // Thursday of this week determines the ISO year.
    let dow_mon0 = (day_of_week(days) + 6) % 7; // Monday = 0
    let thursday = days + 3 - dow_mon0 as i64;
    let (iy, _, _) = civil_from_days(thursday);
    let jan4 = days_from_civil(iy, 1, 4);
    let jan4_dow_mon0 = (day_of_week(jan4) + 6) % 7;
    let week1_monday = jan4 - jan4_dow_mon0 as i64;
    let week = (days - week1_monday) / 7 + 1;
    let _ = (y, m, d);
    (iy, week as u32)
}

/// Truncate a timestamp to a field. Supported fields: millennium,
/// century, decade, year, quarter, month, week, day, hour, minute,
/// second (also milliseconds/microseconds = identity).
pub fn date_trunc(field: &str, micros: i64) -> Result<i64, String> {
    let (days, tod) = split_micros(micros);
    let (y, m, d) = civil_from_days(days);
    let hh = tod / 3_600_000_000;
    let mm = (tod / 60_000_000) % 60;
    let ss = (tod / 1_000_000) % 60;
    let us = tod % 1_000_000;
    let day_micros = |y: i32, m: u32, d: u32| -> i64 { days_from_civil(y, m, d) * 86_400_000_000 };
    let at_hms =
        |base: i64, h: i64, mi: i64, s: i64| -> i64 { base + (h * 3600 + mi * 60 + s) * 1_000_000 };
    Ok(match field {
        "millennium" => day_micros((y / 1000) * 1000, 1, 1),
        "century" => day_micros((y / 100) * 100, 1, 1),
        "decade" => day_micros((y / 10) * 10, 1, 1),
        "year" => day_micros(y, 1, 1),
        "quarter" => day_micros(y, ((m - 1) / 3) * 3 + 1, 1),
        "month" => day_micros(y, m, 1),
        "week" => {
            let dow_mon0 = (day_of_week(days) + 6) % 7;
            (days - dow_mon0 as i64) * 86_400_000_000
        }
        "day" => days * 86_400_000_000,
        "hour" => at_hms(days * 86_400_000_000, hh, 0, 0),
        "minute" => at_hms(days * 86_400_000_000, hh, mm, 0),
        "second" => at_hms(days * 86_400_000_000, hh, mm, ss),
        "milliseconds" | "millisecond" => days * 86_400_000_000 + tod / 1000 * 1000,
        "microseconds" | "microsecond" => micros,
        _ => {
            let _ = us;
            let _ = d;
            return Err(format!("unit {:?} not supported for date_trunc", field));
        }
    })
}

/// EXTRACT(field FROM timestamp) -> f64. Supported fields: millennium,
/// century, decade, year, quarter, month, week, day, dow, doy, hour,
/// minute, second, milliseconds, microseconds, epoch, timezone (0, since
/// we are always UTC).
pub fn extract(field: &str, micros: i64) -> Result<f64, String> {
    let (days, tod) = split_micros(micros);
    let (y, m, d) = civil_from_days(days);
    let hh = tod / 3_600_000_000;
    let mm = (tod / 60_000_000) % 60;
    let ss = (tod / 1_000_000) % 60;
    let us = tod % 1_000_000;
    Ok(match field {
        "millennium" => (y / 1000) as f64,
        "century" => (y / 100) as f64,
        "decade" => (y / 10) as f64,
        "year" => y as f64,
        "quarter" => ((m - 1) / 3 + 1) as f64,
        "month" => m as f64,
        "week" => iso_year_week(days).1 as f64,
        "day" => d as f64,
        "dow" => day_of_week(days) as f64,
        "doy" => day_of_year(y, m, d) as f64,
        "hour" => hh as f64,
        "minute" => mm as f64,
        "second" => ss as f64 + us as f64 / 1_000_000.0,
        "milliseconds" => (tod / 1000) as f64,
        "microseconds" => tod as f64,
        "epoch" => micros as f64 / 1_000_000.0,
        "timezone" => 0.0,
        _ => return Err(format!("unit {:?} not supported for extract", field)),
    })
}

/// EXTRACT(field FROM date): date fields only; time fields are 0.
pub fn extract_date(field: &str, days: i32) -> Result<f64, String> {
    let micros = days as i64 * 86_400_000_000;
    match field {
        "hour" | "minute" | "second" | "milliseconds" | "microseconds" | "epoch" => {
            extract(field, micros)
        }
        "timezone" => Ok(0.0),
        _ => extract(field, micros),
    }
}

// ---------------------------------------------------------------------------
// v0.17: to_date / to_timestamp / to_char format strings.
//
// A documented subset of Postgres' formatting patterns:
//
// ```text
// YYYY  4-digit year (1-4 digits accepted on input)
// YY    2-digit year (00-69 -> 2000s, 70-99 -> 1900s, Postgres rule)
// MM    month 01-12
// DD    day of month 01-31
// HH24  hour 00-23
// HH12  hour 01-12 (HH is the same)
// MI    minutes 00-59
// SS    seconds 00-59
// MS    milliseconds (1-3 digits)
// US    microseconds (1-6 digits)
// AM/PM meridian indicator (case-insensitive, requires HH12)
// ```
//
// Anything else made of ASCII letters is an *unsupported pattern
// element* (`FmtErr::Unsupported`, SQLSTATE 0A000). Non-letter
// characters are literal separators that must match the input
// (case-insensitively); input that does not match is
// `FmtErr::Invalid` (SQLSTATE 22008). Missing date parts default to
// the current date (Postgres behavior); missing time parts default to
// midnight. A trailing run of whitespace in the input is ignored.

/// Format-string processing error. The exec layer maps `Unsupported`
/// to SQLSTATE 0A000 and `Invalid` to SQLSTATE 22008.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FmtErr {
    Unsupported(String),
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FmtTok {
    Year4,
    Year2,
    Month,
    Day,
    Hour24,
    Hour12,
    Minute,
    Second,
    Millis,
    Micros,
    AmPm,
    Lit(char),
}

/// Split a format string into pattern/literal tokens. Matching is
/// case-insensitive; the longest pattern wins ("HH24" before "HH").
fn tokenize_format(fmt: &str) -> Result<Vec<FmtTok>, FmtErr> {
    // Longest-first so "HH24" is not read as "HH" + literal "24".
    const PATS: &[(&str, FmtTok)] = &[
        ("YYYY", FmtTok::Year4),
        ("HH24", FmtTok::Hour24),
        ("HH12", FmtTok::Hour12),
        ("YY", FmtTok::Year2),
        ("MM", FmtTok::Month),
        ("DD", FmtTok::Day),
        ("HH", FmtTok::Hour12),
        ("MI", FmtTok::Minute),
        ("SS", FmtTok::Second),
        ("MS", FmtTok::Millis),
        ("US", FmtTok::Micros),
        ("AM", FmtTok::AmPm),
        ("PM", FmtTok::AmPm),
    ];
    let mut toks = Vec::new();
    let mut rest = fmt;
    while !rest.is_empty() {
        let c = match rest.chars().next() {
            Some(c) => c,
            None => break, // unreachable: rest is non-empty
        };
        if c.is_ascii_alphabetic() {
            let mut matched = false;
            for (pat, tok) in PATS {
                if rest.len() >= pat.len()
                    && rest.as_bytes()[..pat.len()].eq_ignore_ascii_case(pat.as_bytes())
                {
                    toks.push(tok.clone());
                    rest = &rest[pat.len()..];
                    matched = true;
                    break;
                }
            }
            if !matched {
                let run: String = rest
                    .chars()
                    .take_while(|ch| ch.is_ascii_alphabetic())
                    .collect();
                return Err(FmtErr::Unsupported(format!(
                    "pattern element {run:?} is not supported \
                     (supported: YYYY YY MM DD HH24 HH12 HH MI SS MS US AM PM)"
                )));
            }
        } else {
            toks.push(FmtTok::Lit(c));
            rest = &rest[c.len_utf8()..];
        }
    }
    Ok(toks)
}

/// Components parsed from a to_date/to_timestamp input string.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedDt {
    pub year: Option<i32>,
    pub month: Option<u32>,
    pub day: Option<u32>,
    pub hour24: Option<u32>,
    pub hour12: Option<u32>,
    pub pm: Option<bool>,
    pub minute: Option<u32>,
    pub second: Option<u32>,
    /// Sub-second part as microseconds within the second.
    pub micros: Option<u32>,
}

fn fmt_mismatch(input: &str, fmt: &str) -> FmtErr {
    FmtErr::Invalid(format!("input {input:?} does not match format {fmt:?}"))
}

/// Take 1..=max ASCII digits from the front of `s`; returns the value
/// and the byte length consumed.
fn take_digits(s: &str, max: usize, input: &str, fmt: &str) -> Result<(u32, usize), FmtErr> {
    let n = s
        .bytes()
        .take_while(|b| b.is_ascii_digit())
        .take(max)
        .count();
    if n == 0 {
        return Err(fmt_mismatch(input, fmt));
    }
    match s[..n].parse::<u32>() {
        Ok(v) => Ok((v, n)),
        Err(_) => Err(fmt_mismatch(input, fmt)),
    }
}

fn take_ranged(
    s: &str,
    max: usize,
    lo: u32,
    hi: u32,
    input: &str,
    fmt: &str,
) -> Result<(u32, usize), FmtErr> {
    let (v, n) = take_digits(s, max, input, fmt)?;
    if v < lo || v > hi {
        return Err(FmtErr::Invalid(format!(
            "date/time field value out of range: {v} in input {input:?}"
        )));
    }
    Ok((v, n))
}

/// Parse `input` against a to_date/to_timestamp/to_char `format`
/// string. Last occurrence of a repeated pattern wins.
pub fn parse_with_format(input: &str, fmt: &str) -> Result<ParsedDt, FmtErr> {
    let toks = tokenize_format(fmt)?;
    let mut p = ParsedDt::default();
    let mut s = input.trim();
    for tok in &toks {
        match tok {
            FmtTok::Lit(c) => match s.chars().next() {
                Some(ch) if ch.eq_ignore_ascii_case(c) => {
                    s = &s[ch.len_utf8()..];
                }
                _ => return Err(fmt_mismatch(input, fmt)),
            },
            FmtTok::Year4 => {
                let (v, n) = take_digits(s, 4, input, fmt)?;
                p.year = Some(v as i32);
                s = &s[n..];
            }
            FmtTok::Year2 => {
                let (v, n) = take_digits(s, 2, input, fmt)?;
                // Postgres rule: 00-69 -> 2000s, 70-99 -> 1900s.
                p.year = Some(if v <= 69 {
                    2000 + v as i32
                } else {
                    1900 + v as i32
                });
                s = &s[n..];
            }
            FmtTok::Month => {
                let (v, n) = take_ranged(s, 2, 1, 12, input, fmt)?;
                p.month = Some(v);
                s = &s[n..];
            }
            FmtTok::Day => {
                let (v, n) = take_ranged(s, 2, 1, 31, input, fmt)?;
                p.day = Some(v);
                s = &s[n..];
            }
            FmtTok::Hour24 => {
                let (v, n) = take_ranged(s, 2, 0, 23, input, fmt)?;
                p.hour24 = Some(v);
                s = &s[n..];
            }
            FmtTok::Hour12 => {
                let (v, n) = take_ranged(s, 2, 1, 12, input, fmt)?;
                p.hour12 = Some(v);
                s = &s[n..];
            }
            FmtTok::Minute => {
                let (v, n) = take_ranged(s, 2, 0, 59, input, fmt)?;
                p.minute = Some(v);
                s = &s[n..];
            }
            FmtTok::Second => {
                let (v, n) = take_ranged(s, 2, 0, 59, input, fmt)?;
                p.second = Some(v);
                s = &s[n..];
            }
            FmtTok::Millis => {
                let (v, n) = take_digits(s, 3, input, fmt)?;
                p.micros = Some(v * 10u32.pow(6 - n as u32));
                s = &s[n..];
            }
            FmtTok::Micros => {
                let (v, n) = take_digits(s, 6, input, fmt)?;
                p.micros = Some(v * 10u32.pow(6 - n as u32));
                s = &s[n..];
            }
            FmtTok::AmPm => {
                if s.len() >= 2 && s.as_bytes()[..2].eq_ignore_ascii_case(b"am") {
                    p.pm = Some(false);
                    s = &s[2..];
                } else if s.len() >= 2 && s.as_bytes()[..2].eq_ignore_ascii_case(b"pm") {
                    p.pm = Some(true);
                    s = &s[2..];
                } else {
                    return Err(fmt_mismatch(input, fmt));
                }
            }
        }
    }
    if !s.trim_start().is_empty() {
        return Err(FmtErr::Invalid(format!(
            "trailing characters {:?} after input {:?} for format {:?}",
            s, input, fmt
        )));
    }
    Ok(p)
}

/// Resolve the hour of day from HH24 / HH12 + AM/PM. HH12 without a
/// meridian is taken as-is (documented simplification).
fn resolve_hour(p: &ParsedDt) -> Result<u32, FmtErr> {
    if p.pm.is_some() && p.hour24.is_some() {
        return Err(FmtErr::Invalid("cannot use AM/PM with HH24".to_string()));
    }
    match (p.hour12, p.pm) {
        (Some(12), Some(false)) => Ok(0),
        (Some(12), Some(true)) => Ok(12),
        (Some(h), Some(true)) => Ok(h + 12),
        (Some(h), _) => Ok(h),
        (None, Some(_)) => Err(FmtErr::Invalid("AM/PM requires HH12".to_string())),
        (None, None) => Ok(p.hour24.unwrap_or(0)),
    }
}

/// Validate year/month/day and convert to days since 1970-01-01.
fn validate_ymd(y: i64, m: i64, d: i64) -> Result<i32, FmtErr> {
    if !(1..=12).contains(&m) {
        return Err(FmtErr::Invalid(format!(
            "date/time field value out of range: month {m}"
        )));
    }
    let y32 = i32::try_from(y)
        .map_err(|_| FmtErr::Invalid(format!("date/time field value out of range: year {y}")))?;
    let dim = days_in_month(y32, m as u32);
    if d < 1 || d > i64::from(dim) {
        return Err(FmtErr::Invalid(format!(
            "date/time field value out of range: day {d} for month {m}"
        )));
    }
    let days = days_from_civil(y32, m as u32, d as u32);
    i32::try_from(days)
        .map_err(|_| FmtErr::Invalid("date/time field value out of range: date".to_string()))
}

/// `to_date(input, format)` -> days since 1970-01-01. Missing date
/// parts default to the current date; time patterns are accepted but
/// ignored (Postgres behavior).
pub fn to_date_parsed(input: &str, fmt: &str) -> Result<i32, FmtErr> {
    let p = parse_with_format(input, fmt)?;
    let (ty, tm, td) = civil_from_days(i64::from(today_days()));
    let y = p.year.unwrap_or(ty);
    let m = p.month.unwrap_or(tm);
    let d = p.day.unwrap_or(td);
    validate_ymd(i64::from(y), i64::from(m), i64::from(d))
}

/// `to_timestamp(input, format)` -> micros since 1970-01-01 00:00:00
/// UTC. Missing date parts default to the current date; missing time
/// parts default to midnight.
pub fn to_timestamp_parsed(input: &str, fmt: &str) -> Result<i64, FmtErr> {
    let p = parse_with_format(input, fmt)?;
    let (ty, tm, td) = civil_from_days(i64::from(today_days()));
    let y = p.year.unwrap_or(ty);
    let m = p.month.unwrap_or(tm);
    let d = p.day.unwrap_or(td);
    let days = validate_ymd(i64::from(y), i64::from(m), i64::from(d))?;
    let h = resolve_hour(&p)?;
    let mi = p.minute.unwrap_or(0);
    let se = p.second.unwrap_or(0);
    let us = p.micros.unwrap_or(0);
    let tod =
        (i64::from(h) * 3600 + i64::from(mi) * 60 + i64::from(se)) * 1_000_000 + i64::from(us);
    Ok(i64::from(days) * 86_400_000_000 + tod)
}

/// `make_date(year, month, day)` -> days since 1970-01-01.
pub fn make_date_checked(y: i64, m: i64, d: i64) -> Result<i32, FmtErr> {
    validate_ymd(y, m, d)
}

/// `make_timestamp(y, mo, d, h, mi, seconds)` -> micros since
/// 1970-01-01 00:00:00 UTC. `seconds` may carry a fractional part;
/// it must satisfy 0.0 <= s < 60.0 (a value rounding up to 60 is
/// rejected, like Postgres' "date/time field value out of range").
pub fn make_timestamp_checked(
    y: i64,
    mo: i64,
    d: i64,
    h: i64,
    mi: i64,
    s: f64,
) -> Result<i64, FmtErr> {
    let days = validate_ymd(y, mo, d)?;
    if !(0..=23).contains(&h) {
        return Err(FmtErr::Invalid(format!(
            "date/time field value out of range: hour {h}"
        )));
    }
    if !(0..=59).contains(&mi) {
        return Err(FmtErr::Invalid(format!(
            "date/time field value out of range: minute {mi}"
        )));
    }
    if !s.is_finite() || s < 0.0 || s >= 61.0 {
        return Err(FmtErr::Invalid(format!(
            "date/time field value out of range: seconds {s}"
        )));
    }
    let mut whole = s.floor() as u32; // s < 61, so no overflow
    let mut frac = ((s - f64::from(whole)) * 1_000_000.0).round() as u32;
    if frac >= 1_000_000 {
        frac -= 1_000_000;
        whole += 1;
    }
    if whole >= 60 {
        return Err(FmtErr::Invalid(format!(
            "date/time field value out of range: seconds {s}"
        )));
    }
    let tod = (h * 3600 + mi * 60 + i64::from(whole)) * 1_000_000 + i64::from(frac);
    Ok(i64::from(days) * 86_400_000_000 + tod)
}

/// `to_char` formatting with the same pattern subset. `days` is the
/// civil day count, `tod_micros` the (normalized, 0..86400e6)
/// microseconds of day — pass 0 for plain dates.
pub fn format_with_pattern(days: i64, tod_micros: i64, fmt: &str) -> Result<String, FmtErr> {
    let toks = tokenize_format(fmt)?;
    let (y, m, d) = civil_from_days(days);
    let hh = tod_micros / 3_600_000_000;
    let mi = (tod_micros / 60_000_000) % 60;
    let ss = (tod_micros / 1_000_000) % 60;
    let us = tod_micros % 1_000_000;
    let mut out = String::new();
    for tok in &toks {
        match tok {
            FmtTok::Lit(c) => out.push(*c),
            FmtTok::Year4 => out.push_str(&format!("{y:04}")),
            FmtTok::Year2 => out.push_str(&format!("{:02}", y.rem_euclid(100))),
            FmtTok::Month => out.push_str(&format!("{m:02}")),
            FmtTok::Day => out.push_str(&format!("{d:02}")),
            FmtTok::Hour24 => out.push_str(&format!("{hh:02}")),
            FmtTok::Hour12 => {
                let h12 = hh % 12;
                out.push_str(&format!("{:02}", if h12 == 0 { 12 } else { h12 }));
            }
            FmtTok::Minute => out.push_str(&format!("{mi:02}")),
            FmtTok::Second => out.push_str(&format!("{ss:02}")),
            FmtTok::Millis => out.push_str(&format!("{:03}", us / 1000)),
            FmtTok::Micros => out.push_str(&format!("{us:06}")),
            FmtTok::AmPm => out.push_str(if hh < 12 { "AM" } else { "PM" }),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        for (y, m, d) in [
            (1970, 1, 1),
            (2026, 9, 10),
            (2000, 2, 29),
            (1900, 2, 28),
            (1, 1, 1),
        ] {
            let days = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(days), (y, m, d));
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
    }

    #[test]
    fn parse_and_format() {
        let days = parse_date("2026-09-10").unwrap();
        assert_eq!(format_date(days), "2026-09-10");
        assert!(parse_date("2026-13-01").is_err());
        assert!(parse_date("2026-02-29").is_err());
        assert!(parse_date("2024-02-29").is_ok());
        let ts = parse_timestamp("2026-09-10 12:34:56.789").unwrap();
        assert_eq!(format_timestamp(ts), "2026-09-10 12:34:56.789");
        let ts2 = parse_timestamp("2026-09-10 12:00:00+02:00").unwrap();
        assert_eq!(format_timestamptz(ts2), "2026-09-10 10:00:00+00");
        let ts3 = parse_timestamp("2026-09-10").unwrap();
        assert_eq!(format_timestamp(ts3), "2026-09-10 00:00:00");
        assert!(parse_timestamp("not a date").is_err());
    }

    #[test]
    fn trunc_and_extract() {
        let ts = parse_timestamp("2026-09-10 12:34:56.789").unwrap();
        assert_eq!(
            format_timestamp(date_trunc("month", ts).unwrap()),
            "2026-09-01 00:00:00"
        );
        assert_eq!(
            format_timestamp(date_trunc("day", ts).unwrap()),
            "2026-09-10 00:00:00"
        );
        assert_eq!(extract("year", ts).unwrap(), 2026.0);
        assert_eq!(extract("month", ts).unwrap(), 9.0);
        assert_eq!(extract("dow", ts).unwrap(), 4.0); // Thursday
        assert_eq!(extract("quarter", ts).unwrap(), 3.0);
    }

    #[test]
    fn format_tokenizer() {
        let toks = tokenize_format("YYYY-MM-DD HH24:MI:SS").unwrap();
        assert_eq!(toks.len(), 11); // 6 patterns + 5 literal separators
        assert!(tokenize_format("YYYY-QQ").is_err());
        assert!(matches!(
            tokenize_format("YYYY-QQ"),
            Err(FmtErr::Unsupported(_))
        ));
        // Longest match wins: HH24, not HH + "24".
        assert_eq!(tokenize_format("HH24").unwrap(), vec![FmtTok::Hour24]);
        // Case-insensitive patterns.
        assert_eq!(tokenize_format("yyyy/mm/dd").unwrap()[0], FmtTok::Year4);
    }

    #[test]
    fn to_date_happy_path() {
        assert_eq!(
            format_date(to_date_parsed("2026-01-15", "YYYY-MM-DD").unwrap()),
            "2026-01-15"
        );
        assert_eq!(
            format_date(to_date_parsed("2026-1-5", "YYYY-MM-DD").unwrap()),
            "2026-01-05"
        );
        assert_eq!(
            format_date(to_date_parsed("15/01/2026", "DD/MM/YYYY").unwrap()),
            "2026-01-15"
        );
        // YY rule: 00-69 -> 2000s, 70-99 -> 1900s.
        assert_eq!(
            format_date(to_date_parsed("26-09-11", "YY-MM-DD").unwrap()),
            "2026-09-11"
        );
        assert_eq!(
            format_date(to_date_parsed("69-09-11", "YY-MM-DD").unwrap()),
            "2069-09-11"
        );
        assert_eq!(
            format_date(to_date_parsed("70-09-11", "YY-MM-DD").unwrap()),
            "1970-09-11"
        );
        // Leap day accepted in a leap year.
        assert_eq!(
            format_date(to_date_parsed("2024-02-29", "YYYY-MM-DD").unwrap()),
            "2024-02-29"
        );
        // Time patterns are accepted but ignored by to_date.
        assert_eq!(
            format_date(to_date_parsed("2026-09-11 23:59", "YYYY-MM-DD HH24:MI").unwrap()),
            "2026-09-11"
        );
    }

    #[test]
    fn to_date_errors() {
        // Month 13 / Feb 30 -> 22008-class Invalid.
        assert!(matches!(
            to_date_parsed("2026-13-01", "YYYY-MM-DD"),
            Err(FmtErr::Invalid(_))
        ));
        assert!(matches!(
            to_date_parsed("2026-02-30", "YYYY-MM-DD"),
            Err(FmtErr::Invalid(_))
        ));
        assert!(matches!(
            to_date_parsed("2023-02-29", "YYYY-MM-DD"),
            Err(FmtErr::Invalid(_))
        ));
        // Input not matching the format -> Invalid.
        assert!(matches!(
            to_date_parsed("2026/01/15", "YYYY-MM-DD"),
            Err(FmtErr::Invalid(_))
        ));
        assert!(matches!(
            to_date_parsed("2026-01-15 junk", "YYYY-MM-DD"),
            Err(FmtErr::Invalid(_))
        ));
        // Unsupported pattern element -> Unsupported (0A000).
        assert!(matches!(
            to_date_parsed("2026-01-15", "YYYY-MM-DDD"),
            Err(FmtErr::Unsupported(_))
        ));
    }

    #[test]
    fn to_timestamp_happy_path() {
        assert_eq!(
            format_timestamp(
                to_timestamp_parsed("2026-09-11 13:25:01", "YYYY-MM-DD HH24:MI:SS").unwrap()
            ),
            "2026-09-11 13:25:01"
        );
        assert_eq!(
            format_timestamp(to_timestamp_parsed("2026-09-11", "YYYY-MM-DD").unwrap()),
            "2026-09-11 00:00:00"
        );
        // Fractional seconds: MS pads to 3 digits, US to 6.
        assert_eq!(
            format_timestamp(
                to_timestamp_parsed("2026-09-11 13:25:01.5", "YYYY-MM-DD HH24:MI:SS.MS").unwrap()
            ),
            "2026-09-11 13:25:01.5"
        );
        assert_eq!(
            format_timestamp(
                to_timestamp_parsed("2026-09-11 13:25:01.123456", "YYYY-MM-DD HH24:MI:SS.US")
                    .unwrap()
            ),
            "2026-09-11 13:25:01.123456"
        );
        // 12-hour clock with meridian.
        assert_eq!(
            format_timestamp(
                to_timestamp_parsed("2026-09-11 01:25 PM", "YYYY-MM-DD HH12:MI AM").unwrap()
            ),
            "2026-09-11 13:25:00"
        );
        assert_eq!(
            format_timestamp(
                to_timestamp_parsed("2026-09-11 12:00 AM", "YYYY-MM-DD HH12:MI AM").unwrap()
            ),
            "2026-09-11 00:00:00"
        );
        assert_eq!(
            format_timestamp(
                to_timestamp_parsed("2026-09-11 12:00 PM", "YYYY-MM-DD HH12:MI AM").unwrap()
            ),
            "2026-09-11 12:00:00"
        );
        // HH alone is the 12-hour clock.
        assert_eq!(
            format_timestamp(to_timestamp_parsed("2026-09-11 03:00", "YYYY-MM-DD HH:MI").unwrap()),
            "2026-09-11 03:00:00"
        );
    }

    #[test]
    fn to_timestamp_errors() {
        // Hour 25, minute 61.
        assert!(matches!(
            to_timestamp_parsed("2026-09-11 25:00", "YYYY-MM-DD HH24:MI"),
            Err(FmtErr::Invalid(_))
        ));
        assert!(matches!(
            to_timestamp_parsed("2026-09-11 13:61", "YYYY-MM-DD HH24:MI"),
            Err(FmtErr::Invalid(_))
        ));
        // AM/PM with HH24 is rejected.
        assert!(matches!(
            to_timestamp_parsed("2026-09-11 13:00 PM", "YYYY-MM-DD HH24:MI AM"),
            Err(FmtErr::Invalid(_))
        ));
        // AM/PM without HH12 is rejected.
        assert!(matches!(
            to_timestamp_parsed("2026-09-11 PM", "YYYY-MM-DD AM"),
            Err(FmtErr::Invalid(_))
        ));
    }

    #[test]
    fn make_date_checked_cases() {
        assert_eq!(
            format_date(make_date_checked(2026, 2, 28).unwrap()),
            "2026-02-28"
        );
        assert_eq!(
            format_date(make_date_checked(2024, 2, 29).unwrap()),
            "2024-02-29"
        );
        // Invalid: month 13, Feb 30, day 0, Feb 29 in a common year.
        assert!(make_date_checked(2026, 13, 1).is_err());
        assert!(make_date_checked(2026, 2, 30).is_err());
        assert!(make_date_checked(2026, 1, 0).is_err());
        assert!(make_date_checked(2023, 2, 29).is_err());
        assert!(make_date_checked(2026, 0, 10).is_err());
    }

    #[test]
    fn make_timestamp_checked_cases() {
        assert_eq!(
            format_timestamp(make_timestamp_checked(2026, 9, 11, 13, 25, 1.5).unwrap()),
            "2026-09-11 13:25:01.5"
        );
        assert_eq!(
            format_timestamp(make_timestamp_checked(2026, 1, 1, 0, 0, 0.0).unwrap()),
            "2026-01-01 00:00:00"
        );
        // Fractional rounding carries into the whole second.
        assert_eq!(
            format_timestamp(make_timestamp_checked(2026, 1, 1, 0, 0, 1.9999999).unwrap()),
            "2026-01-01 00:00:02"
        );
        // Invalid: hour 24, minute 60, seconds >= 60, negative seconds.
        assert!(make_timestamp_checked(2026, 1, 1, 24, 0, 0.0).is_err());
        assert!(make_timestamp_checked(2026, 1, 1, 0, 60, 0.0).is_err());
        assert!(make_timestamp_checked(2026, 1, 1, 0, 0, 60.0).is_err());
        assert!(make_timestamp_checked(2026, 1, 1, 0, 0, -1.0).is_err());
        assert!(make_timestamp_checked(2026, 1, 1, 0, 0, f64::NAN).is_err());
        assert!(make_timestamp_checked(2026, 13, 1, 0, 0, 0.0).is_err());
    }

    #[test]
    fn format_with_pattern_cases() {
        let ts = parse_timestamp("2026-09-11 13:25:01.123456").unwrap();
        let (days, tod) = split_micros(ts);
        assert_eq!(
            format_with_pattern(days, tod, "YYYY/MM/DD").unwrap(),
            "2026/09/11"
        );
        assert_eq!(
            format_with_pattern(days, tod, "DD-MM-YY").unwrap(),
            "11-09-26"
        );
        assert_eq!(
            format_with_pattern(days, tod, "HH24:MI:SS").unwrap(),
            "13:25:01"
        );
        assert_eq!(
            format_with_pattern(days, tod, "HH12:MI:SS AM").unwrap(),
            "01:25:01 PM"
        );
        assert_eq!(format_with_pattern(days, tod, "SS.MS").unwrap(), "01.123");
        assert_eq!(
            format_with_pattern(days, tod, "SS.US").unwrap(),
            "01.123456"
        );
        // Midnight renders 12 AM.
        let (d0, t0) = split_micros(parse_timestamp("2026-09-11 00:00:00").unwrap());
        assert_eq!(format_with_pattern(d0, t0, "HH12 AM").unwrap(), "12 AM");
        // Plain date: time fields render zero.
        assert_eq!(
            format_with_pattern(days_from_civil(2026, 9, 11), 0, "YYYY-MM-DD HH24:MI").unwrap(),
            "2026-09-11 00:00"
        );
        // Unsupported pattern -> Unsupported.
        assert!(matches!(
            format_with_pattern(days, tod, "YYYY-Q"),
            Err(FmtErr::Unsupported(_))
        ));
        // Literals pass through verbatim (letters that do not form a
        // supported pattern are rejected, so literals use punctuation).
        assert_eq!(format_with_pattern(days, tod, "[YYYY]").unwrap(), "[2026]");
    }
}
