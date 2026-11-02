// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! Bounded, read-only taps from the audio and network code into the recorder.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, TryLockError};

/// Capacity of the received-audio tap.
pub const HEARD_TAP_CAPACITY: usize = 64;
/// Capacity of the sent-audio tap.
pub const SENT_TAP_CAPACITY: usize = 64;
/// Capacity of the codec-frame tap.
pub const FRAME_TAP_CAPACITY: usize = 1024;

/// One block of mono PCM at its own sample rate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcmChunk {
    pub rate: u32,
    pub samples: Vec<i16>,
}

/// The sending half of a bounded channel. A full channel, or a recorder that
/// has gone away, drops the item and bumps a shared counter.
///
/// The receiver must be drained with `try_recv` on its own clock and never
/// parked in `recv`/`recv_timeout`: `try_send` locks and futex-wakes a parked
/// receiver, which the audio thread must not do. See [`tap_channel`].
pub struct Tap<T> {
    tx: SyncSender<T>,
    dropped: Arc<AtomicU64>,
}

impl<T> Clone for Tap<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            dropped: Arc::clone(&self.dropped),
        }
    }
}

/// Make a tap and the receiver the recorder reads from.
///
/// Contract: the receiver must be drained with `try_recv` on the consumer's
/// own 20 ms clock, and must never park in `recv` or `recv_timeout`. The
/// standard channel's `try_send` takes an internal mutex and wakes a parked
/// receiver with a futex call, and the audio thread must never pay for either.
///
/// # Panics
/// Panics if `capacity` is zero (a zero-capacity channel is a rendezvous
/// channel, which would drop every offer).
#[must_use]
pub fn tap_channel<T>(capacity: usize) -> (Tap<T>, Receiver<T>) {
    assert!(capacity > 0, "tap capacity must be at least 1");
    let (tx, rx) = sync_channel(capacity);
    (
        Tap {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
        },
        rx,
    )
}

impl<T> Tap<T> {
    /// Hand over an item without waiting. Returns false (and counts a drop)
    /// when the channel is full or the receiver is gone.
    pub fn offer(&self, item: T) -> bool {
        match self.tx.try_send(item) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// The shared count of dropped items.
    #[must_use]
    pub fn dropped_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.dropped)
    }
}

/// A shared, swappable "maybe a tap" cell. The audio thread only ever
/// `try_lock`s it, so it never waits.
pub struct TapSlot<T> {
    inner: Arc<Mutex<Option<Tap<T>>>>,
    contended: Arc<AtomicU64>,
    /// Bumped by every [`TapSlot::set`], so a producer can tell that the
    /// tap it opened a stream on is no longer the one installed.
    generation: Arc<AtomicU64>,
}

impl<T> Clone for TapSlot<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            contended: Arc::clone(&self.contended),
            generation: Arc::clone(&self.generation),
        }
    }
}

impl<T> Default for TapSlot<T> {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
            contended: Arc::new(AtomicU64::new(0)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl<T> TapSlot<T> {
    /// Install or remove the tap.
    pub fn set(&self, tap: Option<Tap<T>>) {
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *g = tap;
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Which [`TapSlot::set`] the slot is on: a lock-free read, safe on the
    /// audio thread.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Count of offers skipped because the slot's lock was busy, so gaps in a
    /// recording can be explained.
    #[must_use]
    pub fn contended_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.contended)
    }

    /// Whether a tap is installed right now (waits for the lock; not for the
    /// audio thread).
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// Build and offer an item, never blocking. `make` runs only when a tap is
    /// installed and the lock was free; otherwise nothing is made and false is
    /// returned.
    pub fn offer_with(&self, make: impl FnOnce() -> T) -> bool {
        let guard = match self.inner.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => {
                self.contended.fetch_add(1, Ordering::Relaxed);
                return false;
            }
        };
        match guard.as_ref() {
            Some(tap) => tap.offer(make()),
            None => false,
        }
    }
}

/// Whether a stream's opening item (a recording's `Start`) reached the tap
/// installed now. A producer whose `Start` was dropped (no tap yet, a full
/// channel, a busy slot) or went to a tap since replaced offers it again
/// before the stream's next item, so a recording begun mid-stream still
/// gets the rest of it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StartedOn(Option<u64>);

impl StartedOn {
    /// Whether the `Start` must be offered (again) before the next item.
    #[must_use]
    pub fn needs_start<T>(&self, slot: &TapSlot<T>) -> bool {
        self.0 != Some(slot.generation())
    }

