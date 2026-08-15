// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `mrefd`-compatible loopback reflector (iax-f2b8 Task 6), plus an optional
//! "parrot" mode (iax-91f4).
//!
//! [`Reflector`] is the server side of the control/stream protocol
//! [`crate::control::ControlPacket`] and [`crate::frame::StreamPacket`]
//! describe: it accepts `CONN` requests onto single-letter modules, keeps
//! each linked client alive with periodic `PING`/`PONG`, and relays every
//! voice-stream packet a client sends to every OTHER client on the SAME
//! module (never back to the sender, never across modules) — exactly the
//! subset of `mrefd` behavior [`crate::fsm::SessionFsm`] (the client side)
//! expects to talk to.
//!
//! Like the client run-loop in `astar-console`'s `M17Session`, this is a
//! single thread owning a plain [`UdpSocket`] with a short read timeout (50
//! ms) — no mio, no async. That timeout is the cadence at which the PING
//! tick, the client-reap sweep, and (in parrot mode) the silence-flush sweep
//! are re-checked, and it bounds how long [`ReflectorHandle::shutdown`] can
//! take to join the thread. Parrot-mode playback pacing is the ONE thing
//! that does NOT settle for the 50 ms cadence: while a playback is draining
//! the sender is unkeyed, so the read timeout is the run-loop's only wake
//! source, and the fixed 50 ms poll is slower than the 40 ms pacing target
//! (an underrun risk, not a rounding error) — so `run_loop` shortens the read
//! timeout on the fly to the earliest pending playback deadline; see
//! `next_read_timeout` and the "Parrot mode" section below.
//!
//! # Reflector callsign
//!
//! `mrefd` reflectors identify themselves in `PING` frames with their own
//! callsign (conventionally `M17-xxx`). This implementation hard-codes
//! `"M17-REF"` rather than taking a configurable callsign — nothing in this
//! milestone's tests or callers needs a different one, and the constructors
//! specified for this task ([`Reflector::bind`]/[`Reflector::bind_with_timeouts`])
//! only take an address and, for the latter, timing knobs. A future caller
//! that needs a distinct on-air identity can add a `callsign` parameter then.
//!
//! # Parrot mode (iax-91f4)
//!
//! [`Reflector::bind_parrot`] (and [`Reflector::bind_parrot_with_timeouts`])
//! turn on an ADDITIVE echo behavior, layered on top of the normal relay:
//! every client's stream packets are also buffered (per sending address,
//! keyed by the stream's `StreamID` so an abandoned stream never bleeds into
//! the next one), and on the EOS-bit packet — or after 2 s of silence from
//! that sender if no EOS ever arrives — the buffered transmission is played
//! back to THAT SENDER ONLY, never to other clients:
//!
//! - Fresh random `StreamID` (never reuses the original).
//! - `DST` = the sender's OWN callsign, re-encoded — the sender dials in as
//!   e.g. `N0CALL`, and this reflector "calls back" addressed to `N0CALL`, so
//!   their own client UI reads it as "you, talking to yourself."
//! - `SRC` = the sender's own callsign too, not `M17-REF` — deliberately, so
//!   the sender's UI shows THEM as the transmitting station (the `AllStar`
//!   55553 parrot mental model: it's an echo of you, not a transmission from
//!   the reflector). See [`Playback`] for where this choice is threaded in.
//! - Frame numbers re-stamped from 0, EOS bit set on the last frame.
//! - Original ~40 ms inter-packet pacing ([`PARROT_PACE_INTERVAL`]) — played
//!   out by [`drain_playbacks`], never all at once (the client run-loop's RX
//!   side has no jitter buffer or pre-buffer cushion; see iax-e2c8). Holding
//!   this pacing to ~40 ms (not the slower 50 ms poll cadence) is why
//!   `run_loop` shortens its read timeout while a playback is pending — see
//!   `next_read_timeout`.
//!
//! Relay to other same-module clients is UNCHANGED and still happens; parrot
//! is purely additive. Per-sender capture is bounded to
//! [`MAX_PARROT_BUFFER_PACKETS`] packets (30 s at the ~40 ms pacing rate) —
//! beyond that the oldest buffered packet is dropped to bound memory.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::address::encode_callsign;
use crate::control::ControlPacket;
use crate::frame::{Lsf, StreamPacket};

