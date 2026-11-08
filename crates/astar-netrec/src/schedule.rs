// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! When nets happen: the `when` grammar and the windows it produces.
//!
//! A net's `when` text ("Tue 19:00", "daily 21:00", "2026-11-14 18:00") is a
//! wall-clock time in the net's own zone. This module turns it into
//! concrete [`Window`]s on the timeline, each carrying the join and leave
//! instants once the recorder's pre- and post-roll are added.
//!
//! Local times resolve with jiff's compatible disambiguation: a time inside
//! the spring-forward gap moves to just after the gap, and on the fall-back
//! night the earlier of the two instants is taken.

use jiff::civil::{Date, DateTime, Time, Weekday};
use jiff::{SignedDuration, Timestamp};

use crate::config::Config;

/// A parsed `when`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum When {
    /// Certain days of the week at a time.
    Weekly {
        /// The days it runs on.
        days: Vec<Weekday>,
        /// The local start time.
        time: Time,
    },
    /// Every day at a time.
    Daily {
        /// The local start time.
        time: Time,
    },
    /// Once, at a date and time.
    Once {
        /// The local start.
        at: DateTime,
    },
}

/// One occurrence of a net.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Index into `Config.nets`.
    pub net: usize,
    /// The net's start.
    pub start: Timestamp,
    /// The net's end.
    pub end: Timestamp,
    /// When the recorder joins: `start - pre_roll`.
    pub join_at: Timestamp,
    /// When the recorder leaves: `end + post_roll`.
    pub leave_at: Timestamp,
}

fn parse_weekday(w: &str) -> Option<Weekday> {
    Some(match w.to_ascii_lowercase().as_str() {
        "mon" | "monday" => Weekday::Monday,
        "tue" | "tuesday" => Weekday::Tuesday,
        "wed" | "wednesday" => Weekday::Wednesday,
        "thu" | "thursday" => Weekday::Thursday,
        "fri" | "friday" => Weekday::Friday,
        "sat" | "saturday" => Weekday::Saturday,
        "sun" | "sunday" => Weekday::Sunday,
        _ => return None,
    })
}

fn parse_time(s: &str) -> Result<Time, String> {
    let bad = || format!("{s:?} is not a time such as \"19:00\"");
    let (h, m) = s.split_once(':').ok_or_else(bad)?;
    if h.len() != 2 || m.len() != 2 {
        return Err(bad());
    }
    let h: i8 = h.parse().map_err(|_| bad())?;
    let m: i8 = m.parse().map_err(|_| bad())?;
    Time::new(h, m, 0, 0).map_err(|_| bad())
}

/// Parse a `when`: `"Tue 19:00"`, `"Mon Wed Fri 07:30"`, `"daily 21:00"` or
/// `"2026-11-14 18:00"`. Day names are three letters or full, any case.
///
/// # Errors
/// A sentence naming the text that could not be read.
pub fn parse_when(s: &str) -> Result<When, String> {
    let refuse = |why: &str| {
        format!(
            "{s:?} is not a schedule such as \"Tue 19:00\", \"daily 21:00\" or \"2026-11-14 18:00\" ({why})"
        )
    };
    let parts: Vec<&str> = s.split_whitespace().collect();
    let Some((time_text, head)) = parts.split_last() else {
        return Err(refuse("empty"));
    };
    if head.is_empty() {
        return Err(refuse("no day or date"));
    }
    let time = parse_time(time_text).map_err(|e| refuse(&e))?;
    if head.len() == 1 {
        if head[0].eq_ignore_ascii_case("daily") {
            return Ok(When::Daily { time });
        }
        if let Ok(date) = head[0].parse::<Date>() {
            return Ok(When::Once {
                at: date.to_datetime(time),
            });
        }
    }
    let mut days = Vec::new();
    for w in head {
        let d = parse_weekday(w).ok_or_else(|| refuse(&format!("unknown day {w:?}")))?;
        if !days.contains(&d) {
            days.push(d);
        }
    }
    Ok(When::Weekly { days, time })
}

fn roll(d: std::time::Duration) -> SignedDuration {
    SignedDuration::try_from(d).unwrap_or(SignedDuration::MAX)
}

