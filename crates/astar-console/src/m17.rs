// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `M17Session`: a full-transceive M17 reflector client runtime (iax-f2b8
//! Task 3).
//!
//! Unlike [`crate::session::ConsoleSession`] (IAX2/`AllStar`), M17 has no
//! separate call-setup handshake or protocol-level PTT frame: a `CONN`/`ACKN`
//! exchange with a reflector brings the link up, and transmission is carried
//! entirely by voice-stream packets (start of a stream = key-down, the `EOS`
//! bit = key-up).
//!
//! The session does **not** own its audio. It is handed a [`CallAudio`] — the
//! two channel ends of the one lane `ConsoleSession` opened on the station's
//! router (`crate::voice_route`) — and owns nothing else about audio: no
//! [`astar_audio::AudioRouter`], no mic or bus id, no meter mirror, no
//! preference cell, no spectrum copy, and no PTT gate. Meters, DSP
//! preferences and keying are read and driven once, at the lane, by
//! `ConsoleSession`. What [`M17Session`] does own:
//!
//! - a [`SessionFsm`] (link state + keepalive);
//! - a [`Codec2Voice`] instance (Codec 2 mode 3200, the rate M17 payloads
//!   use);
//! - ONE run-loop thread ("iax-m17") that owns the `UdpSocket` (plain,
//!   `set_read_timeout(50ms)` — no mio, no async) and drives all of the
//!   above.
//!
//! The control-side [`M17Session`] handle talks to the run-loop thread only
//! through a small set of atomics (poll-cheap, per [`M17SnapshotState`]) plus
//! a request flag for PTT — never a shared/locked `Codec2Voice`, so that stays
//! single-threaded (owned entirely by the run-loop) with no cross-thread
//! synchronization on the hot audio path.
//!
//! # RX jitter handling (documented choice)
//!
//! This milestone's RX path is a simple in-order pass-through: packets are
//! decoded and forwarded to [`astar_audio::router::CallAudio::rx_frames`]
//! in arrival order, with no reordering/jitter-smoothing buffer. `receiving`
//! reflects "a stream packet has arrived within the last 400 ms", not
//! anything about buffer health. The loopback tests exercise same-process
//! UDP (negligible jitter/reordering), so this is sufficient to validate the
//! milestone; a `astar_codec::jitter::JitterBuf`-backed reorder stage is
//! the natural follow-on hardening before this ships against a real
//! Internet-routed reflector.

use std::net::{ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use astar_audio::CallAudio;
use astar_codec::codec2::Codec2Voice;
use astar_m17::{
    BROADCAST, ControlPacket, FsmAction, LinkState, Lsf, SessionFsm, StreamPacket, encode_callsign,
};

use crate::session::ConsoleError;

/// How long the RX path waits, after the last voice-stream packet, before
/// clearing [`M17SnapshotState::receiving`] back to `false`.
const RX_SILENCE_TIMEOUT: Duration = Duration::from_millis(400);

/// The run-loop thread's socket read timeout: also the cadence at which PTT
/// edges, the FSM keepalive tick, and the RX silence timeout are all
/// re-checked. 50 ms per the Task 3 design brief (a plain read-timeout loop,
/// not mio/async).
const SOCKET_POLL_TIMEOUT: Duration = Duration::from_millis(50);

/// Operator-supplied configuration for an [`M17Session`].
pub struct M17Config {
    /// Reflector hostname or IP address.
    pub host: String,
    /// Reflector UDP port (the M17 IP-framing default is 17000, but this is
    /// caller-supplied — no protocol default is assumed here).
    pub port: u16,
    /// Reflector module letter (e.g. `b'A'`).
    pub module: u8,
    /// This station's callsign (encoded via [`encode_callsign`]; invalid
    /// callsigns fail [`M17Session::connect`] with [`ConsoleError::Device`]).
    pub callsign: String,
    /// Extra directories to search for a runtime `libcodec2` (ahead of the
    /// hard-coded system paths); see [`astar_codec::open_codec2`].
    pub codec_dirs: Vec<std::path::PathBuf>,
    /// How long to wait, with no packet from the reflector, before declaring
    /// the link [`LinkState::Failed`]. Default (via a fresh [`SessionFsm`])
    /// is 30 s; tests shorten this to avoid a 30-real-second wait.
    pub keepalive_timeout: Duration,
}

/// A poll-cheap snapshot of an [`M17Session`]'s live state. Backed by atomics
/// on the control side — [`M17Session::state`] never blocks on the run-loop
/// thread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct M17SnapshotState {
    /// Current reflector link state.
    pub link: LinkState,
    /// `true` while transmit is keyed (mirrors the last [`M17Session::set_ptt`]
    /// applied by the run-loop; the apply is bounded by one
    /// [`SOCKET_POLL_TIMEOUT`] tick, ~50 ms).
    pub ptt: bool,
    /// `true` while voice-stream packets have arrived from the reflector
    /// within the last 400 ms.
    pub receiving: bool,
}

