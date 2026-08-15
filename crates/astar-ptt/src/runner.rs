// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The 20 ms PTT loop: backend lines in/out, [`PttBridge`] decisions, and
//! consumer integration via [`PttIo`] callbacks (no session-type coupling).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::{BridgeConfig, PttBackend, PttBridge};

/// Per-tick inputs for the radio-key decision (`RemotePtt` / `RxActivity` modes).
pub struct RadioKeyInput {
    /// Is the remote end keyed (e.g. `ConsoleState::remote_ptt`)?
    pub remote_keyed: bool,
    /// Current receive level in dBFS (for the `RxActivity` mode).
    pub rx_level_db: f32,
}

/// Consumer integration callbacks. Both run on the runner thread.
pub struct PttIo {
    /// Debounced operator key edge (true = keyed) — key the consumer's call.
    pub on_key: Box<dyn FnMut(bool) + Send>,
    /// Sampled each tick to decide the radio-key line.
    pub radio_input: Box<dyn FnMut() -> RadioKeyInput + Send>,
}

/// RAII fail-safe: releases the radio key on ANY exit — normal stop AND a
/// panic unwind (e.g. inside a consumer callback).
struct FailSafe<'a>(&'a mut dyn PttBackend);
impl Drop for FailSafe<'_> {
    fn drop(&mut self) {
        self.0.fail_safe();
    }
}

