// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Non-blocking facade over the threaded PTT runner (iax-239a). The worker
//! thread owns the backend and performs ALL hardware I/O; [`PttAgent::tick`]
//! is a lock-light mailbox exchange that can never block the caller.
//!
//! Why this exists: a USB control transfer can wedge forever when the device
//! re-enumerates mid-flight (the `IOKit` completion — and its device-side
//! timeout — die with the user client). With the poll-model [`crate::Poller`]
//! that parked the CONSUMER'S thread inside `tick` (astar's main thread →
//! permanent beach ball). Here a wedged transfer parks only the worker;
//! `tick` detects the stalled heartbeat and reports an error so the consumer
//! can tear down and re-open after the device returns.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::runner::{PttIo, RadioKeyInput};
use crate::{BridgeConfig, PttBackend, PttError};

/// Worker heartbeat age beyond which the backend is declared stalled. A
/// healthy tick is bounded by the backend's own I/O timeouts (200 ms per USB
/// control transfer); one second of silence means a transfer wedged.
const STALL_AFTER: Duration = Duration::from_secs(1);

/// How long teardown waits for a healthy worker to exit (running its
/// fail-safe radio unkey) before detaching a wedged one.
const CLOSE_GRACE: Duration = Duration::from_millis(500);

/// Pending debounced key edges are capped, dropping the oldest. Edges are
/// debounce-sparse; a full queue means the consumer stopped draining, and the
/// newest edges are the ones that still reflect the operator's intent.
const MAX_EDGES: usize = 8;

/// Mailbox between the consumer thread and the worker.
struct Shared {
    /// Consumer → worker: latest remote-keyed state from the call snapshot.
    remote_keyed: AtomicBool,
    /// Consumer → worker: latest RX level (f32 bits) from the call snapshot.
    rx_db_bits: AtomicU32,
    /// Worker → consumer: debounced operator key edges awaiting delivery.
    edges: Mutex<VecDeque<bool>>,
    /// Worker liveness: stamped once per completed loop iteration.
    heartbeat: Mutex<Instant>,
}

/// Owns the PTT worker thread; hands the consumer a non-blocking
/// [`tick`](Self::tick) with the same edge semantics as [`crate::Poller::tick`].
pub struct PttAgent {
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    stall_after: Duration,
    close_grace: Duration,
}

impl PttAgent {
    /// Spawn the worker around `backend`. `None` if the thread cannot spawn.
    #[must_use]
    pub fn new(backend: Box<dyn PttBackend>, config: BridgeConfig) -> Option<Self> {
        Self::with_timing(backend, config, STALL_AFTER, CLOSE_GRACE)
    }

    /// [`Self::new`] with explicit stall/teardown windows (test hook).
    #[must_use]
    pub fn with_timing(
        backend: Box<dyn PttBackend>,
        config: BridgeConfig,
        stall_after: Duration,
        close_grace: Duration,
    ) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared {
            remote_keyed: AtomicBool::new(false),
            rx_db_bits: AtomicU32::new((-120.0f32).to_bits()),
            edges: Mutex::new(VecDeque::new()),
            heartbeat: Mutex::new(Instant::now()),
        });
        let on_key_shared = Arc::clone(&shared);
        let input_shared = Arc::clone(&shared);
        let worker = crate::spawn(
            backend,
            config,
            PttIo {
                on_key: Box::new(move |on| {
                    let mut q = on_key_shared.edges.lock().unwrap();
                    if q.len() >= MAX_EDGES {
                        q.pop_front();
                    }
                    q.push_back(on);
                }),
                radio_input: Box::new(move || {
                    *input_shared.heartbeat.lock().unwrap() = Instant::now();
                    RadioKeyInput {
                        remote_keyed: input_shared.remote_keyed.load(Ordering::Relaxed),
                        rx_level_db: f32::from_bits(
                            input_shared.rx_db_bits.load(Ordering::Relaxed),
                        ),
                    }
                }),
            },
            Arc::clone(&stop),
        )?;
        Some(Self {
            stop,
            shared,
            worker: Some(worker),
            stall_after,
            close_grace,
        })
    }

    /// Non-blocking keying exchange: publish the latest call snapshot inputs,
    /// pop the next pending debounced key edge (`None` = no change), and check
    /// worker health. Call on the same ~20 ms cadence as [`crate::Poller::tick`].
    ///
    /// # Errors
    /// [`PttError::Io`] when the worker exited (persistently failing backend —
    /// its fail-safe has already run) or when its heartbeat went stale (a
    /// wedged hardware transfer; tear down and re-open after the device
    /// returns).
    pub fn tick(&self, remote_keyed: bool, rx_level_db: f32) -> Result<Option<bool>, PttError> {
        self.shared
            .remote_keyed
            .store(remote_keyed, Ordering::Relaxed);
        self.shared
            .rx_db_bits
            .store(rx_level_db.to_bits(), Ordering::Relaxed);
        match &self.worker {
            Some(h) if !h.is_finished() => {}
            _ => {
                return Err(PttError::Io(io::Error::other(
                    "ptt worker exited (backend failed)",
                )));
            }
        }
        if self.shared.heartbeat.lock().unwrap().elapsed() > self.stall_after {
            return Err(PttError::Io(io::Error::other(
                "ptt backend stalled (wedged hardware transfer)",
            )));
        }
        Ok(self.shared.edges.lock().unwrap().pop_front())
    }
}