/// The reflector's own callsign, used as the `callsign` field of every
/// `PING` it sends. See the module docs' "Reflector callsign" section.
const REFLECTOR_CALLSIGN: &str = "M17-REF";

/// The socket read-timeout (and thus the polling cadence for the PING tick
/// and the reap sweep) — mirrors `astar-console`'s `M17Session` run-loop.
const SOCKET_POLL_TIMEOUT: Duration = Duration::from_millis(50);

/// Default interval between `PING`s sent to each linked client.
const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(3);

/// Default silence window before a linked client is reaped.
const DEFAULT_CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default silence window, in parrot mode, before a sender's in-progress
/// (no-EOS-seen-yet) capture is flushed and played back anyway.
const DEFAULT_PARROT_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Target spacing between consecutive parrot-playback packets — the same
/// ~40 ms cadence a real M17 stream sends at. Enforced by [`drain_playbacks`]
/// against each playback's own `next_send` deadline. NOTE this is SHORTER
/// than [`SOCKET_POLL_TIMEOUT`] (50 ms): a fixed 50 ms poll cadence would be
/// the bottleneck, not "naturally" fine — while a playback is in progress the
/// sender is unkeyed, so the socket read timeout is the run-loop's ONLY wake
/// source, and a deadline shorter than that timeout can never fire on time.
/// `run_loop` compensates by shortening the read timeout to the earliest
/// pending playback's deadline (see `next_read_timeout`) whenever one is
/// due sooner than the normal 50 ms poll.
const PARROT_PACE_INTERVAL: Duration = Duration::from_millis(40);

/// Floor on the dynamically-shortened parrot read timeout (see
/// `next_read_timeout`) — never zero (a zero-duration `set_read_timeout`
/// is invalid, and would busy-spin the run-loop besides).
const MIN_PARROT_READ_TIMEOUT: Duration = Duration::from_millis(2);

/// Memory bound on one sender's parrot capture buffer: 30 s of audio at the
/// ~40 ms per-packet pacing rate (`30_000` / 40 = 750). Beyond this the oldest
/// buffered packet is dropped to make room for the newest — see
/// [`Capture::push`].
const MAX_PARROT_BUFFER_PACKETS: usize = 750;

/// Everything the reflector tracks about one linked client.
struct ClientEntry {
    /// The callsign this client presented in its `CONN`. Tracked per the
    /// task's client-identity contract even though the relay/PING/reap logic
    /// below never needs to read it back — a natural hook for later
    /// diagnostics (e.g. a client-list dump) without changing the table
    /// shape.
    #[allow(dead_code)]
    callsign: [u8; 6],
    module: u8,
    /// Last time ANY packet (control or stream) was received from this
    /// client; a silence of `client_timeout` since reaps the client.
    last_seen: Instant,
    /// Last time this reflector sent a `PING` to this client; a client is
    /// pinged again once `ping_interval` has elapsed since.
    last_ping_sent: Instant,
}

/// One sender's in-progress parrot capture: the buffered payloads of their
/// current transmission, keyed (by the caller, in the run-loop's `captures`
/// map) on the sender's socket address, and internally tagged with the
/// `StreamID` it started on so a new `StreamID` from the same address is
/// recognized as a new transmission rather than a continuation.
struct Capture {
    /// The `StreamID` this capture started on. A packet from the same sender
    /// with a DIFFERENT `StreamID` means the previous stream ended without an
    /// EOS bit ever arriving; the run-loop flushes what's captured so far
    /// under that old `StreamID` before starting a fresh capture.
    stream_id: u16,
    /// The sender's own callsign (their stream packets' `SRC` field) —
    /// carried through unchanged into the eventual [`Playback`]'s `dst`/`src`.
    sender_callsign: [u8; 6],
    /// Buffered Codec 2 payloads, oldest first, bounded to
    /// [`MAX_PARROT_BUFFER_PACKETS`].
    payloads: VecDeque<[u8; 16]>,
    /// Last time a packet was appended — drives the 2 s silence-flush.
    last_packet_at: Instant,
}

impl Capture {
    fn new(pkt: &StreamPacket, now: Instant) -> Capture {
        Capture {
            stream_id: pkt.stream_id,
            sender_callsign: pkt.lsf.src,
            payloads: VecDeque::new(),
            last_packet_at: now,
        }
    }

