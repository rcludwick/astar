// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The receive half of an M17 reflector link, shared by every M17 run loop
//! in this module.
//!
//! Nothing in this file sends a datagram. [`recv_step`] reads one packet and
//! hands back whatever the state machine wants sent; the caller sends it on
//! its own socket. That split is what lets a receive-only leg reuse this
//! code without inheriting a path to the wire.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use astar_m17::{ControlPacket, FsmAction, SessionFsm, StreamPacket, decode_callsign};
use astar_record::frames::{Dir, FrameEvent};
use astar_record::tap::{StartedOn, TapSlot};

use super::{rec_frame, rec_start};

use crate::heard::{HeardEntry, HeardLog};
use crate::session::ConsoleError;

/// How long the RX path waits, after the last voice-stream packet, before
/// clearing the link's `receiving` flag back to `false`.
pub(crate) const RX_SILENCE_TIMEOUT: Duration = Duration::from_millis(400);

/// The run-loop thread's socket read timeout: also the cadence at which PTT
/// edges, the FSM keepalive tick, and the RX silence timeout are all
/// re-checked. 50 ms per the Task 3 design brief (a plain read-timeout loop,
/// not mio/async).
pub(crate) const SOCKET_POLL_TIMEOUT: Duration = Duration::from_millis(50);

/// Who has been heard on one link: the last-heard callsign and the history.
///
/// Kept apart from a link's atomics so a run loop can record talkers without
/// holding anything else about the link.
#[derive(Debug, Default)]
pub(crate) struct TalkerLog {
    /// The last-heard callsign. A `Mutex<Option<String>>` rather than an
    /// atomic because it is a `String` — the same shape `DstarSession`'s
    /// shared `talker` uses, and touched at most twice per received stream.
    talker: Mutex<Option<String>>,
    /// Everyone heard on this link, newest first — `talker` keeps only the
    /// most recent name, which a short courtesy tail overwrites a second
    /// after the real talker unkeys.
    heard: HeardLog,
}

impl TalkerLog {
    /// An empty log: nobody heard yet.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The last-heard callsign. Poison-tolerant: a writer that panicked left
    /// the last name it stored, which is still the right answer.
    #[must_use]
    pub(crate) fn talker(&self) -> Option<String> {
        self.talker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Everyone heard on this link, newest first.
    #[must_use]
    pub(crate) fn heard(&self) -> Vec<HeardEntry> {
        self.heard.snapshot()
    }

    fn set_talker(&self, call: String) {
        *self
            .talker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(call);
    }
}

/// Record who is talking, at the START and the END of each received stream.
///
/// M17 carries the source address in every packet's LSF, so this could run
/// on all of them; it deliberately does not. The talker only changes when
/// the stream does, and touching the callsign twice per stream keeps a
/// decode and a mutex off the 20 ms RX path.
///
/// A stream's `EOS` packet closes it out here too, so the next voice packet
/// is read as a fresh stream even if the reflector reuses the `StreamID`.
/// The second [`crate::heard::HeardLog::note`] is on that `EOS`, and it is
/// what makes [`crate::heard::HeardEntry::age_ms`] mean the same thing here
/// as on the AMBE links: time since the station was last heard, not time
/// since their over began. That refresh names whoever `talker` already
/// holds rather than re-decoding the `EOS` packet's LSF, so a rewritten or
/// zeroed final header cannot attribute the end of an over to the wrong
/// station. `note` moves an entry already at the front rather than
/// repeating it, so the refresh never duplicates a row.
///
/// An empty decode is ignored rather than stored: `decode_callsign` yields
/// `""` for a null/reserved address, and "Last heard " with nothing after it
/// is worse than no line at all.
pub(crate) fn note_talker(pkt: &StreamPacket, log: &TalkerLog, rx_stream: &mut Option<u16>) {
    // A stream counts as open here only once one of its packets has named a
    // talker. A null/reserved source leaves `rx_stream` unset, so a later
    // packet of the same stream can still name it — and so its EOS has no
    // clock to refresh, because the only name in `talker` would be the
    // PREVIOUS station's. The EOS packet itself never names anybody: its
    // header is the one a reflector may rewrite or zero.
    if *rx_stream != Some(pkt.stream_id) && !pkt.is_last() {
        let call = decode_callsign(&pkt.lsf.src);
        if !call.is_empty() {
            *rx_stream = Some(pkt.stream_id);
            log.heard.note("m17", &call);
            log.set_talker(call);
        }
    }
    if pkt.is_last() && rx_stream.is_some() {
        // Refresh from `talker`, not from this packet's own LSF: the EOS
        // packet is the one frame of an over whose header a reflector may
        // rewrite or zero, and re-decoding it can name somebody the stream
        // never belonged to. `talker` is what the stream's first packet
        // established, which is the station whose age this is refreshing.
        // Same read as the silence-timeout branch.
        let last = log.talker();
        if let Some(call) = last
            && !call.is_empty()
        {
            log.heard.note("m17", &call);
        }
    }
    if pkt.is_last() {
        *rx_stream = None;
    }
}

/// Per-loop receive bookkeeping: the 400 ms silence clock, the stream the
/// last-heard callsign was taken from, and the received over the frame tap
/// is recording.
#[derive(Default)]
pub(crate) struct RxTrack {
    /// When the last voice packet arrived; `None` once the silence timeout
    /// has fired.
    pub(crate) last_voice: Option<Instant>,
    /// Which received stream the last-heard callsign was taken from. `None`
    /// means "the next voice packet opens a new stream", which is also how a
    /// stream that ended (EOS, or the RX silence timeout) leaves it — so a
    /// second transmission that happens to reuse a `StreamID` is still read
    /// as new, and its LSF still names its talker.
    pub(crate) stream: Option<u16>,
    /// The codec-frame recording tap; only ever offered to.
    tap: TapSlot<FrameEvent>,
    /// The stream whose recording is open. Kept apart from `stream`, which
    /// opens only once a packet names a talker: a stream from a null source
    /// is still decoded, so it is still recorded.
    recording: Option<u16>,
    /// Whether that stream's `Start` reached the tap installed now. Until
    /// it has, every packet offers it again, so pressing Record mid-over
    /// records the rest of the over.
    rec_started: StartedOn,
}

impl RxTrack {
    /// Nothing received yet, and nothing recorded.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Nothing received yet; received overs are offered to `tap`.
    #[must_use]
    pub(crate) fn with_tap(tap: TapSlot<FrameEvent>) -> Self {
        Self {
            tap,
            ..Self::default()
        }
    }

