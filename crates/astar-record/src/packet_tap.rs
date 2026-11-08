// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The per-call packet tap: copies a call's media datagrams to the packet
//! log without ever waiting.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Instant, SystemTime};

use crate::packet_pool::{PacketBuf, PacketPool};
use crate::tap::{Offer, StartedOn, Tap, TapSlot, tap_channel};

/// Capacity of the packet tap's channel.
pub const PACKET_TAP_CAPACITY: usize = 4096;

static NEXT_CALL: AtomicU64 = AtomicU64::new(1);

/// Identifies one call across its packet events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PacketCallId(u64);

impl PacketCallId {
    /// A fresh id, unique within the process.
    #[must_use]
    pub fn next() -> Self {
        Self(NEXT_CALL.fetch_add(1, Ordering::Relaxed))
    }
}

/// Which network a call belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacketNetwork {
    AllStar,
    EchoLink,
    M17,
    DStar,
    Ysf,
    Dmr,
    Nxdn,
}

impl PacketNetwork {
    /// The lowercase name used in file names and metadata.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllStar => "allstar",
            Self::EchoLink => "echolink",
            Self::M17 => "m17",
            Self::DStar => "dstar",
            Self::Ysf => "ysf",
            Self::Dmr => "dmr",
            Self::Nxdn => "nxdn",
        }
    }
}

/// Decides whether a datagram is call media worth logging.
pub type MediaFilter = fn(&[u8]) -> bool;

/// The slot packet events are offered to.
pub type PacketSlot = TapSlot<PacketEvent>;

/// Who a call is with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallMeta {
    pub network: PacketNetwork,
    /// The codec a call agreed on, where neither the network nor its packets
    /// say. `None` everywhere else.
    pub codec: Option<&'static str>,
    pub label: String,
    pub local: SocketAddr,
    pub peer: SocketAddr,
}

/// One reading of a call's jitter buffer; every field is optional.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JitterSample {
    pub depth_ms: Option<u32>,
    pub jitter_ms: Option<u32>,
    pub lost: Option<u64>,
    pub late: Option<u64>,
    pub ooo: Option<u64>,
    pub underruns: Option<u64>,
}

/// What the packet tap sends to the packet log.
#[derive(Debug, PartialEq, Eq)]
pub enum PacketEvent {
    Start {
        call: PacketCallId,
        meta: Arc<CallMeta>,
        at: Instant,
        wall: SystemTime,
    },
    Packet {
        call: PacketCallId,
        at: Instant,
        from: SocketAddr,
        dropped_before: u32,
        bytes: PacketBuf,
    },
    Jitter {
        call: PacketCallId,
        at: Instant,
        sample: JitterSample,
    },
    End {
        call: PacketCallId,
        dropped_before: u32,
    },
}

/// Make a tap with a channel of `events` and a pool of `buffers`, and the
/// receiver the packet log reads from. See [`tap_channel`] for how it must be
/// drained.
///
/// # Panics
/// Panics if `events` is zero, as [`tap_channel`] does.
#[must_use]
pub fn packet_tap_channel(
    events: usize,
    buffers: usize,
) -> (Tap<PacketEvent>, Receiver<PacketEvent>) {
    let (tap, rx) = tap_channel(events);
    (tap.with_pool(PacketPool::new(buffers)), rx)
}

/// One call's tap. No method parks the caller: a busy slot, a full channel,
/// an empty pool, an oversized datagram or no tap at all just means the
/// packet is not logged. Capturing never allocates.
pub struct CallPacketTap {
    slot: PacketSlot,
    filter: MediaFilter,
    /// Shared with every `Start`, which a full channel can make the receive
    /// thread offer on each packet.
    meta: Arc<CallMeta>,
    id: PacketCallId,
    started: StartedOn,
    dropped: u32,
}