    /// Appends one payload, dropping the oldest buffered payload first if
    /// already at [`MAX_PARROT_BUFFER_PACKETS`] (bounded memory; oldest audio
    /// is what a real echo test cares least about preserving).
    fn push(&mut self, payload: [u8; 16]) {
        if self.payloads.len() >= MAX_PARROT_BUFFER_PACKETS {
            self.payloads.pop_front();
        }
        self.payloads.push_back(payload);
    }
}

/// A parrot playback in progress: the buffered payloads from one finished (or
/// silence-flushed) capture, being drained back to the ORIGINAL SENDER at the
/// original ~40 ms pacing, one packet per run-loop tick via
/// [`drain_playbacks`].
struct Playback {
    /// `DST` for every packet in this playback: the sender's own callsign, so
    /// their client reads the echo as addressed to them.
    dst: [u8; 6],
    /// `SRC` for every packet in this playback: ALSO the sender's own
    /// callsign (not the reflector's) — see the module docs' "Parrot mode"
    /// section for the rationale (their UI should show them, not `M17-REF`,
    /// as the transmitting station).
    src: [u8; 6],
    /// Fresh random `StreamID`, distinct from the captured transmission's.
    stream_id: u16,
    /// Next frame number to stamp (re-stamped from 0; EOS bit added on the
    /// final packet by [`drain_playbacks`]).
    next_frame: u16,
    /// Remaining payloads to send, oldest first.
    payloads: VecDeque<[u8; 16]>,
    /// Earliest time the next payload may go out — the floor half of
    /// [`PARROT_PACE_INTERVAL`] pacing (never send too SOON). Not sending too
    /// LATE is `next_read_timeout`'s job: it shortens the run-loop's socket
    /// read timeout to wake right as this deadline arrives, rather than
    /// waiting out the (slower) normal poll cadence.
    next_send: Instant,
}

/// An unbound-thread, `mrefd`-compatible loopback reflector: [`Reflector::bind`]
/// opens the socket, [`Reflector::run`] starts the run-loop thread. See the
/// module docs for the full behavior contract.
pub struct Reflector {
    socket: UdpSocket,
    local_addr: SocketAddr,
    ping_interval: Duration,
    client_timeout: Duration,
    parrot: bool,
    parrot_flush_timeout: Duration,
}

impl Reflector {
    /// Binds a reflector to `addr` with the default ping interval (3 s) and
    /// client timeout (30 s). Parrot mode is OFF (plain relay-only reflector,
    /// the iax-f2b8 Task 6 behavior).
    ///
    /// # Errors
    /// Whatever [`UdpSocket::bind`] or `set_read_timeout` returns.
    pub fn bind(addr: SocketAddr) -> io::Result<Reflector> {
        Self::bind_with_timeouts(addr, DEFAULT_PING_INTERVAL, DEFAULT_CLIENT_TIMEOUT)
    }

    /// Binds a reflector to `addr` with caller-supplied `ping_interval` and
    /// `client_timeout` — lets a test shorten both windows rather than
    /// waiting out the real 3 s/30 s defaults. Parrot mode is OFF.
    ///
    /// # Errors
    /// Whatever [`UdpSocket::bind`] or `set_read_timeout` returns.
    pub fn bind_with_timeouts(
        addr: SocketAddr,
        ping_interval: Duration,
        client_timeout: Duration,
    ) -> io::Result<Reflector> {
        Self::bind_inner(
            addr,
            ping_interval,
            client_timeout,
            false,
            DEFAULT_PARROT_FLUSH_TIMEOUT,
        )
    }

    /// Binds a PARROT reflector (iax-91f4) to `addr`: identical CONN/ACKN/
    /// PING/DISC and same-module relay behavior as [`Reflector::bind`], plus
    /// the echo-back-to-sender behavior described in the module docs' "Parrot
    /// mode" section. Default ping interval (3 s), client timeout (30 s), and
    /// silence-flush window (2 s).
    ///
    /// # Errors
    /// Whatever [`UdpSocket::bind`] or `set_read_timeout` returns.
    pub fn bind_parrot(addr: SocketAddr) -> io::Result<Reflector> {
        Self::bind_parrot_with_timeouts(
            addr,
            DEFAULT_PING_INTERVAL,
            DEFAULT_CLIENT_TIMEOUT,
            DEFAULT_PARROT_FLUSH_TIMEOUT,
        )
    }