    /// Decode `pkt`'s two halves, recording each one's `Frame` and then its
    /// `Pcm`, and close the recording on the end-of-stream bit. Every decode
    /// is recorded, whether or not the caller then manages to play it.
    pub(crate) fn decode(
        &mut self,
        decode: &mut dyn FnMut(&[u8; 8]) -> [i16; 160],
        pkt: &StreamPacket,
    ) -> [[i16; 160]; 2] {
        let frames = decode_pair(decode, pkt);
        for (half, pcm) in pkt.payload.as_chunks::<8>().0.iter().zip(&frames) {
            self.tap
                .offer_with(|| rec_frame(Dir::Rx, half, pkt.frame_number));
            self.tap.offer_with(|| FrameEvent::Pcm {
                dir: Dir::Rx,
                pcm: *pcm,
            });
        }
        if pkt.is_last() {
            self.close_recording();
        }
        frames
    }

    /// Close the open recording, if any: the over ended, or the link did.
    pub(crate) fn close_recording(&mut self) {
        if self.recording.take().is_some() && self.rec_started.started() {
            self.tap.offer_with(|| FrameEvent::End { dir: Dir::Rx });
        }
        self.rec_started = StartedOn::default();
    }

    /// Open a recording for `pkt`'s stream unless it is the one already
    /// open. A new stream id without the old one's EOS closes the old one
    /// first. A stream whose `Start` has not reached the installed tap (no
    /// tap when it began, or a new one since) offers it again here.
    fn open_recording(&mut self, pkt: &StreamPacket) {
        if self.recording != Some(pkt.stream_id) {
            self.close_recording();
            self.recording = Some(pkt.stream_id);
        }
        self.rec_started.ensure(&self.tap, || {
            let call = decode_callsign(&pkt.lsf.src);
            rec_start(Dir::Rx, (!call.is_empty()).then_some(call))
        });
    }

    /// A voice packet arrived at `now`: restart the silence clock, raise
    /// `receiving`, and note the talker.
    pub(crate) fn on_voice(
        &mut self,
        pkt: &StreamPacket,
        now: Instant,
        receiving: &AtomicBool,
        log: &TalkerLog,
    ) {
        self.last_voice = Some(now);
        receiving.store(true, Ordering::Relaxed);
        note_talker(pkt, log, &mut self.stream);
        self.open_recording(pkt);
    }

    /// No voice-stream packet for [`RX_SILENCE_TIMEOUT`] clears `receiving`.
    pub(crate) fn expire(&mut self, now: Instant, receiving: &AtomicBool, log: &TalkerLog) {
        if let Some(t) = self.last_voice
            && now.saturating_duration_since(t) >= RX_SILENCE_TIMEOUT
        {
            receiving.store(false, Ordering::Relaxed);
            self.last_voice = None;
            // The stream is over as far as this client is concerned, so the
            // next voice packet starts a new one. `talker` itself is NOT
            // cleared — it is "last heard", and outliving the stream is the
            // whole point.
            //
            // An over whose `EOS` was lost ends HERE instead, so this is the
            // other place the history's clock is refreshed: without it that
            // station's age would count from the start of their over. A
            // move-to-front, never a duplicate row.
            //
            // The gate is `stream`: `note_talker` clears it on the `EOS`
            // packet, so a stream that ended properly has already been
            // refreshed there and has nothing left to refresh here — this
            // arm would only push its age 400 ms newer than the end it
            // actually had.
            if self.stream.is_some() {
                let last = log.talker();
                if let Some(call) = last {
                    log.heard.note("m17", &call);
                }
            }
            self.stream = None;
            self.close_recording();
        }
    }
}

/// Admitted audio may run at most this far ahead of real time.
pub(crate) const GATE_LEAD: Duration = Duration::from_millis(200);

/// One stream packet carries 40 ms of audio (two 20 ms Codec 2 frames).
pub(crate) const PACKET_SPAN: Duration = Duration::from_millis(40);

/// The stream a [`StreamGate`] is currently letting through.
struct OpenStream {
    id: u16,
    started: Instant,
    admitted: u32,
    last_seen: Instant,
}

/// The flood guard in front of a receive-only leg's decoder: one stream at a
/// time, paced to real time.
///
/// The conference drains whatever a member queues into an unbounded buffer,
/// and anyone on a public reflector module can stream at whatever rate the
/// reflector relays. Without a bound, a faster-than-real-time sender could
/// queue minutes of audio into the node's mix. The pace counts packets this
/// gate has ADMITTED, never the sender's `frame_number`, which is
/// attacker-supplied.
///
/// The schedule also survives stream turnover. Otherwise a sender could set
/// EOS on every packet (or every 7th) and each fresh stream would start its
/// budget from zero, letting the whole flood through.
pub(crate) struct StreamGate {
    open: Option<OpenStream>,
    /// When the audio admitted from the last stream an EOS closed finishes
    /// playing in real time. The next stream starts that far into its
    /// budget. It never exceeds `GATE_LEAD + PACKET_SPAN` (240 ms) past the
    /// closing instant, which is less than [`RX_SILENCE_TIMEOUT`], so 400 ms
    /// of silence always clears it.
    carried_until: Option<Instant>,
}

/// How many packets' worth of audio `ahead` covers, rounded up, so a
/// carried lead is never undercounted.
fn packets_covering(ahead: Duration) -> u32 {
    u32::try_from(ahead.as_nanos().div_ceil(PACKET_SPAN.as_nanos())).unwrap_or(u32::MAX)
}

impl StreamGate {
    /// A gate with no stream open and no lead carried.
    pub(crate) fn new() -> Self {
        Self {
            open: None,
            carried_until: None,
        }
    }