/// Atomics shared between the control-side [`M17Session`] and its run-loop
/// thread. `link` is written by the run-loop and read by
/// [`M17Session::state`]; `ptt`/`receiving` follow the same direction. Levels
/// are NOT here: they are read at the lane, by `ConsoleSession`.
struct SharedState {
    link: AtomicU8,
    ptt: AtomicBool,
    receiving: AtomicBool,
}

impl SharedState {
    fn new() -> Self {
        Self {
            link: AtomicU8::new(link_to_u8(LinkState::Idle)),
            ptt: AtomicBool::new(false),
            receiving: AtomicBool::new(false),
        }
    }

    fn snapshot(&self) -> M17SnapshotState {
        M17SnapshotState {
            link: u8_to_link(self.link.load(Ordering::Relaxed)),
            ptt: self.ptt.load(Ordering::Relaxed),
            receiving: self.receiving.load(Ordering::Relaxed),
        }
    }
}

fn link_to_u8(s: LinkState) -> u8 {
    match s {
        LinkState::Idle => 0,
        LinkState::Connecting => 1,
        LinkState::Linked => 2,
        LinkState::Failed => 3,
    }
}

fn u8_to_link(v: u8) -> LinkState {
    match v {
        1 => LinkState::Connecting,
        2 => LinkState::Linked,
        3 => LinkState::Failed,
        _ => LinkState::Idle,
    }
}

/// A full-transceive M17 reflector client: connects, keys/unkeys transmit,
/// and decodes received voice — see the module docs for the architecture.
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
    /// carries no preference, and never touches the PTT gate — see the
    /// module docs.
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
    /// run-loop on its next poll (bounded by [`SOCKET_POLL_TIMEOUT`], ~50 ms)
    /// — this call itself never blocks.
    pub fn set_ptt(&mut self, on: bool) {
        self.ptt_request.store(on, Ordering::Relaxed);
    }

    /// A poll-cheap snapshot of the session's current state.
    #[must_use]
    pub fn state(&self) -> M17SnapshotState {
        self.shared.snapshot()
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

/// Resolves `host:port` and returns a connected, read-timeout-armed
/// [`UdpSocket`] ready for the run-loop — the [`M17Session::connect`] half of
/// the iax-m17-localhost fix, split out to keep `connect` under Clippy's
/// line-count lint.
///
/// `to_socket_addrs` can yield MULTIPLE candidates in resolution order —
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
/// # Errors
/// [`ConsoleError::Resolve`] if `host`/`port` resolve to no addresses, or
/// no candidate can be bound+connected, or the chosen socket's read timeout
/// can't be set.
fn connect_udp_socket(host: &str, port: u16) -> Result<UdpSocket, ConsoleError> {
    let resolve_err = || ConsoleError::Resolve {
        node: host.to_string(),
        source: std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            format!("could not resolve {host}:{port}"),
        ),
    };
    let candidates: Vec<std::net::SocketAddr> = (host, port)
        .to_socket_addrs()
        .map(Iterator::collect)
        .unwrap_or_default();
    let mut connected = None;
    for candidate in &candidates {
        let bind_addr = if candidate.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        };
        let Ok(candidate_socket) = UdpSocket::bind(bind_addr) else {
            continue;
        };
        if candidate_socket.connect(candidate).is_ok() {
            connected = Some(candidate_socket);
            break;
        }
    }
    let socket = connected.ok_or_else(resolve_err)?;
    socket
        .set_read_timeout(Some(SOCKET_POLL_TIMEOUT))
        .map_err(|e| ConsoleError::Resolve {
            node: host.to_string(),
            source: e,
        })?;
    Ok(socket)
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
}

impl TxState {
    fn new() -> Self {
        Self {
            stream_id: 0,
            frame_no: 0,
            pending: None,
        }
    }

