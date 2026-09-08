// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Who has keyed up on the live digital link, newest first.
//!
//! One "last heard" name is not enough on a reflector where a station keys
//! a short tail after every over: the tail's callsign overwrites the real
//! talker's a second after they unkey. A short history keeps both. Each
//! network session owns one of these in its shared state, so the history
//! lives exactly as long as the link does.
//!
//! Callsigns here are whatever the far end put on the wire.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

/// How many stations the log remembers.
pub const HEARD_CAPACITY: usize = 8;

/// One row of [`HeardLog::snapshot`]. `age_ms` is measured when the
/// snapshot is taken, so a consumer polling every 50 ms sees it grow.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct HeardEntry {
    /// The callsign as heard on the wire, trimmed.
    pub callsign: String,
    /// The app's `Network.rawValue`: `"m17"`, `"dstar"`, `"ysf"`, `"nxdn"`, `"dmr"`.
    pub network: &'static str,
    /// How long ago this station was LAST heard, measured at snapshot time:
    /// their most recent frame on the AMBE links, or the end of their over
    /// on M17 and D-Star. Never the moment their over began — a station who
    /// unkeyed a second ago reads as one second old however long they
    /// talked.
    pub age_ms: u64,
}

/// One recorded keyup: a callsign, the network it came in on, and when.
#[derive(Debug)]
struct Heard {
    callsign: String,
    network: &'static str,
    at: Instant,
}

/// A bounded, newest-first log. A mutex rather than atomics because the
/// payload is a `String`.
///
/// How often it is touched depends on where the network puts the talker's
/// name. The AMBE links (YSF, NXDN, DMR) carry it in every frame's header
/// and already wrote their one-name last-heard slot per frame, so they note
/// per frame too. M17 and D-Star note twice per stream — at its first packet
/// and at its end — which keeps a callsign decode and a mutex off their
/// 20 ms receive path while still giving [`HeardEntry::age_ms`] the same
/// meaning everywhere. Nothing here ever touches decoded audio.
///
/// `Debug` because three of the link modules that own one derive it on their
/// whole shared-state struct.
#[derive(Debug, Default)]
pub struct HeardLog {
    inner: Mutex<VecDeque<Heard>>,
}

impl HeardLog {
    /// A fresh, empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `callsign` as heard now on `network`. Blank callsigns are
    /// dropped — "Last heard " with nothing after it is worse than no row.
    /// A station already at the front is moved to now rather than repeated;
    /// a station heard earlier but not at the front is moved to the front,
    /// dropping its older row, so the log never carries two entries for the
    /// same network+callsign.
    pub fn note(&self, network: &'static str, callsign: &str) {
        let callsign = callsign.trim();
        if callsign.is_empty() {
            return;
        }
        let mut log = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(front) = log.front_mut()
            && front.network == network
            && front.callsign == callsign
        {
            front.at = Instant::now();
            return;
        }
        log.retain(|h| !(h.network == network && h.callsign == callsign));
        log.push_front(Heard {
            callsign: callsign.to_owned(),
            network,
            at: Instant::now(),
        });
        log.truncate(HEARD_CAPACITY);
    }

    /// Newest first, with ages measured now.
    #[must_use]
    pub fn snapshot(&self) -> Vec<HeardEntry> {
        let now = Instant::now();
        let log = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        log.iter()
            .map(|h| HeardEntry {
                callsign: h.callsign.clone(),
                network: h.network,
                age_ms: u64::try_from(now.saturating_duration_since(h.at).as_millis())
                    .unwrap_or(u64::MAX),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_log_snapshots_to_nothing() {
        assert!(HeardLog::new().snapshot().is_empty());
    }

    #[test]
    fn newest_first_and_ages_grow_from_zero() {
        let log = HeardLog::new();
        log.note("m17", "KF5ILA");
        log.note("m17", "W6VS");
        let s = log.snapshot();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].callsign, "W6VS");
        assert_eq!(s[1].callsign, "KF5ILA");
        assert_eq!(s[0].network, "m17");
        assert!(s[0].age_ms <= s[1].age_ms, "the newer entry is never older");
        assert!(
            s[1].age_ms < 5_000,
            "ages are measured from the note, not from an epoch"
        );
    }

    #[test]
    fn the_same_station_keying_again_moves_to_the_front_without_a_duplicate() {
        let log = HeardLog::new();
        log.note("m17", "KF5ILA");
        log.note("m17", "W6VS");
        log.note("m17", "KF5ILA");
        let names: Vec<_> = log.snapshot().into_iter().map(|e| e.callsign).collect();
        assert_eq!(names, ["KF5ILA", "W6VS"]);
    }

    #[test]
    fn an_empty_callsign_is_ignored() {
        let log = HeardLog::new();
        log.note("dstar", "");
        log.note("dstar", "   ");
        assert!(log.snapshot().is_empty());
    }

    #[test]
    fn the_log_holds_at_most_heard_capacity() {
        let log = HeardLog::new();
        for i in 0..(HEARD_CAPACITY + 3) {
            log.note("ysf", &format!("N{i}CALL"));
        }
        let s = log.snapshot();
        assert_eq!(s.len(), HEARD_CAPACITY);
        assert_eq!(s[0].callsign, format!("N{}CALL", HEARD_CAPACITY + 2));
    }
}