    /// `true` = decode this packet, `false` = drop it (the caller counts
    /// drops). The rules, in order:
    ///
    /// 1. An open stream last seen [`RX_SILENCE_TIMEOUT`] or more before
    ///    `now` counts as closed, and any carried lead is cleared.
    /// 2. With no open stream, this packet opens one with `started = now`
    ///    and `admitted = ceil(ahead / PACKET_SPAN)`, where `ahead` is how
    ///    far the previous stream's admitted audio still runs past `now`
    ///    (0 if none). A new stream id never resets the pace.
    /// 3. A different `stream_id` while one is open is dropped, and nothing
    ///    about the open stream changes.
    /// 4. Packet `k` of the open stream (`k` = packets already admitted)
    ///    passes iff `k × PACKET_SPAN ≤ elapsed + GATE_LEAD`: a late burst
    ///    after a stall is behind schedule and passes; a flood stops 200 ms
    ///    ahead of real time.
    /// 5. The open stream's EOS packet closes it, admitted or not, and
    ///    records how far its admitted audio runs for rule 2.
    pub(crate) fn admit(&mut self, pkt: &StreamPacket, now: Instant) -> bool {
        if self
            .open
            .as_ref()
            .is_some_and(|s| now.saturating_duration_since(s.last_seen) >= RX_SILENCE_TIMEOUT)
        {
            self.open = None;
            self.carried_until = None;
        }
        if self.open.is_none() {
            let ahead = self
                .carried_until
                .map_or(Duration::ZERO, |t| t.saturating_duration_since(now));
            self.open = Some(OpenStream {
                id: pkt.stream_id,
                started: now,
                admitted: packets_covering(ahead),
                last_seen: now,
            });
        }
        let Some(stream) = self.open.as_mut() else {
            return false;
        };
        if stream.id != pkt.stream_id {
            return false;
        }
        stream.last_seen = now;
        let due = PACKET_SPAN.saturating_mul(stream.admitted);
        let ok = due
            <= now
                .saturating_duration_since(stream.started)
                .saturating_add(GATE_LEAD);
        if ok {
            stream.admitted = stream.admitted.saturating_add(1);
        }
        if pkt.is_last() {
            let runs_to = stream
                .started
                .checked_add(PACKET_SPAN.saturating_mul(stream.admitted))
                .unwrap_or(now);
            self.carried_until = Some(runs_to);
            self.open = None;
        }
        ok
    }
}

/// A well-formed Codec 2 3200 voice packet for tests; `src` names the talker.
#[cfg(test)]
pub(crate) fn voice_packet_for_test(stream_id: u16, frame_number: u16, src: &str) -> StreamPacket {
    StreamPacket {
        stream_id,
        lsf: astar_m17::Lsf {
            dst: astar_m17::BROADCAST,
            src: astar_m17::encode_callsign(src).expect("test callsign encodes"),
            type_field: astar_m17::Lsf::TYPE_VOICE_3200_STREAM,
            meta: [0; 14],
        },
        frame_number,
        payload: [0u8; 16],
    }
}

/// [`voice_packet_for_test`] serialised to its 54 wire bytes, so the
/// monitor's tests can hold a stream packet's bytes without serialising one
/// themselves.
#[cfg(test)]
pub(crate) fn voice_bytes_for_test(stream_id: u16, frame_number: u16, src: &str) -> Vec<u8> {
    voice_packet_for_test(stream_id, frame_number, src)
        .to_bytes()
        .to_vec()
}

/// Decode one received [`StreamPacket`]'s 16-byte Codec 2 payload (two 8-byte
/// mode-3200 chunks, first half first) into two 160-sample frames. The
/// decoder is a closure so a caller can hand in whatever owns its codec,
/// and this file never names an encoder.
#[must_use]
pub(crate) fn decode_pair(
    decode: &mut dyn FnMut(&[u8; 8]) -> [i16; 160],
    pkt: &StreamPacket,
) -> [[i16; 160]; 2] {
    let mut bits_a = [0u8; 8];
    bits_a.copy_from_slice(&pkt.payload[0..8]);
    let mut bits_b = [0u8; 8];
    bits_b.copy_from_slice(&pkt.payload[8..16]);
    [decode(&bits_a), decode(&bits_b)]
}

/// Why the reflector ended the link, when a received packet did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RxEnd {
    /// The reflector answered CONN with NACK.
    Refused,
    /// The reflector sent DISC.
    Disconnected,
}

