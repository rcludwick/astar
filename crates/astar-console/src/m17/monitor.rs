// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `M17Monitor`: a receive-only M17 reflector leg for the node's conference.
//!
//! [`super::M17Session`] is a full transceiver, so a caller that merely never
//! asks it to key up is relying on a convention. This type is built so that
//! sending audio toward a reflector is not something it can express:
//!
//! - its only path to the wire is `ControlOnlySocket::send_control`, which
//!   re-parses every buffer with [`ControlPacket::parse`] and refuses
//!   anything that is not an exact `CONN`, `PONG` or `DISC`-with-callsign
//!   shape, the only three packets this leg may ever send. A 54-byte stream
//!   packet cannot pass, nor can the other control shapes (`ACKN`, `NACK`,
//!   `PING`, the bare `DISC`), and each refusal is counted in
//!   `M17MonitorStatus::refused_sends`;
//! - its Codec 2 handle is a `Decoder` that exposes decoding only;
//! - its audio goes one way, into a [`SourceSink`] that holds a `Sender` and
//!   no `Receiver`, so there is no conference mix here to turn into packets.
//!
//! Everything slow (loading Codec 2, DNS, the first `CONN`) happens on the
//! monitor's own thread, "iax-m17-mon", so [`M17Monitor::spawn`] is safe to
//! call under a session lock.
//!
//! Received streams pass through `StreamGate` first: one stream at a time,
//! paced to real time, so a client flooding a public module cannot queue
//! unbounded audio in the conference. There is no reorder buffer; packets
//! play in arrival order.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use astar_audio::RxFrame;
use astar_codec::codec2::{Codec2Voice, open_codec2};
use astar_m17::{ControlPacket, FsmAction, LinkState, SessionFsm, StreamPacket, encode_callsign};
use astar_record::frames::FrameEvent;
use astar_record::packet_tap::{CallMeta, CallPacketTap, PacketCallId, PacketNetwork, PacketSlot};
use astar_record::tap::TapSlot;

use super::rx::{RxEnd, RxTrack, StreamGate, TalkerLog, connect_udp_socket_with, recv_step};
use super::{link_to_u8, u8_to_link};
use crate::M17MonitorStatus;
use crate::heard::HeardEntry;
use crate::session::ConsoleError;
use crate::voice_route::{RateBridge, SESSION_RATE};

// End reasons. astar-station, astar-server and the Discord commander print
// these verbatim, so they are part of the contract, not prose.
const REFUSED: &str = "reflector refused the link";
const SENT_DISC: &str = "reflector sent DISC";
const NO_DECODER: &str = "no Codec 2 decoder";
const CONFERENCE_GONE: &str = "conference gone";
/// Defensive only: the 8 kHz to bus-rate bridge could not be built, which
/// cannot happen for the two rates `spawn` accepts. Same words the console
/// session uses for a leg that stopped without saying why.
const STOPPED: &str = "monitor stopped";

/// The rates the conference bus runs at: 8 kHz, or 16 kHz under slin16.
const BUS_RATES: [u32; 2] = [8_000, 16_000];

/// How the monitor's thread turns a host name into addresses. Injected so
/// tests never do real DNS.
type Resolver = fn(&str, u16) -> std::io::Result<Vec<SocketAddr>>;

/// What a monitor links as, and where.
///
/// Deliberately has no `Debug`: nothing secret lives here (M17 has no
/// password; the callsign is the station's public identity), and leaving the
/// derive off keeps a future field from being printed by accident.
pub struct M17MonitorConfig {
    /// Reflector host name or IP address. Resolved on the monitor's thread.
    pub host: String,
    /// Reflector UDP port.
    pub port: u16,
    /// ASCII module letter; lowercase is accepted and uppercased before use,
    /// because a reflector refuses a lowercase module.
    pub module: u8,
    /// The callsign the reflector shows on the module. Must survive
    /// `encode_callsign` (1 to 9 base-40 characters).
    pub callsign: String,
    /// Extra directories to search for a runtime libcodec2.
    pub codec_dirs: Vec<PathBuf>,
    /// How long the reflector may stay silent before the link counts as dead
    /// (30 s in production; tests shorten it).
    pub keepalive_timeout: Duration,
}

impl M17MonitorConfig {
    /// Check the two fields a reflector would otherwise only reject after a
    /// wait: the callsign must encode and the module must be a letter.
    /// Nothing else is checked here; reachability is the thread's business.
    pub fn validate(&self) -> Result<(), ConsoleError> {
        if encode_callsign(&self.callsign).is_none() {
            return Err(bad_callsign(&self.callsign));
        }
        if !self.module.is_ascii_alphabetic() {
            return Err(ConsoleError::M17("module must be A-Z".into()));
        }
        Ok(())
    }
}

fn bad_callsign(callsign: &str) -> ConsoleError {
    ConsoleError::M17(format!("invalid M17 callsign {callsign:?}"))
}

