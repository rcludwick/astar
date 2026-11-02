// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! [`M17Session`] and every transmit symbol of the M17 client: `TxState`,
//! [`apply_ptt_edge`], [`drain_tx_frames`], [`send_voice_packet`] and
//! [`send_disc_flushing_eos_if_keyed`]. They live here and only here, so
//! the rest of the `m17` module (the shared receive helpers in `super::rx`
//! and anything built on them) has no way to put a voice-stream packet on
//! the wire. See [`crate::m17`] for the architecture.

use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

use astar_audio::CallAudio;
use astar_codec::codec2::Codec2Voice;
use astar_m17::{
    BROADCAST, ControlPacket, FsmAction, Lsf, SessionFsm, StreamPacket, decode_callsign,
    encode_callsign,
};
use astar_record::frames::{Dir, FrameEvent};
use astar_record::tap::{StartedOn, TapSlot};

use super::rx::{RxTrack, connect_udp_socket, recv_step};
use super::{M17Config, M17SnapshotState, SharedState, link_to_u8, rec_frame, rec_start};
use crate::heard::HeardEntry;
use crate::session::ConsoleError;

/// A full-transceive M17 reflector client: connects, keys/unkeys transmit,
/// and decodes received voice — see [`crate::m17`] for the architecture.
pub struct M17Session {
    /// `Some` until [`M17Session::disconnect`] (or `Drop`) joins it.
    thread: Option<JoinHandle<()>>,
    /// Set to request the run-loop thread send `DISC` and exit. The 50 ms
    /// socket read timeout bounds how long a join can take.
    shutdown: Arc<AtomicBool>,
    /// The last [`M17Session::set_ptt`] request; the run-loop applies it (and
    /// does TX stream bookkeeping) on its next poll.
    ptt_request: Arc<AtomicBool>,
    shared: Arc<SharedState>,
}

impl M17Session {
    /// Connect to an M17 reflector: validates the callsign, opens a Codec 2
    /// instance, binds a UDP socket, and starts the "iax-m17" run-loop
    /// thread, which sends the initial `CONN`.
    ///
    /// `audio` is the one audio lane `ConsoleSession` already opened on the
    /// station's router (`crate::voice_route`): the session encodes whatever
    /// arrives on `audio.tx_frames` while keyed and plays what it decodes
    /// onto `audio.rx_frames`. It builds no router, resolves no device,
    /// carries no preference, and never touches the PTT gate — see
    /// [`crate::m17`]'s docs.
    ///
    /// # Errors
    /// [`ConsoleError::Device`] for an invalid callsign or a missing Codec 2
    /// backend; [`ConsoleError::Resolve`] if `cfg.host`/`cfg.port` don't
    /// resolve, the socket can't be bound, or the run-loop thread can't be
    /// spawned.
    // `cfg` is taken by value per the Task 3 interface contract (matches
    // `ConsoleSession::connect`'s own `ConsoleConfig`-by-value shape); every
    // field is read out (cloned/copied/borrowed) rather than moved, which is
    // why clippy would otherwise suggest a reference here.
    #[allow(clippy::needless_pass_by_value)]
    pub fn connect(cfg: M17Config, audio: CallAudio) -> Result<M17Session, ConsoleError> {
        let callsign = encode_callsign(&cfg.callsign).ok_or_else(|| {
            ConsoleError::Device(format!("invalid M17 callsign {:?}", cfg.callsign))
        })?;

        let (codec, _backend) =
            astar_codec::codec2::open_codec2(&cfg.codec_dirs).ok_or_else(|| {
                ConsoleError::Device(
                    "codec2 unavailable: no runtime libcodec2 found and codec2-static not enabled"
                        .to_string(),
                )
            })?;

        let socket = connect_udp_socket(&cfg.host, cfg.port)?;

        let fsm = SessionFsm::with_keepalive_timeout(callsign, cfg.module, cfg.keepalive_timeout);

        let shutdown = Arc::new(AtomicBool::new(false));
        let ptt_request = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(SharedState::new());

        let thread_shutdown = Arc::clone(&shutdown);
        let thread_ptt_request = Arc::clone(&ptt_request);
        let thread_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("iax-m17".to_string())
            .spawn(move || {
                run_loop(RunLoopParams {
                    socket,
                    fsm,
                    call_audio: audio,
                    codec,
                    callsign,
                    shutdown: thread_shutdown,
                    ptt_request: thread_ptt_request,
                    shared: thread_shared,
                });
            })
            .map_err(|e| ConsoleError::Resolve {
                node: cfg.host.clone(),
                source: e,
            })?;

        Ok(M17Session {
            thread: Some(handle),
            shutdown,
            ptt_request,
            shared,
        })
    }