/// Spawn the PTT loop. Returns `None` if the thread cannot be spawned.
/// The loop runs until `stop` is set; ~20 ms cadence.
#[must_use]
pub fn spawn(
    mut backend: Box<dyn PttBackend>,
    config: BridgeConfig,
    mut io: PttIo,
    stop: Arc<AtomicBool>,
) -> Option<JoinHandle<()>> {
    thread::Builder::new()
        .name("astar-ptt".into())
        .spawn(move || {
            let guard = FailSafe(backend.as_mut());
            let mut bridge = PttBridge::new(config);
            // Desired radio-key line, re-asserted EVERY tick: a single
            // dropped write can never leave the radio keyed.
            let mut radio_level = false;
            // Consecutive backend failures (reads + writes share the counter,
            // reset on any success): tolerate transient line glitches, but
            // exit on a persistently dead backend so the FailSafe guard (and
            // the port's Drop) release the radio key. ~3 ticks.
            let mut errors: u32 = 0;
            while !stop.load(Ordering::Relaxed) {
                let key_raw = if let Ok(v) = guard.0.read_key() {
                    errors = 0;
                    v
                } else {
                    errors += 1;
                    if errors >= 3 {
                        break;
                    }
                    false
                };
                let input = (io.radio_input)();
                let action = bridge.tick(
                    key_raw,
                    input.remote_keyed,
                    input.rx_level_db,
                    Instant::now(),
                );
                if let Some(on) = action.set_local_ptt {
                    (io.on_key)(on);
                }
                if let Some(level) = action.set_rts {
                    radio_level = level;
                }
                if guard.0.set_radio_key(radio_level).is_err() {
                    errors += 1;
                    if errors >= 3 {
                        break;
                    }
                } else {
                    errors = 0;
                }
                thread::sleep(Duration::from_millis(20));
            }
            // FailSafe::drop releases the radio key here (and on any panic).
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PttError, RxKeyMode};
    use std::sync::Mutex;

    /// Scripted backend: queued key reads, recorded radio writes + `fail_safe`.
    struct FakeBackend {
        keys: Arc<Mutex<Vec<bool>>>,   // pop front per read; last repeats
        writes: Arc<Mutex<Vec<bool>>>, // every set_radio_key level
        failsafed: Arc<AtomicBool>,
    }
    impl PttBackend for FakeBackend {
        fn read_key(&mut self) -> Result<bool, PttError> {
            let mut k = self.keys.lock().unwrap();
            Ok(if k.len() > 1 {
                k.remove(0)
            } else {
                *k.first().unwrap_or(&false)
            })
        }
        fn set_radio_key(&mut self, level: bool) -> Result<(), PttError> {
            self.writes.lock().unwrap().push(level);
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

    /// Poll `pred` until it holds or `timeout` elapses. Robust to CI thread-
    /// scheduling jitter, where a fixed sleep can race the runner thread on a
    /// loaded headless runner.
    fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if pred() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        pred()
    }

    #[test]
    fn key_edges_reach_on_key_and_remote_drives_radio_then_failsafe_on_stop() {
        let keys = Arc::new(Mutex::new(vec![false, false, true])); // settle, then key
        let writes = Arc::new(Mutex::new(Vec::new()));
        let failsafed = Arc::new(AtomicBool::new(false));
        let edges = Arc::new(Mutex::new(Vec::new()));
        let remote = Arc::new(AtomicBool::new(false));

        let stop = Arc::new(AtomicBool::new(false));
        let e2 = Arc::clone(&edges);
        let r2 = Arc::clone(&remote);
        let h = spawn(
            Box::new(FakeBackend {
                keys: Arc::clone(&keys),
                writes: Arc::clone(&writes),
                failsafed: Arc::clone(&failsafed),
            }),
            cfg(),
            PttIo {
                on_key: Box::new(move |on| e2.lock().unwrap().push(on)),
                radio_input: Box::new(move || RadioKeyInput {
                    remote_keyed: r2.load(Ordering::Relaxed),
                    rx_level_db: -60.0,
                }),
            },
            Arc::clone(&stop),
        )
        .expect("runner spawns");

        // Key held: after debounce, exactly one true edge. Poll for the edge
        // instead of fixed-sleeping so a starved CI thread can't race the assert.
        wait_until(Duration::from_secs(2), || !edges.lock().unwrap().is_empty());
        assert_eq!(
            edges.lock().unwrap().as_slice(),
            &[true],
            "one debounced edge"
        );

        // Remote keys → radio line goes (and stays re-asserted) high. Poll for
        // the radio write rather than fixed-sleeping, for CI robustness.
        remote.store(true, Ordering::Relaxed);
        wait_until(Duration::from_secs(2), || {
            writes.lock().unwrap().last().copied() == Some(true)
        });
        assert!(
            *writes.lock().unwrap().last().unwrap(),
            "radio keyed on remote"
        );

        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
        assert!(failsafed.load(Ordering::Relaxed), "fail_safe on stop");
    }

    #[test]
    fn dead_backend_exits_loop_and_fail_safes() {
        /// Backend whose reads always fail, like a USB device unplugged mid-TX.
        struct DeadBackend {
            failsafed: Arc<AtomicBool>,
        }
        impl PttBackend for DeadBackend {
            fn read_key(&mut self) -> Result<bool, PttError> {
                Err(PttError::Io(std::io::Error::other("unplugged")))
            }
            fn set_radio_key(&mut self, _level: bool) -> Result<(), PttError> {
                Err(PttError::Io(std::io::Error::other("unplugged")))
            }
            fn fail_safe(&mut self) {
                self.failsafed.store(true, Ordering::Relaxed);
            }
        }

        let failsafed = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false)); // never set: only the error counter can end the loop
        let h = spawn(
            Box::new(DeadBackend {
                failsafed: Arc::clone(&failsafed),
            }),
            cfg(),
            PttIo {
                on_key: Box::new(|_| {}),
                radio_input: Box::new(|| RadioKeyInput {
                    remote_keyed: false,
                    rx_level_db: -60.0,
                }),
            },
            Arc::clone(&stop),
        )
        .expect("runner spawns");
        // The loop must give up after ~3 failing ticks; join blocks until then.
        h.join()
            .expect("persistent backend errors exit cleanly, not via panic");
        assert!(
            failsafed.load(Ordering::Relaxed),
            "fail_safe ran on backend-death exit"
        );
    }

    #[test]
    fn panicking_callback_still_fail_safes() {
        let failsafed = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let h = spawn(
            Box::new(FakeBackend {
                keys: Arc::new(Mutex::new(vec![true])), // immediate key → edge → panic
                writes: Arc::new(Mutex::new(Vec::new())),
                failsafed: Arc::clone(&failsafed),
            }),
            cfg(),
            PttIo {
                on_key: Box::new(|_| panic!("consumer bug")),
                radio_input: Box::new(|| RadioKeyInput {
                    remote_keyed: false,
                    rx_level_db: -60.0,
                }),
            },
            Arc::clone(&stop),
        )
        .expect("runner spawns");
        assert!(h.join().is_err(), "thread died from the callback panic");
        assert!(
            failsafed.load(Ordering::Relaxed),
            "fail_safe ran during unwind"
        );
    }
}