/// Where decoded reflector audio goes: one direction, into the conference.
///
/// Deliberately not the voice-route lane type the console's own M17 client
/// uses, which also carries a channel of microphone audio to send. This holds
/// a `Sender` and no `Receiver`, so there is nothing here to transmit.
pub struct SourceSink {
    /// The conference member's contribution channel.
    pub to_conference: Sender<RxFrame>,
    /// The conference bus rate, 8000 or 16000; anything else is refused by
    /// [`M17Monitor::spawn`].
    pub bus_rate: u32,
}

/// State the monitor's thread writes and the owning handle reads: atomics
/// plus two small mutexes, so `status()` never waits on the thread.
struct MonitorShared {
    link: AtomicU8,
    receiving: AtomicBool,
    talkers: TalkerLog,
    fail_reason: Mutex<Option<String>>,
    dropped: AtomicU64,
    /// Shared with the thread's `ControlOnlySocket`, which bumps it.
    refused: Arc<AtomicU64>,
    sink_lost: AtomicBool,
    finished: AtomicBool,
    /// Set once the socket exists, so a `CONN` may be on its way. Before
    /// that, [`M17Monitor`]'s stop detaches instead of joining (see `stop`).
    on_wire: AtomicBool,
    /// The codec-frame recording tap ([`M17Monitor::frame_tap`]). The
    /// thread only ever offers to it, never waits on it.
    frame_tap: TapSlot<FrameEvent>,
}

impl MonitorShared {
    fn new() -> Self {
        Self {
            // Born linking: the thread's first act on the wire is CONN, and a
            // monitor never reports "idle".
            link: AtomicU8::new(link_to_u8(LinkState::Connecting)),
            receiving: AtomicBool::new(false),
            talkers: TalkerLog::new(),
            fail_reason: Mutex::new(None),
            dropped: AtomicU64::new(0),
            refused: Arc::new(AtomicU64::new(0)),
            sink_lost: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            on_wire: AtomicBool::new(false),
            frame_tap: TapSlot::default(),
        }
    }

    fn store_link(&self, state: LinkState) {
        self.link.store(link_to_u8(state), Ordering::Relaxed);
    }

    /// Record why the leg ended; every failure reads as "failed".
    fn fail(&self, reason: &str) {
        *self
            .fail_reason
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(reason.to_string());
        self.store_link(LinkState::Failed);
    }
}

/// Sets `finished` however the thread leaves `run`, a panic included, so the
/// session's reaper never waits on a thread that is already gone.
struct FinishOnExit(Arc<MonitorShared>);

impl Drop for FinishOnExit {
    fn drop(&mut self) {
        // Release pairs with the Acquire in `finished()`: a poller that sees
        // `finished` must also see the final link state and reason.
        self.0.finished.store(true, Ordering::Release);
    }
}

/// A receive-only M17 reflector leg. Owns the thread "iax-m17-mon", which
/// owns the socket, the decoder and the rate bridge.
///
/// Dropping it (or [`M17Monitor::disconnect`]) sends `DISC` and joins the
/// thread within one 50 ms socket poll once the socket exists. A hang-up
/// while the codec is still loading or the host is still resolving does not
/// wait: the thread is detached, sees the shutdown when that step returns,
/// and leaves without sending anything, not even `CONN`. Callers such as the
/// node's 50 ms pump therefore never block on a slow resolver.
pub struct M17Monitor {
    thread: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    shared: Arc<MonitorShared>,
    /// The packet log's slot ([`M17Monitor::packet_slot`]).
    packet_slot: PacketSlot,
    /// The id of this monitor's [`CallPacketTap`], which the thread owns.
    packet_call: PacketCallId,
}

impl M17Monitor {
    /// Validate, then start the monitor's thread. Opens no socket, loads no
    /// codec and resolves nothing on the caller's thread, so it is safe under
    /// the console session's lock.
    ///
    /// # Errors
    /// [`M17MonitorConfig::validate`]'s errors;
    /// `ConsoleError::M17("unsupported conference rate {rate} Hz")` for a bus
    /// rate other than 8000 or 16000;
    /// `ConsoleError::M17("could not start the M17 monitor thread: {e}")`.
    pub fn spawn(cfg: M17MonitorConfig, sink: SourceSink) -> Result<M17Monitor, ConsoleError> {
        spawn_inner(cfg, sink, system_resolve)
    }

    /// A poll-cheap snapshot: atomics and two short mutex reads.
    #[must_use]
    pub fn status(&self) -> M17MonitorStatus {
        let s = &self.shared;
        M17MonitorStatus {
            link: u8_to_link(s.link.load(Ordering::Relaxed)).as_str(),
            receiving: s.receiving.load(Ordering::Relaxed),
            talker: s.talkers.talker(),
            fail_reason: s
                .fail_reason
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
            dropped_packets: s.dropped.load(Ordering::Relaxed),
            refused_sends: s.refused.load(Ordering::Relaxed),
        }
    }

    /// Who has keyed up on this module, newest first. Attacker-supplied text.
    #[must_use]
    pub fn heard(&self) -> Vec<HeardEntry> {
        self.shared.talkers.heard()
    }

