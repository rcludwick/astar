// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Time helpers: an injectable [`Clock`] so the timing helpers can be tested
//! without sleeping, and UTC timestamp formatting without a date crate.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A monotonic clock that can sleep. [`SystemClock`] is the real one; tests
/// use a fake that advances on `sleep`.
pub trait Clock {
    /// Monotonic time since an arbitrary, fixed origin.
    fn elapsed(&self) -> Duration;
    /// Block for `d` (a fake clock just advances).
    fn sleep(&mut self, d: Duration);
}

/// The real clock: `Instant` plus `thread::sleep`.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn elapsed(&self) -> Duration {
        self.origin.elapsed()
    }
    fn sleep(&mut self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// Seconds since the Unix epoch, now.
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Civil (proleptic Gregorian) date from days since 1970-01-01.
/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    // d is 1..=31 and m is 1..=12 by construction.
    (
        y,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

fn split(unix: u64) -> (i64, u32, u32, u64, u64, u64) {
    let days = i64::try_from(unix / 86_400).unwrap_or(0);
    let secs = unix % 86_400;
    let (y, m, d) = civil_from_days(days);
    (y, m, d, secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// RFC 3339 UTC, e.g. `2026-10-11T20:15:03Z`.
#[must_use]
pub fn rfc3339_utc(unix: u64) -> String {
    let (y, m, d, hh, mm, ss) = split(unix);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// A sortable, filesystem-safe run id, e.g. `20261011T201503Z`.
#[must_use]
pub fn run_id(unix: u64) -> String {
    let (y, m, d, hh, mm, ss) = split(unix);
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

#[cfg(test)]
pub(crate) mod fake {
    use super::Clock;
    use std::time::Duration;

    /// A clock that only moves when slept on.
    #[derive(Debug, Default)]
    pub struct FakeClock {
        pub now: Duration,
    }

    impl Clock for FakeClock {
        fn elapsed(&self) -> Duration {
            self.now
        }
        fn sleep(&mut self, d: Duration) {
            self.now += d;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_formats() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(run_id(0), "19700101T000000Z");
    }

    #[test]
    fn known_dates_format() {
        // 2026-10-11T20:15:03Z
        assert_eq!(rfc3339_utc(1_791_749_703), "2026-10-11T20:15:03Z");
        // Leap day 2024-02-29T12:00:00Z
        assert_eq!(rfc3339_utc(1_709_208_000), "2024-02-29T12:00:00Z");
        // Year end 1999-12-31T23:59:59Z
        assert_eq!(rfc3339_utc(946_684_799), "1999-12-31T23:59:59Z");
    }
}