    /// [`Reflector::bind_parrot`] with caller-supplied `ping_interval`,
    /// `client_timeout`, AND `parrot_flush_timeout` — the last one lets a
    /// test shorten the 2 s no-EOS silence-flush window rather than waiting
    /// it out for real.
    ///
    /// # Errors
    /// Whatever [`UdpSocket::bind`] or `set_read_timeout` returns.
    pub fn bind_parrot_with_timeouts(
        addr: SocketAddr,
        ping_interval: Duration,
        client_timeout: Duration,
        parrot_flush_timeout: Duration,
    ) -> io::Result<Reflector> {
        Self::bind_inner(
            addr,
            ping_interval,
            client_timeout,
            true,
            parrot_flush_timeout,
        )
    }

    fn bind_inner(
        addr: SocketAddr,
        ping_interval: Duration,
        client_timeout: Duration,
        parrot: bool,
        parrot_flush_timeout: Duration,
    ) -> io::Result<Reflector> {
        let socket = UdpSocket::bind(addr)?;
        socket.set_read_timeout(Some(SOCKET_POLL_TIMEOUT))?;
        let local_addr = socket.local_addr()?;
        Ok(Reflector {
            socket,
            local_addr,
            ping_interval,
            client_timeout,
            parrot,
            parrot_flush_timeout,
        })
    }

    /// The address this reflector is bound to (useful when `addr`'s port was
    /// `0` — the OS picks an ephemeral port).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Starts the run-loop thread (named `"iax-m17-refl"`) and returns a
    /// handle to control it. Consumes `self`: the socket moves into the
    /// thread, which owns it exclusively from here on.
    ///
    /// # Panics
    /// If the OS refuses to spawn the thread (mirrors the unconditional
    /// `expect` `astar-console`'s `M17Session` uses for the same
    /// failure — this constructor has no `Result` to propagate it through).
    #[must_use]
    pub fn run(self) -> ReflectorHandle {
        let shutdown = Arc::new(AtomicBool::new(false));
        let client_count = Arc::new(AtomicUsize::new(0));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread_client_count = Arc::clone(&client_count);
        let Reflector {
            socket,
            ping_interval,
            client_timeout,
            parrot,
            parrot_flush_timeout,
            ..
        } = self;
        let thread = std::thread::Builder::new()
            .name("iax-m17-refl".to_string())
            .spawn(move || {
                run_loop(
                    &socket,
                    ping_interval,
                    client_timeout,
                    parrot,
                    parrot_flush_timeout,
                    &thread_shutdown,
                    &thread_client_count,
                );
            })
            .expect("spawn iax-m17-refl thread");
        ReflectorHandle {
            shutdown,
            thread: Some(thread),
            client_count,
        }
    }
}

/// A handle to a running [`Reflector`]'s thread. [`ReflectorHandle::shutdown`]
/// (or dropping the handle) requests the thread stop and joins it, bounded
/// by the reflector's 50 ms socket read timeout.
pub struct ReflectorHandle {
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    client_count: Arc<AtomicUsize>,
}

impl ReflectorHandle {
    /// Requests the run-loop thread stop and joins it. Bounded by the 50 ms
    /// socket read timeout (the loop only checks the shutdown flag between
    /// socket reads).
    pub fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// The current number of linked clients — a test-visible escape hatch
    /// (backed by an `Arc<AtomicUsize>` the run-loop updates every poll) for
    /// asserting a client got reaped without needing a second observable
    /// side effect.
    #[must_use]
    pub fn client_count(&self) -> usize {
        self.client_count.load(Ordering::Relaxed)
    }
}

