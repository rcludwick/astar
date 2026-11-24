// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A live `P25Reflector` link: the socket and the thread that drive
//! [`astar_p25::P25Fsm`].
//!
//! `astar-p25` is pure: its FSM decides what to send and hands up each voice
//! record it receives, and `astar_codec::p25` turns an 11-byte IMBE frame
//! into 20 ms of speech. Something has to own the `UdpSocket`, feed it
//! datagrams, call `tick`, and put the speech on the console's audio lane.
//! That is this module. It has the shape of [`crate::nxdn`], with the
//! differences P25 actually has.
//!
//! # Two ways to open a link
//!
//! [`P25Link::connect`] is the link alone. It polls to hold the registration
//! open, and it reports who is talking (record `0x66`) and on which
//! talkgroup (record `0x65`). Neither needs a vocoder.
//!
//! [`P25Link::connect_with_audio`] adds the voice path. P25 Phase 1 voice is
//! full-rate IMBE, which no DVSI stick astar supports can decode, so the
//! decoder is always the software one ([`astar_codec::p25::open_imbe`]).
//! It needs no hardware, opens no serial port and consults no vocoder trust
//! list. [`P25Link::connect_with_stream`] takes the decoder from the caller,
//! which is how the tests prove the whole path with a fake one.
//!
//! # Transmit
//!
//! **There is none, and a key is refused rather than ignored.**
//! [`P25Link::set_ptt`] stores a request, and `apply_ptt`, the one place a
//! request could become a transmission, clears it and logs the refusal.
//! [`P25Snapshot::ptt`] is therefore always false, and this module builds no
//! voice record. The transmit path is backlog item `p25-transmit`. A key
//! that silently did nothing would leave the operator hearing nothing and
//! blaming the radio, so the console refuses it up front and the app shows a
//! receive-only indicator instead of a PTT button.
//!
//! # What ends a transmission
//!
//! * The end record, tag `0x80` (`REC80` in `MMDVMHost`'s `P25Network.cpp`).
//! * A record `0x66` naming a different source, with no end record between.
//!   That is a gateway switching talkers, or two stations doubling. The
//!   first over is closed (its audio played, its recording closed, the
//!   decoder reset) and a new one starts with the new talker.
//! * [`RX_WATCHDOG`], when a sender stops dead and no end record ever
//!   arrives. It is the reflector's own 1.5 s talker watchdog, so this link
//!   and the reflector agree about when the over ended.
//! * A record arriving [`OVER_GAP`] (360 ms) or more after the previous one
//!   while an over is open. That is a lost end record followed by a new
//!   over, possibly from the same radio, and the old over's clear verdict
//!   must not carry into it. Inside an over, records come in LDU bursts
//!   about 180 ms apart, so a normal over never trips it.
//!
//! # Holding audio until the ALGID is known
//!
//! An over's encryption is named in record `0x70` (index 14, in LDU2): its
//! ALGID. That record arrives about 280 ms into each superframe, after
//! fourteen frames of voice. Playing those frames before the ALGID is known
//! would play an encrypted call's frames as noise for a quarter of a second
//! at the start of every encrypted over, and for a station that links
//! mid-over it could be longer. So a transmission's frames go into a `held`
//! queue (at most [`MAX_HELD`]) until its first ALGID arrives:
//!
//! * a clear ALGID releases the held frames to the decoder, and later
//!   frames are decoded as they come;
//! * any other ALGID drops the held frames, sets
//!   [`P25Snapshot::encrypted`], and nothing more is decoded until the
//!   transmission ends;
//! * a transmission that ends before any ALGID plays nothing.
//!
//! # Confirming the talker in every LDU1
//!
//! The talker is named in record `0x66`, the fifth record of each LDU1. When
//! a second station keys up with no end record between (a lost terminator,
//! a gateway switching talkers), records 0–3 of its first LDU1 arrive
//! before anything says the talker changed. If they were played under the
//! previous over's clear verdict, an encrypted call would leak 80 ms of
//! frames. So records 0–3 of every LDU1 wait in their own small queue until
//! the `0x66`:
//!
//! * the same talker: they follow the over's verdict;
//! * a different talker: the old over ends, a new one opens, and they are
//!   held for the new over's ALGID like the start of any over;
//! * a talker the over had not named yet (it was joined mid-way): frames
//!   still held from the unnamed talker are dropped (up to 60 ms), and a
//!   clear verdict goes back to pending until the next ALGID;
//! * no `0x66` at all (records up to and including it were lost): the
//!   check runs on the first record at index 4 or above, nothing confirms
//!   the talker, and a clear verdict goes back to pending until the next
//!   ALGID. A muted verdict stays muted.
//!
//! A cipher ALGID is applied before its own record is routed, so record 14
//! of an encrypted over is never played either.
//!
//! The cost is up to ~280 ms of extra latency at the start of each over,
//! and a fixed ~80 ms more from the LDU1 wait, paid once (it does not pile
//! up). There is no decryption anywhere, and no key handling: an encrypted
//! call is muted, and only who is talking is shown.
//!
//! # The one case this does not cover
//!
//! The same radio keys up clear, its end record is lost, and it rekeys
//! **encrypted** with less than [`OVER_GAP`] (360 ms) between its last clear
//! record and its first encrypted one. Then nothing on the wire says a new
//! over began: the talker is the same and the gap is inside the over's
//! normal rhythm, so the new over's records 0–13 play under the old clear
//! verdict until its record 14 mutes it. The bound is 14 frames (280 ms).
//! It is implausible rather than impossible: unkeying, keying again and
//! gathering the first 180 ms LDU before it is sent take about 400–600 ms.
//! Shrinking [`OVER_GAP`] to close it would split every clear over at
//! `MMDVMHost`'s 180 ms LDU bursts.
//!
//! "Clear" is ALGID `0x80` **or `0x00`**. `0x80` is the standard "no
//! encryption" value. urfd sends `0x00` on every P25 call it relays
//! (`urfd/reflector/P25Protocol.cpp`, lines 41 and 480–484), and `0x00`
//! (formally ACCORDION, a legacy algorithm no ham reflector uses) would
//! otherwise silence every URF P25 module. Ruled 2026-11-21; see
//! [`algid_is_clear`].
//!
//! # Lost and late records
//!
//! Every voice record carries its place in the superframe (index 0–17,
//! wrapping), so the link knows which record it expects next:
//!
//! * the expected record is used;
//! * a record 1–8 ahead means 1–8 records were lost. Each missing frame is
//!   filled by repeating the last good frame, up to [`MAX_REPEATS`] in a
//!   row, and after that with 160 samples of silence. Repeating a frame for
//!   a short gap is less audible than a hole; repeating one for long is a
//!   buzz, so it gives way to silence;
//! * a record 9–17 ahead is late or out of order, and is dropped rather
//!   than replayed. The one exception is record 0 arriving after this
//!   LDU1's talker check has run: that is a new LDU1 (a new talker keying
//!   right after an over that ended on an LDU1 and lost its end record),
//!   so the sequence restarts there and the talker is checked again.
//!
//! A repeated frame goes back through the decoder, and the recording holds
//! it like a received frame: its 11 bytes, then its audio. Silence fills are
//! played but not recorded.
//!
//! Only a frame of the over in progress is ever repeated, and only one that
//! will play under that over's own clear ALGID: a repeat made while the
//! ALGID is pending is held with the frames around it and goes wherever they
//! go. Nothing is kept to repeat while an over is muted, and the frame to
//! repeat is forgotten when the over mutes, ends, changes talker or loses
//! its clear verdict. A lost record with nothing valid to repeat is silence,
//! so one lost datagram at a talker change can never replay the last
//! talker's frame, encrypted or not, under the next talker's name.
//!
//! # Recording
//!
//! Each clear over is recorded under `p25`, named for its talker. Nothing is
//! recorded until the talker is known from a `0x66`, so an over joined
//! mid-way makes one recording rather than an unnamed one and a named one.
//! Encrypted overs record nothing.
//!
//! # No runaway buffer
//!
//! Decoded frames wait in a queue to be released on the audio clock. That
//! queue holds at most [`MAX_DECODED`] frames (one second); a sender that
//! floods the link loses its oldest frames instead of growing memory and
//! latency without bound.
//!
//! # Threading
//!
//! One thread per link, owning the socket. It blocks on `recv_from` with a
//! read timeout so `tick` still runs when the reflector goes quiet. Only
//! datagrams whose source address is the reflector's reach the FSM; anything
//! else is dropped before it is parsed. The control side sees atomics and
//! two mutex-guarded strings, as [`crate::nxdn`] does.

use std::collections::VecDeque;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use astar_audio::CallAudio;
use astar_codec::p25::{BACKEND, FRAME_BYTES, FRAME_SAMPLES, ImbeVoice, open_imbe};
use astar_p25::{FsmAction, LinkState, P25Fsm, RECORDS, VoiceRecord};
use astar_record::frames::{CaptureMode, Dir, FrameEvent};
use astar_record::packet_tap::{CallMeta, CallPacketTap, PacketCallId, PacketNetwork, PacketSlot};
use astar_record::tap::TapSlot;

use crate::heard::{HeardEntry, HeardLog};
use crate::over_tap::OverTap;
use crate::session::ConsoleError;

/// How long the socket blocks before the loop runs `tick` anyway, on a link
/// with no audio. Well inside the 5 s poll interval, and long enough that an
/// idle link is not a busy loop.
const RECV_TIMEOUT: Duration = Duration::from_millis(250);

/// [`RECV_TIMEOUT`] for a link that plays audio: the loop has to come round
/// every 20 ms to release the next decoded frame on time.
const AUDIO_RECV_TIMEOUT: Duration = Duration::from_millis(20);

/// One voice frame's worth of wall clock, the rate the output bus consumes
/// frames at.
const FRAME_INTERVAL: Duration = Duration::from_millis(20);

/// Decoded frames to gather before releasing the first one. A reflector
/// sends one record every 20 ms, but the internet does not deliver them
/// evenly; three frames (60 ms) of cushion absorb that jitter.
const PRIME_FRAMES: usize = 3;

/// Silence after which a transmission is treated as over although no end
/// record arrived: the reflector's own talker watchdog
/// (`astar_p25::reflector::TALKER_WATCHDOG`, from `P25Reflector.cpp`).
pub const RX_WATCHDOG: Duration = Duration::from_millis(1_500);

/// The most frames a transmission holds while its ALGID is not yet known:
/// one superframe. See the module docs.
pub const MAX_HELD: usize = RECORDS;

/// The most decoded frames waiting for the audio clock: one second. Pushing
/// past it drops the oldest.
pub const MAX_DECODED: usize = 50;