impl CallPacketTap {
    #[must_use]
    pub fn new(slot: PacketSlot, filter: MediaFilter, meta: CallMeta) -> Self {
        Self {
            slot,
            filter,
            meta: Arc::new(meta),
            id: PacketCallId::next(),
            started: StartedOn::default(),
            dropped: 0,
        }
    }

    #[must_use]
    pub fn id(&self) -> PacketCallId {
        self.id
    }

    /// Correct the local address once the socket is bound.
    pub fn set_local(&mut self, local: SocketAddr) {
        Arc::make_mut(&mut self.meta).local = local;
    }

    /// Correct the peer address once the host has resolved.
    pub fn set_peer(&mut self, peer: SocketAddr) {
        Arc::make_mut(&mut self.meta).peer = peer;
    }

    /// Name the codec the call agreed on, for the next `Start`. Safe on a
    /// receive thread, and cheap enough to call before every capture. While a
    /// `Start` is still queued it shares `meta`, and writing would mean copying
    /// it, so this does nothing and the caller's next try sets it.
    pub fn set_codec(&mut self, codec: &'static str) {
        if self.meta.codec != Some(codec)
            && let Some(meta) = Arc::get_mut(&mut self.meta)
        {
            meta.codec = Some(codec);
        }
    }

    /// Log a datagram received now.
    pub fn capture(&mut self, from: SocketAddr, datagram: &[u8]) {
        self.capture_at(Instant::now(), from, datagram);
    }

    /// Log a datagram received at `at`.
    pub fn capture_at(&mut self, at: Instant, from: SocketAddr, datagram: &[u8]) {
        if !(self.filter)(datagram) {
            return;
        }
        if self.started.needs_start(&self.slot) {
            let offer = self.started.try_start(&self.slot, || PacketEvent::Start {
                call: self.id,
                meta: Arc::clone(&self.meta),
                at,
                wall: SystemTime::now(),
            });
            match offer {
                Offer::Sent => {}
                Offer::NoTap => {
                    self.dropped = 0;
                    return;
                }
                Offer::Dropped => {
                    self.dropped = self.dropped.saturating_add(1);
                    return;
                }
            }
        }
        let (call, dropped_before) = (self.id, self.dropped);
        let offer = self.slot.try_offer_from(|tap| {
            let bytes = tap.pool()?.copy_of(datagram)?;
            Some(PacketEvent::Packet {
                call,
                at,
                from,
                dropped_before,
                bytes,
            })
        });
        match offer {
            // With no tap, what was lost belonged to a capture that's over.
            Offer::Sent | Offer::NoTap => self.dropped = 0,
            Offer::Dropped => self.dropped = self.dropped.saturating_add(1),
        }
    }
}