    /// The packet log's slot: install a tap here and the thread offers it
    /// every media datagram it receives, ahead of the flood gate. The slot is
    /// shared: setting it on the returned handle reaches the live thread.
    #[must_use]
    pub fn packet_slot(&self) -> PacketSlot {
        self.packet_slot.clone()
    }

    /// The id this monitor's packet events carry.
    #[must_use]
    pub fn packet_call(&self) -> PacketCallId {
        self.packet_call
    }

    /// The codec-frame recording tap: install a [`astar_record::Tap`] here
    /// and the thread offers every over it decodes to it as [`FrameEvent`]s
    /// — each 8-byte Codec 2 frame, then the 160 samples decoded from it.
    /// Packets the flood gate drops are never decoded, so never recorded.
    /// The slot is shared: setting it on the returned handle reaches the
    /// live thread.
    #[must_use]
    pub fn frame_tap(&self) -> TapSlot<FrameEvent> {
        self.shared.frame_tap.clone()
    }

    /// The conference end of the sink is gone (a send failed). The thread
    /// is on its way out: it records "conference gone", sends `DISC` and
    /// exits, so a join returns promptly.
    #[must_use]
    pub fn sink_lost(&self) -> bool {
        // Acquire pairs with the Release store in `serve`.
        self.shared.sink_lost.load(Ordering::Acquire)
    }

    /// The thread has exited on its own (failed, or sink lost), so a join
    /// is instant.
    #[must_use]
    pub fn finished(&self) -> bool {
        // Acquire pairs with the Release store in `FinishOnExit`, so a caller
        // that sees `true` also sees the final link state and fail reason.
        self.shared.finished.load(Ordering::Acquire)
    }

    /// Shutdown flag, then `DISC`, then join: at most one 50 ms poll once
    /// the socket exists. Before that (codec load or name resolution still
    /// running) the thread is detached rather than joined, and it leaves
    /// without sending anything. `Drop` does the same, so a monitor dropped
    /// without this still says goodbye.
    pub fn disconnect(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        // SeqCst pairs with `run`'s `on_wire` store and the shutdown check
        // after it: if this load misses `on_wire`, the thread's later
        // shutdown load is guaranteed to see the flag, so it exits with
        // nothing sent and detaching cannot leave a CONN behind.
        self.shutdown.store(true, Ordering::SeqCst);
        let Some(t) = self.thread.take() else {
            return;
        };
        if self.shared.on_wire.load(Ordering::SeqCst) {
            // CONN may have gone out: wait (one 50 ms poll at most) for DISC.
            let _ = t.join();
        }
        // Otherwise the thread is still loading the codec or resolving the
        // host. Dropping the handle detaches it; the caller, often the node's
        // 50 ms pump, never waits on a slow resolver.
    }
}

impl Drop for M17Monitor {
    /// Same as [`M17Monitor::disconnect`]: shutdown flag, then `DISC` and a
    /// join within one 50 ms poll if the socket exists, else a detach with
    /// nothing sent.
    fn drop(&mut self) {
        self.stop();
    }
}

/// `M17Monitor::spawn` with DNS replaced, so tests never resolve a real name.
#[cfg(test)]
fn spawn_with_resolver(
    cfg: M17MonitorConfig,
    sink: SourceSink,
    resolve: Resolver,
) -> Result<M17Monitor, ConsoleError> {
    spawn_inner(cfg, sink, resolve)
}

fn spawn_inner(
    cfg: M17MonitorConfig,
    sink: SourceSink,
    resolve: Resolver,
) -> Result<M17Monitor, ConsoleError> {
    cfg.validate()?;
    let rate = sink.bus_rate;
    if !BUS_RATES.contains(&rate) {
        return Err(ConsoleError::M17(format!(
            "unsupported conference rate {rate} Hz"
        )));
    }
    let callsign = encode_callsign(&cfg.callsign).ok_or_else(|| bad_callsign(&cfg.callsign))?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(MonitorShared::new());
    let module = cfg.module.to_ascii_uppercase();
    // The thread resolves the host, so it fills in both addresses.
    let unknown = SocketAddr::from(([0, 0, 0, 0], 0));
    let packet_slot = PacketSlot::default();
    let packet_tap = CallPacketTap::new(
        packet_slot.clone(),
        astar_m17::frame::is_media,
        CallMeta {
            network: PacketNetwork::M17,
            codec: None,
            label: format!("M17 {} {}", cfg.host, char::from(module)),
            local: unknown,
            peer: unknown,
        },
    );
    let packet_call = packet_tap.id();
    let params = RunParams {
        host: cfg.host,
        port: cfg.port,
        callsign,
        module,
        codec_dirs: cfg.codec_dirs,
        keepalive_timeout: cfg.keepalive_timeout,
        sink,
        resolve,
        shutdown: Arc::clone(&shutdown),
        shared: Arc::clone(&shared),
        packet_tap,
    };
    let thread = std::thread::Builder::new()
        .name("iax-m17-mon".to_string())
        .spawn(move || run(params))
        .map_err(|e| ConsoleError::M17(format!("could not start the M17 monitor thread: {e}")))?;
    Ok(M17Monitor {
        thread: Some(thread),
        shutdown,
        shared,
        packet_slot,
        packet_call,
    })
}

