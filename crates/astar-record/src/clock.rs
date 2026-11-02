// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! Civil time without a date crate: Howard Hinnant's days-to-civil and
//! civil-to-days, plus RFC 3339 for UTC timestamps.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A calendar date and wall-clock time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// Days since 1970-01-01 to (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day) to days since 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil time at `unix_secs` in a zone `utc_offset_secs` east of UTC.
#[must_use]
pub fn civil(unix_secs: i64, utc_offset_secs: i32) -> Civil {
    let t = unix_secs + i64::from(utc_offset_secs);
    let days = t.div_euclid(86_400);
    let sod = u32::try_from(t.rem_euclid(86_400)).unwrap_or(0);
    let (year, month, day) = civil_from_days(days);
    Civil {
        year,
        month,
        day,
        hour: sod / 3600,
        minute: sod % 3600 / 60,
        second: sod % 60,
    }
}

pub(crate) fn unix_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs().saturating_add(1)).unwrap_or(i64::MAX),
    }
}

/// `2026-10-26T19:00:04Z`, whole seconds.
#[must_use]
pub fn rfc3339_utc(t: SystemTime) -> String {
    let c = civil(unix_secs(t), 0);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        c.year, c.month, c.day, c.hour, c.minute, c.second
    )
}

fn num(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Parses `YYYY-MM-DDTHH:MM:SS`, optional fractional seconds (`.` and one
/// or more digits, truncated to the whole second), then `Z` or `±HH:MM`.
#[must_use]
pub fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    if !s.is_ascii() || s.len() < 20 {
        return None;
    }
    let b = s.as_bytes();
    if b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let year = num(&s[0..4])?;
    let (month, day) = (num(&s[5..7])?, num(&s[8..10])?);
    let (hour, minute, second) = (num(&s[11..13])?, num(&s[14..16])?, num(&s[17..19])?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut zone = &s[19..];
    if let Some(frac) = zone.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        zone = &frac[digits..];
    }
    let offset: i64 = if zone == "Z" || zone == "z" {
        0
    } else {
        let zb = zone.as_bytes();
        if zone.len() != 6 || zb[3] != b':' {
            return None;
        }
        let sign = match zb[0] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let (oh, om) = (num(&zone[1..3])?, num(&zone[4..6])?);
        if oh > 23 || om > 59 {
            return None;
        }
        sign * i64::from(oh * 3600 + om * 60)
    };
    let days = days_from_civil(i64::from(year), month, day);
    let secs = days * 86_400 + i64::from(hour * 3600 + minute * 60 + second) - offset;
    if secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(secs.cast_unsigned()))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn civil_handles_leap_days_and_offsets_across_midnight() {
        let t = parse_rfc3339("2028-02-29T23:30:00Z").unwrap();
        let c = civil(unix_secs(t), 3600);
        assert_eq!(
            (c.year, c.month, c.day, c.hour, c.minute),
            (2028, 3, 1, 0, 30)
        );
        let c = civil(unix_secs(t), 0);
        assert_eq!((c.year, c.month, c.day), (2028, 2, 29));
        let c = civil(0, 0);
        assert_eq!((c.year, c.month, c.day, c.hour), (1970, 1, 1, 0));
        let c = civil(-1, 0);
        assert_eq!((c.year, c.month, c.day, c.second), (1969, 12, 31, 59));
    }

    #[test]
    fn rfc3339_round_trips() {
        let t = at(1_793_062_804);
        assert_eq!(rfc3339_utc(t), "2026-10-27T01:00:04Z");
        assert_eq!(parse_rfc3339(&rfc3339_utc(t)), Some(t));
        assert_eq!(parse_rfc3339("2026-10-27T03:00:04+02:00"), Some(t));
        assert_eq!(parse_rfc3339("2026-10-26T19:00:04-06:00"), Some(t));
        assert_eq!(parse_rfc3339("nonsense"), None);
        assert_eq!(parse_rfc3339("2026-13-27T01:00:04Z"), None);
        assert_eq!(parse_rfc3339("2026-10-27T01:00:04+0200"), None);
    }

    #[test]
    fn fractional_seconds_are_accepted_and_truncated() {
        let t = at(1_793_062_804);
        assert_eq!(parse_rfc3339("2026-10-27T01:00:04.999Z"), Some(t));
        assert_eq!(parse_rfc3339("2026-10-27T01:00:04.1Z"), Some(t));
        assert_eq!(parse_rfc3339("2026-10-27T03:00:04.123456+02:00"), Some(t));
        assert_eq!(
            parse_rfc3339("2026-10-27T01:00:04.Z"),
            None,
            "a dot needs digits"
        );
        assert_eq!(parse_rfc3339("2026-10-27T01:00:04.5x"), None);
        assert_eq!(parse_rfc3339("2026-10-27T01:00:04,5Z"), None);
    }
}