impl Drop for CallPacketTap {
    fn drop(&mut self) {
        if self.started.started() {
            let (call, dropped_before) = (self.id, self.dropped);
            let _ = self.slot.try_offer(|| PacketEvent::End {
                call,
                dropped_before,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn meta() -> CallMeta {
        CallMeta {
            network: PacketNetwork::AllStar,
            codec: None,
            label: "test".into(),
            local: addr(1),
            peer: addr(2),
        }
    }

    fn all(_: &[u8]) -> bool {
        true
    }

    fn none(_: &[u8]) -> bool {
        false
    }

    fn rig(cap: usize, filter: MediaFilter) -> (CallPacketTap, PacketSlot, Receiver<PacketEvent>) {
        let (tap, rx) = packet_tap_channel(cap, cap);
        let slot = PacketSlot::default();
        slot.set(Some(tap));
        (CallPacketTap::new(slot.clone(), filter, meta()), slot, rx)
    }

    fn drained(rx: &Receiver<PacketEvent>) -> Vec<PacketEvent> {
        rx.try_iter().collect()
    }

    #[test]
    fn an_empty_slot_copies_nothing_and_counts_nothing() {
        let mut t = CallPacketTap::new(PacketSlot::default(), all, meta());
        for _ in 0..5 {
            t.capture(addr(2), b"x");
        }
        assert_eq!(t.dropped, 0);
        assert!(!t.started.started());
    }

    #[test]
    fn a_refused_packet_never_reaches_the_channel() {
        let (mut t, _slot, rx) = rig(8, none);
        t.capture(addr(2), b"x");
        assert_eq!(drained(&rx).len(), 0);
    }

    #[test]
    fn the_first_media_packet_sends_start_then_packet() {
        let (mut t, _slot, rx) = rig(8, all);
        let id = t.id();
        t.capture(addr(2), b"abc");
        let ev = drained(&rx);
        assert_eq!(ev.len(), 2);
        assert!(matches!(&ev[0], PacketEvent::Start { call, meta, .. }
            if *call == id && meta.label == "test"));
        assert!(
            matches!(&ev[1], PacketEvent::Packet { call, from, dropped_before: 0, bytes, .. }
            if *call == id && *from == addr(2) && &**bytes == b"abc")
        );
    }

    #[test]
    fn a_full_channel_drops_and_the_next_event_reports_how_many() {
        let (mut t, _slot, rx) = rig(2, all);
        for _ in 0..10 {
            t.capture(addr(2), b"x");
        }
        // Start and one packet fit; the other nine were lost.
        assert_eq!(drained(&rx).len(), 2);
        t.capture(addr(2), b"y");
        let ev = drained(&rx);
        assert!(matches!(
            ev.as_slice(),
            [PacketEvent::Packet {
                dropped_before: 9,
                ..
            }]
        ));
    }

    #[test]
    fn an_oversized_datagram_is_dropped_and_counted() {
        let (mut t, _slot, rx) = rig(8, all);
        t.capture(addr(2), &[0; 3000]);
        t.capture(addr(2), b"small");
        let ev = drained(&rx);
        assert!(matches!(
            ev.as_slice(),
            [PacketEvent::Start { .. }, PacketEvent::Packet { dropped_before: 1, bytes, .. }]
                if &**bytes == b"small"
        ));
    }

    #[test]
    fn a_held_lock_counts_as_a_drop() {
        let (mut t, slot, rx) = rig(8, all);
        t.capture(addr(2), b"a");
        drained(&rx);
        let held = slot.clone();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let h = std::thread::spawn(move || {
            let _g = held.lock_for_test();
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        locked_rx.recv().unwrap();
        t.capture(addr(2), b"b");
        t.capture(addr(2), b"c");
        release_tx.send(()).unwrap();
        h.join().unwrap();
        t.capture(addr(2), b"d");
        assert!(matches!(
            drained(&rx).as_slice(),
            [PacketEvent::Packet {
                dropped_before: 2,
                ..
            }]
        ));
    }

    #[test]
    fn capture_never_waits() {
        let (mut t, slot, _rx) = rig(1, all);
        t.capture(addr(2), b"x");
        let held = slot.clone();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let h = std::thread::spawn(move || {
            let _g = held.lock_for_test();
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        locked_rx.recv().unwrap();
        let start = Instant::now();
        for _ in 0..10_000 {
            t.capture(addr(2), b"x");
        }
        assert!(start.elapsed() < Duration::from_millis(50));
        release_tx.send(()).unwrap();
        h.join().unwrap();
    }

    #[test]
    fn switching_on_mid_call_starts_at_the_next_packet() {
        let slot = PacketSlot::default();
        let mut t = CallPacketTap::new(slot.clone(), all, meta());
        t.capture(addr(2), b"lost");
        let (tap, rx) = packet_tap_channel(8, 8);
        slot.set(Some(tap));
        t.capture(addr(2), b"seen");
        let ev = drained(&rx);
        assert!(matches!(ev[0], PacketEvent::Start { .. }));
        assert!(matches!(&ev[1], PacketEvent::Packet { bytes, .. } if &**bytes == b"seen"));
    }

    #[test]
    fn switching_off_then_on_sends_a_fresh_start() {
        let (mut t, slot, rx) = rig(8, all);
        t.capture(addr(2), b"a");
        drained(&rx);
        slot.set(None);
        t.capture(addr(2), b"b");
        let (tap, rx2) = packet_tap_channel(8, 8);
        slot.set(Some(tap));
        t.capture(addr(2), b"c");
        let ev = drained(&rx2);
        assert!(matches!(ev[0], PacketEvent::Start { .. }));
        assert!(matches!(&ev[1], PacketEvent::Packet { bytes, .. } if &**bytes == b"c"));
    }

    #[test]
    fn a_count_from_the_last_capture_does_not_reach_the_next() {
        let (mut t, slot, rx) = rig(2, all);
        for _ in 0..10 {
            t.capture(addr(2), b"x");
        }
        drained(&rx);
        slot.set(None);
        t.capture(addr(2), b"between");
        let (tap, rx2) = packet_tap_channel(8, 8);
        slot.set(Some(tap));
        t.capture(addr(2), b"y");
        let ev = drained(&rx2);
        assert!(
            matches!(
                ev.as_slice(),
                [
                    PacketEvent::Start { .. },
                    PacketEvent::Packet {
                        dropped_before: 0,
                        ..
                    }
                ]
            ),
            "{} events",
            ev.len()
        );
    }

    #[test]
    fn dropping_a_started_tap_sends_end_and_an_unstarted_one_sends_nothing() {
        let (mut t, _slot, rx) = rig(8, all);
        let id = t.id();
        t.capture(addr(2), b"a");
        drained(&rx);
        drop(t);
        assert!(matches!(drained(&rx).as_slice(),
            [PacketEvent::End { call, dropped_before: 0 }] if *call == id));

        let (t, _slot, rx) = rig(8, all);
        drop(t);
        assert_eq!(drained(&rx).len(), 0);
    }

    #[test]
    fn a_corrected_peer_reaches_the_start() {
        let (mut t, _slot, rx) = rig(8, all);
        t.set_peer(addr(9));
        t.capture(addr(9), b"x");
        assert!(matches!(&drained(&rx)[0], PacketEvent::Start { meta, .. }
            if meta.peer == addr(9)));
    }

    #[test]
    fn a_named_codec_reaches_the_start() {
        let (mut t, _slot, rx) = rig(8, all);
        t.set_codec("ulaw");
        t.capture(addr(2), b"x");
        assert!(matches!(&drained(&rx)[0], PacketEvent::Start { meta, .. }
            if meta.codec == Some("ulaw")));
    }

    /// A queued `Start` shares the metadata, and changing it then would mean
    /// copying it on the receive thread. So the name waits for the next try.
    #[test]
    fn a_codec_named_while_a_start_is_queued_waits_for_the_writer() {
        let (mut t, slot, rx) = rig(8, all);
        t.capture(addr(2), b"a");
        t.set_codec("ulaw");
        assert_eq!(t.meta.codec, None);
        assert_eq!(drained(&rx).len(), 2);

        t.set_codec("ulaw");
        let (tap, rx2) = packet_tap_channel(8, 8);
        slot.set(Some(tap));
        t.capture(addr(2), b"b");
        assert!(matches!(&drained(&rx2)[0], PacketEvent::Start { meta, .. }
            if meta.codec == Some("ulaw")));
    }

    #[test]
    fn capture_at_keeps_the_given_instant() {
        let (mut t, _slot, rx) = rig(8, all);
        let at = Instant::now() + Duration::from_secs(3);
        t.capture_at(at, addr(2), b"x");
        let ev = drained(&rx);
        assert!(matches!(&ev[0], PacketEvent::Start { at: a, .. } if *a == at));
        assert!(matches!(&ev[1], PacketEvent::Packet { at: a, .. } if *a == at));
    }
}