/// Production DNS, run on the monitor's thread only.
fn system_resolve(host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
    (host, port).to_socket_addrs().map(Iterator::collect)
}

/// The ONLY path from this module to the wire.
///
/// Every buffer is re-parsed as a control packet before it is sent, and only
/// `CONN`, `PONG` and a `DISC` carrying the callsign pass. That parser
/// accepts only exact (magic, length) pairs of 4, 10 or 11 bytes, so a
/// stream packet is refused by construction. `ACKN`, `NACK`, `PING` and the
/// bare `DISC` parse but are refused too: "is a control packet" is wider
/// than what this leg may send. A refusal is counted rather than asserted,
/// which keeps a test that proves the refusal from panicking.
struct ControlOnlySocket {
    socket: UdpSocket,
    refused: Arc<AtomicU64>,
}

impl ControlOnlySocket {
    /// `false` (and `refused += 1`) unless `bytes` parses as a `CONN`, a
    /// `PONG` or a `DISC` with a callsign; otherwise whether the send
    /// succeeded.
    fn send_control(&self, bytes: &[u8]) -> bool {
        let allowed = matches!(
            ControlPacket::parse(bytes),
            Some(
                ControlPacket::Conn { .. }
                    | ControlPacket::Pong { .. }
                    | ControlPacket::Disc { callsign: Some(_) }
            )
        );
        if !allowed {
            self.refused.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.socket.send(bytes).is_ok()
    }

    /// One bounded read (the socket's 50 ms read timeout). The socket is
    /// connected, so the sender is always the reflector.
    fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        self.socket.recv_from(buf)
    }
}

/// A decode-only Codec 2 handle: the one capability this module needs.
struct Decoder(Box<dyn Codec2Voice>);

impl Decoder {
    // By value: clippy's pedantic `trivially_copy_pass_by_ref` fires on a
    // private fn taking an 8-byte `Copy` array by reference.
    fn decode(&mut self, bits: [u8; 8]) -> [i16; 160] {
        self.0.decode(&bits)
    }
}

/// Decoded 8 kHz frames onto the conference bus: passed straight through at
/// 8 kHz, bridged 160 to 320 samples at 16 kHz (the node runs slin16).
struct BusEdge(Option<RateBridge>);

impl BusEdge {
    fn new(bus_rate: u32) -> Result<Self, astar_audio::AudioError> {
        if bus_rate == SESSION_RATE {
            Ok(Self(None))
        } else {
            RateBridge::new(SESSION_RATE, bus_rate).map(|b| Self(Some(b)))
        }
    }

    /// One frame in, one bus-rate frame out; `None` only if the resampler
    /// failed, in which case the frame is dropped.
    fn onto_bus(&mut self, pcm: &[i16; 160]) -> Option<Vec<i16>> {
        match &mut self.0 {
            None => Some(pcm.to_vec()),
            Some(bridge) => bridge.convert(pcm),
        }
    }
}

/// The receive half of the leg: gate, name, decode, bridge, forward.
struct AudioPath {
    decoder: Decoder,
    edge: BusEdge,
    gate: StreamGate,
    track: RxTrack,
    to_conference: Sender<RxFrame>,
}

