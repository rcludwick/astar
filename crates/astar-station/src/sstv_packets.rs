// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The packets behind each `AllStar` SSTV picture.
//!
//! While SSTV decoding is on, every `AllStar` call offers its received voice
//! packets to a second packet tap beside the packet log's (see
//! `CallPacketTap::with_sstv`). The [`Keeper`] drains that tap on its own
//! thread into a bounded ring: a few seconds of history while nothing airs,
//! and everything from just before a picture's header to just after its end
//! while one does. When the picture ends, the packets in its window are handed
//! to a callback on a thread of its own, which saves them and rebuilds the
//! picture from them (`sstv::Finisher`). Nothing here runs on the audio thread
//! or an IAX receive loop, and the receive loop only ever `try_lock`s the tap.
//!
//! Only `AllStar` calls feed the tap, so a picture heard on any other network
//! has no packets and is finished exactly as before.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, UNIX_EPOCH};

use astar_audio::RxJitterConfig;
use astar_iax_core::subclass::VoiceFormat;
use astar_record::packet_tap::{PacketCallId, PacketEvent};
use astar_record::pcap::{PcapRecord, canonical};

/// Capacity of the SSTV packet tap's channel: about 20 s of 20 ms frames.
pub(crate) const SSTV_PACKET_EVENTS: usize = 1024;
/// Buffers in the SSTV packet tap's pool (2 MB), as many as the channel holds.
pub(crate) const SSTV_PACKET_BUFFERS: usize = 1024;

/// An SSTV header (leader, break, leader and VIS code) lasts 910 ms; a
/// second covers it.
const HEADER: Duration = Duration::from_secs(1);
/// How far before a picture's `Started` its window opens: the header, the
/// deepest the receive jitter buffer can be, and three seconds, because the
/// decoder's clock and the packets' arrival clock are only loosely tied.
pub(crate) const BEFORE: Duration = HEADER
    .saturating_add(Duration::from_millis(RxJitterConfig::LIMIT_MS as u64))
    .saturating_add(Duration::from_secs(3));
/// How long after a picture's end its window stays open.
pub(crate) const AFTER: Duration = Duration::from_secs(2);
/// How much is kept while no picture holds it. More than [`BEFORE`], so a
/// decoder running a few seconds behind still finds the header's packets.
const HISTORY: Duration = Duration::from_secs(30);
/// The most bytes the ring's packets may cost, pinned or not: each one's
/// payload and its fixed cost ([`ENTRY_COST`]). A picture's packets are a
/// few megabytes at most.
const MAX_BYTES: usize = 32 << 20;
/// The most packets the ring keeps, pinned or not: the longest picture's
/// window (PD290's cap is about 7½ minutes, some 22 000 frames at 50 a
/// second) from two busy calls, with room to spare. A flood of tiny packets is cut
/// off here long before [`MAX_BYTES`].
const MAX_ENTRIES: usize = 1 << 16;
/// What each kept packet costs beyond its payload: the entry itself, and
/// the payload's allocation (its reference counts and the allocator's own
/// bookkeeping and rounding).
const ENTRY_COST: usize = std::mem::size_of::<Kept>() + 32;
/// How long a call with nothing in the ring is remembered without a word
/// from it. Its `End` is normally what lets it go, but an `End` the full
/// channel dropped never comes.
const CALL_IDLE: Duration = Duration::from_secs(60 * 60);
/// The most calls remembered with nothing in the ring.
const MAX_IDLE_CALLS: usize = 256;
const POLL: Duration = Duration::from_millis(20);

/// The codecs the rebuild can be told about, by their wire names.
const CODECS: [VoiceFormat; 5] = [
    VoiceFormat::G711U,
    VoiceFormat::G711A,
    VoiceFormat::Gsm,
    VoiceFormat::Slin,
    VoiceFormat::Slin16,
];

/// The packets of one picture's window, ready to save and rebuild.
pub(crate) struct Captured {
    /// Every packet in the window, in arrival order, from every `AllStar`
    /// call. The rebuild picks the busiest sender.
    pub(crate) records: Vec<PcapRecord>,
    /// The codec the busiest call agreed on, by its wire name. Real packet
    /// captures carry only mini frames, which do not name it.
    pub(crate) codec_name: Option<&'static str>,
    /// Our own address on that call, so our own stream is never rebuilt.
    pub(crate) local: Option<SocketAddr>,
}

