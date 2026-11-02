// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The fan-out: one encoder writes chunks, every listener reads them at its
//! own position.
//!
//! * The writer never waits for a reader: [`ChunkRing::push`] evicts by age
//!   and appends, under a lock held for a `VecDeque` push.
//! * A reader that falls further behind than the window (about 2 s) skips
//!   to the newest chunk. A stalled phone costs one cursor, never memory.
//! * Every reader holds one slot of a counter shared by both rings, so
//!   `max_listeners` counts the MP3 and Opus listeners together. Dropping
//!   the reader frees the slot.

use std::collections::VecDeque;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicU32, Ordering},
};
use std::time::{Duration, Instant};

use crate::public::http::{Listener, Pull};

/// What a reader's next call returned.
#[derive(Debug, Clone)]
pub enum Next {
    Chunk(Arc<[u8]>),
    /// The reader had fallen past the window; `dropped` chunks were skipped.
    Skipped {
        dropped: u64,
        chunk: Arc<[u8]>,
    },
    Timeout,
    Closed,
}

/// The listener cap is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Full;

struct Inner {
    chunks: VecDeque<(u64, Instant, Arc<[u8]>)>,
    next_seq: u64,
    closed: bool,
}

/// One format's chunks for the last `window`.
pub struct ChunkRing {
    inner: Mutex<Inner>,
    cond: Condvar,
    window: Duration,
    /// How much history a new reader starts with (zero: at the head).
    backlog: Duration,
    max_readers: u32,
    counter: Arc<AtomicU32>,
}

impl ChunkRing {
    /// `window`: how far behind a reader may fall before it skips to the
    /// head. `counter` is shared with the other format's ring.
    #[must_use]
    pub fn new(window: Duration, max_readers: u32, counter: Arc<AtomicU32>) -> Arc<Self> {
        Self::with_backlog(window, Duration::ZERO, max_readers, counter)
    }

    /// As [`Self::new`], but a new reader starts `backlog` behind the head
    /// (capped below `window`) and is handed that history at once. The MP3
    /// stream uses it: a browser will not start an MP3 until it has probed
    /// tens of kilobytes, which at 32 kbit/s is eight seconds of silence on
    /// the page unless the history arrives in one burst.
    #[must_use]
    pub fn with_backlog(
        window: Duration,
        backlog: Duration,
        max_readers: u32,
        counter: Arc<AtomicU32>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                chunks: VecDeque::new(),
                next_seq: 0,
                closed: false,
            }),
            cond: Condvar::new(),
            window,
            backlog: backlog.min(window.saturating_sub(Duration::from_millis(500))),
            max_readers,
            counter,
        })
    }

    /// Append a chunk; never blocks on a reader.
    pub fn push(&self, chunk: Arc<[u8]>) {
        let now = Instant::now();
        let mut g = self.inner.lock().expect("ring poisoned");
        while g
            .chunks
            .front()
            .is_some_and(|(_, at, _)| now.duration_since(*at) > self.window)
        {
            g.chunks.pop_front();
        }
        let seq = g.next_seq;
        g.next_seq += 1;
        g.chunks.push_back((seq, now, chunk));
        drop(g);
        self.cond.notify_all();
    }

    /// A reader starting at the head (the next chunk pushed), or [`Full`].
    ///
    /// # Errors
    /// [`Full`] at `max_readers` across every ring sharing the counter.
    pub fn reader(self: &Arc<Self>) -> Result<RingReader, Full> {
        let mut n = self.counter.load(Ordering::SeqCst);
        loop {
            if n >= self.max_readers {
                return Err(Full);
            }
            match self
                .counter
                .compare_exchange(n, n + 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(cur) => n = cur,
            }
        }
        let g = self.inner.lock().expect("ring poisoned");
        let now = Instant::now();
        let cursor = if self.backlog.is_zero() {
            g.next_seq
        } else {
            g.chunks
                .iter()
                .find(|(_, at, _)| now.duration_since(*at) <= self.backlog)
                .map_or(g.next_seq, |(seq, _, _)| *seq)
        };
        drop(g);
        Ok(RingReader {
            ring: Arc::clone(self),
            cursor,
        })
    }

    /// Wake every reader with [`Next::Closed`]; later pushes are ignored by
    /// readers that already saw the close.
    pub fn close(&self) {
        self.inner.lock().expect("ring poisoned").closed = true;
        self.cond.notify_all();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().expect("ring poisoned").chunks.len()
    }
}

/// One listener's cursor. Dropping it frees its slot.
pub struct RingReader {
    ring: Arc<ChunkRing>,
    cursor: u64,
}