impl AudioPath {
    /// Play one stream packet, received at `now`, into the conference.
    /// `false` means the conference end of the sink is gone. `now` is a
    /// parameter so a test can drive the gate without depending on timing.
    fn play(&mut self, pkt: &StreamPacket, now: Instant, shared: &MonitorShared) -> bool {
        if !self.gate.admit(pkt, now) {
            shared.dropped.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        self.track
            .on_voice(pkt, now, &shared.receiving, &shared.talkers);
        let decoder = &mut self.decoder;
        // Both halves are decoded, and recorded, before either is played:
        // a sink that has gone away does not unpair the recording.
        let frames = self
            .track
            .decode(&mut |bits: &[u8; 8]| decoder.decode(*bits), pkt);
        for pcm in &frames {
            let Some(out) = self.edge.onto_bus(pcm) else {
                continue;
            };
            if self.to_conference.send(RxFrame::from(out)).is_err() {
                return false;
            }
        }
        true
    }
}

/// Everything `run` needs, moved onto the thread in one piece.
struct RunParams {
    host: String,
    port: u16,
    callsign: [u8; 6],
    module: u8,
    codec_dirs: Vec<PathBuf>,
    keepalive_timeout: Duration,
    sink: SourceSink,
    resolve: Resolver,
    shutdown: Arc<AtomicBool>,
    shared: Arc<MonitorShared>,
    packet_tap: CallPacketTap,
}

/// The "iax-m17-mon" thread: codec, bridge, resolve, CONN, then `serve`
/// until told to stop or the link ends, then a best-effort `DISC`. A stop
/// requested before CONN goes out ends the thread with nothing sent.
fn run(p: RunParams) {
    let RunParams {
        host,
        port,
        callsign,
        module,
        codec_dirs,
        keepalive_timeout,
        sink,
        resolve,
        shutdown,
        shared,
        mut packet_tap,
    } = p;
    let _exit = FinishOnExit(Arc::clone(&shared));

    let Some((codec, _backend)) = open_codec2(&codec_dirs) else {
        shared.fail(NO_DECODER);
        return;
    };
    let Ok(edge) = BusEdge::new(sink.bus_rate) else {
        shared.fail(STOPPED);
        return;
    };
    // Loading Codec 2 and resolving the host can both take a while. A
    // hang-up (or a revoke) during either must not be followed by a CONN in
    // the dialer's name, so check before each step that leads to the wire.
    if shutdown.load(Ordering::Relaxed) {
        return;
    }
    let socket = match connect_udp_socket_with(&host, port, resolve) {
        Ok(s) => s,
        Err(e) => {
            shared.fail(&resolve_reason(&host, &e));
            return;
        }
    };
    if let Ok(local) = socket.local_addr() {
        packet_tap.set_local(local);
    }
    if let Ok(peer) = socket.peer_addr() {
        packet_tap.set_peer(peer);
    }
    // From here a CONN may go out, so `stop()` must join and let this thread
    // send DISC. SeqCst pairs with `stop()`: either it sees `on_wire` and
    // joins, or this load sees its shutdown and the thread leaves unheard.
    shared.on_wire.store(true, Ordering::SeqCst);
    if shutdown.load(Ordering::SeqCst) {
        return;
    }
    let sock = ControlOnlySocket {
        socket,
        refused: Arc::clone(&shared.refused),
    };

    let mut fsm = SessionFsm::with_keepalive_timeout(callsign, module, keepalive_timeout);
    sock.send_control(&fsm.connect(Instant::now()));
    shared.store_link(fsm.state());

    let mut audio = AudioPath {
        decoder: Decoder(codec),
        edge,
        gate: StreamGate::new(),
        track: RxTrack::with_tap(shared.frame_tap.clone()),
        to_conference: sink.to_conference,
    };
    if let Some(reason) = serve(
        &sock,
        &mut fsm,
        &mut audio,
        &shared,
        &shutdown,
        keepalive_timeout,
        &mut packet_tap,
    ) {
        shared.fail(&reason);
    }
    audio.track.close_recording();
    // Every exit after CONN says goodbye, so the reflector drops the
    // callsign from the module now rather than at its own timeout.
    sock.send_control(
        &ControlPacket::Disc {
            callsign: Some(callsign),
        }
        .to_bytes(),
    );
}

/// The receive loop. `None` = asked to stop; `Some(reason)` = the link ended.
fn serve(
    sock: &ControlOnlySocket,
    fsm: &mut SessionFsm,
    audio: &mut AudioPath,
    shared: &MonitorShared,
    shutdown: &AtomicBool,
    keepalive_timeout: Duration,
    packet_tap: &mut CallPacketTap,
) -> Option<String> {
    let mut buf = [0u8; 2_048];
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return None;
        }
        let step = recv_step(
            |b| {
                let (n, from) = sock.recv_from(b)?;
                packet_tap.capture(from, &b[..n]);
                Ok(n)
            },
            &mut buf,
            fsm,
        );
        if let Some(reply) = step.reply {
            sock.send_control(&reply);
        }
        if let Some(end) = step.ended {
            return Some(end_reason(end).to_string());
        }
        if let Some(pkt) = step.voice
            && !audio.play(&pkt, Instant::now(), shared)
        {
            // Release pairs with the Acquire in `sink_lost()`.
            shared.sink_lost.store(true, Ordering::Release);
            return Some(CONFERENCE_GONE.to_string());
        }
        match fsm.tick(Instant::now()) {
            FsmAction::Send(bytes) => {
                sock.send_control(&bytes);
            }
            FsmAction::Unlinked => return Some(silence_reason(keepalive_timeout)),
            FsmAction::None | FsmAction::Voice(_) => {}
        }
        shared.store_link(fsm.state());
        audio
            .track
            .expire(Instant::now(), &shared.receiving, &shared.talkers);
    }
}

fn end_reason(end: RxEnd) -> &'static str {
    match end {
        RxEnd::Refused => REFUSED,
        RxEnd::Disconnected => SENT_DISC,
    }
}

/// "no reply for 30 s" for whole seconds, else "no reply for 300 ms".
fn silence_reason(t: Duration) -> String {
    if t.subsec_nanos() == 0 {
        format!("no reply for {} s", t.as_secs())
    } else {
        format!("no reply for {} ms", t.as_millis())
    }
}