/// What one receive step produced. The caller does ALL sending.
#[derive(Debug, Default)]
pub(crate) struct RxStep {
    /// Bytes the state machine wants sent back (a PONG for a PING).
    pub(crate) reply: Option<Vec<u8>>,
    /// A voice-stream packet received while linked.
    pub(crate) voice: Option<StreamPacket>,
    /// Set when this packet unlinked the leg (NACK or DISC).
    pub(crate) ended: Option<RxEnd>,
}

/// One bounded receive: read a datagram with `recv`, feed it to `fsm`, and
/// report what came of it. It sends nothing; a reply comes back in
/// [`RxStep::reply`] for the caller to send on its own socket. A timeout or
/// any other read error is an empty step.
#[must_use]
pub(crate) fn recv_step(
    recv: impl FnOnce(&mut [u8]) -> std::io::Result<usize>,
    buf: &mut [u8],
    fsm: &mut SessionFsm,
) -> RxStep {
    let Ok(n) = recv(&mut *buf) else {
        return RxStep::default();
    };
    let packet = &buf[..n];
    match fsm.on_packet(packet, Instant::now()) {
        FsmAction::Send(bytes) => RxStep {
            reply: Some(bytes),
            ..RxStep::default()
        },
        FsmAction::Voice(pkt) => RxStep {
            voice: Some(pkt),
            ..RxStep::default()
        },
        FsmAction::Unlinked => RxStep {
            ended: match ControlPacket::parse(packet) {
                Some(ControlPacket::Nack) => Some(RxEnd::Refused),
                Some(ControlPacket::Disc { .. }) => Some(RxEnd::Disconnected),
                _ => None,
            },
            ..RxStep::default()
        },
        FsmAction::None => RxStep::default(),
    }
}

/// Resolves `host:port` and returns a connected, read-timeout-armed
/// [`UdpSocket`] ready for a run loop — the connect half of the
/// iax-m17-localhost fix.
///
/// Name resolution can yield MULTIPLE candidates in resolution order —
/// notably `"localhost"` on macOS, which resolves to
/// `[[::1]:port, 127.0.0.1:port]`. The old code took only the first
/// candidate and bound a v4-only `"0.0.0.0:0"` socket regardless of its
/// family, so an IPv6-first resolution could never connect (an `AF_INET`
/// socket can't `connect()` to an `AF_INET6` peer). This tries every
/// candidate, binding a socket that matches ITS family, and uses the first
/// one that connects.
///
/// NOTE: UDP `connect()` succeeding proves only that the local
/// bind/family/routing-table lookup for that peer succeeded — it's
/// connectionless, so this is not proof the peer is reachable or even
/// listening. That's fine here: family mismatch (the actual bug) is exactly
/// what binding a matching-family socket screens for; genuine
/// unreachability still surfaces later as the FSM never reaching `Linked`.
///
/// When nothing connects, the error says why: the resolver's own error if
/// resolution failed, else the last real bind/connect error, and only when
/// resolution produced no candidates at all a synthesised "could not
/// resolve" (the lossy-error half of iax-c1d5).
///
/// # Errors
/// [`ConsoleError::Resolve`] if `host`/`port` fail to resolve or resolve to
/// no addresses, no candidate can be bound and connected, or the chosen
/// socket's read timeout can't be set.
pub(crate) fn connect_udp_socket(host: &str, port: u16) -> Result<UdpSocket, ConsoleError> {
    connect_udp_socket_with(host, port, |h, p| {
        (h, p).to_socket_addrs().map(Iterator::collect)
    })
}