impl Drop for PttAgent {
    /// Never blocks indefinitely: signal stop, give a healthy worker
    /// `close_grace` to exit (its fail-safe unkeys the radio on the way out),
    /// and DETACH a wedged one — a worker parked in a dead USB transfer can
    /// never be joined, and its device is gone, so there is no radio line
    /// left to unkey.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(h) = self.worker.take() else { return };
        let start = Instant::now();
        while !h.is_finished() && start.elapsed() < self.close_grace {
            std::thread::sleep(Duration::from_millis(5));
        }
        if h.is_finished() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RxKeyMode;

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

    /// Poll `pred` until it holds or `timeout` elapses (CI-jitter robust).
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

    /// Healthy backend: constant key level, records radio writes + `fail_safe`.
    struct FakeBackend {
        key: Arc<AtomicBool>,
        radio: Arc<Mutex<Vec<bool>>>,
        failsafed: Arc<AtomicBool>,
    }
    impl PttBackend for FakeBackend {
        fn read_key(&mut self) -> Result<bool, PttError> {
            Ok(self.key.load(Ordering::Relaxed))
        }
        fn set_radio_key(&mut self, level: bool) -> Result<(), PttError> {
            self.radio.lock().unwrap().push(level);
            Ok(())
        }
        fn fail_safe(&mut self) {
            self.failsafed.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn key_edge_reaches_tick_and_drop_fail_safes() {
        let key = Arc::new(AtomicBool::new(false));
        let radio = Arc::new(Mutex::new(Vec::new()));
        let failsafed = Arc::new(AtomicBool::new(false));
        let agent = PttAgent::new(
            Box::new(FakeBackend {
                key: Arc::clone(&key),
                radio,
                failsafed: Arc::clone(&failsafed),
            }),
            cfg(),
        )
        .expect("agent spawns");

        // Let the key settle unkeyed, then press: exactly one debounced true
        // edge must surface through tick's mailbox.
        assert!(
            wait_until(Duration::from_secs(2), || {
                key.store(true, Ordering::Relaxed);
                agent.tick(false, -60.0).unwrap() == Some(true)
            }),
            "debounced key edge delivered via tick"
        );

        drop(agent);
        assert!(
            failsafed.load(Ordering::Relaxed),
            "worker fail-safed during agent drop"
        );
    }

    #[test]
    fn dead_backend_surfaces_as_tick_error_after_worker_exit() {
        struct DeadBackend {
            failsafed: Arc<AtomicBool>,
        }
        impl PttBackend for DeadBackend {
            fn read_key(&mut self) -> Result<bool, PttError> {
                Err(PttError::Io(io::Error::other("unplugged")))
            }
            fn set_radio_key(&mut self, _level: bool) -> Result<(), PttError> {
                Err(PttError::Io(io::Error::other("unplugged")))
            }
            fn fail_safe(&mut self) {
                self.failsafed.store(true, Ordering::Relaxed);
            }
        }
        let failsafed = Arc::new(AtomicBool::new(false));
        let agent = PttAgent::new(
            Box::new(DeadBackend {
                failsafed: Arc::clone(&failsafed),
            }),
            cfg(),
        )
        .expect("agent spawns");

        // The runner exits after ~3 failing ticks; tick then reports it.
        assert!(
            wait_until(Duration::from_secs(2), || agent.tick(false, -60.0).is_err()),
            "worker exit surfaces as a tick error"
        );
        assert!(
            failsafed.load(Ordering::Relaxed),
            "fail_safe ran on the worker's error exit"
        );
    }

    #[test]
    fn wedged_backend_never_blocks_tick_and_drop_detaches() {
        /// A backend whose first read parks forever — the iax-239a wedge: a
        /// USB control transfer whose completion will never arrive.
        struct WedgedBackend;
        impl PttBackend for WedgedBackend {
            fn read_key(&mut self) -> Result<bool, PttError> {
                loop {
                    std::thread::park();
                }
            }
            fn set_radio_key(&mut self, _level: bool) -> Result<(), PttError> {
                Ok(())
            }
            fn fail_safe(&mut self) {}
        }

        let agent = PttAgent::with_timing(
            Box::new(WedgedBackend),
            cfg(),
            Duration::from_millis(50),  // stall_after
            Duration::from_millis(100), // close_grace
        )
        .expect("agent spawns");

        // tick must return immediately even though the worker is parked in
        // "hardware" I/O, and must flag the stall once the heartbeat ages out.
        let t0 = Instant::now();
        let first = agent.tick(false, -60.0);
        assert!(
            t0.elapsed() < Duration::from_millis(250),
            "tick never blocks on the wedged worker"
        );
        assert!(first.is_ok(), "stall not yet detectable on the first tick");
        assert!(
            wait_until(Duration::from_secs(2), || agent.tick(false, -60.0).is_err()),
            "stale heartbeat surfaces as a tick error"
        );

        // Teardown must not hang on the unjoinable worker: detach after grace.
        let t0 = Instant::now();
        drop(agent);
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "drop detaches the wedged worker instead of joining forever"
        );
    }
}