fn add(t: Timestamp, d: SignedDuration) -> Timestamp {
    t.checked_add(d).unwrap_or(t)
}

/// Every window of every net that is open or near-open during `[from, to)`:
/// those whose `[join_at, leave_at)` intersects it. Sorted by `join_at`.
/// A net whose `when` cannot be parsed yields nothing.
#[must_use]
pub fn windows_between(cfg: &Config, from: Timestamp, to: Timestamp) -> Vec<Window> {
    let pre = roll(cfg.recorder.pre_roll);
    let post = roll(cfg.recorder.post_roll);
    let mut out = Vec::new();
    for (idx, net) in cfg.nets.iter().enumerate() {
        let Ok(when) = parse_when(&net.when) else {
            continue;
        };
        let dur = roll(net.duration);
        // A window can start days before `from` and still reach it.
        let reach_days = dur
            .as_secs()
            .saturating_add(pre.as_secs())
            .saturating_add(post.as_secs())
            .clamp(0, 400 * 86_400)
            / 86_400
            + 1;
        let first = from.to_zoned(net.tz.clone()).date();
        let last = to.to_zoned(net.tz.clone()).date();
        let Ok(mut day) = first.checked_sub(jiff::Span::new().days(reach_days)) else {
            continue;
        };
        let Ok(stop) = last.checked_add(jiff::Span::new().days(1)) else {
            continue;
        };
        while day <= stop {
            let time = match &when {
                When::Daily { time } => Some(*time),
                When::Weekly { days, time } => days.contains(&day.weekday()).then_some(*time),
                When::Once { at } => (at.date() == day).then(|| at.time()),
            };
            if let Some(z) = time.and_then(|t| {
                net.tz
                    .to_ambiguous_zoned(day.to_datetime(t))
                    .compatible()
                    .ok()
            }) {
                {
                    let start = z.timestamp();
                    let end = add(start, dur);
                    let w = Window {
                        net: idx,
                        start,
                        end,
                        join_at: add(start, -pre),
                        leave_at: add(end, post),
                    };
                    if w.join_at < to && w.leave_at > from {
                        out.push(w);
                    }
                }
            }
            match day.checked_add(jiff::Span::new().days(1)) {
                Ok(d) => day = d,
                Err(_) => break,
            }
        }
    }
    out.sort_by_key(|w| (w.join_at, w.net));
    out
}

/// The window to act on at `now`: one with `join_at <= now < leave_at` if
/// any (the earliest-joined), otherwise the next to begin. Looks a year
/// ahead.
#[must_use]
pub fn current_or_next(cfg: &Config, now: Timestamp) -> Option<Window> {
    let horizon = add(now, SignedDuration::from_hours(24 * 366));
    let all = windows_between(cfg, now, horizon);
    all.iter()
        .find(|w| w.join_at <= now && now < w.leave_at)
        .or_else(|| all.iter().find(|w| w.join_at > now))
        .copied()
}

/// The fewest days an overlap check looks ahead: a whole year of clock
/// changes, because two zones can drift into a clash for just a few weeks.
const OVERLAP_YEAR_DAYS: u32 = 366;
/// The most it looks ahead. A one-off further out than ten years is not
/// checked until it comes within reach.
const OVERLAP_MAX_DAYS: u32 = 3_660;

/// How far ahead an overlap check must look from `now`, so that a config that
/// passes today cannot start failing as a far-off one-off comes into range:
/// a year, or past the last `Once` net, within [`OVERLAP_MAX_DAYS`].
#[must_use]
pub fn overlap_horizon_days(cfg: &Config, now: Timestamp) -> u32 {
    let days_to = |net: &crate::config::NetCfg| {
        let Ok(When::Once { at }) = parse_when(&net.when) else {
            return None;
        };
        let start = net.tz.to_ambiguous_zoned(at).compatible().ok()?;
        let secs = start.timestamp().duration_since(now).as_secs();
        // Past the start by the longest a window can run (24 h and a 24 h
        // roll), with a day to spare. One that has been and gone counts for
        // nothing.
        let days = (secs / 86_400 + 4).clamp(0, i64::from(OVERLAP_MAX_DAYS));
        u32::try_from(days).ok()
    };
    cfg.nets
        .iter()
        .filter_map(days_to)
        .fold(OVERLAP_YEAR_DAYS, u32::max)
        .min(OVERLAP_MAX_DAYS)
}