impl Captured {
    /// The codec as the rebuild takes it.
    pub(crate) fn codec(&self) -> Option<VoiceFormat> {
        let name = self.codec_name?;
        CODECS.into_iter().find(|c| c.wire_name() == name)
    }
}

/// What runs once a picture's window has closed, on a thread of its own,
/// with its packets. [`Keeper::finish`] hands it back when there are none,
/// for the caller to run with `None`.
pub(crate) type Then = Box<dyn FnOnce(Option<Captured>) + Send>;

/// One call, as its `Start` named it.
struct CallInfo {
    codec: Option<&'static str>,
    local: SocketAddr,
    /// The `Start`'s instant and wall clock, which place every packet's
    /// arrival on the wall clock, as the packet log does.
    at: Instant,
    wall_ns: u64,
    /// Packets of this call in the ring.
    kept: usize,
    ended: bool,
    /// When this call last started or sent a packet.
    heard: Instant,
}

impl CallInfo {
    #[allow(clippy::cast_possible_truncation)] // ns since 1970 fit u64 until 2554.
    fn ts_ns(&self, at: Instant) -> u64 {
        if at >= self.at {
            self.wall_ns
                .saturating_add((at - self.at).as_nanos() as u64)
        } else {
            self.wall_ns
                .saturating_sub((self.at - at).as_nanos() as u64)
        }
    }
}

struct Kept {
    at: Instant,
    call: PacketCallId,
    from: SocketAddr,
    /// Shared, so a closed window takes its packets under the ring's lock
    /// by reference, and copies them out after letting go of it.
    bytes: Arc<[u8]>,
}

impl Kept {
    /// What this packet counts against [`MAX_BYTES`].
    fn cost(&self) -> usize {
        self.bytes.len() + ENTRY_COST
    }
}

/// A closed window's packets, still shared with the ring. Turned into a
/// [`Captured`] (the payloads copied) after the ring's lock is let go.
struct Window {
    packets: Vec<(u64, SocketAddr, SocketAddr, Arc<[u8]>)>,
    codec_name: Option<&'static str>,
    local: Option<SocketAddr>,
}

impl Window {
    /// The packets, copied out; none at all is no capture.
    fn into_captured(self) -> Option<Captured> {
        if self.packets.is_empty() {
            return None;
        }
        Some(Captured {
            records: self
                .packets
                .into_iter()
                .map(|(ts_ns, src, dst, bytes)| PcapRecord {
                    ts_ns,
                    src,
                    dst,
                    payload: bytes.to_vec(),
                })
                .collect(),
            codec_name: self.codec_name,
            local: self.local,
        })
    }
}

/// A picture's window: from `from`, to its end plus [`AFTER`] once it has
/// ended, never past `cap`.
struct Pin {
    seq: u64,
    from: Instant,
    cap: Instant,
    job: Option<(Instant, Then)>,
}

#[derive(Default)]
struct Ring {
    packets: VecDeque<Kept>,
    /// What [`Ring::packets`] cost, by [`Kept::cost`].
    bytes: usize,
    /// The latest arrival in the ring.
    newest: Option<Instant>,
    calls: HashMap<PacketCallId, CallInfo>,
    pins: Vec<Pin>,
}

impl Ring {
    fn take(&mut self, ev: PacketEvent) {
        match ev {
            PacketEvent::Start {
                call,
                meta,
                at,
                wall,
            } => {
                let wall_ns = wall
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
                let kept = self.calls.get(&call).map_or(0, |c| c.kept);
                self.calls.insert(
                    call,
                    CallInfo {
                        codec: meta.codec,
                        local: meta.local,
                        at,
                        wall_ns,
                        kept,
                        ended: false,
                        heard: at,
                    },
                );
            }
            PacketEvent::Packet {
                call,
                at,
                from,
                bytes,
                ..
            } => {
                // Copied out, so the pool's buffer goes straight back.
                let kept = Kept {
                    at,
                    call,
                    from,
                    bytes: Arc::from(&bytes[..]),
                };
                drop(bytes);
                self.bytes += kept.cost();
                if let Some(info) = self.calls.get_mut(&call) {
                    info.kept += 1;
                    info.heard = info.heard.max(at);
                }
                self.newest = Some(self.newest.map_or(at, |n| n.max(at)));
                self.packets.push_back(kept);
                // Held to both caps packet by packet, so a flood never
                // runs past them, even between polls.
                self.trim(|_| false);
            }
            PacketEvent::Jitter { .. } => {}
            PacketEvent::End { call, .. } => {
                if let Some(info) = self.calls.get_mut(&call) {
                    info.ended = true;
                }
            }
        }
    }