    /// Key-down edge: fresh random `StreamID`, counter restarts at 0. Also
    /// drains and discards anything already sitting in `call_audio.tx_frames`
    /// before resetting, so no leftover audio can open a fresh transmission
    /// under a new `StreamID`.
    ///
    /// The gate belongs to `ConsoleSession::set_ptt` now, not to this run
    /// loop, and it opens up to one [`SOCKET_POLL_TIMEOUT`] tick before this
    /// edge is observed. [`run_loop`] therefore drains and drops `tx_frames`
    /// on every tick it is NOT transmitting, which is what keeps the amount
    /// this discard can ever see down to a single tick — and what stops a
    /// mic left open by the route (or a run-loop-forced unkey on a lost
    /// link) from accumulating audio in the channel indefinitely.
    fn key_down(&mut self, call_audio: &CallAudio) {
        while call_audio.tx_frames.try_recv().is_ok() {}
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
    let mut last_rx_voice: Option<Instant> = None;
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
        //    While NOT transmitting, drain and DROP instead. The gate is
        //    `ConsoleSession::set_ptt`'s and it can be open while this loop
        //    is not keyed — it opens up to one poll tick before the key-down
        //    edge lands here, and it stays open after a run-loop-forced unkey
        //    (link lost) until the operator physically releases PTT. Without
        //    this the mic lane would pile audio into `tx_frames` unbounded
        //    and the next transmission would open with somebody's stale
        //    speech.
        if keyed {
            drain_tx_frames(&socket, callsign, codec.as_mut(), &mut tx, &call_audio);
        } else {
            while call_audio.tx_frames.try_recv().is_ok() {}
        }

        // 3. Socket poll (bounded by SOCKET_POLL_TIMEOUT): react to whatever
        //    the FSM says about a received packet.
        poll_socket(
            &socket,
            &mut buf,
            &mut fsm,
            codec.as_mut(),
            &call_audio,
            &shared,
            &mut last_rx_voice,
        );

        // 4. Keepalive tick (answers PING with PONG via FsmAction::Send;
        //    declares Failed after the configured silence window).
        if let FsmAction::Send(bytes) = fsm.tick(Instant::now()) {
            let _ = socket.send(&bytes);
        }
        shared
            .link
            .store(link_to_u8(fsm.state()), Ordering::Relaxed);

        // 5. RX silence timeout: no voice-stream packet in 400 ms clears
        //    `receiving`.
        if let Some(t) = last_rx_voice
            && t.elapsed() >= RX_SILENCE_TIMEOUT
        {
            shared.receiving.store(false, Ordering::Relaxed);
            last_rx_voice = None;
        }
    }
    // `socket` and the `CallAudio` channel ends drop here; the lane's streams
    // belong to the station router and outlive this thread.
}

/// Run-loop step 1: apply a pending PTT edge. M17 carries PTT purely via
/// stream start/`EOS` (there is no protocol PTT frame). The lane's gate is
/// NOT touched here — `ConsoleSession::set_ptt` opened it before forwarding
/// the key and closes it on key-up; this is the protocol edge only.
///
/// Key-down: discards whatever is still queued in `call_audio.tx_frames`
/// (see [`TxState::key_down`]) and starts a fresh random `StreamID`.
///
/// Key-up: drains whatever the lane already queued into ordinary packets
/// ([`drain_tx_frames`]) — up to one [`SOCKET_POLL_TIMEOUT`] tick's worth of
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
        tx.key_down(call_audio);
    } else {
        drain_tx_frames(socket, callsign, codec, tx, call_audio);
        send_voice_packet(
            socket,
            callsign,
            codec,
            tx.stream_id,
            tx.frame_no,
            true,
            tx.pending.take().as_ref(),
            None,
        );
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
    call_audio: &CallAudio,
) {
    while let Ok(frame) = call_audio.tx_frames.try_recv() {
        let Some(pcm) = frame_to_array(&frame) else {
            continue; // defensive: StreamConfig::default() guarantees len 160
        };
        if let Some(first) = tx.pending.take() {
            let frame_no = tx.next_frame_no();
            send_voice_packet(
                socket,
                callsign,
                codec,
                tx.stream_id,
                frame_no,
                false,
                Some(&first),
                Some(&pcm),
            );
        } else {
            tx.pending = Some(pcm);
        }
    }
}

/// Run-loop step 3: poll the socket (bounded by [`SOCKET_POLL_TIMEOUT`]) and
/// react to whatever the FSM says about a received packet: reply to a
/// keepalive `PING`, decode+forward a voice-stream packet (bumping
/// `last_rx_voice`/`receiving`), or do nothing for anything else (including
/// a fresh `Unlinked`, which the run-loop's own `fsm.state()` read picks up
/// regardless of which path set it).
fn poll_socket(
    socket: &UdpSocket,
    buf: &mut [u8],
    fsm: &mut SessionFsm,
    codec: &mut dyn Codec2Voice,
    call_audio: &CallAudio,
    shared: &SharedState,
    last_rx_voice: &mut Option<Instant>,
) {
    match socket.recv(buf) {
        Ok(n) => {
            let action = fsm.on_packet(&buf[..n], Instant::now());
            match action {
                FsmAction::Send(bytes) => {
                    let _ = socket.send(&bytes);
                }
                FsmAction::Voice(pkt) => {
                    *last_rx_voice = Some(Instant::now());
                    shared.receiving.store(true, Ordering::Relaxed);
                    decode_and_forward(codec, &pkt, call_audio);
                }
                FsmAction::Unlinked | FsmAction::None => {}
            }
        }
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => {}
        Err(_) => {}
    }
}