/// "could not resolve {host}: {io error}", with the resolver's own error
/// text rather than a generic one (the other half of iax-c1d5).
fn resolve_reason(host: &str, e: &ConsoleError) -> String {
    match e {
        ConsoleError::Resolve { node, source } => format!("could not resolve {node}: {source}"),
        other => format!("could not resolve {host}: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::m17::rx::{voice_bytes_for_test, voice_packet_for_test};
    use std::net::IpAddr;
    use std::sync::mpsc::{Receiver, TryRecvError, channel};

    fn cfg(host: &str, port: u16, module: u8, callsign: &str) -> M17MonitorConfig {
        M17MonitorConfig {
            host: host.to_string(),
            port,
            module,
            callsign: callsign.to_string(),
            codec_dirs: Vec::new(),
            keepalive_timeout: Duration::from_secs(30),
        }
    }

    fn sink(bus_rate: u32) -> (SourceSink, Receiver<RxFrame>) {
        let (to_conference, rx) = channel();
        (
            SourceSink {
                to_conference,
                bus_rate,
            },
            rx,
        )
    }

    fn started(r: Result<M17Monitor, ConsoleError>) -> M17Monitor {
        match r {
            Ok(m) => m,
            Err(e) => panic!("spawn failed: {e}"),
        }
    }

    /// Poll `pred` every 10 ms until it holds or `timeout_ms` passes
    /// (a deadline loop, never one fixed-timeout read — iax-3f7a).
    fn wait_until(mut pred: impl FnMut() -> bool, timeout_ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while Instant::now() < deadline {
            if pred() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        pred()
    }

    /// The next datagram on a NON-BLOCKING `peer`, within `timeout_ms`.
    fn next_from(peer: &UdpSocket, timeout_ms: u64) -> Option<(Vec<u8>, SocketAddr)> {
        let mut buf = [0u8; 256];
        let mut got = None;
        wait_until(
            || match peer.recv_from(&mut buf) {
                Ok((n, from)) => {
                    got = Some((buf[..n].to_vec(), from));
                    true
                }
                Err(_) => false,
            },
            timeout_ms,
        );
        got
    }

    fn next_datagram(peer: &UdpSocket, timeout_ms: u64) -> Option<Vec<u8>> {
        next_from(peer, timeout_ms).map(|(d, _)| d)
    }

    /// Test resolver: parses an IP literal and never touches DNS.
    fn loopback_resolve(host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        let ip: IpAddr = host
            .parse()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{e}")))?;
        Ok(vec![SocketAddr::new(ip, port)])
    }

    /// Set by [`slow_loopback_resolve`] on entry, so a test can hang up while
    /// resolution is known to be in flight. Only one test uses it.
    static RESOLVING: AtomicBool = AtomicBool::new(false);
    /// Holds [`slow_loopback_resolve`] until the test sets it, so the hang-up
    /// provably lands while resolution is in flight. Only one test uses it.
    static RELEASE: AtomicBool = AtomicBool::new(false);

    /// Test resolver standing in for a slow DNS server: flags that it has
    /// started, waits until the test releases it (at most 10 s, so a broken
    /// test cannot hang the suite), then parses the IP literal.
    fn slow_loopback_resolve(host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        RESOLVING.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !RELEASE.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        loopback_resolve(host, port)
    }

    /// Test resolver that always fails, standing in for an unknown host.
    fn no_such_host(_host: &str, _port: u16) -> std::io::Result<Vec<SocketAddr>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "test resolver: no such host",
        ))
    }

    /// A control-only socket connected to a non-blocking loopback peer.
    fn control_pair() -> (ControlOnlySocket, Arc<AtomicU64>, UdpSocket) {
        let peer = UdpSocket::bind("127.0.0.1:0").expect("bind peer");
        peer.set_nonblocking(true).expect("non-blocking peer");
        let local = UdpSocket::bind("127.0.0.1:0").expect("bind monitor side");
        local
            .connect(peer.local_addr().expect("peer addr"))
            .expect("connect monitor side");
        let refused = Arc::new(AtomicU64::new(0));
        let ctl = ControlOnlySocket {
            socket: local,
            refused: Arc::clone(&refused),
        };
        (ctl, refused, peer)
    }

    /// Spawn must refuse, and prove no thread took the sink: spawn consumed
    /// the only Sender, so a refusal leaves the Receiver disconnected, while
    /// a started thread would still be holding it.
    fn spawn_err(cfg: M17MonitorConfig, bus_rate: u32) -> String {
        let (s, rx) = sink(bus_rate);
        let msg = match M17Monitor::spawn(cfg, s) {
            Ok(_) => panic!("spawn must refuse this config"),
            Err(ConsoleError::M17(m)) => m,
            Err(other) => panic!("expected ConsoleError::M17, got {other}"),
        };
        assert_eq!(
            rx.try_recv(),
            Err(TryRecvError::Disconnected),
            "no monitor thread may hold the sink after a refusal"
        );
        msg
    }

    /// 1 kHz at 8 kHz: eight samples a cycle.
    const TONE_CYCLE: [i16; 8] = [0, 5_657, 8_000, 5_657, 0, -5_657, -8_000, -5_657];

    fn tone() -> [i16; 160] {
        std::array::from_fn(|i| TONE_CYCLE[i % 8])
    }

    #[test]
    fn send_control_refuses_a_stream_packet_and_nothing_reaches_the_wire() {
        let (ctl, refused, peer) = control_pair();
        let voice = voice_bytes_for_test(0x1234, 0, "AJ7HR");
        assert_eq!(voice.len(), 54);
        assert!(
            StreamPacket::parse(&voice).is_some(),
            "the fixture must be a real stream packet"
        );
        assert!(!ctl.send_control(&voice), "a stream packet must be refused");
        assert_eq!(refused.load(Ordering::Relaxed), 1);

        // Control packets the monitor never has a reason to send. Each one
        // parses (so a gate that only asked "is it a control packet?" would
        // let it out), and each one must be refused: only CONN, PONG and a
        // DISC carrying the callsign may go toward M17.
        let ping = ControlPacket::Ping {
            callsign: encode_callsign("AJ7HR").expect("callsign encodes"),
        }
        .to_bytes();
        let others: [(&[u8], &str); 4] = [
            (
                ping.as_slice(),
                "PING must be refused: the monitor never pings",
            ),
            (
                b"ACKN",
                "ACKN must be refused: that is the reflector's word",
            ),
            (
                b"NACK",
                "NACK must be refused: that is the reflector's word",
            ),
            (b"DISC", "a DISC without a callsign must be refused"),
        ];
        for (bytes, why) in others {
            assert!(
                ControlPacket::parse(bytes).is_some(),
                "{why} (fixture must parse)"
            );
            assert!(!ctl.send_control(bytes), "{why}");
        }
        assert_eq!(refused.load(Ordering::Relaxed), 5);
        assert_eq!(
            next_datagram(&peer, 200),
            None,
            "no refused packet may ever reach the peer"
        );
    }

    #[test]
    fn send_control_passes_a_pong_byte_exact() {
        let (ctl, refused, peer) = control_pair();
        let pong = ControlPacket::Pong {
            callsign: encode_callsign("AJ7HR").expect("callsign encodes"),
        }
        .to_bytes();
        assert!(ctl.send_control(&pong));
        assert_eq!(refused.load(Ordering::Relaxed), 0);
        assert_eq!(next_datagram(&peer, 2_000), Some(pong));
    }

    #[test]
    fn bus_edge_upsamples_to_320_at_16k_and_passes_160_through_at_8k() {
        let mut up = BusEdge::new(16_000).expect("8 kHz to 16 kHz bridge");
        let frames: Vec<Vec<i16>> = (0..3)
            .map(|_| up.onto_bus(&tone()).expect("one frame out per frame in"))
            .collect();
        assert_eq!(
            frames.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![320, 320, 320]
        );
        assert!(
            frames[2].iter().any(|&s| s != 0),
            "past the resampler warm-up the tone must come through"
        );

        let mut flat = BusEdge::new(8_000).expect("8 kHz passthrough");
        assert_eq!(flat.onto_bus(&tone()), Some(tone().to_vec()));
    }

    #[test]
    fn play_runs_every_packet_through_the_gate_and_counts_the_drops() {
        // The gate's own tests prove StreamGate; this proves the receive
        // path actually uses it. 100 packets of one stream at one instant:
        // the gate admits 6, so 12 frames (two per packet) reach the
        // conference and the other 94 packets are counted as dropped.
        let Some((codec, _backend)) = open_codec2(&[]) else {
            panic!("test builds link Codec 2 statically (dev-dependency)");
        };
        let (to_conference, rx) = channel();
        let mut audio = AudioPath {
            decoder: Decoder(codec),
            edge: BusEdge::new(8_000).expect("8 kHz passthrough"),
            gate: StreamGate::new(),
            track: RxTrack::new(),
            to_conference,
        };
        let shared = MonitorShared::new();
        let pkt = voice_packet_for_test(0x5555, 0, "N0CALL");
        let t0 = Instant::now();
        for _ in 0..100 {
            assert!(audio.play(&pkt, t0, &shared), "the receiver is still alive");
        }
        assert_eq!(rx.try_iter().count(), 12);
        assert_eq!(shared.dropped.load(Ordering::Relaxed), 94);
        assert!(shared.receiving.load(Ordering::Relaxed));
    }

    #[test]
    fn a_hang_up_during_a_slow_resolve_returns_at_once_and_sends_nothing() {
        let peer = UdpSocket::bind("127.0.0.1:0").expect("bind fake reflector");
        peer.set_nonblocking(true)
            .expect("non-blocking fake reflector");
        let port = peer.local_addr().expect("fake reflector addr").port();
        let (s, _rx) = sink(8_000);
        let mon = started(spawn_with_resolver(
            cfg("127.0.0.1", port, b'A', "AJ7HR"),
            s,
            slow_loopback_resolve,
        ));
        assert!(
            wait_until(|| RESOLVING.load(Ordering::SeqCst), 5_000),
            "the monitor thread must reach the resolver"
        );
        // The resolver is held until RELEASE. Hang up on a helper thread:
        // `disconnect` must return without waiting for resolution, because
        // its real caller is often the node's 50 ms pump.
        let done = Arc::new(AtomicBool::new(false));
        let done_in = Arc::clone(&done);
        let hanger = std::thread::spawn(move || {
            mon.disconnect();
            done_in.store(true, Ordering::SeqCst);
        });
        assert!(
            wait_until(|| done.load(Ordering::SeqCst), 1_000),
            "disconnect must not wait for name resolution"
        );
        hanger.join().expect("the hang-up thread");
        // Only now let the resolver return: the detached thread must see the
        // shutdown and leave without CONN.
        RELEASE.store(true, Ordering::SeqCst);
        assert_eq!(
            next_datagram(&peer, 600),
            None,
            "a monitor hung up during resolution must send nothing at all"
        );
    }

    #[test]
    fn spawn_refuses_a_bad_callsign_module_or_rate_without_a_thread() {
        assert_eq!(
            spawn_err(cfg("127.0.0.1", 9, b'A', "TOOLONGCALL"), 8_000),
            "invalid M17 callsign \"TOOLONGCALL\""
        );
        assert_eq!(
            spawn_err(cfg("127.0.0.1", 9, b'1', "AJ7HR"), 8_000),
            "module must be A-Z"
        );
        assert_eq!(
            spawn_err(cfg("127.0.0.1", 9, b'A', "AJ7HR"), 44_100),
            "unsupported conference rate 44100 Hz"
        );
        assert!(
            cfg("127.0.0.1", 9, b'c', "aj7hr").validate().is_ok(),
            "a lowercase module and callsign are accepted (both are uppercased)"
        );
    }

    #[test]
    fn a_resolve_failure_is_reported_verbatim_and_ends_the_thread() {
        let (s, _rx) = sink(8_000);
        let mon = started(spawn_with_resolver(
            cfg("m17.test", 1, b'A', "AJ7HR"),
            s,
            no_such_host,
        ));
        assert!(
            wait_until(|| mon.finished(), 5_000),
            "the thread must exit on its own"
        );
        let st = mon.status();
        assert_eq!(
            st.fail_reason.as_deref(),
            Some("could not resolve m17.test: test resolver: no such host")
        );
        assert_eq!(st.link, "failed");
        assert_eq!(st.refused_sends, 0);
        assert!(!mon.sink_lost());
        mon.disconnect();
    }

    #[test]
    fn links_with_an_uppercased_module_and_sends_disc_when_dropped() {
        let peer = UdpSocket::bind("127.0.0.1:0").expect("bind fake reflector");
        peer.set_nonblocking(true)
            .expect("non-blocking fake reflector");
        let port = peer.local_addr().expect("fake reflector addr").port();
        let callsign = encode_callsign("AJ7HR").expect("callsign encodes");
        let (s, _rx) = sink(16_000);
        let mon = started(spawn_with_resolver(
            cfg("127.0.0.1", port, b'c', "AJ7HR"),
            s,
            loopback_resolve,
        ));

        let (conn, from) = next_from(&peer, 5_000).expect("the monitor must send CONN");
        assert_eq!(
            ControlPacket::parse(&conn),
            Some(ControlPacket::Conn {
                callsign,
                module: b'C'
            }),
            "the module goes on the wire uppercased"
        );
        assert_eq!(mon.status().link, "linking");

        peer.connect(from).expect("connect to the monitor");
        peer.send(b"ACKN").expect("send ACKN");
        assert!(wait_until(|| mon.status().link == "linked", 5_000));
        assert_eq!(mon.status().refused_sends, 0);

        drop(mon);
        let disc = ControlPacket::Disc {
            callsign: Some(callsign),
        }
        .to_bytes();
        let mut seen: Vec<Vec<u8>> = Vec::new();
        let mut buf = [0u8; 256];
        let got_disc = wait_until(
            || {
                while let Ok(n) = peer.recv(&mut buf) {
                    seen.push(buf[..n].to_vec());
                }
                seen.last() == Some(&disc)
            },
            2_000,
        );
        assert!(
            got_disc,
            "dropping the monitor must send DISC last; saw {seen:?}"
        );
        for d in &seen {
            assert!(
                ControlPacket::parse(d).is_some(),
                "only control packets may leave the monitor: {d:?}"
            );
        }
    }

    #[test]
    fn end_reasons_are_the_shared_strings() {
        assert_eq!(end_reason(RxEnd::Refused), "reflector refused the link");
        assert_eq!(end_reason(RxEnd::Disconnected), "reflector sent DISC");
        assert_eq!(silence_reason(Duration::from_secs(30)), "no reply for 30 s");
        assert_eq!(
            silence_reason(Duration::from_millis(300)),
            "no reply for 300 ms"
        );
        assert_eq!(
            silence_reason(Duration::from_millis(1_500)),
            "no reply for 1500 ms"
        );
    }
}
