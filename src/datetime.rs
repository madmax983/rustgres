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
fn split_micros(micros: i64) -> (i64, i64) {
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
}