/// [`connect_udp_socket`] with the resolver injected, so every error path
/// can be tested without real DNS.
///
/// # Errors
/// As [`connect_udp_socket`].
pub(crate) fn connect_udp_socket_with(
    host: &str,
    port: u16,
    resolve: impl FnOnce(&str, u16) -> std::io::Result<Vec<SocketAddr>>,
) -> Result<UdpSocket, ConsoleError> {
    let resolve_err = |source: std::io::Error| ConsoleError::Resolve {
        node: host.to_string(),
        source,
    };
    let candidates = resolve(host, port).map_err(resolve_err)?;
    let mut last_err: Option<std::io::Error> = None;
    for candidate in &candidates {
        let bind_addr = if candidate.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        };
        let socket = match UdpSocket::bind(bind_addr) {
            Ok(socket) => socket,
            Err(e) => {
                last_err = Some(e);
                continue;
            }
        };
        match socket.connect(candidate) {
            Ok(()) => {
                socket
                    .set_read_timeout(Some(SOCKET_POLL_TIMEOUT))
                    .map_err(resolve_err)?;
                return Ok(socket);
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(resolve_err(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            format!("could not resolve {host}:{port}"),
        )
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::m17::SharedState;
    use astar_m17::{BROADCAST, LinkState, Lsf, encode_callsign};
    use std::io;
    use std::net::{Ipv6Addr, SocketAddrV6};

    fn callsign() -> [u8; 6] {
        encode_callsign("AJ7HR").expect("a valid callsign encodes")
    }

    /// Feed `bytes` to `recv_step` as if the socket had just received them.
    fn feed(bytes: &[u8], fsm: &mut SessionFsm) -> RxStep {
        let mut buf = [0u8; 2_048];
        recv_step(
            |b| {
                b[..bytes.len()].copy_from_slice(bytes);
                Ok(bytes.len())
            },
            &mut buf,
            fsm,
        )
    }

    /// A state machine that has sent CONN and been ACKN'd.
    fn linked_fsm() -> SessionFsm {
        let mut fsm = SessionFsm::new(callsign(), b'A');
        let _ = fsm.connect(Instant::now());
        let step = feed(&ControlPacket::Ackn.to_bytes(), &mut fsm);
        assert_eq!(step.reply, None, "ACKN asks for no reply");
        assert_eq!(step.voice, None);
        assert_eq!(step.ended, None);
        assert_eq!(fsm.state(), LinkState::Linked);
        fsm
    }

    fn voice_packet(stream_id: u16, src: [u8; 6], frame_number: u16) -> StreamPacket {
        StreamPacket {
            stream_id,
            lsf: Lsf {
                dst: BROADCAST,
                src,
                type_field: Lsf::TYPE_VOICE_3200_STREAM,
                meta: [0; 14],
            },
            frame_number,
            payload: [0u8; 16],
        }
    }

    #[test]
    fn recv_step_reports_a_nack_as_refused() {
        let mut fsm = SessionFsm::new(callsign(), b'A');
        let _ = fsm.connect(Instant::now());
        let step = feed(&ControlPacket::Nack.to_bytes(), &mut fsm);
        assert_eq!(step.ended, Some(RxEnd::Refused));
        assert_eq!(step.reply, None);
        assert_eq!(step.voice, None);
        assert_eq!(fsm.state(), LinkState::Failed);
    }

    #[test]
    fn recv_step_reports_a_reflector_disc_as_disconnected() {
        let mut fsm = linked_fsm();
        let step = feed(&ControlPacket::Disc { callsign: None }.to_bytes(), &mut fsm);
        assert_eq!(step.ended, Some(RxEnd::Disconnected));
        assert_eq!(step.reply, None);
        assert_eq!(step.voice, None);
        assert_eq!(fsm.state(), LinkState::Failed);
    }

    #[test]
    fn recv_step_hands_back_a_pong_for_a_ping_instead_of_sending_it() {
        let mut fsm = linked_fsm();
        let ping = ControlPacket::Ping {
            callsign: BROADCAST,
        }
        .to_bytes();
        let step = feed(&ping, &mut fsm);
        assert_eq!(
            step.reply,
            Some(
                ControlPacket::Pong {
                    callsign: callsign()
                }
                .to_bytes()
            )
        );
        assert_eq!(step.voice, None);
        assert_eq!(step.ended, None);
    }

    #[test]
    fn recv_step_passes_a_voice_packet_through() {
        let mut fsm = linked_fsm();
        let pkt = voice_packet(0x1234, callsign(), 0);
        let step = feed(&pkt.to_bytes(), &mut fsm);
        assert_eq!(step.voice, Some(pkt));
        assert_eq!(step.reply, None);
        assert_eq!(step.ended, None);
    }

    #[test]
    fn recv_step_yields_nothing_when_the_read_times_out() {
        let mut fsm = SessionFsm::new(callsign(), b'A');
        let _ = fsm.connect(Instant::now());
        let mut buf = [0u8; 2_048];
        let step = recv_step(
            |_| Err(io::Error::from(io::ErrorKind::WouldBlock)),
            &mut buf,
            &mut fsm,
        );
        assert_eq!(step.reply, None);
        assert_eq!(step.voice, None);
        assert_eq!(step.ended, None);
        assert_eq!(
            fsm.state(),
            LinkState::Connecting,
            "a timeout is not a packet"
        );
    }

    #[test]
    fn connect_udp_socket_with_keeps_the_resolvers_own_error() {
        let result = connect_udp_socket_with("m17.test", 17000, |_, _| {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "test resolver: no such host",
            ))
        });
        match result {
            Err(ConsoleError::Resolve { node, source }) => {
                assert_eq!(node, "m17.test");
                assert_eq!(source.kind(), io::ErrorKind::NotFound);
                assert_eq!(source.to_string(), "test resolver: no such host");
            }
            other => panic!("expected ConsoleError::Resolve, got {other:?}"),
        }
    }

    #[test]
    fn connect_udp_socket_with_no_candidates_says_it_could_not_resolve() {
        let result = connect_udp_socket_with("m17.test", 17000, |_, _| Ok(Vec::new()));
        match result {
            Err(ConsoleError::Resolve { node, source }) => {
                assert_eq!(node, "m17.test");
                assert_eq!(source.kind(), io::ErrorKind::AddrNotAvailable);
                assert_eq!(source.to_string(), "could not resolve m17.test:17000");
            }
            other => panic!("expected ConsoleError::Resolve, got {other:?}"),
        }
    }

    #[test]
    fn connect_udp_socket_with_keeps_the_last_connect_error() {
        // A link-local candidate with a scope id no interface has: the OS
        // refuses it locally, and a UDP connect sends nothing anyway.
        let bad = SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
            17000,
            0,
            u32::MAX,
        ));
        // The same calls the helper makes, so the expected error is whatever
        // this host reports: the connect error, or the bind error on a host
        // without IPv6.
        let expected = UdpSocket::bind("[::]:0")
            .and_then(|s| s.connect(bad))
            .expect_err("the OS must refuse this candidate");
        let result = connect_udp_socket_with("m17.test", 17000, move |_, _| Ok(vec![bad]));
        match result {
            Err(ConsoleError::Resolve { node, source }) => {
                assert_eq!(node, "m17.test");
                assert_eq!(source.kind(), expected.kind());
                assert_eq!(source.to_string(), expected.to_string());
                assert_ne!(source.to_string(), "could not resolve m17.test:17000");
            }
            other => panic!("expected ConsoleError::Resolve, got {other:?}"),
        }
    }

    #[test]
    fn connect_udp_socket_with_connects_to_a_loopback_candidate() {
        // UDP connect sends nothing; the peer socket only gives the
        // candidate a real 127.0.0.1 address.
        let peer = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind loopback peer");
        let peer_addr = peer.local_addr().expect("peer addr");
        let socket = connect_udp_socket_with("m17.test", peer_addr.port(), move |_, _| {
            Ok(vec![peer_addr])
        })
        .expect("a loopback candidate connects");
        assert_eq!(socket.peer_addr().expect("connected"), peer_addr);
        assert_eq!(
            socket.read_timeout().expect("read timeout readable"),
            Some(SOCKET_POLL_TIMEOUT)
        );
    }

    #[test]
    fn decode_pair_decodes_both_halves_in_order() {
        let mut pkt = voice_packet(1, callsign(), 0);
        pkt.payload = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let mut seen: Vec<[u8; 8]> = Vec::new();
        let frames = decode_pair(
            &mut |bits: &[u8; 8]| {
                seen.push(*bits);
                [i16::from(bits[0]); 160]
            },
            &pkt,
        );
        assert_eq!(
            seen,
            vec![[1, 2, 3, 4, 5, 6, 7, 8], [9, 10, 11, 12, 13, 14, 15, 16]]
        );
        assert_eq!(frames, [[1_i16; 160], [9_i16; 160]]);
    }

    /// A recording opens on a stream's first packet, a new stream id
    /// closes the old one before opening its own, and 400 ms of silence
    /// closes whatever is still open.
    #[test]
    fn rx_track_records_each_stream_between_start_and_end() {
        let log = TalkerLog::new();
        let receiving = AtomicBool::new(false);
        let slot = TapSlot::default();
        let (tap, events) = astar_record::tap::tap_channel(32);
        slot.set(Some(tap));
        let mut track = RxTrack::with_tap(slot);
        let t0 = Instant::now();
        let kinds = || -> Vec<&'static str> {
            events
                .try_iter()
                .map(|e| match e {
                    FrameEvent::Start { talker, .. } => {
                        assert_eq!(talker.as_deref(), Some("AJ7HR"));
                        "start"
                    }
                    FrameEvent::Frame { .. } => "frame",
                    FrameEvent::Pcm { .. } => "pcm",
                    FrameEvent::End { .. } => "end",
                })
                .collect()
        };

        for (id, at) in [(0x1111, 0), (0x1111, 40), (0x2222, 80)] {
            let pkt = voice_packet(id, callsign(), 0);
            track.on_voice(&pkt, t0 + Duration::from_millis(at), &receiving, &log);
            let _ = track.decode(&mut |_| [0; 160], &pkt);
        }
        assert_eq!(
            kinds(),
            [
                "start", "frame", "pcm", "frame", "pcm", "frame", "pcm", "frame", "pcm", "end",
                "start", "frame", "pcm", "frame", "pcm",
            ]
        );
        track.expire(
            t0 + Duration::from_millis(80) + RX_SILENCE_TIMEOUT,
            &receiving,
            &log,
        );
        assert_eq!(kinds(), ["end"]);
        track.close_recording();
        assert!(kinds().is_empty(), "nothing open, nothing to close");
    }

    /// Record pressed mid-over: the tap appears after the stream's first
    /// packet, and the next packet opens the recording for the rest of it.
    #[test]
    fn a_tap_set_mid_over_records_the_rest_of_the_over() {
        let log = TalkerLog::new();
        let receiving = AtomicBool::new(false);
        let slot = TapSlot::default();
        let mut track = RxTrack::with_tap(slot.clone());
        let t0 = Instant::now();
        let pkt = voice_packet(0x1111, callsign(), 0);
        track.on_voice(&pkt, t0, &receiving, &log);
        let _ = track.decode(&mut |_| [0; 160], &pkt);
        let (tap, events) = astar_record::tap::tap_channel(32);
        slot.set(Some(tap));
        for at in [40, 80] {
            track.on_voice(&pkt, t0 + Duration::from_millis(at), &receiving, &log);
            let _ = track.decode(&mut |_| [0; 160], &pkt);
        }
        track.close_recording();
        let kinds: Vec<&str> = events
            .try_iter()
            .map(|e| match e {
                FrameEvent::Start { .. } => "start",
                FrameEvent::Frame { .. } => "frame",
                FrameEvent::Pcm { .. } => "pcm",
                FrameEvent::End { .. } => "end",
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "start", "frame", "pcm", "frame", "pcm", "frame", "pcm", "frame", "pcm", "end"
            ]
        );
    }

    #[test]
    fn rx_track_clears_receiving_after_400_ms_of_silence() {
        let log = TalkerLog::new();
        let receiving = AtomicBool::new(false);
        let mut track = RxTrack::new();
        let t0 = Instant::now();

        track.on_voice(&voice_packet(0x1234, callsign(), 0), t0, &receiving, &log);
        assert!(receiving.load(Ordering::Relaxed));
        assert_eq!(track.stream, Some(0x1234));
        assert_eq!(track.last_voice, Some(t0));
        assert_eq!(log.talker().as_deref(), Some("AJ7HR"));

        track.expire(t0 + Duration::from_millis(399), &receiving, &log);
        assert!(
            receiving.load(Ordering::Relaxed),
            "399 ms is not yet silence"
        );
        assert_eq!(track.last_voice, Some(t0));
        assert_eq!(track.stream, Some(0x1234));

        track.expire(t0 + RX_SILENCE_TIMEOUT, &receiving, &log);
        assert!(!receiving.load(Ordering::Relaxed));
        assert_eq!(track.last_voice, None);
        assert_eq!(track.stream, None);
        assert_eq!(
            log.talker().as_deref(),
            Some("AJ7HR"),
            "last heard outlives the stream"
        );
        let heard: Vec<String> = log.heard().into_iter().map(|e| e.callsign).collect();
        assert_eq!(
            heard,
            vec!["AJ7HR".to_string()],
            "a refresh, never a second row"
        );
    }

    /// A stream whose source is null names nobody — and its EOS, even one a
    /// reflector rewrote to carry a real callsign, must neither name that
    /// station nor refresh the previous station's clock.
    #[test]
    fn an_unnamed_stream_neither_names_nor_refreshes_anyone() {
        let shared = SharedState::new();
        let mut rx_stream: Option<u16> = None;
        let packet = |stream_id: u16, src: [u8; 6], frame_number: u16| StreamPacket {
            stream_id,
            lsf: Lsf {
                dst: BROADCAST,
                src,
                type_field: Lsf::TYPE_VOICE_3200_STREAM,
                meta: [0; 14],
            },
            frame_number,
            payload: [0u8; 16],
        };
        let n0call = encode_callsign("N0CALL").expect("a valid callsign encodes");
        let aj7hr = encode_callsign("AJ7HR").expect("a valid callsign encodes");

        // N0CALL talks and finishes.
        note_talker(&packet(0x1234, n0call, 0), &shared.talkers, &mut rx_stream);
        note_talker(
            &packet(0x1234, n0call, StreamPacket::EOS_BIT),
            &shared.talkers,
            &mut rx_stream,
        );
        assert_eq!(shared.snapshot().talker.as_deref(), Some("N0CALL"));

        // A stream with a null source: nobody is named and it never opens.
        note_talker(&packet(0x5678, [0; 6], 0), &shared.talkers, &mut rx_stream);
        assert_eq!(rx_stream, None, "a nameless stream is not an open one");
        assert_eq!(shared.snapshot().talker.as_deref(), Some("N0CALL"));

        // Its EOS carries a rewritten, valid source: still nobody.
        note_talker(
            &packet(0x5678, aj7hr, StreamPacket::EOS_BIT),
            &shared.talkers,
            &mut rx_stream,
        );
        assert_eq!(rx_stream, None);
        assert_eq!(shared.snapshot().talker.as_deref(), Some("N0CALL"));
        let heard = shared.talkers.heard();
        assert_eq!(
            heard.len(),
            1,
            "no row for a station only an EOS named: {heard:?}"
        );
        assert_eq!(heard[0].callsign, "N0CALL");
    }

    /// One received voice packet must name its talker in the snapshot, and
    /// keep naming them after the stream ends — "last heard", not "talking
    /// now" (`receiving` is the field that says that).
    #[test]
    fn a_received_stream_names_its_talker_and_the_name_outlives_it() {
        // Round-trip the address encoding first: a talker line is only worth
        // anything if the six base-40 bytes on the wire come back as the
        // callsign that went in.
        let src = encode_callsign("N0CALL").expect("a valid callsign encodes");
        assert_eq!(decode_callsign(&src), "N0CALL");

        let shared = SharedState::new();
        let mut rx_stream: Option<u16> = None;
        assert!(
            shared.snapshot().talker.is_none(),
            "a link with nothing heard on it yet names nobody"
        );

        let voice = |frame_number: u16| StreamPacket {
            stream_id: 0x1234,
            lsf: Lsf {
                dst: BROADCAST,
                src,
                type_field: Lsf::TYPE_VOICE_3200_STREAM,
                meta: [0; 14],
            },
            frame_number,
            payload: [0u8; 16],
        };

        note_talker(&voice(0), &shared.talkers, &mut rx_stream);
        assert_eq!(
            shared.snapshot().talker.as_deref(),
            Some("N0CALL"),
            "the first packet of a stream names who keyed up"
        );
        assert_eq!(rx_stream, Some(0x1234));

        // The EOS packet ends the stream; the callsign stays.
        note_talker(
            &voice(StreamPacket::EOS_BIT),
            &shared.talkers,
            &mut rx_stream,
        );
        assert_eq!(
            rx_stream, None,
            "EOS closes the stream out so the next packet reads as a new one"
        );
        assert_eq!(
            shared.snapshot().talker.as_deref(),
            Some("N0CALL"),
            "last heard PERSISTS past end-of-stream, exactly as D-Star's does"
        );
        assert!(
            !shared.snapshot().receiving,
            "and `receiving` — never set by note_talker — is what says nobody is on"
        );
        // The EOS refreshes the history's clock so `age_ms` counts from the
        // end of the over. That is a move-to-front, not a second row.
        let after_eos = shared.talkers.heard();
        assert_eq!(
            after_eos.len(),
            1,
            "the EOS refresh must not duplicate the talker: {after_eos:?}"
        );
        assert_eq!(after_eos[0].callsign, "N0CALL");

        // A second talker replaces the first once their stream opens.
        let src2 = encode_callsign("AJ7HR").expect("a valid callsign encodes");
        let mut second = voice(0);
        second.stream_id = 0x4321;
        second.lsf.src = src2;
        note_talker(&second, &shared.talkers, &mut rx_stream);
        assert_eq!(shared.snapshot().talker.as_deref(), Some("AJ7HR"));

        // The one-name `talker` slot only ever holds the newest; the history
        // keeps both, which is the whole point of it.
        let heard: Vec<_> = shared
            .talkers
            .heard()
            .into_iter()
            .map(|e| (e.network, e.callsign))
            .collect();
        assert_eq!(
            heard,
            [("m17", "AJ7HR".to_string()), ("m17", "N0CALL".to_string())],
            "the history keeps the first talker after the second replaces the line"
        );
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn stream_gate_drops_a_second_concurrent_stream() {
        let t0 = Instant::now();
        let mut gate = StreamGate::new();
        assert!(gate.admit(&voice_packet_for_test(0x1111, 0, "N0CALL"), t0));
        assert!(
            !gate.admit(&voice_packet_for_test(0x2222, 0, "AJ7HR"), t0 + ms(40)),
            "a second stream id while the first is open must be dropped"
        );
        assert!(
            gate.admit(&voice_packet_for_test(0x1111, 1, "N0CALL"), t0 + ms(40)),
            "the drop must leave the open stream untouched"
        );
        // Rule 5: the open stream's EOS closes it, so the other talker's
        // stream opens at once instead of waiting out the silence timer.
        // It inherits the 40 ms the first stream is still ahead (3 packets
        // admitted by t0 + 80 ms), which is well inside GATE_LEAD.
        assert!(gate.admit(
            &voice_packet_for_test(0x1111, 2 | StreamPacket::EOS_BIT, "N0CALL"),
            t0 + ms(80)
        ));
        assert!(
            gate.admit(&voice_packet_for_test(0x2222, 0, "AJ7HR"), t0 + ms(80)),
            "after EOS a new stream opens immediately"
        );
    }

    #[test]
    fn stream_gate_admits_every_packet_of_a_late_burst() {
        // A network stall delivers 300 ms of a stream at once. It is BEHIND
        // schedule (k × 40 ms ≤ 300 ms + 200 ms for k ≤ 8), so all of it
        // plays.
        let t0 = Instant::now();
        let mut gate = StreamGate::new();
        assert!(gate.admit(&voice_packet_for_test(0x1234, 0, "N0CALL"), t0));
        let admitted = (1..=8_u16)
            .filter(|&n| gate.admit(&voice_packet_for_test(0x1234, n, "N0CALL"), t0 + ms(300)))
            .count();
        assert_eq!(admitted, 8);
    }

    #[test]
    fn stream_gate_caps_a_same_instant_flood_at_six_packets() {
        // 100 packets of one stream in the same instant. The pace counts
        // ADMITTED packets: k = 0..=5 pass (5 × 40 ms = 200 ms = GATE_LEAD),
        // k = 6 would be 240 ms ahead. frame_number is held at 0 on purpose:
        // it is attacker-supplied, and a gate that paced on it would let all
        // 100 through.
        let t0 = Instant::now();
        let mut gate = StreamGate::new();
        let pkt = voice_packet_for_test(0x5555, 0, "N0CALL");
        let admitted = (0..100).filter(|_| gate.admit(&pkt, t0)).count();
        assert_eq!(admitted, 6);
    }

    #[test]
    fn stream_gate_treats_400_ms_of_silence_as_closed() {
        let t0 = Instant::now();
        let mut gate = StreamGate::new();
        assert!(gate.admit(&voice_packet_for_test(0x1111, 0, "N0CALL"), t0));
        assert!(
            !gate.admit(&voice_packet_for_test(0x2222, 0, "AJ7HR"), t0 + ms(399)),
            "399 ms after its last packet the first stream is still open"
        );
        assert!(
            gate.admit(&voice_packet_for_test(0x2222, 0, "AJ7HR"), t0 + ms(400)),
            "RX_SILENCE_TIMEOUT after its last packet the first stream counts as closed"
        );
    }

    #[test]
    fn stream_gate_caps_an_eos_on_every_packet_flood_at_six_packets() {
        // Every packet carries EOS and a fresh stream id, so each one opens a
        // stream and closes it again. The lead already admitted must carry
        // into the next stream, or all 100 would pass.
        let t0 = Instant::now();
        let mut gate = StreamGate::new();
        let admitted = (0..100_u16)
            .filter(|&n| {
                gate.admit(
                    &voice_packet_for_test(0x1000 + n, StreamPacket::EOS_BIT, "N0CALL"),
                    t0,
                )
            })
            .count();
        assert_eq!(admitted, 6);
    }

    #[test]
    fn stream_gate_caps_an_eos_on_every_seventh_packet_flood_at_six_packets() {
        // One stream id, EOS on every 7th packet: the 7th is dropped but
        // still closes the stream. Reopening must not reset the pace.
        let t0 = Instant::now();
        let mut gate = StreamGate::new();
        let admitted = (0..100_u16)
            .filter(|&n| {
                let frame = if n % 7 == 6 { StreamPacket::EOS_BIT } else { 0 };
                gate.admit(&voice_packet_for_test(0x5555, frame, "N0CALL"), t0)
            })
            .count();
        assert_eq!(admitted, 6);
    }
}