/// How many lost frames in a row are filled by repeating the last good one
/// before the fill turns to silence.
pub const MAX_REPEATS: u8 = 3;

/// The index of record `0x66`, the one that names the talker.
const SOURCE_INDEX: u8 = 4;

/// A silence inside an open over after which the next record is taken to
/// start a new over, as though the end record had been lost.
///
/// `MMDVMHost` sends each LDU's nine records together once the LDU's 180 ms
/// of speech is in, so gaps of about 180 ms are normal inside an over. Two
/// LDUs' worth is past that and its jitter, and below the time a radio
/// takes to unkey, key again and gather its first LDU. Getting it wrong
/// costs a held start (audio is late), never an encrypted frame played.
pub const OVER_GAP: Duration = Duration::from_millis(360);

/// The furthest ahead a record may be and still count as following lost
/// ones. Anything further is late or out of order.
const MAX_GAP: u8 = 8;

/// What the control side can see of a link, without touching the thread.
///
/// `PartialEq` but not `Eq`: it carries levels in dBFS.
#[derive(Debug, Clone, PartialEq)]
pub struct P25Snapshot {
    /// The link state's ABI string: `idle`, `linking`, `linked`,
    /// `unlinking`, `failed` ([`LinkState::as_str`]).
    pub link_state: &'static str,
    /// The reflector this link was opened to, `host:port` as given.
    pub reflector: String,
    /// The radio ID of the most recent talker, as its decimal string.
    ///
    /// P25 names stations by number: record `0x66` carries a 24-bit source
    /// ID and no callsign. Turning it into a callsign is a directory lookup
    /// astar does not hold, so this is the number.
    pub last_heard: Option<String>,
    /// That same ID, as a number.
    pub last_heard_id: Option<u32>,
    /// The talkgroup of the most recent over (record `0x65`). A label only:
    /// this link never filters on it and never sends it.
    pub talkgroup: Option<u32>,
    /// Voice records received since the link came up.
    pub frames_rx: u64,
    /// Whether a transmission is in progress.
    pub receiving: bool,
    /// Whether the transmission in progress is encrypted, and so muted. See
    /// the module docs: astar never decrypts.
    pub encrypted: bool,
    /// The decoder's backend, or `None` for a link with no audio. Always
    /// `"software"` for P25.
    pub backend: Option<&'static str>,
    /// Whether this station is transmitting. **Always false**: see the
    /// module's Transmit section.
    pub ptt: bool,
    /// Transmit level in dBFS. Console-owned, as
    /// [`crate::nxdn::NxdnSnapshot::tx_dbfs`] is: the link fills in the
    /// -60.0 floor and the session overwrites it.
    pub tx_dbfs: f32,
    /// Receive level in dBFS, console-owned like [`Self::tx_dbfs`].
    pub rx_dbfs: f32,
}

/// What an operator supplies to open a link with audio.
pub struct P25Config {
    /// Reflector `host:port`. The port is already there: the caller applies
    /// `astar_p25::with_default_port`.
    pub host: String,
    /// This station's callsign: 1–10 printable ASCII characters, carried in
    /// the poll.
    pub callsign: String,
}

/// A live link to one `P25Reflector`.
pub struct P25Link {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// `Some` only for a link opened with audio.
    backend: Option<&'static str>,
    reflector: String,
    frame_tap: TapSlot<FrameEvent>,
    packet_slot: PacketSlot,
    packet_call: PacketCallId,
}

/// Sentinel for an unset 24-bit field in an `AtomicU32`.
const NONE_U24: u32 = u32::MAX;

#[derive(Debug)]
struct Shared {
    link_state: AtomicU32,
    last_heard: Mutex<Option<String>>,
    heard: HeardLog,
    last_heard_id: AtomicU32,
    talkgroup: AtomicU32,
    frames_rx: AtomicU64,
    receiving: AtomicBool,
    encrypted: AtomicBool,
    stop: AtomicBool,
    /// What the operator asked for. Only [`P25Link::set_ptt`] sets it, and
    /// `apply_ptt` refuses it.
    ptt_request: AtomicBool,
    /// What the run loop applied. False for the life of this module.
    ptt: AtomicBool,
    /// The deepest the decoded queue has ever been, for the flood test.
    #[cfg(test)]
    decoded_high_water: AtomicUsize,
}

impl Shared {
    fn new() -> Shared {
        Shared {
            link_state: AtomicU32::new(state_index(LinkState::Idle)),
            last_heard: Mutex::new(None),
            heard: HeardLog::new(),
            last_heard_id: AtomicU32::new(NONE_U24),
            talkgroup: AtomicU32::new(NONE_U24),
            frames_rx: AtomicU64::new(0),
            receiving: AtomicBool::new(false),
            encrypted: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            ptt_request: AtomicBool::new(false),
            ptt: AtomicBool::new(false),
            #[cfg(test)]
            decoded_high_water: AtomicUsize::new(0),
        }
    }
}

fn state_index(s: LinkState) -> u32 {
    match s {
        LinkState::Idle => 0,
        LinkState::Linking => 1,
        LinkState::Linked => 2,
        LinkState::Unlinking => 3,
        LinkState::Failed => 4,
    }
}

fn state_from_index(i: u32) -> LinkState {
    match i {
        1 => LinkState::Linking,
        2 => LinkState::Linked,
        3 => LinkState::Unlinking,
        4 => LinkState::Failed,
        _ => LinkState::Idle,
    }
}

fn optional(v: u32) -> Option<u32> {
    (v != NONE_U24).then_some(v)
}

/// Whether an ALGID means the call is in the clear.
///
/// `0x80` is the standard "unencrypted". `0x00` counts as clear too, because
/// urfd sends `0x00` on every P25 call it relays
/// (`urfd/reflector/P25Protocol.cpp`, lines 41 and 480–484). `0x00` is
/// formally ACCORDION, a legacy algorithm no ham reflector uses, and muting
/// it would silence every URF P25 module (ruled 2026-11-21). Any other
/// value is muted; nothing here decodes it.
#[must_use]
pub fn algid_is_clear(algid: u8) -> bool {
    algid == astar_p25::ALGID_CLEAR || algid == 0x00
}

/// One 20 ms slot of an over, on its way to the decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// A frame that arrived.
    Frame([u8; FRAME_BYTES]),
    /// The last good frame again, standing in for a lost one.
    Repeat([u8; FRAME_BYTES]),
    /// Silence, for a lost frame past [`MAX_REPEATS`].
    Silence,
}

/// What is known about the ALGID of the transmission in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Verdict {
    /// No ALGID yet: hold the frames.
    #[default]
    Pending,
    /// In the clear: decode.
    Clear,
    /// Encrypted: decode nothing until the transmission ends.
    Muted,
}

/// The transmission in progress.
#[derive(Debug, Default)]
struct Over {
    open: bool,
    source: Option<u32>,
    verdict: Verdict,
    held: VecDeque<Slot>,
    /// An LDU1's slots before its `0x66` (records 0–3), waiting for the
    /// talker to be confirmed. See [`RxPath::confirm_talker`].
    ldu1: VecDeque<Slot>,
    /// Whether this superframe's LDU1 has had its talker check. Cleared by
    /// an accepted record below index 4 or by the index wrapping, set by the
    /// first accepted record at index 4 or above.
    ldu1_confirmed: bool,
    /// The index of the record expected next, once one has arrived.
    expected: Option<u8>,
    /// The frame a lost record repeats: always this over's, and `None`
    /// while it is muted. See the module's "Lost and late records".
    last_good: Option<[u8; FRAME_BYTES]>,
    /// Lost frames filled by repeating, in a row.
    repeats: u8,
}

impl Over {
    /// Place one record in the sequence: the slots it yields, in order
    /// (fills for any lost records, then its own frame), or none when it is
    /// late. See the module's "Lost and late records".
    fn sequence(&mut self, index: u8, imbe: [u8; FRAME_BYTES]) -> Vec<Slot> {
        let records = u8::try_from(RECORDS).expect("18 fits");
        let mut out = Vec::new();
        if let Some(expected) = self.expected {
            let ahead = (index + records - expected) % records;
            if ahead > MAX_GAP {
                return out;
            }
            for _ in 0..ahead {
                match self.last_good {
                    Some(frame) if self.repeats < MAX_REPEATS => {
                        self.repeats += 1;
                        out.push(Slot::Repeat(frame));
                    }
                    _ => out.push(Slot::Silence),
                }
            }
        }
        out.push(Slot::Frame(imbe));
        self.expected = Some((index + 1) % records);
        // A muted over's frames are never kept to stand in for a lost one.
        self.last_good = (self.verdict != Verdict::Muted).then_some(imbe);
        self.repeats = 0;
        out
    }

    /// The talker check has started a new over, or can no longer vouch for
    /// the old one: turn every repeat in `held_ldu1` and `current` into
    /// silence, since the frame it repeats may be another talker's. The
    /// frame to repeat from here on is the checking record's own (the last
    /// frame of `current`), which belongs to the over as it now stands, and
    /// none at all while that over is muted.
    fn forget_repeats(&mut self, held_ldu1: &mut [Slot], current: &mut [Slot]) {
        for slot in held_ldu1.iter_mut().chain(current.iter_mut()) {
            if matches!(slot, Slot::Repeat(_)) {
                *slot = Slot::Silence;
            }
        }
        self.repeats = 0;
        self.last_good = match current.last() {
            Some(Slot::Frame(frame)) if self.verdict != Verdict::Muted => Some(*frame),
            _ => None,
        };
    }

    fn talker(&self) -> Option<String> {
        self.source.map(|s| s.to_string())
    }
}

/// Everything the run loop needs to turn frames into sound. Owned by the
/// link thread; `None` for a link with no audio.
struct Audio {
    imbe: Box<dyn ImbeVoice>,
    /// The one audio lane `ConsoleSession` opened for this link.
    bus: CallAudio,
    /// Decoded frames waiting for the audio clock, at most [`MAX_DECODED`].
    decoded: VecDeque<[i16; FRAME_SAMPLES]>,
    /// When the next frame is due; `None` while priming.
    next_release: Option<Instant>,
    /// The codec-frame recording tap and the received over it has open.
    rec: OverTap,
}

impl Audio {
    fn new(imbe: Box<dyn ImbeVoice>, bus: CallAudio) -> Audio {
        Audio {
            imbe,
            bus,
            decoded: VecDeque::new(),
            next_release: None,
            rec: OverTap::new("p25", CaptureMode::P25, BACKEND),
        }
    }
}

/// The receive half of the run loop: the over in progress and the audio it
/// feeds.
struct RxPath {
    over: Over,
    audio: Option<Audio>,
    /// When the last voice record arrived, for [`RX_WATCHDOG`].
    last_data: Option<Instant>,
}

