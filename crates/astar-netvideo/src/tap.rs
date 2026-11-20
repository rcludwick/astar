// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The live tap: what a recording session tells the live stream.
//!
//! The recorder sends one [`TapMsg::Audio`] per 20 ms tick and a message at
//! each over's start, late talker and end. The tap is a bounded channel and
//! [`LiveTap::send`] never waits: when the live stream has fallen behind and
//! the channel is full, the message is dropped and counted. A slow, stuck or
//! missing live stream costs video, never a tick of the recording.
//!
//! A dropped over message is survivable: the scene closes an over still open
//! when the next one starts, and the stream will have jumped ahead anyway,
//! because a full tap means it is twenty seconds behind.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};

/// How many messages the daemon's tap holds: 20 s of ticks.
pub const LIVE_TAP_CAPACITY: usize = 1000;

/// One thing the session tells the live stream, in the session's order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TapMsg {
    /// One tick of mono PCM at the session's rate, `offset_ms` into it.
    Audio { offset_ms: u64, samples: Box<[i16]> },
    /// Over `seq` began `offset_ms` into the session.
    OverStart {
        seq: u32,
        talker: Option<String>,
        offset_ms: u64,
    },
    /// The talker of over `seq` became known after it began.
    OverTalker { seq: u32, talker: String },
    /// Over `seq` ended after `duration_ms`.
    OverEnd { seq: u32, duration_ms: u64 },
}

/// The recorder's end of the tap. Sending never blocks.
#[derive(Clone)]
pub struct LiveTap {
    tx: SyncSender<TapMsg>,
    dropped: Arc<AtomicU64>,
}

/// The live stream's end of the tap.
pub struct TapReceiver {
    rx: Receiver<TapMsg>,
    dropped: Arc<AtomicU64>,
}

/// A tap holding up to `capacity` messages, and its receiver.
///
/// # Panics
/// When `capacity` is 0, which would drop every message.
#[must_use]
pub fn live_tap(capacity: usize) -> (LiveTap, TapReceiver) {
    assert!(capacity > 0, "a live tap needs room for a message");
    let (tx, rx) = sync_channel(capacity);
    let dropped = Arc::new(AtomicU64::new(0));
    (
        LiveTap {
            tx,
            dropped: Arc::clone(&dropped),
        },
        TapReceiver { rx, dropped },
    )
}

impl LiveTap {
    /// Hand `m` over without waiting. `false` (and one more drop counted)
    /// when the tap is full or the live stream has gone.
    #[allow(clippy::must_use_candidate)] // a drop is counted; most senders need no more
    pub fn send(&self, m: TapMsg) -> bool {
        match self.tx.try_send(m) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Messages dropped so far.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl TapReceiver {
    /// The next message if one is waiting. Never parks: the live stream
    /// polls on its own clock, so a send never has to wake it.
    ///
    /// # Errors
    /// [`TryRecvError::Empty`] when nothing is waiting,
    /// [`TryRecvError::Disconnected`] once the session has gone.
    pub fn try_recv(&self) -> Result<TapMsg, TryRecvError> {
        self.rx.try_recv()
    }

    /// Messages the session dropped because the tap was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Runs `f` on a thread of its own and fails if it is not done within
    /// `limit`, so a regression that blocks fails the test rather than
    /// hanging the run.
    fn within(limit: Duration, f: impl FnOnce() + Send + 'static) {
        let (done, finished) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            f();
            let _ = done.send(());
        });
        match finished.recv_timeout(limit) {
            Ok(()) => worker.join().expect("worker"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("not done within {limit:?}: something blocked")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                std::panic::resume_unwind(worker.join().expect_err("it panicked"))
            }
        }
    }

    #[test]
    #[should_panic(expected = "something blocked")]
    fn a_blocking_test_fails_rather_than_hangs() {
        within(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_secs(2));
        });
    }

    #[test]
    fn tap_never_blocks() {
        within(Duration::from_secs(5), || {
            let (tap, rx) = live_tap(LIVE_TAP_CAPACITY);
            let t = Instant::now();
            let mut taken = 0;
            for n in 0..5_000u64 {
                if tap.send(TapMsg::Audio {
                    offset_ms: n * 20,
                    samples: vec![0i16; 160].into_boxed_slice(),
                }) {
                    taken += 1;
                }
            }
            assert!(t.elapsed() < Duration::from_millis(50), "{:?}", t.elapsed());
            assert_eq!(taken, 1_000);
            assert_eq!(tap.dropped(), 4_000);
            assert_eq!(rx.dropped(), 4_000);
            // The receiver sees the first thousand, in order.
            assert!(matches!(
                rx.try_recv(),
                Ok(TapMsg::Audio { offset_ms: 0, .. })
            ));
            // A gone receiver is a drop too, never a block or a panic.
            drop(rx);
            assert!(!tap.send(TapMsg::OverEnd {
                seq: 1,
                duration_ms: 10
            }));
            assert_eq!(tap.dropped(), 4_001);
        });
    }
}
