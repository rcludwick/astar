// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Pure keepalive / liveness / RTT bookkeeping for an `Active` call
//! (iax-a307). No I/O, no wall clock — every method takes `now: Instant`,
//! so it is unit-testable and drivable with simulated time.
//!
//! The module knows nothing of `AppEvent`: it returns edge flags, and the
//! FSM maps them onto `AppEvent::ConnectionLost` / `ConnectionRestored`.
//! Liveness model (RFC 5456 §6.7.2/§7.2 + locked decision Q2): a PING and a
//! LAGRQ go out every `ping_interval`; if no inbound frame has arrived for
//! `lost_after` or more, the lost edge fires exactly once; the first inbound
//! frame afterwards fires the restored edge exactly once. Loss never tears
//! the call down.

use std::time::{Duration, Instant};

/// Tunable keepalive knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeepaliveConfig {
    /// PING + LAGRQ cadence.
    pub ping_interval: Duration,
    /// Inbound-silence deadline that flips the lost edge. Checked on the
    /// `ping_interval` grid, so it should be a multiple of it; the check is
    /// `>=` so a deadline landing exactly on a grid point still trips.
    pub lost_after: Duration,
}

impl Default for KeepaliveConfig {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(2),
            lost_after: Duration::from_secs(4),
        }
    }
}

/// Outcome of a keepalive timer fire. A PING + LAGRQ is sent every fire,
/// unconditionally — this only carries the edge signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeepaliveTick {
    /// `true` exactly once when the silence deadline is first exceeded.
    pub connection_lost: bool,
}

/// Liveness + smoothed-RTT state for one active call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepaliveState {
    cfg: KeepaliveConfig,
    /// When the last inbound frame (full or mini) arrived.
    last_inbound: Instant,
    /// Timestamp stamped into the last PING/LAGRQ pair, and when it was
    /// sent. Kept until the next timer fire so PONG *and* LAGRP can each
    /// contribute an RTT sample.
    pending_ping: Option<(u32, Instant)>,
    /// Smoothed round-trip estimate (`srtt = 7/8·srtt + 1/8·sample`).
    srtt: Option<Duration>,
    /// Currently in the "connection lost" condition (edge-trigger state).
    lost: bool,
}

impl KeepaliveState {
    #[must_use]
    pub fn new(cfg: KeepaliveConfig, now: Instant) -> Self {
        Self {
            cfg,
            last_inbound: now,
            pending_ping: None,
            srtt: None,
            lost: false,
        }
    }

    #[must_use]
    pub fn config(&self) -> KeepaliveConfig {
        self.cfg
    }

    /// Record any inbound frame from the peer. Returns `true` exactly once
    /// when recovering from a lost connection (the restored edge).
    pub fn on_inbound(&mut self, now: Instant) -> bool {
        self.last_inbound = now;
        let restored = self.lost;
        self.lost = false;
        restored
    }

    /// The keepalive timer fired: a PING + LAGRQ stamped with `ts` is about
    /// to go out. Records the pending echo and evaluates the silence
    /// deadline.
    pub fn on_ping_timer(&mut self, ts: u32, now: Instant) -> KeepaliveTick {
        self.pending_ping = Some((ts, now));
        let silent_for = now.duration_since(self.last_inbound);
        let connection_lost = !self.lost && silent_for >= self.cfg.lost_after;
        if connection_lost {
            self.lost = true;
        }
        KeepaliveTick { connection_lost }
    }

    /// PONG received echoing `echoed_ts` (RFC 5456 §6.7.3). Folds an RTT
    /// sample iff it matches the outstanding PING/LAGRQ timestamp; an
    /// unmatched echo is ignored (never poisons `srtt`).
    pub fn on_pong(&mut self, echoed_ts: u32, now: Instant) {
        let Some((ts, sent_at)) = self.pending_ping else {
            return;
        };
        if ts != echoed_ts {
            return;
        }
        let sample = now.duration_since(sent_at);
        self.srtt = Some(match self.srtt {
            Some(srtt) => (srtt * 7 + sample) / 8,
            None => sample,
        });
    }

    /// LAGRP received (RFC 5456 §6.7.5) — same echo/sample semantics as PONG.
    pub fn on_lagrp(&mut self, echoed_ts: u32, now: Instant) {
        self.on_pong(echoed_ts, now);
    }

    /// Current smoothed round-trip estimate; `None` until the first echo.
    #[must_use]
    pub fn rtt(&self) -> Option<Duration> {
        self.srtt
    }