/// Pairs of nets whose windows (with rolls) overlap within `horizon_days`
/// from `from`: `(earlier net, later net, where the overlap begins)`.
#[must_use]
pub fn overlaps_from(
    cfg: &Config,
    from: Timestamp,
    horizon_days: u32,
) -> Vec<(usize, usize, Timestamp)> {
    let to = add(
        from,
        SignedDuration::from_hours(24 * i64::from(horizon_days)),
    );
    let ws = windows_between(cfg, from, to);
    let mut found: Vec<(usize, usize, Timestamp)> = Vec::new();
    for (i, a) in ws.iter().enumerate() {
        for b in &ws[i + 1..] {
            // Sorted by `join_at`, so nothing later can reach back to `a`.
            if b.join_at >= a.leave_at {
                break;
            }
            if a.net != b.net && a.join_at < b.leave_at {
                let (x, y) = if a.net < b.net {
                    (a.net, b.net)
                } else {
                    (b.net, a.net)
                };
                found.push((x, y, a.join_at.max(b.join_at)));
            }
        }
    }
    found.sort_by_key(|&(_, _, t)| t);
    found
}

/// [`overlaps_from`] starting now.
#[must_use]
pub fn overlaps(cfg: &Config, horizon_days: u32) -> Vec<(usize, usize, Timestamp)> {
    overlaps_from(cfg, Timestamp::now(), horizon_days)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::from_toml_str;
    use std::fmt::Write as _;

    fn cfg(nets: &[(&str, &str, &str)]) -> Config {
        let mut s = String::from(
            "[recorder]\nroot = \"/tmp/x\"\ncallsign = \"N0CALL\"\npre_roll = \"5m\"\npost_roll = \"10m\"\n",
        );
        for (name, when, dur) in nets {
            let _ = write!(
                s,
                "[[net]]\nname = \"{name}\"\nnetwork = \"m17\"\nhost = \"h\"\nmodule = \"C\"\nwhen = \"{when}\"\nduration = \"{dur}\"\ntz = \"America/Denver\"\n"
            );
        }
        from_toml_str(&s).unwrap()
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn parses_every_form() {
        let t = |h, m| Time::new(h, m, 0, 0).unwrap();
        assert_eq!(
            parse_when("Tue 19:00").unwrap(),
            When::Weekly {
                days: vec![Weekday::Tuesday],
                time: t(19, 0)
            }
        );
        assert_eq!(
            parse_when("mon WEDNESDAY fri 07:30").unwrap(),
            When::Weekly {
                days: vec![Weekday::Monday, Weekday::Wednesday, Weekday::Friday],
                time: t(7, 30)
            }
        );
        assert_eq!(
            parse_when("Daily 21:00").unwrap(),
            When::Daily { time: t(21, 0) }
        );
        assert_eq!(
            parse_when("2026-11-14 18:00").unwrap(),
            When::Once {
                at: jiff::civil::date(2026, 11, 14).at(18, 0, 0, 0)
            }
        );
        for bad in [
            "",
            "19:00",
            "Tue",
            "Tue 25:00",
            "Funday 19:00",
            "Tue 7:00",
            "2026-13-01 10:00",
        ] {
            assert!(parse_when(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn weekly_lists_tuesdays_in_denver() {
        let c = cfg(&[("t", "Tue 19:00", "90m")]);
        let ws = windows_between(&c, ts("2026-11-01T00:00:00Z"), ts("2026-11-09T00:00:00Z"));
        let starts: Vec<_> = ws.iter().map(|w| w.start).collect();
        assert_eq!(
            starts,
            vec![ts("2026-11-04T02:00:00Z"), ts("2026-11-11T02:00:00Z")][..1]
        );
        assert_eq!(ws[0].end, ts("2026-11-04T03:30:00Z"));
    }

    #[test]
    fn dst_gap_and_fold() {
        let c = cfg(&[("t", "daily 02:30", "30m")]);
        let ws = windows_between(&c, ts("2026-03-08T00:00:00Z"), ts("2026-03-09T00:00:00Z"));
        // 03:30 MDT = 09:30Z
        assert!(
            ws.iter().any(|w| w.start == ts("2026-03-08T09:30:00Z")),
            "{ws:?}"
        );
        let c = cfg(&[("t", "daily 01:30", "30m")]);
        let ws = windows_between(&c, ts("2026-11-01T06:00:00Z"), ts("2026-11-01T12:00:00Z"));
        let on_day: Vec<_> = ws
            .iter()
            .filter(|w| {
                w.start >= ts("2026-11-01T00:00:00Z") && w.start < ts("2026-11-02T00:00:00Z")
            })
            .collect();
        assert_eq!(on_day.len(), 1);
        // 01:30 MDT = 07:30Z
        assert_eq!(on_day[0].start, ts("2026-11-01T07:30:00Z"));
    }

    #[test]
    fn pre_and_post_roll_applied() {
        let c = cfg(&[("t", "Tue 19:00", "90m")]);
        let w = windows_between(&c, ts("2026-11-03T00:00:00Z"), ts("2026-11-05T00:00:00Z"))[0];
        assert_eq!(w.join_at, ts("2026-11-04T01:55:00Z"));
        assert_eq!(w.leave_at, ts("2026-11-04T03:40:00Z"));
        assert_eq!(w.net, 0);
    }

    #[test]
    fn current_or_next_prefers_open_window() {
        let c = cfg(&[("t", "Tue 19:00", "90m")]);
        // In the pre-roll: the open window wins.
        let w = current_or_next(&c, ts("2026-11-04T01:57:00Z")).unwrap();
        assert_eq!(w.start, ts("2026-11-04T02:00:00Z"));
        // After leave: the next week's.
        let w = current_or_next(&c, ts("2026-11-04T03:40:00Z")).unwrap();
        assert_eq!(w.start, ts("2026-11-11T02:00:00Z"));
        // A one-off in the past yields nothing.
        let c = cfg(&[("t", "2020-01-01 10:00", "30m")]);
        assert!(current_or_next(&c, ts("2026-11-04T00:00:00Z")).is_none());
    }

    #[test]
    fn overlap_detected_across_rolls() {
        // A leaves at +100 min; B joins at +95-5 = +90 min.
        let c = cfg(&[("a", "Tue 19:00", "90m"), ("b", "Tue 20:35", "90m")]);
        let o = overlaps_from(&c, ts("2026-11-01T00:00:00Z"), 35);
        assert_eq!(o.len(), 5); // one per Tuesday in 35 days
        assert_eq!((o[0].0, o[0].1), (0, 1));
        // Far enough apart: none.
        let c = cfg(&[("a", "Tue 19:00", "90m"), ("b", "Tue 22:00", "90m")]);
        assert_eq!(overlaps_from(&c, ts("2026-11-01T00:00:00Z"), 35).len(), 0);
    }

    #[test]
    fn overlap_horizon_reaches_past_the_last_one_off() {
        let now = ts("2026-11-03T12:00:00Z");
        let days = |when| overlap_horizon_days(&cfg(&[("a", when, "90m")]), now);
        assert_eq!(days("Tue 19:00"), OVERLAP_YEAR_DAYS);
        assert_eq!(days("2026-12-01 19:00"), OVERLAP_YEAR_DAYS);
        assert_eq!(days("1999-01-01 19:00"), OVERLAP_YEAR_DAYS);
        assert_eq!(days("2028-03-07 19:00"), 494);
        assert_eq!(days("2100-01-05 19:00"), OVERLAP_MAX_DAYS);
    }

    /// Sorted windows end the search early, and a long window still reaches
    /// past a short one to the next.
    #[test]
    fn overlap_is_found_past_a_window_that_ends_first() {
        let c = cfg(&[
            ("long", "2026-11-10 19:00", "5h"),
            ("short", "2026-11-10 19:30", "10m"),
            ("late", "2026-11-10 23:00", "10m"),
        ]);
        let o = overlaps_from(&c, ts("2026-11-01T00:00:00Z"), 35);
        let pairs: Vec<(usize, usize)> = o.iter().map(|&(a, b, _)| (a, b)).collect();
        assert_eq!(pairs, [(0, 1), (0, 2)]);
    }
}