    /// Engage/release transmit. M17 carries PTT purely via stream
    /// start/`EOS` (there is no protocol PTT frame): the run-loop starts a
    /// fresh random `StreamID` on the next key-down edge it observes, and
    /// flushes a final `EOS`-marked packet on key-up. Applied by the
    /// run-loop on its next poll (bounded by
    /// [`SOCKET_POLL_TIMEOUT`](super::rx::SOCKET_POLL_TIMEOUT), ~50 ms)
    /// — this call itself never blocks.
    pub fn set_ptt(&mut self, on: bool) {
        self.ptt_request.store(on, Ordering::Relaxed);
    }

    /// A poll-cheap snapshot of the session's current state.
    #[must_use]
    pub fn state(&self) -> M17SnapshotState {
        self.shared.snapshot()
    }

    /// Who has keyed up on this link, newest first — see
    /// [`crate::heard::HeardLog`]. Attacker-supplied text, like
    /// [`M17SnapshotState::talker`].
    #[must_use]
    pub fn heard(&self) -> Vec<HeardEntry> {
        self.shared.talkers.heard()
    }

    /// The codec-frame recording tap: install a [`astar_record::Tap`] here
    /// and the run loop offers every received and transmitted over to it as
    /// [`FrameEvent`]s — each 8-byte Codec 2 frame as it comes out of the
    /// encoder or goes into the decoder, then the 160 samples paired with
    /// it. The slot is shared: setting it on the returned handle reaches the
    /// live session.
    #[must_use]
    pub fn frame_tap(&self) -> TapSlot<FrameEvent> {
        self.shared.frame_tap.clone()
    }