    /// `true` while inside a lost (silence-deadline exceeded) episode.
    #[must_use]
    pub fn is_lost(&self) -> bool {
        self.lost
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (KeepaliveState, Instant) {
        let now = Instant::now();
        (KeepaliveState::new(KeepaliveConfig::default(), now), now)
    }

    #[test]
    fn timer_below_deadline_does_not_trip_lost() {
        let (mut k, t0) = state();
        let tick = k.on_ping_timer(2_000, t0 + Duration::from_secs(2));
        assert!(!tick.connection_lost, "2s silent < 4s deadline");
        assert!(!k.is_lost());
    }

    #[test]
    fn lost_edge_fires_exactly_once_at_the_deadline() {
        let (mut k, t0) = state();
        let _ = k.on_ping_timer(2_000, t0 + Duration::from_secs(2));
        // Exactly at the deadline: >= must trip (the check runs on the 2s grid).
        let tick = k.on_ping_timer(4_000, t0 + Duration::from_secs(4));
        assert!(tick.connection_lost, "4s silent >= 4s deadline trips");
        // Still silent: no second edge.
        let tick = k.on_ping_timer(6_000, t0 + Duration::from_secs(6));
        assert!(!tick.connection_lost, "edge-triggered: fires once");
        assert!(k.is_lost());
    }

    #[test]
    fn inbound_resets_the_silence_deadline() {
        let (mut k, t0) = state();
        assert!(!k.on_inbound(t0 + Duration::from_secs(3)));
        let tick = k.on_ping_timer(5_000, t0 + Duration::from_secs(5));
        assert!(!tick.connection_lost, "only 2s since last inbound");
    }

    #[test]
    fn restored_edge_fires_exactly_once() {
        let (mut k, t0) = state();
        let _ = k.on_ping_timer(4_000, t0 + Duration::from_secs(4));
        assert!(k.is_lost());
        assert!(
            k.on_inbound(t0 + Duration::from_secs(5)),
            "first inbound after lost restores"
        );
        assert!(
            !k.on_inbound(t0 + Duration::from_secs(5)),
            "second inbound is not another edge"
        );
        assert!(!k.is_lost());
    }

    #[test]
    fn first_rtt_sample_is_taken_verbatim() {
        let (mut k, t0) = state();
        let sent = t0 + Duration::from_secs(2);
        let _ = k.on_ping_timer(2_000, sent);
        k.on_pong(2_000, sent + Duration::from_millis(100));
        assert_eq!(k.rtt(), Some(Duration::from_millis(100)));
    }

    #[test]
    fn rtt_is_smoothed_seven_eighths() {
        let (mut k, t0) = state();
        let _ = k.on_ping_timer(2_000, t0 + Duration::from_secs(2));
        k.on_pong(
            2_000,
            t0 + Duration::from_secs(2) + Duration::from_millis(100),
        );
        let _ = k.on_ping_timer(4_000, t0 + Duration::from_secs(4));
        k.on_pong(
            4_000,
            t0 + Duration::from_secs(4) + Duration::from_millis(200),
        );
        // (100ms * 7 + 200ms) / 8 = 112.5ms
        assert_eq!(k.rtt(), Some(Duration::from_micros(112_500)));
    }

    #[test]
    fn unmatched_echo_timestamp_is_ignored() {
        let (mut k, t0) = state();
        let _ = k.on_ping_timer(2_000, t0 + Duration::from_secs(2));
        k.on_pong(9_999, t0 + Duration::from_secs(3));
        assert_eq!(k.rtt(), None, "stale/foreign echo must not poison srtt");
    }

    #[test]
    fn pong_and_lagrp_both_sample_the_same_pending_ping() {
        let (mut k, t0) = state();
        let sent = t0 + Duration::from_secs(2);
        let _ = k.on_ping_timer(2_000, sent);
        k.on_pong(2_000, sent + Duration::from_millis(80));
        k.on_lagrp(2_000, sent + Duration::from_millis(160));
        // 80ms then (80*7 + 160)/8 = 90ms
        assert_eq!(k.rtt(), Some(Duration::from_millis(90)));
    }

    #[test]
    fn echo_with_no_pending_ping_is_ignored() {
        let (mut k, t0) = state();
        k.on_pong(2_000, t0 + Duration::from_secs(1));
        assert_eq!(k.rtt(), None);
    }

    #[test]
    fn stale_echo_from_previous_ping_cycle_is_ignored() {
        let (mut k, t0) = state();
        let _ = k.on_ping_timer(2_000, t0 + Duration::from_secs(2));
        // Next cycle replaces the pending ping...
        let _ = k.on_ping_timer(4_000, t0 + Duration::from_secs(4));
        // ...so a late echo of the old timestamp must not fold a sample.
        k.on_pong(
            2_000,
            t0 + Duration::from_secs(4) + Duration::from_millis(50),
        );
        assert_eq!(k.rtt(), None, "late echo of a superseded ping is ignored");
    }
}