/// Decode one received [`StreamPacket`]'s 16-byte Codec 2 payload (two 8-byte
/// mode-3200 chunks) into 2×160 PCM samples and forward them to
/// `call_audio.rx_frames`. A pure in-order pass-through — see the module
/// docs' "RX jitter handling" section for why no reorder buffer is used yet.
fn decode_and_forward(codec: &mut dyn Codec2Voice, pkt: &StreamPacket, call_audio: &CallAudio) {
    let mut bits_a = [0u8; 8];
    bits_a.copy_from_slice(&pkt.payload[0..8]);
    let mut bits_b = [0u8; 8];
    bits_b.copy_from_slice(&pkt.payload[8..16]);
    let pcm_a = codec.decode(&bits_a);
    let pcm_b = codec.decode(&bits_b);
    let _ = call_audio.rx_frames.send(pcm_a.to_vec());
    let _ = call_audio.rx_frames.send(pcm_b.to_vec());
}

/// Build and send one voice-stream packet. `a`/`b` are the two 160-sample
/// halves; `None` for a half means "send zero bytes for this half" —
/// literal silence, not a Codec 2-encoded silent frame — which is exactly
/// what the key-up flush needs for a fully- or partially-empty final packet
/// (see [`M17Session`]'s module docs and the Task 3 design brief: "zero-pad
/// the second half if only one frame is pending; if zero frames pending,
/// send an EOS packet with all-zero payload").
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
) {
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

    /// Build a [`CallAudio`] by hand — no `AudioRouter`/`MicLane` involved —
    /// so a test can push raw frames straight onto the exact channel
    /// `TxState`/`apply_ptt_edge`/`drain_tx_frames` read from. Returns the
    /// `CallAudio` plus the `Sender` half of `tx_frames` (the test's stand-in
    /// for "the mic lane already queued this") and the `Receiver` half of
    /// `rx_frames` (unused by the TX-side tests below, but part of the real
    /// struct).
    fn fake_call_audio() -> (CallAudio, Sender<Vec<i16>>, Receiver<Vec<i16>>) {
        let (tx_tx, tx_rx) = channel::<Vec<i16>>();
        let (rx_tx, rx_rx) = channel::<Vec<i16>>();
        let call_audio = CallAudio {
            tx_frames: tx_rx,
            rx_frames: rx_tx,
            preroll_lead: Arc::new(AtomicU32::new(0)),
        };
        (call_audio, tx_tx, rx_rx)
    }

    #[test]
    fn key_down_discards_stale_frames_left_in_the_channel() {
        // Simulates the gate window `ConsoleSession::set_ptt` opens up to
        // one poll tick before this run loop observes the key-down edge, or
        // any other leftover-frame race: something pushed frames onto
        // `tx_frames` before this key-down ever ran.
        let (call_audio, push, _rx) = fake_call_audio();
        push.send(vec![1_i16; 160]).unwrap();
        push.send(vec![2_i16; 160]).unwrap();

        let mut tx = TxState::new();
        tx.key_down(&call_audio);

        assert!(
            call_audio.tx_frames.try_recv().is_err(),
            "key_down must drain/discard anything already queued before a fresh transmission starts"
        );
        assert!(tx.pending.is_none());
    }

    #[test]
    fn unkey_drains_queued_frames_before_the_eos_flush() {
        // Reproduces the reviewed race directly: three frames (a full pair
        // plus one odd frame) are sitting in `tx_frames` — audio the mic
        // lane queued in the ~50ms since the run-loop's last drain — when
        // the unkey edge is observed. The fix must send them as an ordinary
        // (non-EOS) packet BEFORE the EOS-flushed final packet, not drop
        // them and not leak them into the next transmission.
        let recv_sock = UdpSocket::bind("127.0.0.1:0").expect("bind recv socket");
        recv_sock
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        let recv_addr = recv_sock.local_addr().expect("recv addr");
        let send_sock = UdpSocket::bind("127.0.0.1:0").expect("bind send socket");
        send_sock.connect(recv_addr).expect("connect send socket");

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
}