    /// Let go of the oldest packets while `stale` says so, or while the
    /// ring is past [`MAX_BYTES`] or [`MAX_ENTRIES`].
    fn trim(&mut self, stale: impl Fn(&Kept) -> bool) {
        while let Some(front) = self.packets.front() {
            let over = self.bytes > MAX_BYTES || self.packets.len() > MAX_ENTRIES;
            if !(over || stale(front)) {
                break;
            }
            let gone = self.packets.pop_front().expect("front exists");
            self.bytes -= gone.cost();
            if let Some(info) = self.calls.get_mut(&gone.call) {
                info.kept -= 1;
            }
        }
        if self.packets.is_empty() {
            self.newest = None;
        }
    }

    /// Let go of what no window needs: packets older than [`HISTORY`] that
    /// no pin covers, the oldest of all past either cap, and calls with
    /// nothing in the ring that ended or have been quiet past
    /// [`CALL_IDLE`].
    fn evict(&mut self, now: Instant) {
        // A pin whose picture never said it ended stops holding anything
        // once its cap is well past.
        self.pins
            .retain(|p| p.job.is_some() || p.cap + HISTORY > now);
        let held_from = self.pins.iter().map(|p| p.from).min();
        let old = now.checked_sub(HISTORY);
        self.trim(|front| {
            old.is_some_and(|old| front.at < old) && held_from.is_none_or(|from| front.at < from)
        });
        let quiet = now.checked_sub(CALL_IDLE);
        self.calls
            .retain(|_, c| c.kept > 0 || !(c.ended || quiet.is_some_and(|quiet| c.heard < quiet)));
        let mut idle: Vec<(Instant, PacketCallId)> = self
            .calls
            .iter()
            .filter(|(_, c)| c.kept == 0)
            .map(|(id, c)| (c.heard, *id))
            .collect();
        if idle.len() > MAX_IDLE_CALLS {
            idle.sort_unstable_by_key(|(heard, _)| *heard);
            for (_, id) in &idle[..idle.len() - MAX_IDLE_CALLS] {
                self.calls.remove(id);
            }
        }
    }

    /// The windows that have closed by `now` (every one, when `all`), with
    /// what to run on each.
    fn due(&mut self, now: Instant, all: bool) -> Vec<(Window, Then)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.pins.len() {
            let ready = self.pins[i]
                .job
                .as_ref()
                .is_some_and(|(until, _)| all || *until <= now);
            if !ready {
                i += 1;
                continue;
            }
            let pin = self.pins.swap_remove(i);
            let (until, then) = pin.job.expect("ready means a job");
            out.push((self.window(pin.from, until), then));
        }
        if all {
            self.pins.clear();
        }
        out
    }

    /// The packets between `from` and `until`, by reference: no payload is
    /// copied while the ring's lock is held.
    fn window(&self, from: Instant, until: Instant) -> Window {
        let mut counts: Vec<(PacketCallId, usize)> = Vec::new();
        let mut packets = Vec::new();
        for p in self
            .packets
            .iter()
            .filter(|p| p.at >= from && p.at <= until)
        {
            let Some(info) = self.calls.get(&p.call) else {
                continue;
            };
            match counts.iter_mut().find(|(c, _)| *c == p.call) {
                Some((_, n)) => *n += 1,
                None => counts.push((p.call, 1)),
            }
            packets.push((
                info.ts_ns(p.at),
                canonical(p.from),
                canonical(info.local),
                Arc::clone(&p.bytes),
            ));
        }
        // The busiest call, the first heard between equals.
        let busiest = counts
            .iter()
            .enumerate()
            .max_by_key(|(i, (_, n))| (*n, std::cmp::Reverse(*i)))
            .and_then(|(_, (call, _))| self.calls.get(call));
        Window {
            packets,
            codec_name: busiest.and_then(|c| c.codec),
            local: busiest.map(|c| canonical(c.local)),
        }
    }
}

fn lock(ring: &Mutex<Ring>) -> MutexGuard<'_, Ring> {
    ring.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Drains the SSTV packet tap into the ring on its own thread, and hands