impl Drop for ReflectorHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Parrot mode (iax-91f4 fix-forward): the socket read timeout to use for
/// THIS loop iteration. Normally [`SOCKET_POLL_TIMEOUT`] (50 ms) — but if any
/// playback has a `next_send` deadline sooner than that, shortened to (about)
/// that deadline instead, so the recv call returns (via `WouldBlock`/
/// `TimedOut`, same as any other poll timeout) right when `drain_playbacks`
/// needs to run again, rather than overshooting the ~40 ms pacing target by
/// waiting out the full 50 ms.
///
/// Floored at [`MIN_PARROT_READ_TIMEOUT`] (never zero — a zero-duration
/// `set_read_timeout` is rejected by the OS, and would busy-spin the loop
/// besides) and capped at [`SOCKET_POLL_TIMEOUT`] (never LONGER than the
/// normal poll cadence, so PING/reap timing is unaffected).
#[must_use]
fn next_read_timeout(playbacks: &HashMap<SocketAddr, Playback>, now: Instant) -> Duration {
    playbacks
        .values()
        .map(|pb| pb.next_send.saturating_duration_since(now))
        .min()
        .map_or(SOCKET_POLL_TIMEOUT, |until| {
            until.clamp(MIN_PARROT_READ_TIMEOUT, SOCKET_POLL_TIMEOUT)
        })
}

/// The `"iax-m17-refl"` run-loop: owns the socket and the client table for
/// as long as the reflector runs. Normally driven by one 50 ms poll cadence
/// (the socket's read timeout) — reacting to an incoming packet, ticking
/// PINGs, reaping silent clients, and (parrot mode only) flushing stale
/// captures and draining in-progress playbacks — mirroring the client-side
/// run-loop in `astar-console`'s `M17Session`. In parrot mode, THIS loop
/// also shortens that read timeout on the fly (via [`next_read_timeout`])
/// whenever a pending playback's ~40 ms pacing deadline is due sooner than
/// the normal 50 ms poll — otherwise the poll cadence itself would bottleneck
/// the pacing, since an unkeyed sender gives the loop no other wake source.
fn run_loop(
    socket: &UdpSocket,
    ping_interval: Duration,
    client_timeout: Duration,
    parrot: bool,
    parrot_flush_timeout: Duration,
    shutdown: &AtomicBool,
    client_count: &AtomicUsize,
) {
    let reflector_callsign =
        encode_callsign(REFLECTOR_CALLSIGN).expect("REFLECTOR_CALLSIGN encodes");
    let mut clients: HashMap<SocketAddr, ClientEntry> = HashMap::new();
    let mut captures: HashMap<SocketAddr, Capture> = HashMap::new();
    let mut playbacks: HashMap<SocketAddr, Playback> = HashMap::new();
    let mut buf = [0u8; 2_048];

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        if parrot {
            let timeout = next_read_timeout(&playbacks, Instant::now());
            let _ = socket.set_read_timeout(Some(timeout));
        }

        match socket.recv_from(&mut buf) {
            Ok((n, src)) => handle_packet(
                socket,
                &buf[..n],
                src,
                &mut clients,
                parrot,
                &mut captures,
                &mut playbacks,
            ),
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            }
            Err(_) => {}
        }

        tick(
            socket,
            reflector_callsign,
            ping_interval,
            client_timeout,
            &mut clients,
        );

        if parrot {
            let now = Instant::now();
            flush_stale_captures(&mut captures, &mut playbacks, parrot_flush_timeout, now);
            drain_playbacks(socket, &mut playbacks, now);
            // A client that DISC'd or got reaped above shouldn't keep
            // receiving (or accumulating) parrot traffic addressed to it.
            captures.retain(|addr, _| clients.contains_key(addr));
            playbacks.retain(|addr, _| clients.contains_key(addr));
        }

        client_count.store(clients.len(), Ordering::Relaxed);
    }
}

/// One run-loop tick (after the socket poll): PING every client whose
/// `ping_interval` has elapsed since its last PING, then reap every client
/// silent past `client_timeout`.
fn tick(
    socket: &UdpSocket,
    reflector_callsign: [u8; 6],
    ping_interval: Duration,
    client_timeout: Duration,
    clients: &mut HashMap<SocketAddr, ClientEntry>,
) {
    let now = Instant::now();
    let ping_bytes = ControlPacket::Ping {
        callsign: reflector_callsign,
    }
    .to_bytes();
    for (addr, client) in clients.iter_mut() {
        if now.duration_since(client.last_ping_sent) >= ping_interval {
            let _ = socket.send_to(&ping_bytes, *addr);
            client.last_ping_sent = now;
        }
    }
    clients.retain(|_, client| now.duration_since(client.last_seen) < client_timeout);
}

