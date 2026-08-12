// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Pure keying-decision logic for the UCI150 serial PTT bridge (iax-8e3b).
//! No I/O — the serial thread feeds it samples and applies the actions.

use std::time::{Duration, Instant};

/// What drives RTS (the radio PTT) while the harness is receiving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxKeyMode {
    /// Key the radio while the peer is keyed (`RemotePtt`). Default.
    RemotePtt,
    /// Key the radio while inbound audio is active (level over a floor),
    /// held for a hang time after it stops.
    RxActivity,
}

#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// `true`: CTS asserted == handset keyed. `false`: inverted.
    pub cts_keyed_high: bool,
    /// `true`: assert RTS to key the radio. `false`: inverted.
    pub rts_key_high: bool,
    /// CTS must hold a value this long before it counts (de-glitch).
    pub cts_debounce: Duration,
    pub rx_mode: RxKeyMode,
    /// `RxActivity`: `rx_level_db` strictly above this counts as active.
    pub rx_floor_db: f32,
    /// `RxActivity`: keep the radio keyed this long after audio stops.
    pub rx_hang: Duration,
}

/// Edge-triggered actions. `None` == nothing to do this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BridgeAction {
    /// Set the harness network PTT (handset key).
    pub set_local_ptt: Option<bool>,
    /// Set the physical RTS line (radio key).
    pub set_rts: Option<bool>,
}

pub struct PttBridge {
    config: BridgeConfig,
    last_keyed: bool,
    stable_since: Option<Instant>,
    emitted_local_ptt: Option<bool>,
    rx_active_until: Option<Instant>,
    emitted_rts: Option<bool>,
}

impl PttBridge {
    #[must_use]
    pub fn new(config: BridgeConfig) -> Self {
        // Pre-seed emitted_local_ptt so the idle (unkeyed) state at startup
        // does not count as an edge and spuriously fires on the first stable tick.
        Self {
            config,
            last_keyed: false,
            stable_since: None,
            emitted_local_ptt: Some(false),
            rx_active_until: None,
            emitted_rts: None,
        }
    }

