// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! Reusable packet buffers, so a receive thread can copy a datagram for the
//! packet log without touching the heap.
//!
//! Every buffer is allocated up front. A receive thread takes one, copies the
//! datagram in and sends it down the tap. The writer hands it back by dropping
//! it. The buffers sit in a few small stripes, each behind its own mutex, and
//! both sides only ever `try_lock` them. A stripe that's busy gets skipped, so
//! neither side can wait, and two threads rarely want the same stripe.

use std::fmt;
use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// The largest datagram the pool can hold. A voice packet is a few hundred
/// bytes at most, so anything longer isn't one.
pub const PACKET_BUF_BYTES: usize = 2048;
/// How many buffers a capture gets: 8 MB, enough to match the tap's channel.
pub const PACKET_POOL_BUFFERS: usize = 4096;

/// The most stripes a pool has.
const STRIPES: usize = 8;

/// What a new buffer is filled with. It isn't zero, because the compiler can
/// turn an allocation filled with zeros into a `calloc`, which leaves the pages
/// untouched.
const TOUCH: u8 = 0xA5;

/// A fixed set of buffers, shared between the taps that fill them and the
/// [`PacketBuf`]s that hold them.
pub struct PacketPool {
    /// Each stripe's capacity is its share of the buffers, so together they
    /// have room for all of them and a return never has to grow one.
    stripes: Box<[Mutex<Vec<Vec<u8>>>]>,
    /// Where the next take starts, which spreads takes across the stripes.
    cursor: AtomicUsize,
}

impl PacketPool {
    /// Allocate `buffers` buffers of [`PACKET_BUF_BYTES`], dealt round-robin
    /// across eight stripes, or across `buffers` stripes if there are fewer.
    ///
    /// Everything that would otherwise cost something on first use is paid for
    /// here, on the caller's thread, so a receive thread never meets it: each
    /// buffer's memory is written once, so it can't page-fault, and each mutex
    /// is locked once, because on macOS that's when std allocates it.
    #[must_use]
    pub fn new(buffers: usize) -> Arc<Self> {
        let count = buffers.clamp(1, STRIPES);
        let stripes = (0..count)
            .map(|stripe| {
                let share = buffers / count + usize::from(stripe < buffers % count);
                let mut held = Vec::with_capacity(share);
                held.resize_with(share, touched_buffer);
                let held = Mutex::new(held);
                drop(held.try_lock());
                held
            })
            .collect();
        Arc::new(Self {
            stripes,
            cursor: AtomicUsize::new(0),
        })
    }

    /// Copy `datagram` into a free buffer. `None` means it's too long, the
    /// pool is empty or every stripe is busy. Never allocates and never waits.
    #[must_use]
    pub fn copy_of(self: &Arc<Self>, datagram: &[u8]) -> Option<PacketBuf> {
        if datagram.len() > PACKET_BUF_BYTES {
            return None;
        }
        let (mut data, stripe) = self.take()?;
        data.clear();
        // Can't reallocate: the length was checked against the capacity above.
        data.extend_from_slice(datagram);
        Some(PacketBuf {
            data,
            pool: Arc::clone(self),
            stripe,
        })
    }

    /// A free buffer and the stripe it came from. A poisoned stripe fails
    /// `try_lock` just as a busy one does, so it's skipped the same way.
    fn take(&self) -> Option<(Vec<u8>, usize)> {
        let count = self.stripes.len();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % count;
        (0..count).find_map(|offset| {
            let stripe = (start + offset) % count;
            let data = self.stripes[stripe].try_lock().ok()?.pop()?;
            Some((data, stripe))
        })
    }

    /// Put a buffer back, trying its own stripe first. If no stripe can take
    /// it right now it's freed, and the pool is one buffer smaller from then on.
    fn give_back(&self, data: Vec<u8>, home: usize) {
        let count = self.stripes.len();
        for offset in 0..count {
            let Ok(mut held) = self.stripes[(home + offset) % count].try_lock() else {
                continue;
            };
            if held.len() < held.capacity() {
                held.push(data);
                return;
            }
        }
    }
}

/// An empty buffer whose whole capacity has been written.
fn touched_buffer() -> Vec<u8> {
    let mut data = Vec::with_capacity(PACKET_BUF_BYTES);
    data.resize(PACKET_BUF_BYTES, TOUCH);
    data.clear();
    data
}

/// One datagram in a pooled buffer. Dropping it returns the buffer, and it
/// keeps its pool alive until then, so it can outlive the tap that made it.
pub struct PacketBuf {
    data: Vec<u8>,
    pool: Arc<PacketPool>,
    stripe: usize,
}

impl PacketBuf {
    /// A buffer of any length that belongs to no stripe, for tests of what the
    /// writer does with a datagram the pool would refuse.
    #[cfg(all(test, feature = "writer"))]
    pub(crate) fn unpooled(bytes: &[u8]) -> Self {
        Self {
            data: bytes.to_vec(),
            pool: PacketPool::new(0),
            stripe: 0,
        }
    }
}