/// Parrot mode: flushes every capture that's gone silent (no packet appended)
/// for at least `parrot_flush_timeout`, starting its playback exactly as an
/// EOS-bit packet would have. Called once per run-loop tick.
fn flush_stale_captures(
    captures: &mut HashMap<SocketAddr, Capture>,
    playbacks: &mut HashMap<SocketAddr, Playback>,
    parrot_flush_timeout: Duration,
    now: Instant,
) {
    let stale: Vec<SocketAddr> = captures
        .iter()
        .filter(|(_, cap)| now.duration_since(cap.last_packet_at) >= parrot_flush_timeout)
        .map(|(addr, _)| *addr)
        .collect();
    for addr in stale {
        if let Some(cap) = captures.remove(&addr) {
            start_playback(playbacks, addr, cap);
        }
    }
}

/// Parrot mode: sends at most one due packet per in-progress playback per
/// tick, enforcing [`PARROT_PACE_INTERVAL`] via each playback's own
/// `next_send` deadline. A playback is dropped from the map once its last
/// (EOS-bit) packet has gone out.
fn drain_playbacks(
    socket: &UdpSocket,
    playbacks: &mut HashMap<SocketAddr, Playback>,
    now: Instant,
) {
    playbacks.retain(|addr, pb| {
        if now < pb.next_send {
            return true; // not due this tick yet
        }
        let Some(payload) = pb.payloads.pop_front() else {
            return false; // nothing left to send (shouldn't normally happen — start_playback skips empty captures)
        };
        let is_last = pb.payloads.is_empty();
        let mut frame_number = pb.next_frame;
        if is_last {
            frame_number |= StreamPacket::EOS_BIT;
        }
        let pkt = StreamPacket {
            stream_id: pb.stream_id,
            lsf: Lsf {
                dst: pb.dst,
                src: pb.src,
                type_field: Lsf::TYPE_VOICE_3200_STREAM,
                meta: [0; 14],
            },
            frame_number,
            payload,
        };
        let _ = socket.send_to(&pkt.to_bytes(), *addr);
        pb.next_frame = pb.next_frame.wrapping_add(1);
        pb.next_send = now + PARROT_PACE_INTERVAL;
        !is_last
    });
}

/// Parrot mode: turns a finished [`Capture`] into a queued [`Playback`] keyed
/// on the same sender address, with a fresh random `StreamID` and
/// `DST`/`SRC` both set to the sender's own callsign (see the module docs'
/// "Parrot mode" section). A capture with no buffered payloads at all (e.g.
/// an EOS packet that carried the only frame, already captured, then flushed
/// with nothing further appended — or a stream that silence-flushed before a
/// single payload arrived) produces no playback.
///
/// If a playback is already in progress for this address (the sender keyed up
/// again before their previous echo finished draining), the new one REPLACES
/// it — remaining old payloads are dropped. A dev-tool-grade simplification:
/// queueing multiple pending playbacks per sender isn't needed for the
/// echo-test use case this exists for.
fn start_playback(playbacks: &mut HashMap<SocketAddr, Playback>, addr: SocketAddr, cap: Capture) {
    if cap.payloads.is_empty() {
        return;
    }
    playbacks.insert(
        addr,
        Playback {
            dst: cap.sender_callsign,
            src: cap.sender_callsign,
            stream_id: rand::random(),
            next_frame: 0,
            payloads: cap.payloads,
            next_send: Instant::now(),
        },
    );
}