    /// One sample. `cts_asserted` is the raw CTS read; `remote_keyed` and
    /// `rx_level_db` come from the latest console snapshot.
    pub fn tick(
        &mut self,
        cts_asserted: bool,
        remote_keyed: bool,
        rx_level_db: f32,
        now: Instant,
    ) -> BridgeAction {
        let mut action = BridgeAction::default();

        // --- local PTT from CTS, debounced ---
        let keyed = cts_asserted == self.config.cts_keyed_high;
        if let Some(since) = self.stable_since {
            if self.last_keyed == keyed {
                if now.duration_since(since) >= self.config.cts_debounce
                    && self.emitted_local_ptt != Some(keyed)
                {
                    action.set_local_ptt = Some(keyed);
                    self.emitted_local_ptt = Some(keyed);
                }
            } else {
                self.last_keyed = keyed;
                self.stable_since = Some(now);
            }
        } else {
            self.last_keyed = keyed;
            self.stable_since = Some(now);
        }

        // --- RTS from receive state ---
        let should_key_radio = match self.config.rx_mode {
            RxKeyMode::RemotePtt => remote_keyed,
            RxKeyMode::RxActivity => {
                if rx_level_db > self.config.rx_floor_db {
                    self.rx_active_until = Some(now + self.config.rx_hang);
                    true
                } else {
                    matches!(self.rx_active_until, Some(t) if now < t)
                }
            }
        };
        let rts_line = should_key_radio == self.config.rts_key_high;
        if self.emitted_rts != Some(rts_line) {
            action.set_rts = Some(rts_line);
            self.emitted_rts = Some(rts_line);
        }

        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn cfg() -> BridgeConfig {
        BridgeConfig {
            cts_keyed_high: true,
            rts_key_high: true,
            cts_debounce: Duration::from_millis(30),
            rx_mode: RxKeyMode::RemotePtt,
            rx_floor_db: -45.0,
            rx_hang: Duration::from_millis(250),
        }
    }

    #[test]
    fn cts_emits_keyed_after_stable_debounce() {
        let t0 = Instant::now();
        let mut b = PttBridge::new(cfg());
        assert_eq!(b.tick(false, false, -60.0, t0).set_local_ptt, None);
        assert_eq!(
            b.tick(true, false, -60.0, t0 + Duration::from_millis(5))
                .set_local_ptt,
            None
        );
        assert_eq!(
            b.tick(true, false, -60.0, t0 + Duration::from_millis(40))
                .set_local_ptt,
            Some(true)
        );
        assert_eq!(
            b.tick(true, false, -60.0, t0 + Duration::from_millis(80))
                .set_local_ptt,
            None
        );
        let _ = b.tick(false, false, -60.0, t0 + Duration::from_millis(85));
        assert_eq!(
            b.tick(false, false, -60.0, t0 + Duration::from_millis(130))
                .set_local_ptt,
            Some(false)
        );
    }

    #[test]
    fn cts_glitch_shorter_than_debounce_is_ignored() {
        let t0 = Instant::now();
        let mut b = PttBridge::new(cfg());
        let _ = b.tick(false, false, -60.0, t0);
        assert_eq!(
            b.tick(true, false, -60.0, t0 + Duration::from_millis(5))
                .set_local_ptt,
            None
        );
        assert_eq!(
            b.tick(false, false, -60.0, t0 + Duration::from_millis(10))
                .set_local_ptt,
            None
        );
        assert_eq!(
            b.tick(false, false, -60.0, t0 + Duration::from_millis(60))
                .set_local_ptt,
            None
        );
    }

    #[test]
    fn cts_polarity_inverted() {
        let t0 = Instant::now();
        let mut c = cfg();
        c.cts_keyed_high = false;
        let mut b = PttBridge::new(c);
        let _ = b.tick(true, false, -60.0, t0);
        let _ = b.tick(false, false, -60.0, t0 + Duration::from_millis(5));
        assert_eq!(
            b.tick(false, false, -60.0, t0 + Duration::from_millis(40))
                .set_local_ptt,
            Some(true)
        );
    }

    #[test]
    fn remote_ptt_drives_rts_on_edges_only() {
        let t0 = Instant::now();
        let mut b = PttBridge::new(cfg());
        assert_eq!(b.tick(false, false, -60.0, t0).set_rts, Some(false));
        assert_eq!(
            b.tick(false, true, -60.0, t0 + Duration::from_millis(20))
                .set_rts,
            Some(true)
        );
        assert_eq!(
            b.tick(false, true, -60.0, t0 + Duration::from_millis(40))
                .set_rts,
            None
        );
        assert_eq!(
            b.tick(false, false, -60.0, t0 + Duration::from_millis(60))
                .set_rts,
            Some(false)
        );
    }

    #[test]
    fn rts_polarity_inverted() {
        let t0 = Instant::now();
        let mut c = cfg();
        c.rts_key_high = false;
        let mut b = PttBridge::new(c);
        assert_eq!(b.tick(false, false, -60.0, t0).set_rts, Some(true));
        assert_eq!(
            b.tick(false, true, -60.0, t0 + Duration::from_millis(20))
                .set_rts,
            Some(false)
        );
    }

    #[test]
    fn rx_activity_mode_holds_through_hang_then_releases() {
        let t0 = Instant::now();
        let mut c = cfg();
        c.rx_mode = RxKeyMode::RxActivity;
        let mut b = PttBridge::new(c);
        assert_eq!(b.tick(false, false, -60.0, t0).set_rts, Some(false));
        assert_eq!(
            b.tick(false, false, -30.0, t0 + Duration::from_millis(20))
                .set_rts,
            Some(true)
        );
        assert_eq!(
            b.tick(false, false, -60.0, t0 + Duration::from_millis(100))
                .set_rts,
            None
        );
        assert_eq!(
            b.tick(false, false, -60.0, t0 + Duration::from_millis(300))
                .set_rts,
            Some(false)
        );
    }
}