impl Drop for RingReader {
    fn drop(&mut self) {
        self.ring.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

impl RingReader {
    /// The next chunk, waiting up to `timeout`; skips forward when lagged.
    pub fn next(&mut self, timeout: Duration) -> Next {
        let deadline = Instant::now() + timeout;
        let mut g = self.ring.inner.lock().expect("ring poisoned");
        loop {
            if g.closed {
                return Next::Closed;
            }
            if let Some((first, _, _)) = g.chunks.front() {
                let first = *first;
                if self.cursor < first {
                    // Evicted under us: jump to the newest chunk.
                    let (seq, _, chunk) = g.chunks.back().expect("non-empty").clone();
                    let dropped = seq - self.cursor;
                    self.cursor = seq + 1;
                    return Next::Skipped { dropped, chunk };
                }
                let idx = usize::try_from(self.cursor - first).unwrap_or(usize::MAX);
                if let Some((_, _, chunk)) = g.chunks.get(idx) {
                    self.cursor += 1;
                    return Next::Chunk(Arc::clone(chunk));
                }
            } else if self.cursor < g.next_seq {
                // Everything we had not read aged out and nothing is new.
                self.cursor = g.next_seq;
            }
            let now = Instant::now();
            if now >= deadline {
                return Next::Timeout;
            }
            g = self
                .ring
                .cond
                .wait_timeout(g, deadline - now)
                .expect("ring poisoned")
                .0;
        }
    }
}

impl Listener for RingReader {
    fn pull(&mut self, timeout: Duration) -> Pull {
        match self.next(timeout) {
            Next::Chunk(c) | Next::Skipped { chunk: c, .. } => Pull::Chunk(c),
            Next::Timeout => Pull::Idle,
            Next::Closed => Pull::Closed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(b: &[u8]) -> Arc<[u8]> {
        Arc::from(b)
    }

    fn ring(window_ms: u64, max: u32) -> Arc<ChunkRing> {
        ChunkRing::new(
            Duration::from_millis(window_ms),
            max,
            Arc::new(AtomicU32::new(0)),
        )
    }

    fn chunk(n: Next) -> Vec<u8> {
        match n {
            Next::Chunk(c) => c.to_vec(),
            other => panic!("expected a chunk, got {other:?}"),
        }
    }

    #[test]
    fn readers_get_every_chunk_in_order() {
        let r = ring(2000, 25);
        let mut rd = r.reader().unwrap();
        for i in 1..=3u8 {
            r.push(bytes(&[i]));
        }
        for i in 1..=3u8 {
            assert_eq!(chunk(rd.next(Duration::from_millis(10))), [i]);
        }
        assert!(matches!(rd.next(Duration::from_millis(10)), Next::Timeout));
    }

    #[test]
    fn new_reader_starts_at_the_head_not_the_tail() {
        let r = ring(2000, 25);
        for i in 1..=5u8 {
            r.push(bytes(&[i]));
        }
        let mut rd = r.reader().unwrap();
        r.push(bytes(&[6]));
        assert_eq!(chunk(rd.next(Duration::from_millis(10))), [6]);
    }

    #[test]
    fn lagging_reader_skips_forward_and_memory_stays_bounded() {
        let r = ring(50, 25);
        let mut slow = r.reader().unwrap();
        for i in 0..10_000u32 {
            r.push(Arc::from(i.to_le_bytes().as_slice()));
        }
        std::thread::sleep(Duration::from_millis(60));
        r.push(bytes(&[9; 4]));
        match slow.next(Duration::from_millis(10)) {
            Next::Skipped { dropped, chunk } => {
                assert!(dropped > 0);
                assert_eq!(&*chunk, &[9u8; 4]);
            }
            other => panic!("{other:?}"),
        }
        assert!(r.len() <= 2, "ring kept {} chunks past the window", r.len());
    }

    #[test]
    fn cap_counts_across_rings_and_drop_frees_the_slot() {
        let c = Arc::new(AtomicU32::new(0));
        let a = ChunkRing::new(Duration::from_secs(2), 2, Arc::clone(&c));
        let b = ChunkRing::new(Duration::from_secs(2), 2, Arc::clone(&c));
        let r1 = a.reader().unwrap();
        let _r2 = b.reader().unwrap();
        assert!(a.reader().is_err() && b.reader().is_err());
        assert_eq!(c.load(Ordering::SeqCst), 2);
        drop(r1);
        assert!(b.reader().is_ok());
    }

    #[test]
    fn push_never_blocks_with_a_reader_that_never_reads() {
        let r = ring(2000, 25);
        let _held = r.reader().unwrap();
        let t = Instant::now();
        for _ in 0..100_000 {
            r.push(bytes(&[0; 40]));
        }
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    }

    #[test]
    fn a_backlog_reader_gets_recent_history_at_once() {
        let r = ChunkRing::with_backlog(
            Duration::from_secs(10),
            Duration::from_millis(80),
            25,
            Arc::new(AtomicU32::new(0)),
        );
        r.push(bytes(&[1]));
        std::thread::sleep(Duration::from_millis(150));
        r.push(bytes(&[2]));
        r.push(bytes(&[3]));
        let mut rd = r.reader().unwrap();
        assert_eq!(
            chunk(rd.next(Duration::from_millis(10))),
            [2],
            "older than the backlog: left out"
        );
        assert_eq!(chunk(rd.next(Duration::from_millis(10))), [3]);
        assert!(matches!(rd.next(Duration::from_millis(10)), Next::Timeout));
    }

    #[test]
    fn close_wakes_a_blocked_reader() {
        let r = ring(2000, 25);
        let mut rd = r.reader().unwrap();
        let t = Instant::now();
        let h = std::thread::spawn(move || rd.next(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(50));
        r.close();
        assert!(matches!(h.join().unwrap(), Next::Closed));
        assert!(t.elapsed() < Duration::from_secs(2));
    }
}
