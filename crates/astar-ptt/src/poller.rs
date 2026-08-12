// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Single-step PTT driver for the poll model: the consumer calls [`Poller::tick`]
//! on its own ~20 ms cadence instead of the library owning a thread (see
//! [`crate::spawn`] for the threaded variant). One tick reads the operator key
//! line, runs one [`PttBridge`] decision, and writes the radio-key line.

use std::time::Instant;

use crate::{BridgeConfig, PttBackend, PttBridge, PttError};

/// What a tick decided for the consumer's call PTT. `None` == no change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TickOutcome {
    pub set_local_ptt: Option<bool>,
}

/// Owns a backend + bridge; drives one keying step per [`tick`](Self::tick).
pub struct Poller {
    backend: Box<dyn PttBackend>,
    bridge: PttBridge,
    radio_level: bool,
}

impl Poller {
    #[must_use]
    pub fn new(backend: Box<dyn PttBackend>, config: BridgeConfig) -> Self {
        Self {
            backend,
            bridge: PttBridge::new(config),
            radio_level: false,
        }
    }

    /// One keying step. `remote_keyed`/`rx_level_db` come from the consumer's
    /// `AstarStation` snapshot; the returned `set_local_ptt` should be forwarded to
    /// the call (`iax_station_set_ptt`). The radio line is written here.
    ///
    /// # Errors
    /// [`PttError`] if the backend read or write fails.
    pub fn tick(
        &mut self,
        remote_keyed: bool,
        rx_level_db: f32,
        now: Instant,
    ) -> Result<TickOutcome, PttError> {
        let key_raw = self.backend.read_key()?;
        let action = self.bridge.tick(key_raw, remote_keyed, rx_level_db, now);
        if let Some(level) = action.set_rts {
            self.radio_level = level;
        }
        self.backend.set_radio_key(self.radio_level)?;
        Ok(TickOutcome {
            set_local_ptt: action.set_local_ptt,
        })
    }

    /// Drop the radio key (called on teardown).
    pub fn fail_safe(&mut self) {
        self.backend.fail_safe();
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        self.fail_safe();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RxKeyMode;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    struct FakeBackend {
        cts: Arc<AtomicBool>,
        radio_writes: Arc<Mutex<Vec<bool>>>,
        failsafed: Arc<AtomicBool>,
    }
    impl PttBackend for FakeBackend {
        fn read_key(&mut self) -> Result<bool, PttError> {
            Ok(self.cts.load(Ordering::Relaxed))
        }
        fn set_radio_key(&mut self, level: bool) -> Result<(), PttError> {
            self.radio_writes.lock().unwrap().push(level);
            Ok(())
        }
        fn fail_safe(&mut self) {
            self.failsafed.store(true, Ordering::Relaxed);
        }
    }

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
    fn debounced_key_edge_surfaces_and_remote_drives_radio_then_failsafe() {
        let cts = Arc::new(AtomicBool::new(false));
        let radio = Arc::new(Mutex::new(Vec::new()));
        let failsafed = Arc::new(AtomicBool::new(false));
        let mut p = Poller::new(
            Box::new(FakeBackend {
                cts: Arc::clone(&cts),
                radio_writes: Arc::clone(&radio),
                failsafed: Arc::clone(&failsafed),
            }),
            cfg(),
        );
        let t0 = Instant::now();
        // Key held from t0; edge emerges only after the 30 ms debounce window.
        cts.store(true, Ordering::Relaxed);
        assert_eq!(p.tick(false, -60.0, t0).unwrap().set_local_ptt, None);
        assert_eq!(
            p.tick(false, -60.0, t0 + Duration::from_millis(40))
                .unwrap()
                .set_local_ptt,
            Some(true)
        );
        // Remote keys → radio line goes high this tick.
        let _ = p.tick(true, -60.0, t0 + Duration::from_millis(60)).unwrap();
        assert_eq!(radio.lock().unwrap().last(), Some(&true));
        // Drop runs fail-safe.
        drop(p);
        assert!(failsafed.load(Ordering::Relaxed));
    }
}