impl RxPath {
    fn new(audio: Option<Audio>) -> RxPath {
        RxPath {
            over: Over::default(),
            audio,
            last_data: None,
        }
    }

    /// One voice record from the reflector.
    fn on_voice(&mut self, shared: &Shared, record: &VoiceRecord, now: Instant) {
        shared.frames_rx.fetch_add(1, Ordering::Relaxed);
        // A quiet spell inside an open over means its terminator was lost
        // and this record may begin another over. Its clear verdict must not
        // carry over, so the old over is closed before anything else.
        if self.over.open
            && self
                .last_data
                .is_some_and(|last| now.saturating_duration_since(last) >= OVER_GAP)
        {
            self.end_over(shared);
        }
        self.last_data = Some(now);

        if let Some(tg) = record.talkgroup() {
            shared.talkgroup.store(tg, Ordering::Relaxed);
        }
        if !self.over.open {
            self.over.open = true;
            shared.receiving.store(true, Ordering::Relaxed);
        }

        let records = u8::try_from(RECORDS).expect("18 fits");
        // Record 0 that the gap rule would drop as late, after this LDU1's
        // talker check has already run (so its record 4 or later arrived):
        // that is not jitter, which reorders neighbours inside a burst, but
        // a new LDU1, typically a new talker keying straight after an over
        // that ended on an LDU1 and lost its end record. Take it as the
        // start of one, so the talker check runs again on its `0x66`,
        // instead of dropping records 0-8 and playing the next LDU2 under
        // the old over's verdict.
        if record.index == 0
            && self.over.ldu1_confirmed
            && self
                .over
                .expected
                .is_some_and(|e| (records - e) % records > MAX_GAP)
        {
            self.over.expected = None;
            self.over.last_good = None;
            self.over.repeats = 0;
        }
        let previous = self.over.expected.map(|e| (e + records - 1) % records);
        let mut slots = self.over.sequence(record.index, record.imbe);
        // A late record (no slots) moves nothing; an accepted one below
        // index 4, or one whose index wrapped, begins a new LDU1 whose
        // talker has not been checked.
        let accepted = !slots.is_empty();
        if accepted && (record.index < SOURCE_INDEX || previous.is_some_and(|p| record.index < p)) {
            self.over.ldu1_confirmed = false;
        }
        if record.index < SOURCE_INDEX {
            // LDU1 before its `0x66`: the talker is not confirmed yet.
            self.over.ldu1.extend(slots);
            while self.over.ldu1.len() > MAX_HELD {
                self.over.ldu1.pop_front();
            }
        } else {
            if accepted && !self.over.ldu1_confirmed {
                // The first record at index 4 or above in this LDU1: the
                // `0x66` itself, or, if it was lost, whatever came next.
                self.confirm_talker(shared, record.source_id(), &mut slots, now);
                self.over.ldu1_confirmed = true;
            }
            // A cipher ALGID applies before its own record is routed, so
            // record 14 of an encrypted over is never played.
            if record.algid().is_some_and(|a| !algid_is_clear(a)) {
                self.mute(shared);
            }
            self.route(shared, slots, now);
        }
        // Noted on every record, encrypted or not, so the heard list's age
        // is the talker's latest frame.
        if let Some(source) = self.over.source {
            shared.heard.note("p25", &source.to_string());
        }

        if record.algid().is_some_and(algid_is_clear) && self.over.verdict == Verdict::Pending {
            self.over.verdict = Verdict::Clear;
            let held: Vec<Slot> = self.over.held.drain(..).collect();
            self.play(shared, held, now);
        }
    }

    /// The over is encrypted: whatever is held is never decoded, and nothing
    /// more is until it ends.
    fn mute(&mut self, shared: &Shared) {
        self.over.verdict = Verdict::Muted;
        self.over.held.clear();
        self.over.ldu1.clear();
        self.over.last_good = None;
        shared.encrypted.store(true, Ordering::Relaxed);
    }

    /// The `0x66` of an LDU1 has arrived (`source`), or the LDU1 went past
    /// index 4 without one (`None`): decide whose the held LDU1 frames are,
    /// then route them.
    ///
    /// * The same talker as the over's: the over's verdict stands.
    /// * A different talker: the old over ends and a new one opens, with
    ///   its ALGID unknown, so these frames are held until it arrives.
    /// * A talker the over had not named yet: frames still held from the
    ///   unnamed talker are dropped, and a clear verdict goes back to
    ///   pending. A muted one stays muted.
    /// * No `0x66` at all (it was lost): nothing says this is still the
    ///   talker whose ALGID was clear, so a clear verdict goes back to
    ///   pending until the next ALGID. Frames still held are dropped too:
    ///   they may be an earlier talker's whose ALGID was never seen, and the
    ///   next clear ALGID must release only this LDU1's.
    ///
    /// In every case but the same talker, the frame to repeat is forgotten,
    /// and any repeat in the held LDU1 or in `current` (the slots of the
    /// record that triggered the check) becomes silence: it was made from a
    /// frame that may belong to another talker, or to an encrypted over.
    fn confirm_talker(
        &mut self,
        shared: &Shared,
        source: Option<u32>,
        current: &mut [Slot],
        now: Instant,
    ) {
        let mut ldu1: Vec<Slot> = self.over.ldu1.drain(..).collect();
        match (source, self.over.source) {
            (Some(new), Some(old)) if new == old => {}
            (Some(new), Some(_)) => {
                // A new talker with no end record between: close the first
                // over before this one opens, keeping the place in the
                // superframe but nothing of the first over's audio.
                let expected = self.over.expected;
                self.end_over(shared);
                self.over.expected = expected;
                self.over.forget_repeats(&mut ldu1, current);
                self.over.open = true;
                self.over.source = Some(new);
                shared.receiving.store(true, Ordering::Relaxed);
            }
            (new, _) => {
                // Either the over had no talker yet (it was joined mid-way),
                // or this LDU1's `0x66` was lost. Either way nothing confirms
                // that what is still held is this talker's, so it goes.
                self.over.held.clear();
                if new.is_some() {
                    self.over.source = new;
                }
                if self.over.verdict == Verdict::Clear {
                    self.over.verdict = Verdict::Pending;
                }
                self.over.forget_repeats(&mut ldu1, current);
            }
        }
        if let Some(source) = source {
            shared.last_heard_id.store(source, Ordering::Relaxed);
            if let Ok(mut slot) = shared.last_heard.lock() {
                *slot = Some(source.to_string());
            }
        }
        self.route(shared, ldu1, now);
    }

    /// Send slots on by the over's verdict: hold, play or drop.
    fn route(&mut self, shared: &Shared, slots: Vec<Slot>, now: Instant) {
        match self.over.verdict {
            Verdict::Pending => {
                self.over.held.extend(slots);
                while self.over.held.len() > MAX_HELD {
                    self.over.held.pop_front();
                }
            }
            Verdict::Clear => self.play(shared, slots, now),
            Verdict::Muted => {}
        }
    }

    /// Decode `slots` and queue the speech for the audio clock.
    fn play(&mut self, shared: &Shared, slots: impl IntoIterator<Item = Slot>, now: Instant) {
        let talker = self.over.talker();
        let Some(a) = self.audio.as_mut() else {
            return;
        };
        for slot in slots {
            let pcm = match slot {
                // A repeat fill is decoded and recorded like a received
                // frame. Nothing is recorded until the talker is known, so
                // an over joined mid-way makes one recording, not two.
                Slot::Frame(frame) | Slot::Repeat(frame) => {
                    let pcm = a.imbe.decode(&frame);
                    if let Some(talker) = talker.as_deref() {
                        if a.rec.rotates(talker, now) {
                            drain_tail(a);
                        }
                        a.rec.note_rx(talker, now);
                        a.rec.frame_with(Dir::Rx, || frame.to_vec());
                        a.rec.pcm(Dir::Rx, &pcm);
                    }
                    pcm
                }
                Slot::Silence => [0; FRAME_SAMPLES],
            };
            push_decoded(a, shared, &pcm);
        }
        release(a, now);
    }

    /// The transmission is over: play what is queued, close the recording,
    /// reset the decoder, and forget the over.
    fn end_over(&mut self, shared: &Shared) {
        if let Some(a) = self.audio.as_mut() {
            drain_tail(a);
            a.rec.close(Dir::Rx);
            a.imbe.reset();
        }
        self.over = Over::default();
        shared.receiving.store(false, Ordering::Relaxed);
        shared.encrypted.store(false, Ordering::Relaxed);
    }

    /// Close a transmission that stopped without an end record.
    fn watchdog(&mut self, shared: &Shared, now: Instant) {
        let Some(last) = self.last_data else {
            return;
        };
        if now.saturating_duration_since(last) < RX_WATCHDOG {
            return;
        }
        self.last_data = None;
        if self.over.open {
            tracing::debug!(
                "p25: no record for {RX_WATCHDOG:?}, treating the transmission as over"
            );
            self.end_over(shared);
        }
    }
}

/// Queue one decoded frame, dropping the oldest past [`MAX_DECODED`].
fn push_decoded(a: &mut Audio, shared: &Shared, pcm: &[i16; FRAME_SAMPLES]) {
    if a.decoded.len() >= MAX_DECODED {
        a.decoded.pop_front();
    }
    a.decoded.push_back(*pcm);
    #[cfg(test)]
    shared
        .decoded_high_water
        .fetch_max(a.decoded.len(), Ordering::Relaxed);
    #[cfg(not(test))]
    let _ = shared;
}

impl P25Link {
    /// Resolve `host` (`host:port`), bind a local socket and link. The link
    /// makes no sound; it reports who is talking.
    ///
    /// # Errors
    /// [`ConsoleError::P25`] if the callsign is not one the wire can carry,
    /// if `host` does not resolve, or if the socket cannot be bound.
    pub fn connect(host: &str, callsign: &str) -> Result<P25Link, ConsoleError> {
        Self::spawn(host, callsign, None)
    }

    /// Open a link that plays the audio on it through the software IMBE
    /// decoder, onto `audio`, the lane `ConsoleSession` opened.
    ///
    /// # Errors
    /// As [`Self::connect`].
    pub fn connect_with_audio(cfg: &P25Config, audio: CallAudio) -> Result<P25Link, ConsoleError> {
        Self::connect_with_stream(cfg, audio, open_imbe())
    }

    /// [`Self::connect_with_audio`] with the decoder supplied by the caller.
    ///
    /// # Errors
    /// As [`Self::connect`].
    pub fn connect_with_stream(
        cfg: &P25Config,
        audio: CallAudio,
        imbe: Box<dyn ImbeVoice>,
    ) -> Result<P25Link, ConsoleError> {
        Self::spawn(&cfg.host, &cfg.callsign, Some(Audio::new(imbe, audio)))
    }