/// Parrot mode: appends `pkt`'s payload to `src`'s capture (starting a fresh
/// one if none is in progress, or if `pkt` carries a DIFFERENT `StreamID`
/// than the capture already in progress — see [`Capture::stream_id`]), then
/// starts playback immediately if `pkt` carries the EOS bit.
///
/// EDGE CASE: `StreamID` collision. If the SAME sender starts a genuinely new
/// transmission whose random `StreamID` happens to collide with the one an
/// already-in-progress (no-EOS-yet) capture is using, `needs_fresh` below is
/// `false` — the new packets silently APPEND onto the old capture instead of
/// starting a fresh one, merging what are logically two separate
/// transmissions into one buffered/played-back stream. Accepted: a 16-bit
/// `StreamID` collision between two back-to-back transmissions from the same
/// station is rare, the merged capture is still bounded by
/// [`MAX_PARROT_BUFFER_PACKETS`], and nothing panics or leaks — just a worse
/// echo for that one unlucky pair of transmissions.
fn parrot_capture(
    captures: &mut HashMap<SocketAddr, Capture>,
    playbacks: &mut HashMap<SocketAddr, Playback>,
    src: SocketAddr,
    pkt: &StreamPacket,
    now: Instant,
) {
    let needs_fresh = captures
        .get(&src)
        .is_none_or(|cap| cap.stream_id != pkt.stream_id);
    if needs_fresh {
        if let Some(stale) = captures.remove(&src) {
            // A new StreamID arrived without the old one ever seeing EOS —
            // flush what was captured under it before starting fresh.
            start_playback(playbacks, src, stale);
        }
        captures.insert(src, Capture::new(pkt, now));
    }
    let cap = captures
        .get_mut(&src)
        .expect("just inserted above if absent");
    cap.push(pkt.payload);
    cap.last_packet_at = now;
    if pkt.is_last()
        && let Some(finished) = captures.remove(&src)
    {
        start_playback(playbacks, src, finished);
    }
}

