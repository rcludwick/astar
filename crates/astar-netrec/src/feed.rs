// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The audio a session records: one 20 ms frame, or nothing, per tick.
//!
//! The two networks hand their audio over in different places. An M17
//! monitor's sound exists only in the conference, which sends a member frames
//! while someone talks and for a short hangover after. D-Star client audio
//! exists only on the output bus, which the pull backend drains on its own
//! clock. Each gets a feed here, and everything past this file sees the same
//! [`AudioFeed`].
//!
//! Neither feed has a way to send audio anywhere. They only hold receivers.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use astar_audio::pull_backend::PulledFrame;

/// 20 ms frames in a second.
const FRAMES_PER_SECOND: u32 = 50;

/// How many frames may wait for a slow consumer: 5 s of 20 ms frames. Past
/// this, frames are lost and counted.
pub(crate) const FEED_DEPTH: usize = 250;

/// How long a [`Pump`] waits for a frame before looking at its stop flag.
const PUMP_POLL: Duration = Duration::from_millis(20);

/// One 20 ms frame of mono 16-bit audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Sample rate of `samples`, in Hz.
    pub rate: u32,
    /// The samples: `rate / 50` of them.
    pub samples: Vec<i16>,
}

/// Where a session gets its audio.
pub trait AudioFeed: Send {
    /// The next frame, or `None` if nothing arrived this tick.
    fn take(&mut self) -> Option<Frame>;
}

/// Says whether the reflector is sending audio right now.
pub(crate) type Receiving = Box<dyn FnMut() -> bool + Send>;

/// The feed for a conference member, which is how an M17 monitor is heard.
///
/// The conference goes on sending frames for a short hangover after the
/// talker stops, so a frame counts only if it arrived while the reflector was
/// sending. A [`Pump`] decides that the moment each frame arrives. The
/// consumer may stall on a slow disk, but its stall cannot change which frames
/// were audio: the end of an over stays queued, and the silence after it never
/// gets in.
pub(crate) struct ConferenceFeed {
    queue: Arc<Mutex<VecDeque<Vec<i16>>>>,
    rate: u32,
}