impl Deref for PacketBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.data
    }
}

impl fmt::Debug for PacketBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl PartialEq for PacketBuf {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for PacketBuf {}

impl Drop for PacketBuf {
    fn drop(&mut self) {
        self.pool
            .give_back(std::mem::take(&mut self.data), self.stripe);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tap::tap_channel;

    #[test]
    fn a_copy_holds_the_exact_bytes() {
        let pool = PacketPool::new(1);
        let buf = pool.copy_of(b"abc").unwrap();
        assert_eq!(&*buf, b"abc");
    }

    #[test]
    fn a_reused_buffer_keeps_nothing_of_the_last_packet() {
        let pool = PacketPool::new(1);
        drop(pool.copy_of(b"a longer packet").unwrap());
        assert_eq!(&*pool.copy_of(b"abc").unwrap(), b"abc");
    }

    #[test]
    fn a_dropped_buffer_goes_back() {
        let pool = PacketPool::new(1);
        let held = pool.copy_of(b"a").unwrap();
        assert!(pool.copy_of(b"b").is_none());
        drop(held);
        assert!(pool.copy_of(b"c").is_some());
    }

    #[test]
    fn an_oversized_datagram_is_refused() {
        let pool = PacketPool::new(1);
        assert!(pool.copy_of(&[0; PACKET_BUF_BYTES + 1]).is_none());
        assert!(pool.copy_of(b"the buffer was not spent").is_some());
    }

    #[test]
    fn a_full_size_datagram_fits() {
        let pool = PacketPool::new(1);
        let buf = pool.copy_of(&[7; PACKET_BUF_BYTES]).unwrap();
        assert_eq!(buf.len(), PACKET_BUF_BYTES);
        assert_eq!(buf.data.capacity(), PACKET_BUF_BYTES);
    }

    #[test]
    fn every_buffer_can_be_taken() {
        let pool = PacketPool::new(100);
        let held: Vec<PacketBuf> = (0..100).map(|_| pool.copy_of(b"x").unwrap()).collect();
        assert!(pool.copy_of(b"x").is_none());
        drop(held);
        assert!(pool.copy_of(b"x").is_some());
    }

    #[test]
    fn a_pool_with_no_buffers_refuses_every_copy() {
        assert!(PacketPool::new(0).copy_of(b"x").is_none());
    }

    #[test]
    fn a_buffer_outliving_its_tap_still_returns() {
        let pool = PacketPool::new(1);
        let (tap, _rx) = tap_channel::<u8>(1);
        let tap = tap.with_pool(Arc::clone(&pool));
        let buf = tap.pool().unwrap().copy_of(b"x").unwrap();
        drop(tap);
        drop(buf);
        assert!(pool.copy_of(b"y").is_some());
    }

    #[test]
    fn a_new_buffer_has_had_its_whole_capacity_written() {
        let pool = PacketPool::new(1);
        let mut data = pool.stripes[0].lock().unwrap().pop().unwrap();
        assert_eq!((data.len(), data.capacity()), (0, PACKET_BUF_BYTES));
        // SAFETY: `touched_buffer` initialised all of it, which is the claim
        // under test, and `u8` has no invalid values.
        unsafe { data.set_len(PACKET_BUF_BYTES) };
        assert!(data.iter().all(|&byte| byte == TOUCH));
    }

    #[test]
    fn busy_stripes_refuse_a_copy_without_waiting() {
        let pool = PacketPool::new(2);
        let _first = pool.stripes[0].lock().unwrap();
        let _second = pool.stripes[1].lock().unwrap();
        assert!(pool.copy_of(b"x").is_none());
    }

    /// With its own stripe busy a buffer goes to the other one, which fills
    /// it, so the next return has to skip that one rather than grow it.
    #[test]
    fn a_return_finds_room_without_growing_a_stripe() {
        let pool = PacketPool::new(2);
        let (one, two) = (pool.copy_of(b"1").unwrap(), pool.copy_of(b"2").unwrap());
        let (home, other) = (one.stripe, two.stripe);
        assert_ne!(home, other);

        let busy = pool.stripes[home].lock().unwrap();
        drop(one);
        drop(busy);
        drop(two);

        for stripe in &pool.stripes {
            let stripe = stripe.lock().unwrap();
            assert_eq!((stripe.len(), stripe.capacity()), (1, 1));
        }
    }

    #[test]
    fn a_poisoned_stripe_counts_as_busy() {
        let pool = PacketPool::new(1);
        let poisoner = Arc::clone(&pool);
        let _ = std::thread::spawn(move || {
            let _held = poisoner.stripes[0].lock().unwrap();
            panic!("poison the stripe");
        })
        .join();
        assert!(pool.copy_of(b"x").is_none());
    }
}
