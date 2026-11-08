// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A receive thread must never touch the heap for the packet log. With logging
//! off, a media datagram costs it only the filter and a slot check. With logging
//! on, it costs a copy into a buffer from the pool.
//!
//! The counter is per thread, and each test drains its receiver on its own
//! thread, so only the capture path and that drain can show up here.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::net::SocketAddr;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use astar_record::packet_tap::{
    CallMeta, CallPacketTap, PacketEvent, PacketNetwork, PacketSlot, packet_tap_channel,
};

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

/// Counts this thread's allocations, so the test harness's own don't show.
struct Counting;

// SAFETY: every call goes straight to `System`, which upholds the contract.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        // SAFETY: the caller's `layout` is passed through unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System.alloc` above, with this `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// A voice datagram the size of a ulaw mini frame.
const VOICE: &[u8] = &[0x55; 164];

fn peer() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 4569))
}

fn meta() -> CallMeta {
    CallMeta {
        network: PacketNetwork::AllStar,
        codec: None,
        label: "AllStar 1999".to_string(),
        local: SocketAddr::from(([127, 0, 0, 1], 4570)),
        peer: peer(),
    }
}

/// A call already up when capture starts, on a pool nothing has touched yet.
/// Counting starts at the call's first capture: anything a pool or a tap
/// leaves to its first use would land on a receive thread, so no test here
/// gets a warm-up.
fn logging(events: usize, buffers: usize) -> (CallPacketTap, Receiver<PacketEvent>) {
    let slot = PacketSlot::default();
    let call = CallPacketTap::new(slot.clone(), |_| true, meta());
    let (tap, rx) = packet_tap_channel(events, buffers);
    slot.set(Some(tap));
    (call, rx)
}

fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

#[test]
fn an_empty_slot_captures_without_allocating() {
    let mut tap = CallPacketTap::new(PacketSlot::default(), |_| true, meta());

    let before = allocations();
    for _ in 0..1000 {
        tap.capture(peer(), b"voice");
    }
    assert_eq!(allocations() - before, 0);
}

#[test]
fn capturing_allocates_nothing_from_the_first_packet() {
    const CAPTURES: usize = 10_000;

    let (mut tap, rx) = logging(64, 64);
    let mut delivered = 0;

    let before = allocations();
    for _ in 0..CAPTURES {
        tap.capture(peer(), VOICE);
        delivered += rx.try_iter().count();
    }
    assert_eq!(allocations() - before, 0);
    assert_eq!(delivered, CAPTURES + 1, "and the Start");
}

#[test]
fn an_empty_pool_drops_without_allocating() {
    const BUFFERS: u32 = 2;
    const CAPTURES: u32 = 1_000;

    let (mut tap, rx) = logging(4096, BUFFERS as usize);

    let before = allocations();
    for _ in 0..CAPTURES {
        tap.capture(peer(), VOICE);
    }
    assert_eq!(allocations() - before, 0);

    // The two buffers went to the first two captures, and every one after
    // that found the pool empty. Draining hands them back.
    assert_eq!(rx.try_iter().count(), BUFFERS as usize + 1, "and the Start");
    tap.capture(peer(), VOICE);
    let event = rx.try_recv();
    assert!(
        matches!(event, Ok(PacketEvent::Packet { dropped_before, .. })
            if dropped_before == CAPTURES - BUFFERS),
        "{event:?}"
    );
}

/// A `Start` that finds the channel full is offered again at the next packet,
/// so its cost is paid per packet for as long as the channel stays full.
#[test]
fn a_start_retried_on_a_full_channel_allocates_nothing() {
    let slot = PacketSlot::default();
    let mut waiting = CallPacketTap::new(slot.clone(), |_| true, meta());
    let (tap, _rx) = packet_tap_channel(1, 8);
    slot.set(Some(tap));
    // The other call's Start takes the channel's one place.
    CallPacketTap::new(slot, |_| true, meta()).capture(peer(), VOICE);

    let before = allocations();
    for _ in 0..1_000 {
        waiting.capture(peer(), VOICE);
    }
    assert_eq!(allocations() - before, 0);
}

/// `AllStar` names its codec before every capture, on the receive thread, and
/// a `Start` still in the channel shares the metadata that name goes in.
#[test]
fn naming_the_codec_allocates_nothing_with_a_start_queued() {
    let (mut tap, rx) = logging(64, 64);
    tap.capture(peer(), VOICE);

    let before = allocations();
    tap.set_codec("ulaw");
    assert_eq!(allocations() - before, 0);
    assert_eq!(rx.try_iter().count(), 2, "the Start and its packet");
}

#[test]
fn a_capture_stays_cheap() {
    const CAPTURES: u32 = 100_000;

    let (mut tap, rx) = logging(4096, 4096);
    let mut delivered = 0;
    let start = Instant::now();
    for n in 0..CAPTURES {
        tap.capture_at(start, peer(), VOICE);
        if n % 64 == 0 {
            delivered += rx.try_iter().count();
        }
    }
    let each = start.elapsed() / CAPTURES;
    delivered += rx.try_iter().count();

    // A quarter of one percent of a 20 ms packet interval, and far above any
    // real cost, so a busy machine can't make this flake.
    assert!(each < Duration::from_micros(50), "{each:?} per capture");
    assert_eq!(delivered, CAPTURES as usize + 1, "and the Start");
}