    /// Disconnect: requests the run-loop send `DISC` and exit, then joins the
    /// thread. The 50 ms socket read timeout bounds the join.
    pub fn disconnect(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for M17Session {
    fn drop(&mut self) {
        // Defensive: a session dropped without an explicit `disconnect()`
        // call still shuts its thread down cleanly (same DISC-then-join path)
        // rather than leaking it. A no-op after `disconnect()` already ran
        // (thread is `None` by then).
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Everything [`run_loop`] needs, bundled to keep the spawn call's arity sane
/// (`clippy::too_many_arguments`).
struct RunLoopParams {
    socket: UdpSocket,
    fsm: SessionFsm,
    /// The lane's two channel ends. Its `preroll_lead` cell is carried but
    /// never read: it tells a media-clock ladder how many frames the VOX
    /// pre-roll flush put ahead of the live stream, and M17 has no ladder —
    /// its frame counter is a plain per-transmission sequence.
    call_audio: CallAudio,
    codec: Box<dyn Codec2Voice>,
    callsign: [u8; 6],
    shutdown: Arc<AtomicBool>,
    ptt_request: Arc<AtomicBool>,
    shared: Arc<SharedState>,
}

/// Per-transmission TX bookkeeping: the current random `StreamID`, the
/// running frame counter (low 15 bits; restarts at 0 on every key-down), and
/// any half-paired 160-sample frame awaiting its partner before a full
/// 54-byte packet can be built.
struct TxState {
    stream_id: u16,
    frame_no: u16,
    pending: Option<[i16; 160]>,
    /// The transmission's recording while keyed: whether its `Start`
    /// reached the tap installed now. Until it has, the next sent frame
    /// offers it again, so Record pressed mid-over records the rest.
    rec: Option<StartedOn>,
}

impl TxState {
    fn new() -> Self {
        Self {
            stream_id: 0,
            frame_no: 0,
            pending: None,
            rec: None,
        }
    }

    /// Key-down edge: fresh random `StreamID`, counter restarts at 0.
    ///
    /// It must NOT drain `call_audio.tx_frames`. The gate belongs to
    /// `ConsoleSession::set_ptt` now, and it opens up to one
    /// [`SOCKET_POLL_TIMEOUT`](super::rx::SOCKET_POLL_TIMEOUT) tick BEFORE this
    /// edge is observed — so by the
    /// time this runs the channel holds the VOX pre-roll ring (flushed by the
    /// mic lane's own false→true gate edge, iax-2733) and the speech onset
    /// that followed it. Draining here would throw both away and silently
    /// turn pre-roll off for M17.
    ///
    /// Nothing STALE can be in the channel either, which is what makes the
    /// no-drain safe: the lane emits nothing while the gate is closed, and
    /// [`run_loop`] drains and drops every tick it is not transmitting, so
    /// anything present at this edge arrived after the gate opened and is
    /// legitimate audio for this transmission.
    fn key_down(&mut self) {
        self.stream_id = rand::random();
        self.frame_no = 0;
        self.pending = None;
    }

    /// Consume the current frame number for a just-completed pair and
    /// advance the counter, masking off [`StreamPacket::EOS_BIT`] so the
    /// counter itself can never collide with it.
    fn next_frame_no(&mut self) -> u16 {
        let n = self.frame_no;
        self.frame_no = self.frame_no.wrapping_add(1) & !StreamPacket::EOS_BIT;
        n
    }
}

/// The "iax-m17" run-loop: the ONE thread that owns the socket, the
/// [`CallAudio`] channel ends and the [`Codec2Voice`] instance for this
/// session. Single poll cadence (the socket's 50 ms read timeout) drives
/// everything: PTT edges, TX framing, RX decode, and the FSM's keepalive
/// tick. No router, no gate, no meters — those are the lane's, and
/// `ConsoleSession` reads and drives them there.
fn run_loop(p: RunLoopParams) {
    let RunLoopParams {
        socket,
        mut fsm,
        call_audio,
        mut codec,
        callsign,
        shutdown,
        ptt_request,
        shared,
    } = p;

    let now = Instant::now();
    let conn_bytes = fsm.connect(now);
    let _ = socket.send(&conn_bytes);
    shared
        .link
        .store(link_to_u8(fsm.state()), Ordering::Relaxed);

    let mut keyed = false;
    let mut tx = TxState::new();
    // The 400 ms silence clock and the stream the last-heard callsign was
    // taken from; see `RxTrack`.
    let mut rx = RxTrack::with_tap(shared.frame_tap.clone());
    let mut buf = [0u8; 2_048];

    loop {
        if shutdown.load(Ordering::Relaxed) {
            send_disc_flushing_eos_if_keyed(
                &socket,
                callsign,
                codec.as_mut(),
                &mut tx,
                &shared,
                &call_audio,
                keyed,
            );
            // A received over still open when the link goes closes here;
            // frames that never arrived are simply not in it.
            rx.close_recording();
            break;
        }

        // 1. Apply a pending PTT edge (set_ptt only requests; this is where
        //    it actually takes effect).
        let want_key = ptt_request.load(Ordering::Relaxed);
        if want_key != keyed {
            keyed = apply_ptt_edge(
                &socket,
                callsign,
                codec.as_mut(),
                &mut tx,
                &shared,
                &call_audio,
                want_key,
            );
        }

        // 2. Drain any ready TX frames, pairing two 160-sample frames per
        //    54-byte stream packet.
        //
        //    While NOT transmitting, drain and DROP instead — this is the
        //    ONLY discard on the TX path; `TxState::key_down` deliberately
        //    keeps what it finds. The gate is `ConsoleSession::set_ptt`'s and
        //    it can be open while this loop is not keyed: it stays open after
        //    a run-loop-forced unkey (link lost) until the operator releases
        //    PTT, and without this the lane would pile audio into
        //    `tx_frames` unbounded and the next transmission would open with
        //    somebody's stale speech.
        //
        //    It cannot eat the pre-roll: `set_ptt` stores the PTT request
        //    within nanoseconds of opening the gate, whereas the lane's
        //    flush waits for its next capture callback (up to a frame, ~20
        //    ms) — so any tick that can see flushed frames has already read
        //    `want_key` as true and taken the `if` arm.
        if keyed {
            drain_tx_frames(
                &socket,
                callsign,
                codec.as_mut(),
                &mut tx,
                &shared,
                &call_audio,
            );
        } else {
            while call_audio.tx_frames.try_recv().is_ok() {}
        }

        // 3. Socket poll (bounded by SOCKET_POLL_TIMEOUT). `recv_step` only
        //    reads and reports; every send happens here, on this loop's own
        //    socket.
        let step = recv_step(|b| socket.recv(b), &mut buf, &mut fsm);
        if let Some(reply) = step.reply {
            let _ = socket.send(&reply);
        }
        if step.ended.is_some() {
            // NACK or DISC: the FSM already holds Failed. Publish it now
            // rather than after the tick below; the value is the same, since
            // the tick leaves a failed FSM alone.
            shared
                .link
                .store(link_to_u8(fsm.state()), Ordering::Relaxed);
        }
        if let Some(pkt) = step.voice {
            rx.on_voice(&pkt, Instant::now(), &shared.receiving, &shared.talkers);
            decode_and_forward(codec.as_mut(), &mut rx, &pkt, &call_audio);
        }

        // 4. Keepalive tick (answers PING with PONG via FsmAction::Send;
        //    declares Failed after the configured silence window).
        if let FsmAction::Send(bytes) = fsm.tick(Instant::now()) {
            let _ = socket.send(&bytes);
        }
        shared
            .link
            .store(link_to_u8(fsm.state()), Ordering::Relaxed);

        // 5. RX silence timeout: no voice-stream packet in 400 ms clears
        //    `receiving` and, for an over whose EOS was lost, refreshes the
        //    talker's heard clock (see `RxTrack::expire`).
        rx.expire(Instant::now(), &shared.receiving, &shared.talkers);
    }
    // `socket` and the `CallAudio` channel ends drop here; the lane's streams
    // belong to the station router and outlive this thread.
}

/// Run-loop step 1: apply a pending PTT edge. M17 carries PTT purely via
/// stream start/`EOS` (there is no protocol PTT frame). The lane's gate is
/// NOT touched here — `ConsoleSession::set_ptt` opened it before forwarding
/// the key and closes it on key-up; this is the protocol edge only.
///
/// Key-down: starts a fresh random `StreamID` and leaves whatever the lane
/// already queued in place — that is the VOX pre-roll and the speech onset,
/// and it is encoded first (see [`TxState::key_down`]).
///
/// Key-up: drains whatever the lane already queued into ordinary packets
/// ([`drain_tx_frames`]) — up to one
/// [`SOCKET_POLL_TIMEOUT`](super::rx::SOCKET_POLL_TIMEOUT) tick's worth of
/// audio the run-loop hadn't gotten to yet — and only THEN flushes the
/// final `EOS`-marked packet (the one pending half-frame, if any, from that
/// drain, zero-padded, or an all-zero payload if nothing was left). Getting
/// this order backwards is exactly what clips the tail of a transmission.
///
/// Returns the newly-applied keyed state (mirrors `want_key`; only called
/// when it differs from the previous poll's).
#[allow(clippy::too_many_arguments)]
fn apply_ptt_edge(
    socket: &UdpSocket,
    callsign: [u8; 6],
    codec: &mut dyn Codec2Voice,
    tx: &mut TxState,
    shared: &SharedState,
    call_audio: &CallAudio,
    want_key: bool,
) -> bool {
    if want_key {
        tx.key_down();
        tx.rec = Some(StartedOn::default());
        ensure_tx_start(&shared.frame_tap, &mut tx.rec, callsign);
    } else {
        drain_tx_frames(socket, callsign, codec, tx, shared, call_audio);
        let last = tx.pending.take();
        let payload = send_voice_packet(
            socket,
            callsign,
            codec,
            tx.stream_id,
            tx.frame_no,
            true,
            last.as_ref(),
            None,
        );
        record_sent(
            &shared.frame_tap,
            (&mut tx.rec, callsign),
            tx.frame_no,
            &payload,
            last.as_ref(),
            None,
        );
        if tx.rec.take().is_some_and(|r| r.started()) {
            shared
                .frame_tap
                .offer_with(|| FrameEvent::End { dir: Dir::Tx });
        }
    }
    shared.ptt.store(want_key, Ordering::Relaxed);
    want_key
}

/// Run-loop shutdown branch (iax-f2b8-fix Fix 5): if still `keyed`, flush the
/// SAME EOS-marked packet a normal unkey would — reusing [`apply_ptt_edge`]'s
/// unkey path — BEFORE sending `DISC`. Without this, disconnecting while
/// keyed left the far end's stream open (no EOS bit ever seen) until ITS OWN
/// silence timeout closed it out.
fn send_disc_flushing_eos_if_keyed(
    socket: &UdpSocket,
    callsign: [u8; 6],
    codec: &mut dyn Codec2Voice,
    tx: &mut TxState,
    shared: &SharedState,
    call_audio: &CallAudio,
    keyed: bool,
) {
    if keyed {
        let _ = apply_ptt_edge(socket, callsign, codec, tx, shared, call_audio, false);
    }
    let disc = ControlPacket::Disc {
        callsign: Some(callsign),
    }
    .to_bytes();
    let _ = socket.send(&disc);
}

/// Run-loop step 2: drain any TX frames ready in `call_audio`, pairing two
/// 160-sample frames per 54-byte voice-stream packet. Only called while
/// keyed; the mic lane's gate stops forwarding frames the instant it's
/// unkeyed, so this simply drains whatever was already buffered and returns
/// (an inner `loop`/`break`, not a `while` on an outer flag, since nothing
/// here needs to re-check the keyed state itself).
fn drain_tx_frames(
    socket: &UdpSocket,
    callsign: [u8; 6],
    codec: &mut dyn Codec2Voice,
    tx: &mut TxState,
    shared: &SharedState,
    call_audio: &CallAudio,
) {
    while let Ok(frame) = call_audio.tx_frames.try_recv() {
        let Some(pcm) = frame_to_array(&frame) else {
            continue; // defensive: the route's 8 kHz edge guarantees len 160
        };
        if let Some(first) = tx.pending.take() {
            let frame_no = tx.next_frame_no();
            let payload = send_voice_packet(
                socket,
                callsign,
                codec,
                tx.stream_id,
                frame_no,
                false,
                Some(&first),
                Some(&pcm),
            );
            record_sent(
                &shared.frame_tap,
                (&mut tx.rec, callsign),
                frame_no,
                &payload,
                Some(&first),
                Some(&pcm),
            );
        } else {
            tx.pending = Some(pcm);
        }
    }
}

/// Decode one received [`StreamPacket`] (two Codec 2 mode-3200 chunks, via
/// [`RxTrack::decode`], which also records them) and forward the 2×160 PCM
/// samples to `call_audio.rx_frames`. A pure in-order pass-through — see the
/// [`crate::m17`] docs' "RX jitter handling" section for why no reorder
/// buffer is used yet.
fn decode_and_forward(
    codec: &mut dyn Codec2Voice,
    rx: &mut RxTrack,
    pkt: &StreamPacket,
    call_audio: &CallAudio,
) {
    let frames = rx.decode(&mut |bits: &[u8; 8]| codec.decode(bits), pkt);
    // Codec 2 frames arrive decoded on the protocol's own tick and carry no
    // sender clock, so they play straight through (iax-rxjb).
    for pcm in frames {
        let _ = call_audio.rx_frames.send(pcm.to_vec().into());
    }
}

/// Build and send one voice-stream packet. `a`/`b` are the two 160-sample
/// halves; `None` for a half means "send zero bytes for this half" —
/// literal silence, not a Codec 2-encoded silent frame — which is exactly
/// what the key-up flush needs for a fully- or partially-empty final packet
/// (see [`crate::m17`]'s docs and the Task 3 design brief: "zero-pad
/// the second half if only one frame is pending; if zero frames pending,
/// send an EOS packet with all-zero payload").
///
/// Returns the 16-byte payload it built, for the recording.
#[allow(clippy::too_many_arguments)]
fn send_voice_packet(
    socket: &UdpSocket,
    callsign: [u8; 6],
    codec: &mut dyn Codec2Voice,
    stream_id: u16,
    frame_number: u16,
    eos: bool,
    a: Option<&[i16; 160]>,
    b: Option<&[i16; 160]>,
) -> [u8; 16] {
    let bits_a = a.map_or([0u8; 8], |pcm| codec.encode(pcm));
    let bits_b = b.map_or([0u8; 8], |pcm| codec.encode(pcm));
    let mut payload = [0u8; 16];
    payload[0..8].copy_from_slice(&bits_a);
    payload[8..16].copy_from_slice(&bits_b);
    let mut fnum = frame_number & !StreamPacket::EOS_BIT;
    if eos {
        fnum |= StreamPacket::EOS_BIT;
    }
    let pkt = StreamPacket {
        stream_id,
        lsf: Lsf {
            // BROADCAST dst: mirrors astar_m17's own FSM test fixtures —
            // voice frames relayed through a reflector module go to every
            // listener on that module, not a single peer address.
            dst: BROADCAST,
            src: callsign,
            type_field: Lsf::TYPE_VOICE_3200_STREAM,
            meta: [0; 14],
        },
        frame_number: fnum,
        payload,
    };
    let _ = socket.send(&pkt.to_bytes());
    payload
}

/// Record one sent packet: for each half that carried audio, its encoded
/// `Frame` and then the `Pcm` it was encoded from. A `None` half is the
/// EOS flush's zero padding — no audio went into it, so nothing is recorded
/// for it. Codec 2 encodes synchronously, so every frame is in hand with its
/// input already.
fn record_sent(
    tap: &TapSlot<FrameEvent>,
    (rec, callsign): (&mut Option<StartedOn>, [u8; 6]),
    frame_number: u16,
    payload: &[u8; 16],
    a: Option<&[i16; 160]>,
    b: Option<&[i16; 160]>,
) {
    for (half, pcm) in payload.as_chunks::<8>().0.iter().zip([a, b]) {
        let Some(pcm) = pcm else {
            continue;
        };
        ensure_tx_start(tap, rec, callsign);
        tap.offer_with(|| rec_frame(Dir::Tx, half, frame_number));
        tap.offer_with(|| FrameEvent::Pcm {
            dir: Dir::Tx,
            pcm: *pcm,
        });
    }
}

/// Offer the open transmission's `Start` if it has not reached the tap
/// installed now (a lock-free read once it has). Nothing while unkeyed.
fn ensure_tx_start(tap: &TapSlot<FrameEvent>, rec: &mut Option<StartedOn>, callsign: [u8; 6]) {
    if let Some(r) = rec.as_mut() {
        r.ensure(tap, || rec_start(Dir::Tx, Some(decode_callsign(&callsign))));
    }
}

/// Convert one TX frame (`StreamConfig::default()` guarantees 160 i16
/// samples per frame while keyed) into the fixed-size array Codec 2 wants.
/// Returns `None` on an unexpected length (defensive only — should not
/// happen given the router's frame chunking).
fn frame_to_array(v: &[i16]) -> Option<[i16; 160]> {
    if v.len() != 160 {
        return None;
    }
    let mut a = [0i16; 160];
    a.copy_from_slice(v);
    Some(a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::time::Duration;

    /// Build a [`CallAudio`] by hand — no `AudioRouter`/`MicLane` involved —
    /// so a test can push raw frames straight onto the exact channel
    /// `TxState`/`apply_ptt_edge`/`drain_tx_frames` read from. Returns the
    /// `CallAudio` plus the `Sender` half of `tx_frames` (the test's stand-in
    /// for "the mic lane already queued this") and the `Receiver` half of
    /// `rx_frames` (unused by the TX-side tests below, but part of the real
    /// struct).
    fn fake_call_audio() -> (CallAudio, Sender<Vec<i16>>, Receiver<astar_audio::RxFrame>) {
        let (tx_tx, tx_rx) = channel::<Vec<i16>>();
        let (rx_tx, rx_rx) = channel::<astar_audio::RxFrame>();
        let call_audio = CallAudio {
            tx_frames: tx_rx,
            rx_frames: rx_tx,
            preroll_lead: Arc::new(AtomicU32::new(0)),
        };
        (call_audio, tx_tx, rx_rx)
    }

    /// Bind a connected sender plus the socket that receives from it, with a
    /// read timeout so a missing packet fails the test instead of hanging.
    fn loopback_pair() -> (UdpSocket, UdpSocket) {
        let recv = UdpSocket::bind("127.0.0.1:0").expect("bind recv socket");
        recv.set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        let recv_addr = recv.local_addr().expect("recv addr");
        let send = UdpSocket::bind("127.0.0.1:0").expect("bind send socket");
        send.connect(recv_addr).expect("connect send socket");
        (recv, send)
    }

    #[test]
    fn key_down_keeps_the_preroll_the_gate_already_flushed() {
        // The regression this pins: `ConsoleSession::set_ptt` opens the mic
        // lane's gate up to one poll tick BEFORE this run loop observes the
        // key-down edge, and that false->true gate edge flushes the whole VOX
        // look-back ring into `tx_frames` (iax-2733). If `key_down` drained
        // the channel, that pre-roll and the speech onset behind it would be
        // thrown away and pre-roll would be silently off for M17.
        let (recv_sock, send_sock) = loopback_pair();

        let (call_audio, push, _rx) = fake_call_audio();
        // Two frames = one complete 54-byte voice packet once paired.
        push.send(vec![10_i16; 160]).unwrap();
        push.send(vec![20_i16; 160]).unwrap();

        let (mut codec, _backend) = astar_codec::codec2::open_codec2(&[])
            .expect("a codec must be available under this crate's dev-dependency codec2-static");
        let mut tx = TxState::new();
        let shared = SharedState::new();

        let keyed = apply_ptt_edge(
            &send_sock,
            [0; 6],
            codec.as_mut(),
            &mut tx,
            &shared,
            &call_audio,
            true, // key-down edge
        );
        assert!(keyed);
        // Run-loop step 2 while keyed: exactly what the loop does next.
        drain_tx_frames(
            &send_sock,
            [0; 6],
            codec.as_mut(),
            &mut tx,
            &shared,
            &call_audio,
        );

        let mut buf = [0u8; 128];
        let (n, _) = recv_sock
            .recv_from(&mut buf)
            .expect("the frames queued before the key-down edge must be encoded and sent");
        let pkt = StreamPacket::parse(&buf[..n]).expect("valid stream packet");
        assert_eq!(
            pkt.stream_id, tx.stream_id,
            "sent under this transmission's id"
        );
        assert!(!pkt.is_last(), "an ordinary packet, not the EOS flush");
        assert_eq!(pkt.frame_number, 0, "the pre-roll opens the transmission");
        assert!(
            call_audio.tx_frames.try_recv().is_err(),
            "both queued frames must have been consumed, none left behind"
        );
    }

    #[test]
    fn unkey_drains_queued_frames_before_the_eos_flush() {
        // Reproduces the reviewed race directly: three frames (a full pair
        // plus one odd frame) are sitting in `tx_frames` — audio the mic
        // lane queued in the ~50ms since the run-loop's last drain — when
        // the unkey edge is observed. The fix must send them as an ordinary
        // (non-EOS) packet BEFORE the EOS-flushed final packet, not drop
        // them and not leak them into the next transmission.
        let (recv_sock, send_sock) = loopback_pair();

        let (call_audio, push, _rx) = fake_call_audio();
        push.send(vec![10_i16; 160]).unwrap();
        push.send(vec![20_i16; 160]).unwrap();
        push.send(vec![30_i16; 160]).unwrap();

        let (mut codec, _backend) = astar_codec::codec2::open_codec2(&[])
            .expect("a codec must be available under this crate's dev-dependency codec2-static");
        let mut tx = TxState::new();
        tx.stream_id = 0xBEEF;
        tx.frame_no = 5;
        let shared = SharedState::new();

        let keyed = apply_ptt_edge(
            &send_sock,
            [0; 6],
            codec.as_mut(),
            &mut tx,
            &shared,
            &call_audio,
            false, // unkey edge
        );
        assert!(!keyed);

        // First packet: the drained pair (frames 10+20), NOT EOS-marked.
        let mut buf = [0u8; 128];
        let (n1, _) = recv_sock
            .recv_from(&mut buf)
            .expect("the drained pair must be sent as an ordinary packet");
        let p1 = StreamPacket::parse(&buf[..n1]).expect("valid stream packet");
        assert_eq!(p1.stream_id, 0xBEEF);
        assert!(
            !p1.is_last(),
            "the drained pair must not itself carry the EOS bit"
        );

        // Second packet: the EOS flush carrying the odd leftover frame (30),
        // zero-padded in its second half.
        let (n2, _) = recv_sock
            .recv_from(&mut buf)
            .expect("the EOS flush must follow the drained pair");
        let p2 = StreamPacket::parse(&buf[..n2]).expect("valid stream packet");
        assert_eq!(p2.stream_id, 0xBEEF);
        assert!(p2.is_last(), "the final flushed packet must carry EOS");

        // Nothing left over: the tail wasn't clipped (all 3 queued frames
        // went out) and nothing leaks into a future transmission.
        assert!(
            call_audio.tx_frames.try_recv().is_err(),
            "every queued frame must have been drained, none left for the next transmission"
        );
        assert!(tx.pending.is_none());
    }

    /// F12: the key-up flush carries one leftover frame and a zero-filled
    /// half. The leftover records as a Frame and its Pcm; the padding, which
    /// no audio went into, records nothing.
    #[test]
    fn the_eos_flush_records_only_the_half_that_carried_audio() {
        let (recv_sock, send_sock) = loopback_pair();
        let (call_audio, push, _rx) = fake_call_audio();
        push.send(vec![30_i16; 160]).unwrap();
        let (mut codec, _backend) = astar_codec::codec2::open_codec2(&[])
            .expect("a codec must be available under this crate's dev-dependency codec2-static");
        let mut tx = TxState::new();
        tx.frame_no = 7;
        let shared = SharedState::new();
        let (tap, events) = astar_record::tap::tap_channel(16);
        shared.frame_tap.set(Some(tap));
        // The transmission's recording is open, its Start on this tap.
        tx.rec = Some(StartedOn::default());
        ensure_tx_start(&shared.frame_tap, &mut tx.rec, [0; 6]);
        assert!(matches!(events.try_recv(), Ok(FrameEvent::Start { .. })));

        assert!(!apply_ptt_edge(
            &send_sock,
            [0; 6],
            codec.as_mut(),
            &mut tx,
            &shared,
            &call_audio,
            false,
        ));
        let mut buf = [0u8; 128];
        let (n, _) = recv_sock.recv_from(&mut buf).expect("the EOS flush");
        let pkt = StreamPacket::parse(&buf[..n]).expect("valid stream packet");
        assert!(pkt.is_last());
        assert_eq!(pkt.payload[8..], [0u8; 8], "the second half is padding");

        let got: Vec<FrameEvent> = events.try_iter().collect();
        assert_eq!(
            got,
            vec![
                FrameEvent::Frame {
                    dir: Dir::Tx,
                    bytes: pkt.payload[..8].to_vec(),
                    seq: Some(astar_record::frames::Seq {
                        value: 7,
                        modulus: 0x8000
                    }),
                },
                FrameEvent::Pcm {
                    dir: Dir::Tx,
                    pcm: [30; 160]
                },
                FrameEvent::End { dir: Dir::Tx },
            ]
        );
    }

    /// Record pressed mid-transmission: the tap appears after key-down,
    /// and the next sent frame opens the recording for the rest of it.
    #[test]
    fn a_tap_set_mid_transmission_records_the_rest_of_it() {
        let (_recv_sock, send_sock) = loopback_pair();
        let (call_audio, push, _rx) = fake_call_audio();
        let (mut codec, _backend) = astar_codec::codec2::open_codec2(&[])
            .expect("a codec must be available under this crate's dev-dependency codec2-static");
        let mut tx = TxState::new();
        let shared = SharedState::new();
        assert!(apply_ptt_edge(
            &send_sock,
            [0; 6],
            codec.as_mut(),
            &mut tx,
            &shared,
            &call_audio,
            true,
        ));
        let (tap, events) = astar_record::tap::tap_channel(16);
        shared.frame_tap.set(Some(tap));
        push.send(vec![10_i16; 160]).unwrap();
        push.send(vec![20_i16; 160]).unwrap();
        drain_tx_frames(
            &send_sock,
            [0; 6],
            codec.as_mut(),
            &mut tx,
            &shared,
            &call_audio,
        );
        assert!(!apply_ptt_edge(
            &send_sock,
            [0; 6],
            codec.as_mut(),
            &mut tx,
            &shared,
            &call_audio,
            false,
        ));
        let kinds: Vec<&str> = events
            .try_iter()
            .map(|e| match e {
                FrameEvent::Start { .. } => "start",
                FrameEvent::Frame { .. } => "frame",
                FrameEvent::Pcm { .. } => "pcm",
                FrameEvent::End { .. } => "end",
            })
            .collect();
        assert_eq!(kinds, ["start", "frame", "pcm", "frame", "pcm", "end"]);
    }

    /// The split moved the PONG send out of the receive helper and into
    /// `run_loop` (`recv_step` only hands the reply back). Pin that the loop
    /// still sends it: without it a real reflector drops the link once its
    /// keepalive window passes. Everything stays on 127.0.0.1.
    #[test]
    fn run_loop_answers_a_reflector_ping_with_pong() {
        let fake = UdpSocket::bind("127.0.0.1:0").expect("bind the fake reflector");
        fake.set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        let port = fake.local_addr().expect("fake reflector addr").port();
        let call = encode_callsign("AJ7HR").expect("a valid callsign encodes");

        let session = M17Session::connect(
            M17Config {
                host: "127.0.0.1".to_string(),
                port,
                module: b'A',
                callsign: "AJ7HR".to_string(),
                codec_dirs: vec![],
                keepalive_timeout: Duration::from_secs(30),
            },
            fake_call_audio().0,
        )
        .expect("connect to the loopback fake reflector");

        let mut buf = [0u8; 64];
        let (n, from) = fake.recv_from(&mut buf).expect("the session sends CONN");
        assert_eq!(
            &buf[..n],
            ControlPacket::Conn {
                callsign: call,
                module: b'A'
            }
            .to_bytes()
            .as_slice()
        );
        fake.send_to(&ControlPacket::Ackn.to_bytes(), from)
            .expect("send ACKN");
        fake.send_to(
            &ControlPacket::Ping {
                callsign: BROADCAST,
            }
            .to_bytes(),
            from,
        )
        .expect("send PING");

        let (n, _) = fake
            .recv_from(&mut buf)
            .expect("the run loop answers PING (a recv timeout here means no PONG went out)");
        assert_eq!(
            &buf[..n],
            ControlPacket::Pong { callsign: call }.to_bytes().as_slice(),
            "a PING must be answered with a PONG carrying the station's callsign"
        );

        session.disconnect();
        let (n, _) = fake.recv_from(&mut buf).expect("disconnect sends DISC");
        assert_eq!(
            &buf[..n],
            ControlPacket::Disc {
                callsign: Some(call)
            }
            .to_bytes()
            .as_slice()
        );
    }
}