/// Reacts to one received packet from `src`:
///
/// - `CONN`: registers the client (or replaces its module/callsign if it
///   was already linked) and replies `ACKN` for an A-Z module, `NACK`
///   otherwise.
/// - Anything else from an address that ISN'T a currently-linked client is
///   ignored outright — including `DISC`/`PONG`/stream packets.
/// - Any packet at all from a known client refreshes `last_seen` — not just
///   recognized ones (a client is "alive" if it's talking, even if this
///   reflector can't parse what it sent).
/// - `DISC` from a known client: reply with the bare (no-callsign) `DISC`
///   acknowledgement and drop the client immediately (and, in parrot mode,
///   any in-progress capture/playback for it).
/// - A valid 54-byte `"M17 "` stream packet from a known client: relay the
///   exact bytes to every OTHER client on the SAME module (unchanged by
///   parrot mode), and — when `parrot` is on — also buffer it into that
///   sender's capture via [`parrot_capture`].
#[allow(clippy::too_many_arguments)]
fn handle_packet(
    socket: &UdpSocket,
    buf: &[u8],
    src: SocketAddr,
    clients: &mut HashMap<SocketAddr, ClientEntry>,
    parrot: bool,
    captures: &mut HashMap<SocketAddr, Capture>,
    playbacks: &mut HashMap<SocketAddr, Playback>,
) {
    let now = Instant::now();

    if let Some(ControlPacket::Conn { callsign, module }) = ControlPacket::parse(buf) {
        if module.is_ascii_uppercase() {
            clients.insert(
                src,
                ClientEntry {
                    callsign,
                    module,
                    last_seen: now,
                    last_ping_sent: now,
                },
            );
            let _ = socket.send_to(&ControlPacket::Ackn.to_bytes(), src);
        } else {
            let _ = socket.send_to(&ControlPacket::Nack.to_bytes(), src);
        }
        return;
    }

    // Everything below requires an already-linked client; unknown senders
    // are silently ignored. Any packet at all from a known client refreshes
    // `last_seen` (per the task brief: "PONG (and any packet) from a known
    // client updates last_seen") — set it unconditionally here, before
    // branching on what the packet actually is.
    let Some(client) = clients.get_mut(&src) else {
        return;
    };
    client.last_seen = now;
    let module = client.module;

    if let Some(ControlPacket::Disc { .. }) = ControlPacket::parse(buf) {
        let _ = socket.send_to(&ControlPacket::Disc { callsign: None }.to_bytes(), src);
        clients.remove(&src);
        captures.remove(&src);
        playbacks.remove(&src);
        return;
    }

    if let Some(pkt) = StreamPacket::parse(buf) {
        for (addr, client) in clients.iter() {
            if *addr != src && client.module == module {
                let _ = socket.send_to(buf, *addr);
            }
        }
        if parrot {
            parrot_capture(captures, playbacks, src, &pkt, now);
        }
    }
    // Anything else (a stray PONG/ACKN/NACK/PING, or an unrecognized
    // magic/length): last_seen is already refreshed above; nothing further
    // to do.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::encode_callsign;

    // --- iax-f7a3: the parrot pacing deadline, tested deterministically ---
    //
    // These replace a wall-clock assertion on observed inter-packet arrival
    // gaps. That test measured the machine as much as the code: it failed CI at
    // 48.03ms against a 48ms ceiling (27µs over), and again at 44.00035ms
    // against 44ms even running alone on an idle box — the runner simply paces
    // on a coarser grid than the development Mac. The property that actually
    // regressed is arithmetic, so it is tested as arithmetic.

    fn addr(n: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], n))
    }

    fn playback_due_at(next_send: Instant) -> Playback {
        Playback {
            dst: [0; 6],
            src: [0; 6],
            stream_id: 0,
            next_frame: 0,
            payloads: VecDeque::new(),
            next_send,
        }
    }

    #[test]
    fn no_playbacks_polls_at_the_normal_cadence() {
        assert_eq!(
            next_read_timeout(&HashMap::new(), Instant::now()),
            SOCKET_POLL_TIMEOUT
        );
    }

    #[test]
    fn a_pending_playback_shortens_the_read_timeout_to_its_deadline() {
        // THE regression guard. The run-loop used a fixed 50ms socket read
        // timeout, so a 40ms pacing deadline could not be met and playback
        // drifted to a measured ~50.9ms per packet. With a deadline 40ms out,
        // the timeout must be that 40ms — not the full poll.
        let now = Instant::now();
        let mut playbacks = HashMap::new();
        playbacks.insert(addr(1), playback_due_at(now + PARROT_PACE_INTERVAL));
        let timeout = next_read_timeout(&playbacks, now);
        assert_eq!(timeout, PARROT_PACE_INTERVAL);
        assert!(
            timeout < SOCKET_POLL_TIMEOUT,
            "a pending playback must not wait out the full poll cadence: {timeout:?}"
        );
    }

    #[test]
    fn the_earliest_deadline_across_playbacks_wins() {
        // Several clients parroting at once: the loop must wake for whichever
        // is due first, or the others starve that one.
        let now = Instant::now();
        let mut playbacks = HashMap::new();
        playbacks.insert(addr(1), playback_due_at(now + Duration::from_millis(30)));
        playbacks.insert(addr(2), playback_due_at(now + Duration::from_millis(10)));
        playbacks.insert(addr(3), playback_due_at(now + Duration::from_millis(45)));
        assert_eq!(
            next_read_timeout(&playbacks, now),
            Duration::from_millis(10)
        );
    }

    #[test]
    fn an_overdue_playback_floors_at_the_minimum_and_never_zero() {
        // A zero-duration set_read_timeout is rejected by the OS, and would
        // busy-spin the loop besides.
        let now = Instant::now();
        let mut playbacks = HashMap::new();
        let overdue = now
            .checked_sub(Duration::from_millis(500))
            .expect("test clock has headroom");
        playbacks.insert(addr(1), playback_due_at(overdue));
        let timeout = next_read_timeout(&playbacks, now);
        assert_eq!(timeout, MIN_PARROT_READ_TIMEOUT);
        assert!(!timeout.is_zero(), "never zero");
    }

    #[test]
    fn a_distant_deadline_is_capped_at_the_normal_cadence() {
        // PING ticking and client reaping ride this same poll, so the timeout
        // must never stretch past it however far off the playback is.
        let now = Instant::now();
        let mut playbacks = HashMap::new();
        playbacks.insert(addr(1), playback_due_at(now + Duration::from_secs(5)));
        assert_eq!(next_read_timeout(&playbacks, now), SOCKET_POLL_TIMEOUT);
    }

    #[test]
    fn reflector_callsign_encodes_and_pings_use_it() {
        // Guards the module doc's hard-coded-callsign claim: REFLECTOR_CALLSIGN
        // must itself be a valid base-40 callsign (encode_callsign succeeds),
        // and a PING built from it round-trips through ControlPacket.
        let cs = encode_callsign(REFLECTOR_CALLSIGN).expect("valid callsign");
        let ping = ControlPacket::Ping { callsign: cs };
        assert_eq!(
            ControlPacket::parse(&ping.to_bytes()),
            Some(ControlPacket::Ping { callsign: cs })
        );
    }
}