impl ConferenceFeed {
    /// Start pumping `from_conference`. The feed is for the consumer, and the
    /// [`Pump`] is for whoever ends the join.
    pub(crate) fn start(
        from_conference: Receiver<Vec<i16>>,
        rate: u32,
        mut receiving: Receiving,
        dropped: Arc<AtomicU64>,
    ) -> io::Result<(Self, Pump)> {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (t_queue, t_stop) = (Arc::clone(&queue), Arc::clone(&stop));
        let thread = thread::Builder::new()
            .name("netrec-pump".to_string())
            .spawn(move || {
                while !t_stop.load(Ordering::Acquire) {
                    match from_conference.recv_timeout(PUMP_POLL) {
                        Ok(samples) if receiving() => {
                            let mut queue = t_queue.lock().expect("feed queue poisoned");
                            queue.push_back(samples);
                            if queue.len() > FEED_DEPTH {
                                queue.pop_front();
                                dropped.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })?;
        let pump = Pump {
            stop,
            thread: Some(thread),
        };
        Ok((Self { queue, rate }, pump))
    }
}

impl AudioFeed for ConferenceFeed {
    fn take(&mut self) -> Option<Frame> {
        let samples = self
            .queue
            .lock()
            .expect("feed queue poisoned")
            .pop_front()?;
        Some(Frame {
            rate: self.rate,
            samples,
        })
    }
}

/// The thread behind a [`ConferenceFeed`]. Stopping it, or dropping it, waits
/// for at most one [`PUMP_POLL`]. The feed keeps what is already queued.
pub(crate) struct Pump {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Pump {
    pub(crate) fn stop(self) {
        drop(self);
    }
}

impl Drop for Pump {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The pull backend's end of a feed: what the sink calls on every tick.
///
/// A fresh channel per join means a rejoin never inherits a stale frame, and
/// an old feed can never steal one from the new join.
pub(crate) struct BusTap {
    sender: Mutex<Option<SyncSender<PulledFrame>>>,
    dropped: Arc<AtomicU64>,
}

impl BusTap {
    pub(crate) fn new(dropped: Arc<AtomicU64>) -> Self {
        Self {
            sender: Mutex::new(None),
            dropped,
        }
    }

    /// Start delivering to a new feed, whose frames are at `rate`.
    pub(crate) fn open(&self, rate: u32) -> BusFeed {
        let (tx, frames) = sync_channel(FEED_DEPTH);
        *self.sender.lock().expect("bus tap poisoned") = Some(tx);
        BusFeed {
            frames,
            rate,
            per_frame: (rate / FRAMES_PER_SECOND) as usize,
            pending: VecDeque::new(),
        }
    }

    /// Stop delivering. The feed keeps what was already queued.
    pub(crate) fn close(&self) {
        *self.sender.lock().expect("bus tap poisoned") = None;
    }

    /// Runs on the pull thread, so it never blocks. A tick with no audio is
    /// not a frame, and neither is a tick with no feed open. Only a full
    /// channel is a loss: a feed that went away wanted nothing more.
    pub(crate) fn push(&self, frame: PulledFrame) {
        if frame.samples.is_empty() {
            return;
        }
        let sender = self.sender.lock().expect("bus tap poisoned");
        if let Some(tx) = sender.as_ref()
            && let Err(TrySendError::Full(_)) = tx.try_send(frame)
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The feed for the output bus, which is how a D-Star session is heard.
///
/// The bus hands over however many real samples it had, so they are cut back
/// into 20 ms frames here. A partial frame waits for the rest. It reads the
/// channel only as far as one frame needs, so a slow consumer backs up into
/// the bounded channel, where the loss is counted.
pub(crate) struct BusFeed {
    frames: Receiver<PulledFrame>,
    rate: u32,
    per_frame: usize,
    pending: VecDeque<i16>,
}

impl AudioFeed for BusFeed {
    fn take(&mut self) -> Option<Frame> {
        while self.pending.len() < self.per_frame {
            let frame = self.frames.try_recv().ok()?;
            debug_assert_eq!(frame.rate, self.rate, "the bus runs at the engine rate");
            self.pending.extend(frame.samples);
        }
        Some(Frame {
            rate: self.rate,
            samples: self.pending.drain(..self.per_frame).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::{Sender, channel};
    use std::time::Instant;

    use super::*;

    fn counter() -> Arc<AtomicU64> {
        Arc::new(AtomicU64::new(0))
    }

    fn pulled(samples: Vec<i16>) -> PulledFrame {
        PulledFrame {
            rate: 8_000,
            samples,
        }
    }

    /// A `receiving` the test can flip, which also counts its readings: the
    /// pump reads it once per frame, so that count says how far it has got.
    struct Flag {
        on: Arc<AtomicBool>,
        reads: Arc<AtomicUsize>,
    }

    impl Flag {
        fn new(on: bool) -> (Self, Receiving) {
            let on = Arc::new(AtomicBool::new(on));
            let reads = Arc::new(AtomicUsize::new(0));
            let (r_on, r_reads) = (Arc::clone(&on), Arc::clone(&reads));
            let receiving = Box::new(move || {
                r_reads.fetch_add(1, Ordering::SeqCst);
                r_on.load(Ordering::SeqCst)
            });
            (Self { on, reads }, receiving)
        }

        fn set(&self, on: bool) {
            self.on.store(on, Ordering::SeqCst);
        }

        /// Wait for the pump to have read `frames` frames in all. Gives up
        /// quietly: the test's own asserts say what went wrong.
        fn settle(&self, frames: usize) {
            let deadline = Instant::now() + Duration::from_secs(1);
            while self.reads.load(Ordering::SeqCst) < frames && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// Poll until `done` holds, up to a second. Gives up quietly: the test's
    /// own asserts say what went wrong. The pump reads `receiving` before it
    /// locks and pushes, so a read count cannot say a frame is queued.
    fn wait_for(done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !done() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn queued(feed: &ConferenceFeed) -> usize {
        feed.queue.lock().expect("feed queue poisoned").len()
    }

    fn send(tx: &Sender<Vec<i16>>, frames: usize, value: i16) {
        for _ in 0..frames {
            tx.send(vec![value; 160]).unwrap();
        }
    }

    fn drain(feed: &mut ConferenceFeed) -> Vec<Vec<i16>> {
        std::iter::from_fn(|| feed.take().map(|f| f.samples)).collect()
    }

    fn start(on: bool) -> (Sender<Vec<i16>>, Flag, ConferenceFeed, Pump, Arc<AtomicU64>) {
        let dropped = counter();
        let (tx, rx) = channel();
        let (flag, receiving) = Flag::new(on);
        let (feed, pump) =
            ConferenceFeed::start(rx, 8_000, receiving, Arc::clone(&dropped)).expect("pump");
        (tx, flag, feed, pump, dropped)
    }

    #[test]
    fn conference_silence_never_becomes_a_frame() {
        let (tx, flag, mut feed, _pump, _) = start(false);
        send(&tx, 2, 0);
        flag.settle(2);
        assert_eq!(feed.take(), None);
        assert_eq!(
            feed.take(),
            None,
            "what arrived unheard is gone, not queued"
        );
    }

    #[test]
    fn conference_gives_one_frame_per_take_while_receiving() {
        let (tx, _flag, mut feed, _pump, _) = start(true);
        tx.send(vec![1; 160]).unwrap();
        tx.send(vec![2; 160]).unwrap();
        wait_for(|| queued(&feed) == 2);
        let first = feed.take().expect("first frame");
        assert_eq!(first.rate, 8_000);
        assert_eq!(first.samples, vec![1; 160]);
        assert_eq!(feed.take().expect("second frame").samples, vec![2; 160]);
        assert_eq!(feed.take(), None);
    }

    /// Nobody calls `take` for the whole of the stall: a slow disk.
    #[test]
    fn stall_at_over_end_keeps_the_tail() {
        let (tx, flag, mut feed, _pump, dropped) = start(true);
        send(&tx, 30, 7);
        wait_for(|| queued(&feed) == 30);
        flag.set(false);
        send(&tx, 30, 0);
        flag.settle(60);

        let got = drain(&mut feed);
        assert_eq!(got.len(), 30, "the over's frames survive the stall");
        assert!(
            got.iter().all(|f| f[0] == 7),
            "and the silence after it does not"
        );
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn stall_in_silence_drops_the_silence() {
        let (tx, flag, mut feed, _pump, dropped) = start(false);
        send(&tx, 100, 0);
        flag.settle(100);
        flag.set(true);
        send(&tx, 5, 7);
        wait_for(|| queued(&feed) == 5);

        let got = drain(&mut feed);
        assert_eq!(got.len(), 5, "nothing ahead of the over");
        assert!(got.iter().all(|f| f[0] == 7));
        assert_eq!(dropped.load(Ordering::Relaxed), 0, "silence is not a loss");
    }

    #[test]
    fn conference_overflow_loses_the_oldest_and_counts_it() {
        let (tx, _flag, mut feed, _pump, dropped) = start(true);
        send(&tx, 10, 1);
        send(&tx, FEED_DEPTH, 2);
        wait_for(|| dropped.load(Ordering::Relaxed) >= 10 && queued(&feed) == FEED_DEPTH);

        assert_eq!(dropped.load(Ordering::Relaxed), 10);
        let got = drain(&mut feed);
        assert_eq!(got.len(), FEED_DEPTH);
        assert!(
            got.iter().all(|f| f[0] == 2),
            "the newest frames are the ones kept"
        );
    }

    #[test]
    fn conference_feed_ends_quietly_when_the_member_is_gone() {
        let (tx, _flag, mut feed, pump, _) = start(true);
        drop(tx);
        pump.stop();
        assert_eq!(feed.take(), None);
    }

    #[test]
    fn stopping_the_pump_waits_for_one_poll_at_most() {
        let (tx, _flag, mut feed, pump, _) = start(true);
        send(&tx, 1, 7);
        wait_for(|| queued(&feed) == 1);
        let began = Instant::now();
        pump.stop();
        assert!(began.elapsed() < Duration::from_millis(500));

        assert!(tx.send(vec![8; 160]).is_err(), "the pump has let go");
        assert_eq!(drain(&mut feed).len(), 1, "what it queued stays");
    }

    #[test]
    fn bus_reframes_uneven_pulls_into_20_ms_frames() {
        let tap = BusTap::new(counter());
        let mut feed = tap.open(8_000);
        tap.push(pulled(vec![1; 100]));
        assert_eq!(feed.take(), None, "100 samples are not a frame yet");
        tap.push(pulled(vec![2; 100]));
        tap.push(pulled(vec![3; 120]));
        let first = feed.take().expect("first frame");
        assert_eq!(first.rate, 8_000);
        assert_eq!(first.samples.len(), 160);
        assert_eq!(&first.samples[..100], &[1; 100]);
        assert_eq!(&first.samples[100..], &[2; 60]);
        let second = feed.take().expect("second frame");
        assert_eq!(&second.samples[..40], &[2; 40]);
        assert_eq!(&second.samples[40..], &[3; 120]);
        assert_eq!(feed.take(), None);
    }

    #[test]
    fn bus_frame_length_follows_the_rate() {
        let tap = BusTap::new(counter());
        let mut feed = tap.open(16_000);
        tap.push(PulledFrame {
            rate: 16_000,
            samples: vec![1; 320],
        });
        let frame = feed.take().expect("frame");
        assert_eq!(frame.rate, 16_000);
        assert_eq!(frame.samples.len(), 320);
    }

    #[test]
    fn bus_counts_what_a_slow_consumer_loses_but_not_empty_ticks() {
        let dropped = counter();
        let tap = BusTap::new(Arc::clone(&dropped));
        let _feed = tap.open(8_000);
        for _ in 0..FEED_DEPTH * 2 {
            tap.push(pulled(Vec::new()));
        }
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        for _ in 0..FEED_DEPTH + 3 {
            tap.push(pulled(vec![1; 160]));
        }
        assert_eq!(dropped.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn bus_counts_full_only_not_a_feed_that_went_away() {
        let dropped = counter();
        let tap = BusTap::new(Arc::clone(&dropped));
        drop(tap.open(8_000));
        tap.push(pulled(vec![1; 160]));
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn bus_with_no_feed_open_discards_without_counting() {
        let dropped = counter();
        let tap = BusTap::new(Arc::clone(&dropped));
        tap.push(pulled(vec![1; 160]));
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_closed_feed_ends_and_a_new_one_starts_clean() {
        let tap = BusTap::new(counter());
        let mut old = tap.open(8_000);
        tap.push(pulled(vec![1; 160]));
        tap.close();
        let mut new = tap.open(8_000);
        tap.push(pulled(vec![2; 160]));
        assert_eq!(new.take().expect("new feed's frame").samples, vec![2; 160]);
        assert_eq!(new.take(), None);
        assert_eq!(old.take().expect("what was queued").samples, vec![1; 160]);
        assert_eq!(old.take(), None);
    }
}