/// each picture's packets on once its window closes. Dropping it hands on
/// every picture that has ended at once, and joins every rebuild thread
/// still running: saving and decoding each takes seconds, so a drop (SSTV
/// turned off, or the station shutting down) can wait that long.
pub(crate) struct Keeper {
    ring: Arc<Mutex<Ring>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Keeper {
    /// Start draining `rx`. Its sender must only ever `try_send` (the
    /// packet tap does), and it is drained here with `try_recv` every 20 ms,
    /// never parked on.
    pub(crate) fn spawn(rx: Receiver<PacketEvent>) -> io::Result<Self> {
        let ring = Arc::new(Mutex::new(Ring::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let ring = Arc::clone(&ring);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("astar-sstv-packets".to_owned())
                .spawn(move || run(&rx, &ring, &stop))?
        };
        Ok(Self {
            ring,
            stop,
            thread: Some(thread),
        })
    }

    /// A picture started at `at`: hold its packets from [`BEFORE`] that,
    /// up to `cap` at the latest.
    ///
    /// A pin that comes late (the decoder running more than [`BEFORE`]
    /// behind) still finds its packets in the [`HISTORY`] kept unpinned.
    /// One later than that holds only what is still there; the rebuild
    /// then finds no whole picture, and the live one is kept.
    pub(crate) fn pin(&self, seq: u64, at: Instant, cap: Instant) {
        let from = at.checked_sub(BEFORE).unwrap_or(at);
        let mut ring = lock(&self.ring);
        ring.pins.retain(|p| p.seq != seq);
        ring.pins.push(Pin {
            seq,
            from,
            cap: cap.max(at),
            job: None,
        });
    }

    /// The picture `seq` ended at `at`. When packets arrived in its window,
    /// `then` runs with them [`AFTER`] later (or at its cap), on a thread
    /// of its own, and this returns `Ok`. With none (another network, or a
    /// picture never pinned), it is handed back for the caller to run now.
    pub(crate) fn finish(&self, seq: u64, at: Instant, then: Then) -> Result<(), Then> {
        let mut ring = lock(&self.ring);
        let Some(i) = ring.pins.iter().position(|p| p.seq == seq) else {
            return Err(then);
        };
        let from = ring.pins[i].from;
        if ring.newest.is_none_or(|newest| newest < from) {
            ring.pins.swap_remove(i);
            return Err(then);
        }
        let until = (at + AFTER).min(ring.pins[i].cap).max(from);
        ring.pins[i].job = Some((until, then));
        Ok(())
    }
}

impl Drop for Keeper {
    /// Hands on every ended picture and joins the rebuild threads, which
    /// can take seconds.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Run a closed window's callback, once, whoever gets there.
fn hand_on(cell: &Mutex<Option<(Window, Then)>>) {
    let job = cell.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some((window, then)) = job {
        // The payloads are copied here, with the ring's lock long let go.
        then(window.into_captured());
    }
}

/// Take up to `max` waiting events into `ring`; how many it took. Bounded,
/// so calls that refill the tap as fast as it drains cannot keep the ring
/// locked: the rest wait for the next poll.
fn drain(rx: &Receiver<PacketEvent>, ring: &mut Ring, max: usize) -> usize {
    let mut n = 0;
    for ev in rx.try_iter().take(max) {
        ring.take(ev);
        n += 1;
    }
    n
}

fn run(rx: &Receiver<PacketEvent>, ring: &Mutex<Ring>, stop: &AtomicBool) {
    let mut handing: Vec<JoinHandle<()>> = Vec::new();
    loop {
        let stopping = stop.load(Ordering::Acquire);
        let due = {
            let mut ring = lock(ring);
            drain(rx, &mut ring, SSTV_PACKET_EVENTS);
            let now = Instant::now();
            let due = ring.due(now, stopping);
            ring.evict(now);
            due
        };
        handing.retain(|h| !h.is_finished());
        for job in due {
            // Saving and decoding take seconds, and the tap must keep
            // draining meanwhile. When the OS refuses a thread, the
            // picture is finished right here instead.
            let cell = Arc::new(Mutex::new(Some(job)));
            let theirs = Arc::clone(&cell);
            let spawned = std::thread::Builder::new()
                .name("astar-sstv-rebuild".to_owned())
                .spawn(move || hand_on(&theirs));
            match spawned {
                Ok(h) => handing.push(h),
                Err(_) => hand_on(&cell),
            }
        }
        if stopping {
            for h in handing {
                let _ = h.join();
            }
            return;
        }
        // A sender gone (SSTV turned off) changes nothing: the windows
        // still open are waited out, or handed on when this keeper drops.
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astar_record::packet_tap::{
        CallMeta, CallPacketTap, PacketNetwork, PacketSlot, packet_tap_channel,
    };
    use std::sync::mpsc;
    use std::time::SystemTime;

    fn unix_ns(t: SystemTime) -> u64 {
        t.duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn tap(slot: &PacketSlot) -> CallPacketTap {
        CallPacketTap::new(
            PacketSlot::default(),
            |_| true,
            CallMeta {
                network: PacketNetwork::AllStar,
                codec: Some("ulaw"),
                label: "test".into(),
                local: addr(4569),
                peer: addr(4570),
            },
        )
        .with_sstv(slot.clone())
    }

    fn rig() -> (Keeper, CallPacketTap) {
        let (t, rx) = packet_tap_channel(64, 64);
        let slot = PacketSlot::default();
        slot.set(Some(t));
        (Keeper::spawn(rx).unwrap(), tap(&slot))
    }

    fn wait_for(mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ok() {
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn kept(k: &Keeper) -> usize {
        lock(&k.ring).packets.len()
    }

    #[test]
    fn a_window_hands_on_its_packets_with_the_codec_and_our_address() {
        let (k, mut t) = rig();
        let t0 = Instant::now();
        let before = t0.checked_sub(BEFORE + Duration::from_secs(1)).unwrap();
        t.capture_at(before, addr(4570), b"too early");
        for i in 0..5u8 {
            t.capture_at(
                t0 + Duration::from_millis(u64::from(i) * 20),
                addr(4570),
                &[i],
            );
        }
        wait_for(|| kept(&k) == 6);
        k.pin(1, t0, t0 + Duration::from_secs(60));
        let (tx, rx) = mpsc::channel();
        assert!(
            k.finish(
                1,
                t0 + Duration::from_millis(100),
                Box::new(move |c| tx.send(c.expect("packets")).unwrap())
            )
            .is_ok()
        );
        let c = rx.recv_timeout(Duration::from_secs(5)).expect("handed on");
        assert_eq!(c.records.len(), 5, "the one before the window is left out");
        assert_eq!(c.records[0].payload, vec![0]);
        assert_eq!(c.records[0].dst, addr(4569));
        assert_eq!(c.codec_name, Some("ulaw"));
        assert_eq!(c.codec(), Some(VoiceFormat::G711U));
        assert_eq!(c.local, Some(addr(4569)));
        let gap = c.records[1].ts_ns - c.records[0].ts_ns;
        assert_eq!(gap, 20_000_000, "arrival times kept to the nanosecond");
        assert!(c.records[0].ts_ns.abs_diff(unix_ns(SystemTime::now())) < 60_000_000_000);
    }

    #[test]
    fn a_picture_with_no_packets_is_handed_back() {
        let (k, _t) = rig();
        let now = Instant::now();
        k.pin(1, now, now + Duration::from_secs(60));
        let back = k.finish(1, now, Box::new(|_| panic!("nothing to hand on")));
        assert!(back.is_err());
        let back = k.finish(2, now, Box::new(|_| panic!("never pinned")));
        assert!(back.is_err());
    }

    #[test]
    fn the_window_waits_after_the_end() {
        let (k, mut t) = rig();
        let t0 = Instant::now();
        t.capture_at(t0, addr(4570), b"a");
        wait_for(|| kept(&k) == 1);
        k.pin(1, t0, t0 + Duration::from_secs(60));
        let (tx, rx) = mpsc::channel();
        assert!(
            k.finish(
                1,
                Instant::now(),
                Box::new(move |c| tx.send(c.expect("packets")).unwrap())
            )
            .is_ok()
        );
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "still open"
        );
        t.capture_at(Instant::now(), addr(4570), b"b");
        let c = rx.recv_timeout(Duration::from_secs(5)).expect("closes");
        assert_eq!(c.records.len(), 2, "a packet after the end is still in");
    }

    #[test]
    fn dropping_the_keeper_hands_on_every_ended_picture_at_once() {
        let (k, mut t) = rig();
        let t0 = Instant::now();
        t.capture_at(t0, addr(4570), b"a");
        wait_for(|| kept(&k) == 1);
        k.pin(1, t0, t0 + Duration::from_secs(600));
        let (tx, rx) = mpsc::channel();
        assert!(
            k.finish(
                1,
                Instant::now(),
                Box::new(move |c| tx.send(c.expect("packets")).unwrap())
            )
            .is_ok()
        );
        let start = Instant::now();
        drop(k);
        assert!(start.elapsed() < AFTER, "did not wait out the window");
        assert_eq!(
            rx.try_recv()
                .expect("ran before drop returned")
                .records
                .len(),
            1
        );
    }

    #[test]
    fn unpinned_history_is_bounded_in_time() {
        let mut ring = Ring::default();
        let (t, rx) = packet_tap_channel(8, 8);
        let slot = PacketSlot::default();
        slot.set(Some(t));
        let mut tap = tap(&slot);
        let now = Instant::now();
        let old = now.checked_sub(HISTORY + Duration::from_secs(1)).unwrap();
        tap.capture_at(old, addr(4570), b"old");
        tap.capture_at(now, addr(4570), b"new");
        for ev in rx.try_iter() {
            ring.take(ev);
        }
        ring.evict(now);
        assert_eq!(ring.packets.len(), 1);
        assert_eq!(&ring.packets[0].bytes[..], b"new");

        // A pin holds older packets.
        tap.capture_at(old, addr(4570), b"old again");
        for ev in rx.try_iter() {
            ring.take(ev);
        }
        ring.pins.push(Pin {
            seq: 1,
            from: old,
            cap: now,
            job: None,
        });
        ring.evict(now);
        assert_eq!(ring.packets.len(), 2);
    }

    /// One poll takes at most so many events, so a tap refilled as fast as
    /// it drains cannot hold the ring's lock without end.
    #[test]
    fn a_poll_drains_a_bounded_number_of_events() {
        let mut ring = Ring::default();
        let (t, rx) = packet_tap_channel(64, 64);
        let slot = PacketSlot::default();
        slot.set(Some(t));
        let mut tap = tap(&slot);
        let now = Instant::now();
        for i in 0..10u8 {
            tap.capture_at(now, addr(4570), &[i]);
        }
        let first = drain(&rx, &mut ring, 4);
        assert_eq!(first, 4);
        let rest = drain(&rx, &mut ring, 64);
        assert!(rest >= 6, "the rest waits for the next poll: {rest}");
        assert_eq!(drain(&rx, &mut ring, 64), 0);
    }

    /// A flood of tiny packets while a long picture is pinned: each one
    /// costs far more than its payload, so the count is capped too.
    #[test]
    fn a_flood_of_tiny_packets_stays_under_both_caps() {
        let mut ring = Ring::default();
        let (t, rx) = packet_tap_channel(1024, 1024);
        let slot = PacketSlot::default();
        slot.set(Some(t));
        let mut tap = tap(&slot);
        let now = Instant::now();
        ring.pins.push(Pin {
            seq: 1,
            from: now.checked_sub(Duration::from_secs(1)).unwrap(),
            cap: now + Duration::from_secs(600),
            job: None,
        });
        for _ in 0..100 {
            for _ in 0..1024 {
                tap.capture_at(now, addr(4570), b"x");
            }
            for ev in rx.try_iter() {
                ring.take(ev);
            }
            ring.evict(now);
        }
        assert_eq!(ring.packets.len(), MAX_ENTRIES, "capped by count");
        let cost: usize = ring.packets.iter().map(Kept::cost).sum();
        assert_eq!(ring.bytes, cost, "every entry's own cost is counted");
        assert!(cost > MAX_ENTRIES, "more than the payloads alone");
        assert!(ring.bytes <= MAX_BYTES);

        // The window closing takes the packets by reference: nothing is
        // copied while the ring's lock is held.
        ring.pins[0].job = Some((now, Box::new(|_| {})));
        let (window, _then) = ring.due(now, false).pop().expect("due");
        assert_eq!(window.packets.len(), MAX_ENTRIES);
        assert!(Arc::ptr_eq(&window.packets[0].3, &ring.packets[0].bytes));
        assert_eq!(
            window.into_captured().expect("packets").records.len(),
            MAX_ENTRIES
        );
    }

    /// The same flood through a running keeper, at the most its channel
    /// takes, while a long picture is pinned and finished over and over:
    /// every pin and finish returns, and the ring never passes its caps.
    #[test]
    fn a_running_keeper_stays_capped_and_answers_under_a_flood() {
        let (t, rx) = packet_tap_channel(SSTV_PACKET_EVENTS, SSTV_PACKET_BUFFERS);
        let slot = PacketSlot::default();
        slot.set(Some(t));
        let k = Keeper::spawn(rx).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flood = {
            let stop = Arc::clone(&stop);
            let mut tap = tap(&slot);
            std::thread::spawn(move || {
                let mut sent = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    tap.capture_at(Instant::now(), addr(4570), b"x");
                    sent += 1;
                    if sent.is_multiple_of(1024) {
                        std::thread::yield_now();
                    }
                }
                sent
            })
        };
        // Until the ring is full by count.
        wait_for(|| kept(&k) >= MAX_ENTRIES / 2);
        let (tx, done) = mpsc::channel();
        for seq in 1..=20u64 {
            let now = Instant::now();
            k.pin(seq, now, now + Duration::from_secs(445));
            let tx = tx.clone();
            assert!(
                k.finish(seq, now, Box::new(move |c| tx.send(c.is_some()).unwrap()))
                    .is_ok(),
                "packets arrived in the window"
            );
            let ring = lock(&k.ring);
            assert!(ring.packets.len() <= MAX_ENTRIES);
            assert!(ring.bytes <= MAX_BYTES);
        }
        stop.store(true, Ordering::Relaxed);
        assert!(flood.join().unwrap() > MAX_ENTRIES as u64);
        drop(k);
        drop(tx);
        assert_eq!(done.iter().filter(|kept| *kept).count(), 20);
    }

    /// A call whose `End` never came (the channel was full) is let go once
    /// it has nothing in the ring and has been quiet for an hour.
    #[test]
    fn a_call_that_never_ended_is_forgotten_once_quiet() {
        let mut ring = Ring::default();
        let (t, rx) = packet_tap_channel(8, 8);
        let slot = PacketSlot::default();
        slot.set(Some(t));
        let mut quiet = tap(&slot);
        let mut busy = tap(&slot);
        let t0 = Instant::now();
        quiet.capture_at(t0, addr(4570), b"a");
        for ev in rx.try_iter() {
            ring.take(ev);
        }
        let later = t0 + CALL_IDLE + Duration::from_secs(1);
        busy.capture_at(later, addr(4571), b"b");
        for ev in rx.try_iter() {
            ring.take(ev);
        }
        ring.evict(t0);
        assert_eq!(ring.calls.len(), 2, "both still remembered");
        ring.evict(later);
        assert_eq!(ring.packets.len(), 1, "the quiet call's packet is history");
        assert_eq!(ring.calls.len(), 1, "and the quiet call is forgotten");
        assert_eq!(
            ring.packets[0].call,
            ring.calls.keys().copied().next().unwrap()
        );

        // However many there are, only so many idle calls are kept.
        for _ in 0..MAX_IDLE_CALLS + 10 {
            ring.calls.insert(
                PacketCallId::next(),
                CallInfo {
                    codec: None,
                    local: addr(1),
                    at: later,
                    wall_ns: 0,
                    kept: 0,
                    ended: false,
                    heard: later,
                },
            );
        }
        ring.evict(later);
        let idle = ring.calls.values().filter(|c| c.kept == 0).count();
        assert!(idle <= MAX_IDLE_CALLS, "{idle}");
        assert_eq!(ring.calls.len() - idle, 1, "the busy call stays");
    }

    #[test]
    fn the_byte_cap_holds_even_when_pinned() {
        let mut ring = Ring::default();
        let now = Instant::now();
        ring.pins.push(Pin {
            seq: 1,
            from: now.checked_sub(Duration::from_secs(1)).unwrap(),
            cap: now,
            job: None,
        });
        let big: Arc<[u8]> = vec![0u8; 1 << 20].into();
        for _ in 0..40 {
            let kept = Kept {
                at: now,
                call: PacketCallId::next(),
                from: addr(1),
                bytes: Arc::clone(&big),
            };
            ring.bytes += kept.cost();
            ring.packets.push_back(kept);
        }
        ring.evict(now);
        assert!(ring.bytes <= MAX_BYTES);
        assert_eq!(ring.packets.len(), MAX_BYTES / ((1 << 20) + ENTRY_COST));
    }
}