    fn spawn(host: &str, callsign: &str, audio: Option<Audio>) -> Result<P25Link, ConsoleError> {
        let fsm = P25Fsm::new(callsign).map_err(|e| ConsoleError::P25(format!("callsign: {e}")))?;

        let addr: SocketAddr = host
            .to_socket_addrs()
            .map_err(|e| ConsoleError::P25(format!("resolve {host}: {e}")))?
            .next()
            .ok_or_else(|| ConsoleError::P25(format!("resolve {host}: no addresses")))?;

        let bind: SocketAddr = if addr.is_ipv4() {
            "0.0.0.0:0".parse().expect("valid v4 bind")
        } else {
            "[::]:0".parse().expect("valid v6 bind")
        };
        let socket = UdpSocket::bind(bind).map_err(|e| ConsoleError::P25(format!("bind: {e}")))?;
        let timeout = if audio.is_some() {
            AUDIO_RECV_TIMEOUT
        } else {
            RECV_TIMEOUT
        };
        socket
            .set_read_timeout(Some(timeout))
            .map_err(|e| ConsoleError::P25(format!("socket timeout: {e}")))?;

        let shared = Arc::new(Shared::new());
        let backend = audio.as_ref().map(|_| BACKEND);
        let frame_tap = audio
            .as_ref()
            .map_or_else(TapSlot::default, |a| a.rec.slot());

        let packet_slot = PacketSlot::default();
        let packet_tap = CallPacketTap::new(
            packet_slot.clone(),
            astar_p25::wire::is_media,
            CallMeta {
                network: PacketNetwork::P25,
                codec: None,
                label: format!("P25 {}", reflector_name(host)),
                local: socket.local_addr().unwrap_or(bind),
                peer: addr,
            },
        );
        let packet_call = packet_tap.id();

        let thread = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("astar-p25-link".into())
                .spawn(move || run(&socket, addr, fsm, &shared, RxPath::new(audio), packet_tap))
                .map_err(|e| ConsoleError::P25(format!("thread: {e}")))?
        };

        Ok(P25Link {
            shared,
            thread: Some(thread),
            backend,
            reflector: host.to_string(),
            frame_tap,
            packet_slot,
            packet_call,
        })
    }

    /// Current link state, cheap enough to poll.
    #[must_use]
    pub fn snapshot(&self) -> P25Snapshot {
        P25Snapshot {
            link_state: self.link_state().as_str(),
            reflector: self.reflector.clone(),
            last_heard: self.shared.last_heard.lock().map_or(None, |g| g.clone()),
            last_heard_id: optional(self.shared.last_heard_id.load(Ordering::Relaxed)),
            talkgroup: optional(self.shared.talkgroup.load(Ordering::Relaxed)),
            frames_rx: self.shared.frames_rx.load(Ordering::Relaxed),
            receiving: self.shared.receiving.load(Ordering::Relaxed),
            encrypted: self.shared.encrypted.load(Ordering::Relaxed),
            backend: self.backend,
            ptt: self.shared.ptt.load(Ordering::Relaxed),
            tx_dbfs: -60.0,
            rx_dbfs: -60.0,
        }
    }

    /// The packet log's slot. Setting it reaches the live link.
    #[must_use]
    pub fn packet_slot(&self) -> PacketSlot {
        self.packet_slot.clone()
    }

    /// The id this link's packet events carry.
    #[must_use]
    pub fn packet_call(&self) -> PacketCallId {
        self.packet_call
    }

    /// The codec-frame recording tap: each decoded over under `p25`, each
    /// IMBE frame's 11 bytes followed by the 160 samples it decoded to.
    /// Encrypted overs record nothing.
    #[must_use]
    pub fn frame_tap(&self) -> TapSlot<FrameEvent> {
        self.frame_tap.clone()
    }

    /// Who has keyed up on this link, newest first, as radio IDs.
    #[must_use]
    pub fn heard(&self) -> Vec<HeardEntry> {
        self.shared.heard.snapshot()
    }

    /// Request transmit on or off. While transmit is not built, a key-down
    /// is refused by the run loop and [`P25Snapshot::ptt`] stays false.
    pub fn set_ptt(&self, on: bool) {
        self.shared.ptt_request.store(on, Ordering::Relaxed);
    }

    /// The link state as the protocol crate's enum.
    #[must_use]
    pub fn link_state(&self) -> LinkState {
        state_from_index(self.shared.link_state.load(Ordering::Relaxed))
    }

    /// Send the three unlinks and stop the thread. Consumes the link.
    pub fn disconnect(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// The deepest the decoded queue has been.
    #[cfg(test)]
    fn decoded_high_water(&self) -> usize {
        self.shared.decoded_high_water.load(Ordering::Relaxed)
    }
}

impl std::fmt::Debug for P25Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("P25Link")
            .field("snapshot", &self.snapshot())
            .finish_non_exhaustive()
    }
}

impl Drop for P25Link {
    /// Dropping a link unlinks it, so the reflector stops sending to a
    /// socket nobody reads.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The host part of `host:port`, for the packet log's label.
fn reflector_name(host: &str) -> &str {
    host.rsplit_once(':').map_or(host, |(name, _)| name)
}

/// Bigger than any datagram the wire defines, so an oversized one arrives
/// whole and is refused by `parse` rather than truncated into a valid-looking
/// record.
const RECV_BUF: usize = 512;

/// The link thread's body.
fn run(
    socket: &UdpSocket,
    addr: SocketAddr,
    mut fsm: P25Fsm,
    shared: &Arc<Shared>,
    mut rx: RxPath,
    mut packet_tap: CallPacketTap,
) {
    // Read the state, not an action: a voice record while linking moves the
    // FSM to linked and returns `Voice`.
    let publish = |fsm: &P25Fsm| {
        shared
            .link_state
            .store(state_index(fsm.state()), Ordering::Relaxed);
    };

    for datagram in fsm.connect(Instant::now()) {
        if socket.send_to(&datagram, addr).is_err() {
            shared
                .link_state
                .store(state_index(LinkState::Failed), Ordering::Relaxed);
            return;
        }
    }
    publish(&fsm);

    let mut buf = [0_u8; RECV_BUF];
    // A receive or send error ends the link; it reports `failed`, not
    // `idle`, so the app offers Reconnect and releases the lane.
    let mut failed = false;
    while !shared.stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            // Only the reflector reaches the FSM. A datagram from anywhere
            // else on this port is a stranger's, and is dropped unparsed.
            Ok((n, from)) if from == addr => {
                packet_tap.capture(from, &buf[..n]);
                let now = Instant::now();
                let action = fsm.on_packet(&buf[..n], now);
                match handle(&action, socket, addr, shared, &mut rx, now) {
                    Step::Go => {}
                    Step::Stop => break,
                    Step::Fail => {
                        failed = true;
                        break;
                    }
                }
            }
            Ok(_) => {}
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            }
            Err(_) => {
                failed = true;
                break;
            }
        }
        let now = Instant::now();
        let action = fsm.tick(now);
        match handle(&action, socket, addr, shared, &mut rx, now) {
            Step::Go => {}
            Step::Stop => break,
            Step::Fail => {
                failed = true;
                break;
            }
        }
        publish(&fsm);
        rx.watchdog(shared, now);
        if let Some(a) = rx.audio.as_mut() {
            run_ptt_step(a, shared);
            release(a, Instant::now());
        }
    }

    if let Some(a) = rx.audio.as_mut() {
        a.rec.close(Dir::Rx);
    }
    let failed = failed || fsm.state() == LinkState::Failed;
    // Best effort: the reflector drops us on its own timeout anyway.
    for datagram in fsm.unlink(Instant::now()) {
        let _ = socket.send_to(&datagram, addr);
    }
    let last = if failed {
        LinkState::Failed
    } else {
        LinkState::Idle
    };
    shared
        .link_state
        .store(state_index(last), Ordering::Relaxed);
}

/// What the run loop does after one FSM action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Carry on.
    Go,
    /// Stop; the FSM's own state says how the link ended.
    Stop,
    /// Stop, and end the link `failed`: the network refused a send (say,
    /// unreachable after a Wi-Fi drop), so the link is lost, not hung up.
    Fail,
}

/// Apply one FSM action.
fn handle(
    action: &FsmAction,
    socket: &UdpSocket,
    addr: SocketAddr,
    shared: &Shared,
    rx: &mut RxPath,
    now: Instant,
) -> Step {
    match action {
        FsmAction::None | FsmAction::Linked => Step::Go,
        FsmAction::Send(bytes) => match socket.send_to(bytes, addr) {
            Ok(_) => Step::Go,
            Err(e) => {
                tracing::debug!("p25: send failed, the link is lost: {e}");
                Step::Fail
            }
        },
        FsmAction::Voice(record) => {
            rx.on_voice(shared, record, now);
            Step::Go
        }
        FsmAction::End => {
            rx.last_data = None;
            if rx.over.open {
                rx.end_over(shared);
            }
            Step::Go
        }
        FsmAction::Timeout => {
            rx.end_over(shared);
            Step::Stop
        }
    }
}

/// One pass's transmit step: refuse any key, then drop whatever the capture
/// lane queued. In that order, on the same pass, as [`crate::nxdn`] does and
/// for the same reason: the console's gate can be open before this loop
/// sees the request, and without the drop the lane would pile up stale
/// audio.
fn run_ptt_step(audio: &mut Audio, shared: &Shared) {
    apply_ptt(shared);
    while audio.bus.tx_frames.try_recv().is_ok() {}
}

/// The receive-only gate: a key request is cleared, never applied.
fn apply_ptt(shared: &Shared) {
    if shared.ptt_request.swap(false, Ordering::Relaxed) {
        // Transmit is backlog item `p25-transmit`. A key that silently did
        // nothing would be worse than one that is refused.
        tracing::warn!("p25: transmit is not implemented; the key was refused, not queued");
    }
    shared.ptt.store(false, Ordering::Relaxed);
}

/// Hand decoded frames to the bus on the audio clock, one per
/// [`FRAME_INTERVAL`], after priming [`PRIME_FRAMES`]. Running dry
/// re-primes.
fn release(audio: &mut Audio, now: Instant) {
    if audio.next_release.is_none() {
        if audio.decoded.len() < PRIME_FRAMES {
            return;
        }
        audio.next_release = Some(now);
    }
    while let Some(due) = audio.next_release {
        if now < due {
            break;
        }
        let Some(pcm) = audio.decoded.pop_front() else {
            audio.next_release = None;
            break;
        };
        let _ = audio.bus.rx_frames.send(pcm.to_vec().into());
        audio.next_release = Some(due + FRAME_INTERVAL);
    }
}