    /// Offer the `Start` that `make` builds; remembers the tap it reached.
    pub fn offer_start<T>(&mut self, slot: &TapSlot<T>, make: impl FnOnce() -> T) -> bool {
        let generation = slot.generation();
        let ok = slot.offer_with(make);
        if ok {
            self.0 = Some(generation);
        }
        ok
    }

    /// Offer the `Start` only when [`StartedOn::needs_start`].
    pub fn ensure<T>(&mut self, slot: &TapSlot<T>, make: impl FnOnce() -> T) {
        if self.needs_start(slot) {
            self.offer_start(slot, make);
        }
    }

    /// Whether some tap got this stream's `Start` (so it is owed an `End`).
    #[must_use]
    pub fn started(&self) -> bool {
        self.0.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    #[test]
    fn a_full_tap_drops_and_counts() {
        let (tap, _rx) = tap_channel::<u32>(2);
        let results: Vec<bool> = (0..5).map(|i| tap.offer(i)).collect();
        assert_eq!(results, vec![true, true, false, false, false]);
        assert_eq!(tap.dropped_counter().load(Ordering::Relaxed), 3);
    }

    #[test]
    fn an_empty_slot_never_calls_make() {
        let slot = TapSlot::<u32>::default();
        assert!(!slot.is_set());
        assert!(!slot.offer_with(|| panic!("make must not run")));
    }

    #[test]
    fn a_set_slot_delivers() {
        let (tap, rx) = tap_channel::<u32>(4);
        let slot = TapSlot::default();
        slot.set(Some(tap));
        assert!(slot.is_set());
        assert!(slot.offer_with(|| 7));
        assert_eq!(rx.try_recv().unwrap(), 7);
        slot.set(None);
        assert!(!slot.offer_with(|| 8));
    }

    #[test]
    fn a_contended_slot_does_not_block() {
        let (tap, _rx) = tap_channel::<u32>(4);
        let slot = TapSlot::default();
        slot.set(Some(tap));
        let held = slot.clone();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let t = std::thread::spawn(move || {
            let _guard = held.inner.lock().unwrap();
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        locked_rx.recv().unwrap();
        let start = Instant::now();
        let made = slot.offer_with(|| panic!("make must not run while contended"));
        assert!(!made);
        assert!(start.elapsed() < Duration::from_millis(50));
        assert_eq!(slot.contended_counter().load(Ordering::Relaxed), 1);
        release_tx.send(()).unwrap();
        t.join().unwrap();
    }

    #[test]
    fn a_dropped_receiver_counts_as_dropped() {
        let (tap, rx) = tap_channel::<u32>(4);
        drop(rx);
        assert!(!tap.offer(1));
        assert_eq!(tap.dropped_counter().load(Ordering::Relaxed), 1);
    }

    #[test]
    #[should_panic(expected = "at least 1")]
    fn zero_capacity_is_refused() {
        let _ = tap_channel::<u32>(0);
    }

    #[test]
    fn try_recv_draining_delivers_everything_within_capacity() {
        let (tap, rx) = tap_channel::<u32>(64);
        let t = std::thread::spawn(move || {
            for i in 0..50 {
                assert!(tap.offer(i));
            }
        });
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while got.len() < 50 && Instant::now() < deadline {
            match rx.try_recv() {
                Ok(v) => got.push(v),
                Err(_) => std::thread::sleep(Duration::from_millis(1)),
            }
        }
        t.join().unwrap();
        assert_eq!(got, (0..50).collect::<Vec<_>>());
    }

    #[test]
    fn started_on_reoffers_until_a_start_lands_and_again_on_a_new_tap() {
        let slot = TapSlot::<u32>::default();
        let mut on = StartedOn::default();
        assert!(on.needs_start(&slot));
        assert!(!on.offer_start(&slot, || 1), "no tap yet");
        assert!(!on.started() && on.needs_start(&slot));
        let (tap, rx) = tap_channel(4);
        slot.set(Some(tap));
        on.ensure(&slot, || 2);
        on.ensure(&slot, || 3);
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![2], "offered once");
        assert!(on.started());
        let (tap, rx2) = tap_channel(4);
        slot.set(Some(tap));
        on.ensure(&slot, || 4);
        assert_eq!(
            rx2.try_iter().collect::<Vec<_>>(),
            vec![4],
            "a new tap gets it"
        );
    }
}