/// Push whatever is still queued at the end of an over, unpaced, and
/// re-prime for the next one.
fn drain_tail(audio: &mut Audio) {
    while let Some(pcm) = audio.decoded.pop_front() {
        let _ = audio.bus.rx_frames.send(pcm.to_vec().into());
    }
    audio.next_release = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use astar_p25::{Reflector, Superframe, superframe, wire};
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::{Receiver, Sender, channel};

    const SOURCE: u32 = 1_234_567;
    const OTHER: u32 = 7_654_321;
    const TG: u32 = 10_200;
    /// An ALGID that is neither clear value, standing for any cipher.
    const CIPHER: u8 = 0x84;

    /// A decoder that answers each frame with 160 copies of its first byte,
    /// so the audio can be traced to the frame it came from, and counts
    /// its decodes and resets.
    struct FakeImbe {
        decodes: Arc<AtomicUsize>,
        resets: Arc<AtomicUsize>,
    }

    impl ImbeVoice for FakeImbe {
        fn decode(&mut self, frame: &[u8; FRAME_BYTES]) -> [i16; FRAME_SAMPLES] {
            self.decodes.fetch_add(1, Ordering::Relaxed);
            [i16::from(frame[0]); FRAME_SAMPLES]
        }
        fn reset(&mut self) {
            self.resets.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn fake_imbe() -> (Box<dyn ImbeVoice>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let decodes = Arc::new(AtomicUsize::new(0));
        let resets = Arc::new(AtomicUsize::new(0));
        (
            Box::new(FakeImbe {
                decodes: Arc::clone(&decodes),
                resets: Arc::clone(&resets),
            }),
            decodes,
            resets,
        )
    }

    /// A lane wired to channels the test reads, with no device anywhere.
    fn test_lane() -> (CallAudio, Receiver<astar_audio::RxFrame>, Sender<Vec<i16>>) {
        let (rx_tx, rx_rx) = channel::<astar_audio::RxFrame>();
        let (tx_tx, tx_rx) = channel::<Vec<i16>>();
        (
            CallAudio {
                tx_frames: tx_rx,
                rx_frames: rx_tx,
                preroll_lead: Arc::new(AtomicU32::new(0)),
            },
            rx_rx,
            tx_tx,
        )
    }

    /// One superframe's 18 records. Frame `i` starts with byte `base + i`,
    /// so a test can tell every frame apart by the audio it decodes to.
    fn sf(source: u32, algid: u8, base: u8) -> Vec<Vec<u8>> {
        let frames: [[u8; FRAME_BYTES]; RECORDS] = std::array::from_fn(|i| {
            let mut f = [0u8; FRAME_BYTES];
            f[0] = base + u8::try_from(i).expect("small");
            f
        });
        superframe(
            &Superframe {
                source_id: source,
                talkgroup: TG,
                algid,
                kid: 0,
            },
            &frames,
        )
    }

    /// `base..base + n` as decoded first samples.
    fn run_of(base: i16, n: i16) -> Vec<i16> {
        (base..base + n).collect()
    }

    /// The receive path on its own, driven record by record.
    struct Harness {
        rx: RxPath,
        shared: Shared,
        out: Receiver<astar_audio::RxFrame>,
        mic: Sender<Vec<i16>>,
        decodes: Arc<AtomicUsize>,
        resets: Arc<AtomicUsize>,
        sock: UdpSocket,
        addr: SocketAddr,
        _peer: UdpSocket,
    }

    impl Harness {
        fn new() -> Harness {
            let (lane, out, mic) = test_lane();
            let (imbe, decodes, resets) = fake_imbe();
            let peer = UdpSocket::bind("127.0.0.1:0").expect("bind peer");
            let addr = peer.local_addr().expect("addr");
            Harness {
                rx: RxPath::new(Some(Audio::new(imbe, lane))),
                shared: Shared::new(),
                out,
                mic,
                decodes,
                resets,
                sock: UdpSocket::bind("127.0.0.1:0").expect("bind"),
                addr,
                _peer: peer,
            }
        }

        fn tap(&self) -> Receiver<FrameEvent> {
            let (tap, events) = astar_record::tap::tap_channel(1024);
            self.rx
                .audio
                .as_ref()
                .expect("audio")
                .rec
                .slot()
                .set(Some(tap));
            events
        }

        /// One datagram, as the FSM would hand it up from a linked
        /// reflector.
        fn feed(&mut self, datagram: &[u8]) {
            self.feed_at(datagram, Instant::now());
        }

        /// [`Self::feed`] on a clock the test chooses.
        fn feed_at(&mut self, datagram: &[u8], now: Instant) {
            let action = match wire::parse(datagram) {
                Some(wire::Packet::Voice(r)) => FsmAction::Voice(Box::new(r)),
                Some(wire::Packet::End) => FsmAction::End,
                other => panic!("not a voice or end record: {other:?}"),
            };
            assert_eq!(
                handle(
                    &action,
                    &self.sock,
                    self.addr,
                    &self.shared,
                    &mut self.rx,
                    now
                ),
                Step::Go
            );
        }

        fn feed_all<'a>(&mut self, datagrams: impl IntoIterator<Item = &'a Vec<u8>>) {
            for d in datagrams {
                self.feed(d);
            }
        }

        /// What reached the lane, with the queue drained: each frame's
        /// first sample.
        fn played(&mut self) -> Vec<i16> {
            drain_tail(self.rx.audio.as_mut().expect("audio"));
            self.out.try_iter().map(|f| f.pcm[0]).collect()
        }

        fn decodes(&self) -> usize {
            self.decodes.load(Ordering::Relaxed)
        }

        fn last_heard_id(&self) -> Option<u32> {
            optional(self.shared.last_heard_id.load(Ordering::Relaxed))
        }

        fn encrypted(&self) -> bool {
            self.shared.encrypted.load(Ordering::Relaxed)
        }

        fn receiving(&self) -> bool {
            self.shared.receiving.load(Ordering::Relaxed)
        }
    }

    fn wait_for(link: &P25Link, want: &str, within: Duration) -> P25Snapshot {
        let deadline = Instant::now() + within;
        loop {
            let snap = link.snapshot();
            if snap.link_state == want || Instant::now() > deadline {
                return snap;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// A stand-in reflector: a bare socket on `127.0.0.1` that echoes the
    /// link's first poll so it links, and then sends only what the test
    /// tells it to. Returns the socket, the link's address and the link.
    fn fake_reflector(
        audio: Option<(CallAudio, Box<dyn ImbeVoice>)>,
    ) -> (UdpSocket, SocketAddr, P25Link) {
        let refl = UdpSocket::bind("127.0.0.1:0").expect("bind reflector");
        refl.set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let host = refl.local_addr().expect("addr").to_string();
        let link = match audio {
            Some((lane, imbe)) => P25Link::connect_with_stream(
                &P25Config {
                    host,
                    callsign: "KC0ABC".into(),
                },
                lane,
                imbe,
            ),
            None => P25Link::connect(&host, "KC0ABC"),
        }
        .expect("connect");
        let mut buf = [0u8; 64];
        let (n, from) = refl.recv_from(&mut buf).expect("a poll");
        refl.send_to(&buf[..n], from).expect("echo");
        assert_eq!(
            wait_for(&link, "linked", Duration::from_secs(5)).link_state,
            "linked"
        );
        (refl, from, link)
    }

    // ── The link ────────────────────────────────────────────────────────

    #[test]
    fn linking_to_a_reflector_reaches_linked() {
        let r = Reflector::bind("127.0.0.1:0".parse().expect("v4")).expect("bind");
        let addr = r.local_addr();
        let reflector = r.run();
        let link = P25Link::connect(&addr.to_string(), "KC0ABC").expect("connect");
        let snap = wait_for(&link, "linked", Duration::from_secs(5));
        assert_eq!(snap.link_state, "linked", "{snap:?}");
        assert_eq!(snap.reflector, addr.to_string());
        assert_eq!(snap.frames_rx, 0, "a poll is not a voice record");
        assert_eq!(snap.last_heard, None);
        assert_eq!(snap.backend, None, "no audio, no decoder");
        assert!(!snap.encrypted && !snap.receiving && !snap.ptt);
        link.disconnect();
        reflector.shutdown();
    }

    #[test]
    fn an_impossible_callsign_is_refused_at_connect() {
        for (callsign, why) in [("", "empty"), ("0123456789A", "longer than 10")] {
            let err = P25Link::connect("127.0.0.1:41000", callsign).expect_err("refused");
            assert!(matches!(err, ConsoleError::P25(_)), "{err:?}");
            let text = err.to_string();
            assert!(text.starts_with("p25: callsign"), "{text}");
            assert!(text.contains(why), "{text}");
        }
    }

    #[test]
    fn an_unresolvable_host_is_refused_at_connect() {
        let err = P25Link::connect("no-such-host.invalid:41000", "KC0ABC").expect_err("refused");
        assert!(matches!(err, ConsoleError::P25(_)), "{err:?}");
        assert!(err.to_string().contains("resolve"), "{err}");
    }

    // ── Who is talking ──────────────────────────────────────────────────

    #[test]
    fn the_talker_and_talkgroup_are_read_from_the_records() {
        let mut h = Harness::new();
        h.feed_all(&sf(SOURCE, 0x80, 1));
        assert_eq!(h.last_heard_id(), Some(1_234_567));
        assert_eq!(
            optional(h.shared.talkgroup.load(Ordering::Relaxed)),
            Some(10_200)
        );
        assert_eq!(
            h.shared.last_heard.lock().expect("lock").as_deref(),
            Some("1234567")
        );
        let heard = h.shared.heard.snapshot();
        assert_eq!(
            (heard[0].network, heard[0].callsign.as_str()),
            ("p25", "1234567")
        );
        assert_eq!(h.shared.frames_rx.load(Ordering::Relaxed), 18);
        assert!(h.receiving());
    }

    // ── Audio and the ALGID hold ────────────────────────────────────────

    #[test]
    fn a_clear_superframe_becomes_eighteen_frames_of_speech() {
        let mut h = Harness::new();
        h.feed_all(&sf(SOURCE, 0x80, 1));
        assert_eq!(h.played(), run_of(1, 18));
    }

    #[test]
    fn audio_is_held_until_the_algid_then_released() {
        let mut h = Harness::new();
        let records = sf(SOURCE, 0x80, 1);
        for r in &records[..14] {
            h.feed(r);
            assert_eq!(h.decodes(), 0, "nothing is decoded before record 0x70");
            assert_eq!(h.out.try_iter().count(), 0, "nothing plays before 0x70");
        }
        h.feed(&records[14]);
        assert_eq!(h.decodes(), 15, "the 14 held frames, then 0x70's own");
        assert_eq!(h.played(), run_of(1, 15));
    }

    /// The ALGID ruling: `0x80` and `0x00` are clear; anything else mutes.
    #[test]
    fn algid_0x80_plays() {
        let mut h = Harness::new();
        h.feed_all(&sf(SOURCE, 0x80, 1));
        assert_eq!(h.played().len(), 18);
        assert!(!h.encrypted());
    }

    /// urfd sends ALGID `0x00` on every P25 call; it must play.
    #[test]
    fn algid_0x00_plays() {
        let mut h = Harness::new();
        h.feed_all(&sf(SOURCE, 0x00, 1));
        assert_eq!(h.played(), run_of(1, 18));
        assert!(!h.encrypted());
    }

    #[test]
    fn algid_0x81_is_muted() {
        let mut h = Harness::new();
        h.feed_all(&sf(SOURCE, 0x81, 1));
        assert_eq!(h.played(), Vec::<i16>::new());
        assert_eq!(h.decodes(), 0);
        assert!(h.encrypted());
    }

    #[test]
    fn the_clear_rule_is_exactly_0x80_and_0x00() {
        for algid in 0..=u8::MAX {
            assert_eq!(
                algid_is_clear(algid),
                algid == 0x80 || algid == 0x00,
                "{algid:#04x}"
            );
        }
    }

    /// An over that ends before its ALGID record arrives plays nothing: its
    /// cipher was never known.
    #[test]
    fn an_over_whose_algid_never_arrives_plays_nothing() {
        let mut h = Harness::new();
        let records = sf(SOURCE, 0x80, 1);
        h.feed_all(&records[..14]);
        h.feed(&wire::end_record());
        assert_eq!(h.played(), Vec::<i16>::new());
        assert_eq!(h.decodes(), 0);
        assert!(!h.receiving());
        assert_eq!(h.last_heard_id(), Some(SOURCE), "the talker is still shown");
    }

    #[test]
    fn an_encrypted_over_plays_nothing_and_says_so() {
        let mut h = Harness::new();
        let events = h.tap();
        h.feed_all(&sf(SOURCE, CIPHER, 1));
        assert!(h.receiving());
        assert!(h.encrypted(), "encrypted while receiving");
        assert_eq!(h.last_heard_id(), Some(SOURCE));
        // A second superframe of the same over stays muted.
        h.feed_all(&sf(SOURCE, CIPHER, 30));
        h.feed(&wire::end_record());
        assert_eq!(
            h.played(),
            Vec::<i16>::new(),
            "no RxFrame for an encrypted over"
        );
        assert_eq!(h.decodes(), 0);
        assert!(
            !events
                .try_iter()
                .any(|e| matches!(e, FrameEvent::Frame { .. })),
            "an encrypted over records nothing"
        );
    }

    #[test]
    fn the_end_record_clears_receiving_and_encrypted_and_resets_the_decoder() {
        let mut h = Harness::new();
        h.feed_all(&sf(SOURCE, CIPHER, 1));
        assert!(h.receiving() && h.encrypted());
        h.feed(&wire::end_record());
        assert!(!h.receiving());
        assert!(!h.encrypted());
        assert_eq!(h.resets.load(Ordering::Relaxed), 1);

        // And the next over starts afresh: held, then clear.
        h.feed_all(&sf(SOURCE, 0x80, 1));
        assert_eq!(h.played(), run_of(1, 18));
    }

    #[test]
    fn a_transmission_that_stops_dead_is_closed_by_the_watchdog() {
        let mut h = Harness::new();
        let events = h.tap();
        h.feed_all(&sf(SOURCE, CIPHER, 1));
        h.rx.watchdog(&h.shared, Instant::now());
        assert!(h.receiving(), "a record just arrived");
        let late = Instant::now() + RX_WATCHDOG + Duration::from_millis(1);
        h.rx.watchdog(&h.shared, late);
        assert!(!h.receiving());
        assert!(!h.encrypted());
        assert_eq!(h.resets.load(Ordering::Relaxed), 1);
        assert!(h.rx.last_data.is_none(), "and it does not fire twice");
        drop(events);

        // A clear over closed by the watchdog plays its tail and closes its
        // recording.
        let events = h.tap();
        h.feed_all(&sf(SOURCE, 0x80, 1));
        h.rx.watchdog(&h.shared, Instant::now() + RX_WATCHDOG * 2);
        assert_eq!(h.out.try_iter().count(), 18, "the tail is played");
        assert_eq!(
            events.try_iter().last(),
            Some(FrameEvent::End { dir: Dir::Rx })
        );
    }

    // ── Lost and late records ───────────────────────────────────────────

    #[test]
    fn a_lost_record_is_filled_by_repeating_the_last_frame() {
        let mut h = Harness::new();
        let records = sf(SOURCE, 0x80, 1);
        for (i, r) in records.iter().enumerate() {
            if i != 7 {
                h.feed(r);
            }
        }
        let played = h.played();
        assert_eq!(played.len(), 18);
        assert_eq!(played[7], played[6], "frame 7 repeats frame 6");
        assert_eq!(played[8], 9);
    }

    #[test]
    fn four_lost_records_repeat_three_times_then_mute() {
        let mut h = Harness::new();
        let records = sf(SOURCE, 0x80, 1);
        for (i, r) in records.iter().enumerate() {
            if !(5..=8).contains(&i) {
                h.feed(r);
            }
        }
        let mut want = run_of(1, 5);
        want.extend([5, 5, 5, 0]);
        want.extend(run_of(10, 9));
        assert_eq!(h.played(), want);
    }

    #[test]
    fn a_late_record_is_dropped_not_replayed() {
        let mut h = Harness::new();
        let records = sf(SOURCE, 0x80, 1);
        h.feed_all(&records[..10]);
        h.feed(&records[5]);
        h.feed_all(&records[10..]);
        assert_eq!(h.played(), run_of(1, 18), "5 is not decoded twice");
        assert_eq!(h.decodes(), 18);
    }

    /// Nine lost records is past the gap rule: the record reads as late, and
    /// the sequence picks up again when the index comes round.
    #[test]
    fn a_long_loss_resyncs_on_the_next_superframe() {
        let mut h = Harness::new();
        let first = sf(SOURCE, 0x80, 1);
        h.feed_all(&first[..15]);
        let second = sf(SOURCE, 0x80, 40);
        // 15, 16, 17 and 0..5 lost; 6.. is 9 ahead of 15, so it is dropped
        // until index 15 comes round again.
        h.feed_all(&second[6..]);
        let played = h.played();
        assert_eq!(&played[..15], run_of(1, 15).as_slice());
        assert_eq!(&played[15..], run_of(55, 3).as_slice());
    }

    // ── No runaway buffer ───────────────────────────────────────────────

    #[test]
    fn the_decoded_queue_never_passes_its_cap() {
        let mut h = Harness::new();
        for k in 0..10 {
            h.feed_all(&sf(SOURCE, 0x80, 1 + k));
        }
        let queued = h.rx.audio.as_ref().expect("audio").decoded.len();
        assert!(
            queued <= MAX_DECODED,
            "180 frames at once, capped: {queued}"
        );
        assert_eq!(
            h.shared.decoded_high_water.load(Ordering::Relaxed),
            MAX_DECODED
        );
    }

    #[test]
    fn a_flood_never_grows_the_queue_past_its_cap() {
        let (lane, _out, _mic) = test_lane();
        let (imbe, decodes, _) = fake_imbe();
        let (refl, link_addr, link) = fake_reflector(Some((lane, imbe)));
        let mut sent = 0;
        while sent < 500 {
            for r in sf(SOURCE, 0x80, 1) {
                refl.send_to(&r, link_addr).expect("send");
                sent += 1;
                // A breath every 64 records, so the kernel's receive buffer
                // does not overflow; the link itself never paces.
                if sent % 64 == 0 {
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while link.snapshot().frames_rx < 400 && Instant::now() < deadline {
            assert!(link.decoded_high_water() <= MAX_DECODED);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(link.snapshot().frames_rx >= 400, "the flood arrived");
        assert!(decodes.load(Ordering::Relaxed) > MAX_DECODED);
        let hw = link.decoded_high_water();
        assert!(
            (PRIME_FRAMES..=MAX_DECODED).contains(&hw),
            "high water {hw}"
        );
        link.disconnect();
    }

    // ── Review focus ────────────────────────────────────────────────────

    /// Review Focus 1: a second station keys up with no end record between.
    #[test]
    fn a_new_source_without_an_end_record_starts_a_new_over() {
        let mut h = Harness::new();
        let events = h.tap();
        h.feed_all(&sf(SOURCE, 0x80, 1));
        assert_eq!(h.resets.load(Ordering::Relaxed), 0);
        h.feed_all(&sf(OTHER, 0x80, 40));
        assert_eq!(h.last_heard_id(), Some(OTHER));
        let heard: Vec<String> = h
            .shared
            .heard
            .snapshot()
            .into_iter()
            .map(|e| e.callsign)
            .collect();
        assert_eq!(heard, ["7654321", "1234567"], "B then A");
        let edges: Vec<FrameEvent> = events
            .try_iter()
            .filter(|e| matches!(e, FrameEvent::Start { .. } | FrameEvent::End { .. }))
            .collect();
        assert_eq!(edges.len(), 3, "{edges:?}");
        assert!(
            matches!(&edges[0], FrameEvent::Start { talker: Some(t), mode, network, .. }
                if t == "1234567" && *mode == CaptureMode::P25 && *network == "p25")
        );
        assert_eq!(edges[1], FrameEvent::End { dir: Dir::Rx });
        assert!(matches!(&edges[2], FrameEvent::Start { talker: Some(t), .. } if t == "7654321"));
        assert_eq!(
            h.resets.load(Ordering::Relaxed),
            1,
            "one decoder reset between the overs"
        );
        assert!(h.receiving());
    }

    /// Review Focus 1, encrypted: a clear talker, then an encrypted one with
    /// no end record between. Records 0–3 of the second talker's first LDU1
    /// arrive before its `0x66`; none of the second talker's frames may play.
    #[test]
    fn a_clear_over_followed_by_an_encrypted_talker_without_end_plays_none_of_the_second() {
        let mut h = Harness::new();
        h.feed_all(&sf(SOURCE, 0x80, 1));
        h.feed_all(&sf(OTHER, CIPHER, 100));
        let played = h.played();
        assert_eq!(played, run_of(1, 18), "only the first talker plays");
        assert!(h.encrypted());
        assert_eq!(h.last_heard_id(), Some(OTHER));
        assert_eq!(h.decodes(), 18, "not one encrypted frame was decoded");
    }

    /// The same radio rekeys encrypted inside the 1.5 s watchdog after its
    /// terminator was lost. The quiet spell closes the clear over, so the
    /// new one is held for its own ALGID.
    #[test]
    fn the_same_radio_rekeying_encrypted_after_a_lost_terminator_plays_none_of_it() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        for r in &sf(SOURCE, 0x80, 1) {
            h.feed_at(r, t0);
        }
        let rekey = t0 + Duration::from_millis(400);
        assert!(rekey - t0 < RX_WATCHDOG, "inside the watchdog");
        for r in &sf(SOURCE, CIPHER, 100) {
            h.feed_at(r, rekey);
        }
        assert_eq!(h.played(), run_of(1, 18));
        assert_eq!(h.decodes(), 18);
        assert!(h.encrypted());
        assert_eq!(
            h.resets.load(Ordering::Relaxed),
            1,
            "the gap ended the first over"
        );
    }

    /// LDU bursts arrive about 180 ms apart inside a normal over; that is
    /// not a new over, and the audio is not held again.
    #[test]
    fn ldu_bursts_inside_one_over_are_not_a_new_over() {
        let mut h = Harness::new();
        let t0 = Instant::now();
        for (n, chunk) in sf(SOURCE, 0x80, 1)
            .chunks(9)
            .chain(sf(SOURCE, 0x80, 40).chunks(9))
            .enumerate()
        {
            let at = t0 + Duration::from_millis(180 * u64::try_from(n).expect("small"));
            for r in chunk {
                h.feed_at(r, at);
            }
        }
        let mut want = run_of(1, 18);
        want.extend(run_of(40, 18));
        assert_eq!(h.played(), want);
        assert_eq!(h.resets.load(Ordering::Relaxed), 0);
    }

    /// The `0x66` of the second talker's LDU1 is lost: nothing confirms the
    /// talker, so the LDU1 is held for the next ALGID, which mutes it.
    #[test]
    fn a_lost_talker_record_holds_the_ldu1_until_the_next_algid() {
        let mut h = Harness::new();
        h.feed_all(&sf(SOURCE, 0x80, 1));
        let second = sf(OTHER, CIPHER, 100);
        for (i, r) in second.iter().enumerate() {
            if i != 4 {
                h.feed(r);
            }
        }
        assert_eq!(h.played(), run_of(1, 18));
        assert_eq!(h.decodes(), 18);
        assert!(h.encrypted());
    }

    /// The same talker's next LDU1 plays through with no hold once its
    /// `0x66` confirms them.
    #[test]
    fn the_same_talker_keeps_playing_across_superframes() {
        let mut h = Harness::new();
        h.feed_all(&sf(SOURCE, 0x80, 1));
        let next = sf(SOURCE, 0x80, 40);
        h.feed_all(&next[..4]);
        assert_eq!(h.decodes(), 18, "records 0-3 wait for the 0x66");
        h.feed(&next[4]);
        assert_eq!(h.decodes(), 23, "and play once it confirms the talker");
        h.feed_all(&next[5..]);
        let mut want = run_of(1, 18);
        want.extend(run_of(40, 18));
        assert_eq!(h.played(), want);
    }

    /// Joining mid-over makes one recording, named for the talker, not an
    /// unnamed one followed by a named one.
    #[test]
    fn joining_mid_over_makes_one_named_recording() {
        let mut h = Harness::new();
        let events = h.tap();
        h.feed_all(&sf(SOURCE, 0x80, 1)[9..]);
        h.feed_all(&sf(SOURCE, 0x80, 40));
        h.feed(&wire::end_record());
        let starts: Vec<FrameEvent> = events
            .try_iter()
            .filter(|e| matches!(e, FrameEvent::Start { .. }))
            .collect();
        assert_eq!(starts.len(), 1, "{starts:?}");
        assert!(matches!(&starts[0], FrameEvent::Start { talker: Some(t), .. } if t == "1234567"));
    }

    /// Records 0–4 of an encrypted second talker are all lost, so no `0x66`
    /// names them. The talker check still runs on the first record at index
    /// 4 or above, demotes the over, and nothing of theirs plays or records.
    #[test]
    fn losing_records_0_to_4_at_a_seam_still_checks_the_talker() {
        let mut h = Harness::new();
        let events = h.tap();
        h.feed_all(&sf(SOURCE, 0x80, 1));
        h.feed_all(&sf(OTHER, CIPHER, 100)[5..]);
        assert_eq!(h.played(), run_of(1, 18));
        assert_eq!(h.decodes(), 18, "no encrypted frame was decoded");
        assert!(h.encrypted());
        assert!(
            !events.try_iter().any(|e| matches!(
                e,
                FrameEvent::Frame { bytes, .. } if bytes[0] >= 100
            )),
            "no encrypted frame was recorded"
        );
    }

    /// Linked during encrypted talker A's last LDU2 (after its `0x70`), so
    /// A's records 15–17 are held with no talker named. A's end record is
    /// lost and clear talker B keys inside `OVER_GAP`: none of A's frames
    /// may play when B's ALGID releases B's audio.
    #[test]
    fn held_frames_from_an_unnamed_talker_never_reach_the_next_talker() {
        let mut h = Harness::new();
        h.feed_all(&sf(OTHER, CIPHER, 100)[15..]);
        assert!(!h.encrypted(), "A's ALGID was never seen");
        h.feed_all(&sf(SOURCE, 0x80, 1));
        assert_eq!(h.played(), run_of(1, 18), "only B plays");
        assert_eq!(h.decodes(), 18);
        assert_eq!(h.last_heard_id(), Some(SOURCE));
    }

    /// Final review finding 3: a send the network refuses ends the link
    /// `failed`, so the app shows the lost link and offers Reconnect. The
    /// refusal is made locally and deterministically, on loopback only: an
    /// IPv4 socket cannot send to an IPv6 address.
    #[test]
    fn a_refused_send_is_a_lost_link_not_a_hang_up() {
        let mut h = Harness::new();
        let unreachable: SocketAddr = "[::1]:9".parse().expect("addr");
        let step = handle(
            &FsmAction::Send(wire::end_record().to_vec()),
            &h.sock,
            unreachable,
            &h.shared,
            &mut h.rx,
            Instant::now(),
        );
        assert_eq!(step, Step::Fail);
        let ok = handle(
            &FsmAction::Send(wire::end_record().to_vec()),
            &h.sock,
            h.addr,
            &h.shared,
            &mut h.rx,
            Instant::now(),
        );
        assert_eq!(ok, Step::Go, "the control: a send that goes through");
    }

    /// Final review finding 1: no lost-record fill may repeat a frame from
    /// an encrypted over. Encrypted A keys a whole superframe, then clear B
    /// keys with no end record between and B's records listed in `lost`
    /// never arrive. Returns what played and whether any encrypted frame
    /// (first byte 100 or more) reached the recorder.
    fn encrypted_then_clear_with_lost(lost: &[usize]) -> (Vec<i16>, bool) {
        let mut h = Harness::new();
        let events = h.tap();
        h.feed_all(&sf(OTHER, CIPHER, 100));
        for (i, r) in sf(SOURCE, 0x80, 1).iter().enumerate() {
            if !lost.contains(&i) {
                h.feed(r);
            }
        }
        h.feed(&wire::end_record());
        let recorded_encrypted = events.try_iter().any(|e| {
            matches!(
                e,
                FrameEvent::Frame { bytes, .. } if bytes[0] >= 100
            )
        });
        (h.played(), recorded_encrypted)
    }

    #[test]
    fn a_lost_record_after_an_encrypted_over_is_silence_not_its_last_frame() {
        let (played, recorded_encrypted) = encrypted_then_clear_with_lost(&[0]);
        let mut want = vec![0];
        want.extend(run_of(2, 17));
        assert_eq!(played, want, "record 0 is silence, never 117");
        assert!(!recorded_encrypted, "no encrypted frame was recorded");
    }

    #[test]
    fn three_lost_records_after_an_encrypted_over_are_silence() {
        let (played, recorded_encrypted) = encrypted_then_clear_with_lost(&[0, 1, 2]);
        let mut want = vec![0, 0, 0];
        want.extend(run_of(4, 15));
        assert_eq!(played, want);
        assert!(!recorded_encrypted);
    }

    #[test]
    fn four_lost_records_after_an_encrypted_over_are_silence() {
        let (played, recorded_encrypted) = encrypted_then_clear_with_lost(&[0, 1, 2, 3]);
        let mut want = vec![0, 0, 0, 0];
        want.extend(run_of(5, 14));
        assert_eq!(played, want, "not [117, 117, 117, 0, ...]");
        assert!(!recorded_encrypted);
    }

    /// The unnamed variant: encrypted A is joined after its ALGID, so its
    /// records 15-17 are held unnamed; clear B keys with record 0 lost.
    #[test]
    fn a_lost_record_after_an_unnamed_encrypted_talker_is_silence() {
        let mut h = Harness::new();
        h.feed_all(&sf(OTHER, CIPHER, 100)[15..]);
        h.feed_all(&sf(SOURCE, 0x80, 1)[1..]);
        let mut want = vec![0];
        want.extend(run_of(2, 17));
        assert_eq!(h.played(), want);
        assert_eq!(h.decodes(), 17, "no encrypted frame was decoded");
    }

    /// A frame is repeated only within its own over: a clear talker's last
    /// frame does not stand in for the next talker's lost record 0 either.
    #[test]
    fn a_lost_record_at_a_talker_change_never_repeats_the_last_talker() {
        let mut h = Harness::new();
        h.feed_all(&sf(OTHER, 0x80, 40));
        h.feed_all(&sf(SOURCE, 0x80, 1)[1..]);
        let mut want = run_of(40, 18);
        want.push(0);
        want.extend(run_of(2, 17));
        assert_eq!(h.played(), want);
    }

    /// Losing B's `0x66` demotes the over to pending. A repeat made across
    /// that seam is from a talker nobody has confirmed, so it is silence.
    #[test]
    fn a_lost_record_and_lost_talker_record_never_repeat_across_the_seam() {
        let mut h = Harness::new();
        h.feed_all(&sf(OTHER, 0x80, 40));
        for (i, r) in sf(SOURCE, 0x80, 1).iter().enumerate() {
            if i != 0 && i != 4 {
                h.feed(r);
            }
        }
        let played = h.played();
        assert_eq!(&played[..18], run_of(40, 18).as_slice());
        assert_eq!(played[18], 0, "record 0 is silence, not 57");
        assert!(!played[18..].contains(&57), "{played:?}");
    }

    /// Re-review probe 1: encrypted A loses records 14 (its ALGID) and 16,
    /// so A's frames sit held with no verdict. A's end record is lost and
    /// clear B keys inside `OVER_GAP` with B's `0x66` lost. B's clear ALGID
    /// must release only B's frames, never A's held ones.
    #[test]
    fn held_frames_die_when_the_next_talker_cannot_be_confirmed() {
        let mut h = Harness::new();
        let events = h.tap();
        for (i, r) in sf(OTHER, CIPHER, 100).iter().enumerate() {
            if i != 14 && i != 16 {
                h.feed(r);
            }
        }
        for (i, r) in sf(SOURCE, 0x80, 1).iter().enumerate() {
            if i != 4 {
                h.feed(r);
            }
        }
        h.feed(&wire::end_record());
        let played = h.played();
        assert!(
            played.iter().all(|&v| v < 100),
            "no frame of A's played: {played:?}"
        );
        assert!(
            !events.try_iter().any(|e| matches!(
                e,
                FrameEvent::Frame { bytes, .. } if bytes[0] >= 100
            )),
            "no frame of A's was recorded"
        );
        assert_eq!(
            &played[played.len() - 13..],
            run_of(6, 13).as_slice(),
            "B plays"
        );
    }

    /// Re-review probe 2: clear A ends after an LDU1 (records 0-8), its end
    /// record is lost, and encrypted B keys straight away. B's record 0
    /// starts a new LDU1, so B's `0x66` is checked and none of B plays.
    #[test]
    fn a_jump_to_record_0_after_an_ldu1_checks_the_new_talker() {
        let mut h = Harness::new();
        let events = h.tap();
        h.feed_all(&sf(OTHER, 0x80, 40));
        h.feed_all(&sf(OTHER, 0x80, 60)[..9]);
        h.feed_all(&sf(SOURCE, CIPHER, 100));
        let played = h.played();
        let mut want = run_of(40, 18);
        want.extend(run_of(60, 9));
        assert_eq!(played, want, "none of B's records 9-13 play");
        assert!(h.encrypted());
        assert_eq!(h.last_heard_id(), Some(SOURCE));
        assert!(
            !events.try_iter().any(|e| matches!(
                e,
                FrameEvent::Frame { bytes, .. } if bytes[0] >= 100
            )),
            "no encrypted frame was recorded"
        );
    }

    /// The same, with a clear B: B is a new over, held until its own ALGID,
    /// then played whole.
    #[test]
    fn a_jump_to_record_0_after_an_ldu1_opens_the_new_talkers_over() {
        let mut h = Harness::new();
        h.feed_all(&sf(OTHER, 0x80, 40));
        h.feed_all(&sf(OTHER, 0x80, 60)[..9]);
        h.feed_all(&sf(SOURCE, 0x80, 1));
        let mut want = run_of(40, 18);
        want.extend(run_of(60, 9));
        want.extend(run_of(1, 18));
        assert_eq!(h.played(), want);
        assert_eq!(h.resets.load(Ordering::Relaxed), 1, "two overs");
    }

    /// Jitter inside a burst is not a new LDU1: record 0 arriving just
    /// after record 1 is late and dropped, as before, and the over plays
    /// on with no hold.
    #[test]
    fn record_0_swapped_with_record_1_is_still_just_late() {
        let mut h = Harness::new();
        h.feed_all(&sf(SOURCE, 0x80, 1));
        let next = sf(SOURCE, 0x80, 40);
        h.feed(&next[1]);
        h.feed(&next[0]);
        h.feed_all(&next[2..]);
        let mut want = run_of(1, 18);
        want.push(18);
        want.extend(run_of(41, 17));
        assert_eq!(h.played(), want);
        assert_eq!(h.resets.load(Ordering::Relaxed), 0);
    }

    /// The same talker keying a new LDU1 straight after an LDU1 (its end
    /// record lost) stays one over: the talker check finds the same radio.
    #[test]
    fn a_jump_to_record_0_from_the_same_talker_keeps_playing() {
        let mut h = Harness::new();
        h.feed_all(&sf(SOURCE, 0x80, 1));
        h.feed_all(&sf(SOURCE, 0x80, 40)[..9]);
        h.feed_all(&sf(SOURCE, 0x80, 60));
        let mut want = run_of(1, 18);
        want.extend(run_of(40, 9));
        want.extend(run_of(60, 18));
        assert_eq!(h.played(), want);
        assert_eq!(h.resets.load(Ordering::Relaxed), 0);
    }

    /// A cipher ALGID mutes before its own record is routed: in an over
    /// already playing clear, record 14 of the encrypted superframe is not
    /// played. Records 0–13 of it are, which is the documented residual
    /// for the same talker with no gap (14 frames).
    #[test]
    fn a_cipher_algid_mutes_its_own_record() {
        let mut h = Harness::new();
        h.feed_all(&sf(SOURCE, 0x80, 1));
        h.feed_all(&sf(SOURCE, CIPHER, 100));
        let played = h.played();
        let mut want = run_of(1, 18);
        want.extend(run_of(100, 14));
        assert_eq!(played, want, "record 14 (value 114) never plays");
        assert!(h.encrypted());
    }

    /// Review Focus 2: linking mid-over, the first records seen are LDU2's.
    #[test]
    fn joining_mid_over_holds_audio_until_the_algid() {
        let mut h = Harness::new();
        let records = sf(SOURCE, 0x80, 1);
        for r in &records[9..14] {
            h.feed(r);
            assert_eq!(h.decodes(), 0, "held until 0x70");
            assert_eq!(h.out.try_iter().count(), 0);
        }
        h.feed_all(&records[14..]);
        assert_eq!(h.played(), run_of(10, 9), "indices 9..17");
        assert_eq!(h.last_heard_id(), None, "no 0x66 seen yet");
    }

    /// Review Focus 2, encrypted: nothing ever plays.
    #[test]
    fn joining_an_encrypted_over_mid_way_plays_nothing() {
        let mut h = Harness::new();
        let first = sf(SOURCE, CIPHER, 1);
        h.feed_all(&first[12..]);
        assert!(h.encrypted());
        assert_eq!(h.last_heard_id(), None, "no talker until a 0x66");
        let next = sf(SOURCE, CIPHER, 30);
        h.feed_all(&next[..4]);
        assert_eq!(h.last_heard_id(), None);
        h.feed(&next[4]);
        assert_eq!(h.last_heard_id(), Some(SOURCE));
        h.feed_all(&next[5..]);
        assert!(h.encrypted());
        assert_eq!(h.played(), Vec::<i16>::new());
        assert_eq!(h.decodes(), 0);
    }

    /// Review Focus 3: a datagram from anywhere but the reflector is
    /// dropped before it is parsed.
    #[test]
    fn a_datagram_from_another_address_is_ignored() {
        let (lane, out, _mic) = test_lane();
        let (imbe, _, _) = fake_imbe();
        let (refl, link_addr, link) = fake_reflector(Some((lane, imbe)));
        let stranger = UdpSocket::bind("127.0.0.1:0").expect("bind stranger");
        for r in sf(OTHER, 0x80, 1) {
            stranger.send_to(&r, link_addr).expect("send");
            thread::sleep(Duration::from_millis(2));
        }
        stranger
            .send_to(&wire::end_record(), link_addr)
            .expect("send");
        thread::sleep(Duration::from_millis(300));
        let snap = link.snapshot();
        assert_eq!(snap.last_heard_id, None);
        assert_eq!(snap.frames_rx, 0);
        assert!(!snap.receiving);
        assert_eq!(out.try_iter().count(), 0, "no RxFrame");

        // The control: the same records from the reflector do play.
        for r in sf(SOURCE, 0x80, 1) {
            refl.send_to(&r, link_addr).expect("send");
            thread::sleep(Duration::from_millis(2));
        }
        refl.send_to(&wire::end_record(), link_addr).expect("send");
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut frames = 0;
        while frames < 18 && Instant::now() < deadline {
            if out.recv_timeout(Duration::from_millis(100)).is_ok() {
                frames += 1;
            }
        }
        assert_eq!(frames, 18);
        assert_eq!(link.snapshot().last_heard_id, Some(SOURCE));
        link.disconnect();
    }

    // ── Transmit, which does not exist ──────────────────────────────────

    #[test]
    fn set_ptt_is_refused_until_transmit_lands() {
        let (_refl, _addr, link) = fake_reflector(None);
        link.set_ptt(true);
        thread::sleep(Duration::from_millis(300));
        assert!(!link.snapshot().ptt, "a request is not a transmission");
        link.disconnect();

        // With audio, the request is cleared rather than left latched.
        let mut h = Harness::new();
        h.shared.ptt_request.store(true, Ordering::Relaxed);
        run_ptt_step(h.rx.audio.as_mut().expect("audio"), &h.shared);
        assert!(!h.shared.ptt.load(Ordering::Relaxed));
        assert!(!h.shared.ptt_request.load(Ordering::Relaxed));
    }

    #[test]
    fn an_unkeyed_pass_drops_whatever_the_lane_captured() {
        let mut h = Harness::new();
        for _ in 0..10 {
            h.mic.send(vec![9; 160]).expect("queue a mic frame");
        }
        run_ptt_step(h.rx.audio.as_mut().expect("audio"), &h.shared);
        assert!(
            h.rx.audio
                .as_ref()
                .expect("audio")
                .bus
                .tx_frames
                .try_recv()
                .is_err(),
            "an unkeyed pass empties the capture channel"
        );
    }

    /// And the safety property: with a key held down and speech in the
    /// lane, the link puts nothing on the wire but polls and, at the end,
    /// exactly three unlinks.
    #[test]
    fn disconnect_sends_three_unlinks() {
        let (lane, _out, mic) = test_lane();
        let (imbe, _, _) = fake_imbe();
        let (refl, _addr, link) = fake_reflector(Some((lane, imbe)));
        refl.set_read_timeout(Some(Duration::from_millis(200)))
            .expect("timeout");
        link.set_ptt(true);
        for _ in 0..50 {
            mic.send(vec![9; 160]).expect("queue");
        }
        thread::sleep(Duration::from_millis(100));
        link.disconnect();
        let mut buf = [0u8; 64];
        let mut unlinks = 0;
        while let Ok((n, _)) = refl.recv_from(&mut buf) {
            match wire::parse(&buf[..n]) {
                Some(wire::Packet::Unlink(c)) => {
                    assert_eq!(c.to_trimmed_string(), "KC0ABC");
                    unlinks += 1;
                }
                Some(wire::Packet::Poll(_)) => {}
                other => panic!("the link put this on the wire: {other:?}"),
            }
        }
        assert_eq!(unlinks, 3);
    }
}
