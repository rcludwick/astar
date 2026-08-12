// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A loopback `DExtra`-compatible reflector, plus an optional "parrot" echo
//! mode — this crate's ONLY I/O module (everything else in `astar-dstar`
//! is pure decode/encode with no sockets or threads; see the crate root
//! doc). Mirrors `astar_m17::reflector`'s shape and design decisions
//! (socket read-timeout run-loop, shutdown flag, `client_count` escape
//! hatch, timeout injection for tests) — see that module for the fuller
//! rationale write-up; only the differences are called out here.
//!
//! [`Reflector`] speaks the wire layout [`crate::fsm::DextraFsm`] (the
//! client side) expects, per that module's doc "Wire layout" section and
//! research §2: an 11-byte connect (ACKed/NAKed with a 14-byte reply), a
//! 9-byte keepalive (liveness-refresh only — see below), an 11-byte unlink
//! (dest module blanked to `' '`, answered with the literal 12-byte
//! `"DISCONNECTED"`), and DSVT header/voice packets
//! ([`crate::dsvt::DsvtPacket`]) relayed verbatim to every OTHER linked
//! client on the SAME dest module.
//!
//! # Keepalive direction (no echo)
//!
//! An inbound client keepalive ONLY refreshes `last_seen`; this reflector
//! never replies to one. The reflector→client direction is covered
//! entirely by the periodic `tick` ping (every `keepalive_interval`, see
//! [`tick`]). This matches real `xlxd`: an inbound client keepalive is
//! liveness-refresh only, never answered — the reflector's own periodic
//! tick ping is what the client answers. Echoing an inbound keepalive back
//! (an earlier version of this reflector did) sets up an unbounded
//! ping-pong storm against any real client (or [`crate::fsm::DextraFsm`]),
//! which correctly answers every keepalive it receives per the client-side
//! contract: "reflector pings, client answers" is the only direction that
//! terminates.
//!
//! Unlike `astar_m17::reflector`, this module needs no `rand`: a parrot
//! playback replays the captured stream's `StreamID` **verbatim** (per this
//! task's constraints — no fresh random id), so the crate stays std-only.
//!
//! # Parrot mode
//!
//! [`Reflector::bind_parrot`] (and [`Reflector::bind_parrot_with_timeouts`])
//! turn on an ADDITIVE echo behavior, layered on top of the normal relay:
//! a client's stream — one [`crate::dsvt::DsvtPacket::Header`] followed by
//! voice frames — is buffered as it arrives, and on the EOT-bit voice frame
//! the whole buffered transmission (header, then every voice frame, in
//! order, byte-for-byte as received) is replayed back to THAT SENDER ONLY,
//! after a short (default 150ms) delay — not paced frame-by-frame like
//! `astar_m17::reflector`'s parrot: this crate has no client run-loop
//! consuming the echo in real time (it's a protocol/decode library, not a
//! live audio pipeline), so there's nothing for per-frame pacing to protect
//! against; one flat pre-playback delay (long enough to be observably
//! distinct from "instant relay," short enough not to slow tests down) is
//! simpler and sufficient.
//!
//! While a playback is queued or draining for a given sender, any further
//! header/voice packets from that SAME sender are ignored for capture
//! purposes (relay to other clients is unaffected) — "ignore overlapping
//! streams while replaying" per the task brief. A capture with no header
//! ever seen (an orphan voice frame, or a voice frame whose `StreamID`
//! doesn't match the header currently buffered) is dropped rather than
//! guessed at.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::dsvt::DsvtPacket;

/// This reflector's own identity, used as the 8-byte callsign field of every
/// ACK/NAK/keepalive it sends. Real `DExtra` reflectors bake their reflector
/// module letter into this field (e.g. `"XRF757 G"`, per `fsm.rs`'s test
/// fixtures) — this implementation hard-codes a single fixed 8-byte value
/// instead, mirroring `astar_m17::reflector`'s single hard-coded
/// `REFLECTOR_CALLSIGN` rather than threading a configurable/per-module
/// identity through: nothing in this milestone's tests or callers reads this
/// field back (`DextraFsm::on_packet` only checks the trailing 4 bytes of an
/// ACK/NAK, and ignores the keepalive echo's callsign entirely), and the
/// constructors specified for this task only take an address and timing
/// knobs.
const REFLECTOR_CALLSIGN: [u8; 8] = *b"DSTR-REF";

/// Dest-module byte a connect/unlink request blanks to signal "unlink"
/// (research §2, matching `crate::fsm`'s `BLANK_MODULE`).
const BLANK_MODULE: u8 = b' ';

const LINK_REQUEST_LEN: usize = 11;
const ACK_NAK_LEN: usize = 14;
const KEEPALIVE_LEN: usize = 9;
const DISCONNECTED: &[u8] = b"DISCONNECTED";

/// The socket read-timeout (and thus the polling cadence for the keepalive
/// tick, the client-reap sweep, and the parrot playback sweep) — mirrors
/// `astar_m17::reflector`'s `SOCKET_POLL_TIMEOUT`. Parrot mode's ~150ms
/// replay delay is comfortably coarser than this cadence, so (unlike the
/// m17 reflector) no dynamic timeout-shortening is needed here.
const SOCKET_POLL_TIMEOUT: Duration = Duration::from_millis(50);

/// Default interval between keepalives sent to each linked client (research
/// §2: xlxd `DEXTRA_KEEPALIVE_PERIOD=3`).
const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(3);

/// Default silence window before a linked client is reaped (research §2:
/// xlxd `DEXTRA_KEEPALIVE_TIMEOUT=30`).
const DEFAULT_CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default delay between a parrot capture completing (EOT bit seen) and its
/// playback starting.
const DEFAULT_PARROT_REPLAY_DELAY: Duration = Duration::from_millis(150);

/// Cap on simultaneously linked clients; a connect request received while at
/// capacity is `NAKed` rather than accepted (research doesn't pin a specific
/// reflector-side capacity — this is a defensive bound for a loopback/dev
/// reflector, not a claim about real `xlxd`/`XRF` limits).
const MAX_LINKED_CLIENTS: usize = 32;

/// Memory bound on one sender's in-progress parrot capture, counting voice
/// frames only (the header is kept separately and never evicted): roughly
/// 30s of D-Star audio at 20ms/frame (`30_000 / 20 = 1_500`). Beyond this
/// the oldest buffered voice frame is dropped to bound memory — mirrors
/// `astar_m17::reflector::MAX_PARROT_BUFFER_PACKETS`.
const MAX_CAPTURE_VOICE_FRAMES: usize = 1_500;

/// Everything the reflector tracks about one linked client.
struct ClientEntry {
    /// The dest module this client linked into (research §2's `dest_module`)
    /// — governs relay routing (same-module clients only) exactly like
    /// `astar_m17::reflector::ClientEntry::module`.
    module: u8,
    /// Last time ANY packet was received from this client; a silence of
    /// `client_timeout` since reaps the client.
    last_seen: Instant,
    /// Last time this reflector sent a keepalive to this client; a client is
    /// pinged again once `keepalive_interval` has elapsed since.
    last_keepalive_sent: Instant,
}

/// One sender's in-progress parrot capture: the header that started it plus
/// whatever voice frames have arrived since, all as raw wire bytes (so
/// replay is a verbatim byte-for-byte resend, no re-encoding).
struct Capture {
    /// The header packet's `StreamID` — a voice frame carrying a DIFFERENT
    /// `StreamID` means this capture was abandoned without ever seeing EOT;
    /// see [`parrot_capture`].
    stream_id: u16,
    header_bytes: Vec<u8>,
    /// Wire bytes of each voice frame received so far, oldest first,
    /// bounded to [`MAX_CAPTURE_VOICE_FRAMES`].
    voice_frames: VecDeque<Vec<u8>>,
}

/// A finished capture queued for playback: the header and voice-frame wire
/// bytes are resent verbatim, in order, to the original sender once `now`
/// reaches `play_at`.
struct Playback {
    header_bytes: Vec<u8>,
    voice_frames: VecDeque<Vec<u8>>,
    play_at: Instant,
}

/// An unbound-thread, `DExtra`-compatible loopback reflector:
/// [`Reflector::bind`] opens the socket, [`Reflector::run`] starts the
/// run-loop thread. See the module docs for the full behavior contract.
pub struct Reflector {
    socket: UdpSocket,
    local_addr: SocketAddr,
    keepalive_interval: Duration,
    client_timeout: Duration,
    parrot: bool,
    parrot_replay_delay: Duration,
}

impl Reflector {
    /// Binds a reflector to `addr` with the default keepalive interval (3s)
    /// and client timeout (30s). Parrot mode is OFF (plain relay-only
    /// reflector).
    ///
    /// # Errors
    /// Whatever [`UdpSocket::bind`] or `set_read_timeout` returns.
    pub fn bind(addr: SocketAddr) -> io::Result<Reflector> {
        Self::bind_with_timeouts(addr, DEFAULT_KEEPALIVE_INTERVAL, DEFAULT_CLIENT_TIMEOUT)
    }

    /// Binds a reflector to `addr` with caller-supplied `keepalive_interval`
    /// and `client_timeout` — lets a test shorten both windows rather than
    /// waiting out the real 3s/30s defaults. Parrot mode is OFF.
    ///
    /// # Errors
    /// Whatever [`UdpSocket::bind`] or `set_read_timeout` returns.
    pub fn bind_with_timeouts(
        addr: SocketAddr,
        keepalive_interval: Duration,
        client_timeout: Duration,
    ) -> io::Result<Reflector> {
        Self::bind_inner(
            addr,
            keepalive_interval,
            client_timeout,
            false,
            DEFAULT_PARROT_REPLAY_DELAY,
        )
    }

    /// Binds a PARROT reflector to `addr`: identical connect/ACK-NAK/
    /// keepalive/unlink and same-module relay behavior as [`Reflector::bind`],
    /// plus the echo-back-to-sender behavior described in the module docs'
    /// "Parrot mode" section. Default keepalive interval (3s), client
    /// timeout (30s), and replay delay (150ms).
    ///
    /// # Errors
    /// Whatever [`UdpSocket::bind`] or `set_read_timeout` returns.
    pub fn bind_parrot(addr: SocketAddr) -> io::Result<Reflector> {
        Self::bind_parrot_with_timeouts(
            addr,
            DEFAULT_KEEPALIVE_INTERVAL,
            DEFAULT_CLIENT_TIMEOUT,
            DEFAULT_PARROT_REPLAY_DELAY,
        )
    }

    /// [`Reflector::bind_parrot`] with caller-supplied `keepalive_interval`,
    /// `client_timeout`, AND `parrot_replay_delay` — the last one lets a
    /// test shorten the 150ms pre-playback delay rather than waiting it out
    /// for real.
    ///
    /// # Errors
    /// Whatever [`UdpSocket::bind`] or `set_read_timeout` returns.
    pub fn bind_parrot_with_timeouts(
        addr: SocketAddr,
        keepalive_interval: Duration,
        client_timeout: Duration,
        parrot_replay_delay: Duration,
    ) -> io::Result<Reflector> {
        Self::bind_inner(
            addr,
            keepalive_interval,
            client_timeout,
            true,
            parrot_replay_delay,
        )
    }

    fn bind_inner(
        addr: SocketAddr,
        keepalive_interval: Duration,
        client_timeout: Duration,
        parrot: bool,
        parrot_replay_delay: Duration,
    ) -> io::Result<Reflector> {
        let socket = UdpSocket::bind(addr)?;
        socket.set_read_timeout(Some(SOCKET_POLL_TIMEOUT))?;
        let local_addr = socket.local_addr()?;
        Ok(Reflector {
            socket,
            local_addr,
            keepalive_interval,
            client_timeout,
            parrot,
            parrot_replay_delay,
        })
    }

    /// The address this reflector is bound to (useful when `addr`'s port was
    /// `0` — the OS picks an ephemeral port).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Starts the run-loop thread (named `"iax-dstar-refl"`) and returns a
    /// handle to control it. Consumes `self`: the socket moves into the
    /// thread, which owns it exclusively from here on.
    ///
    /// # Panics
    /// If the OS refuses to spawn the thread.
    #[must_use]
    pub fn run(self) -> ReflectorHandle {
        let shutdown = Arc::new(AtomicBool::new(false));
        let client_count = Arc::new(AtomicUsize::new(0));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread_client_count = Arc::clone(&client_count);
        let Reflector {
            socket,
            keepalive_interval,
            client_timeout,
            parrot,
            parrot_replay_delay,
            ..
        } = self;
        let thread = std::thread::Builder::new()
            .name("iax-dstar-refl".to_string())
            .spawn(move || {
                run_loop(
                    &socket,
                    keepalive_interval,
                    client_timeout,
                    parrot,
                    parrot_replay_delay,
                    &thread_shutdown,
                    &thread_client_count,
                );
            })
            .expect("spawn iax-dstar-refl thread");
        ReflectorHandle {
            shutdown,
            thread: Some(thread),
            client_count,
        }
    }
}

/// A handle to a running [`Reflector`]'s thread. [`ReflectorHandle::shutdown`]
/// (or dropping the handle) requests the thread stop and joins it, bounded
/// by the reflector's 50ms socket read timeout.
pub struct ReflectorHandle {
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    client_count: Arc<AtomicUsize>,
}

impl ReflectorHandle {
    /// Requests the run-loop thread stop and joins it. Bounded by the 50ms
    /// socket read timeout (the loop only checks the shutdown flag between
    /// socket reads).
    pub fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// The current number of linked clients — a test-visible escape hatch
    /// for asserting a client got linked/reaped without needing a second
    /// observable side effect.
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

/// The `"iax-dstar-refl"` run-loop: owns the socket and the client table for
/// as long as the reflector runs. Driven by one 50ms poll cadence (the
/// socket's read timeout) — reacting to an incoming packet, ticking
/// keepalives, reaping silent clients, and (parrot mode only) draining due
/// playbacks.
fn run_loop(
    socket: &UdpSocket,
    keepalive_interval: Duration,
    client_timeout: Duration,
    parrot: bool,
    parrot_replay_delay: Duration,
    shutdown: &AtomicBool,
    client_count: &AtomicUsize,
) {
    let mut clients: HashMap<SocketAddr, ClientEntry> = HashMap::new();
    let mut captures: HashMap<SocketAddr, Capture> = HashMap::new();
    let mut playbacks: HashMap<SocketAddr, Playback> = HashMap::new();
    let mut buf = [0u8; 2_048];

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        match socket.recv_from(&mut buf) {
            Ok((n, src)) => handle_packet(
                socket,
                &buf[..n],
                src,
                &mut clients,
                parrot,
                parrot_replay_delay,
                &mut captures,
                &mut playbacks,
            ),
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            }
            Err(_) => {}
        }

        tick(socket, keepalive_interval, client_timeout, &mut clients);

        if parrot {
            let now = Instant::now();
            drain_playbacks(socket, &mut playbacks, now);
            // A client that unlinked or got reaped above shouldn't keep
            // receiving (or accumulating) parrot traffic addressed to it.
            captures.retain(|addr, _| clients.contains_key(addr));
            playbacks.retain(|addr, _| clients.contains_key(addr));
        }

        client_count.store(clients.len(), Ordering::Relaxed);
    }
}

/// One run-loop tick (after the socket poll): send a keepalive to every
/// client whose `keepalive_interval` has elapsed since its last one, then
/// reap every client silent past `client_timeout`.
fn tick(
    socket: &UdpSocket,
    keepalive_interval: Duration,
    client_timeout: Duration,
    clients: &mut HashMap<SocketAddr, ClientEntry>,
) {
    let now = Instant::now();
    let keepalive = keepalive_bytes();
    for (addr, client) in clients.iter_mut() {
        if now.duration_since(client.last_keepalive_sent) >= keepalive_interval {
            let _ = socket.send_to(&keepalive, *addr);
            client.last_keepalive_sent = now;
        }
    }
    clients.retain(|_, client| now.duration_since(client.last_seen) < client_timeout);
}

/// Parrot mode: sends every playback whose `play_at` deadline has arrived —
/// header bytes first, then every buffered voice frame in order, all at
/// once (no per-frame pacing; see the module docs for why that's fine here).
fn drain_playbacks(
    socket: &UdpSocket,
    playbacks: &mut HashMap<SocketAddr, Playback>,
    now: Instant,
) {
    let due: Vec<SocketAddr> = playbacks
        .iter()
        .filter(|(_, pb)| now >= pb.play_at)
        .map(|(addr, _)| *addr)
        .collect();
    for addr in due {
        if let Some(pb) = playbacks.remove(&addr) {
            let _ = socket.send_to(&pb.header_bytes, addr);
            for frame in &pb.voice_frames {
                let _ = socket.send_to(frame, addr);
            }
        }
    }
}

/// Parrot mode: folds one received [`DsvtPacket`] (already known to have
/// parsed successfully) into `src`'s in-progress capture:
///
/// - A [`DsvtPacket::Header`] always starts a fresh capture, replacing
///   whatever was in progress (an abandoned prior stream that never saw
///   EOT).
/// - A [`DsvtPacket::Voice`] frame is appended to the in-progress capture IF
///   its `StreamID` matches the header's; otherwise (no capture in
///   progress, or a mismatched `StreamID`) it's an orphan and is dropped —
///   this reflector never guesses at a header it didn't see. On the EOT bit,
///   the finished capture is moved into `playbacks`, scheduled for
///   `parrot_replay_delay` from now.
///
/// Per the module docs' "ignore overlapping streams" rule, the caller
/// ([`handle_packet`]) skips calling this at all while a playback is queued
/// or draining for `src`.
fn parrot_capture(
    captures: &mut HashMap<SocketAddr, Capture>,
    playbacks: &mut HashMap<SocketAddr, Playback>,
    src: SocketAddr,
    pkt: &DsvtPacket,
    raw: &[u8],
    parrot_replay_delay: Duration,
    now: Instant,
) {
    match pkt {
        DsvtPacket::Voice { stream_id, end, .. } => {
            let Some(cap) = captures.get_mut(&src) else {
                return; // orphan voice frame, no header seen yet
            };
            if cap.stream_id != *stream_id {
                // A new StreamID arrived without the buffered one ever
                // seeing EOT: the old capture is abandoned. Drop it
                // silently (no header for a NEW capture has arrived either
                // — just a stray voice frame from an unrelated/dead
                // stream) rather than guessing.
                captures.remove(&src);
                return;
            }
            if cap.voice_frames.len() >= MAX_CAPTURE_VOICE_FRAMES {
                cap.voice_frames.pop_front();
            }
            cap.voice_frames.push_back(raw.to_vec());
            if *end && let Some(finished) = captures.remove(&src) {
                playbacks.insert(
                    src,
                    Playback {
                        header_bytes: finished.header_bytes,
                        voice_frames: finished.voice_frames,
                        play_at: now + parrot_replay_delay,
                    },
                );
            }
        }
        DsvtPacket::Header { stream_id, .. } => {
            captures.insert(
                src,
                Capture {
                    stream_id: *stream_id,
                    header_bytes: raw.to_vec(),
                    voice_frames: VecDeque::new(),
                },
            );
        }
    }
}

fn keepalive_bytes() -> [u8; KEEPALIVE_LEN] {
    let mut buf = [0u8; KEEPALIVE_LEN];
    buf[..8].copy_from_slice(&REFLECTOR_CALLSIGN);
    buf[8] = 0x00;
    buf
}

fn ack_nak_bytes(own_module: u8, dest_module: u8, ok: bool) -> [u8; ACK_NAK_LEN] {
    let mut buf = [0u8; ACK_NAK_LEN];
    buf[0..8].copy_from_slice(&REFLECTOR_CALLSIGN);
    buf[8] = own_module;
    buf[9] = dest_module;
    buf[10..14].copy_from_slice(if ok { b"ACK\0" } else { b"NAK\0" });
    buf
}

/// Parses an 11-byte connect/unlink-shaped packet into `(own_module,
/// dest_module)`, per research §2 / `crate::fsm`'s wire layout: 8-byte
/// callsign (unused reflector-side, beyond framing the packet), 1 own-module
/// byte, 1 dest-module byte, 1 revision byte. Returns `None` for any other
/// length.
fn parse_link_request(buf: &[u8]) -> Option<(u8, u8)> {
    if buf.len() != LINK_REQUEST_LEN {
        return None;
    }
    Some((buf[8], buf[9]))
}

/// Reacts to one received packet from `src`:
///
/// - An 11-byte connect/unlink-shaped packet: if the dest module is
///   [`BLANK_MODULE`], treat it as an unlink — reply `"DISCONNECTED"` and
///   drop the client (and, in parrot mode, any in-progress capture/playback
///   for it) unconditionally, whether or not `src` was actually linked
///   (idempotent, matches a real reflector answering a stray/duplicate
///   unlink the same way). Otherwise treat it as a connect: link the client
///   (or refresh its module if already linked) and reply ACK if the dest
///   module is `A`-`Z` and there's room ([`MAX_LINKED_CLIENTS`]); NAK
///   otherwise.
/// - Anything else from an address that ISN'T a currently-linked client is
///   ignored outright.
/// - Any packet at all from a known client refreshes `last_seen`.
/// - A 9-byte keepalive-shaped packet from a known client: liveness-refresh
///   only (`last_seen` is already updated above) — NOT replied to. See the
///   module docs' "Keepalive direction (no echo)" section for why.
/// - A valid DSVT header/voice packet from a known client: relayed verbatim
///   to every OTHER client on the SAME dest module (unchanged by parrot
///   mode), and — when `parrot` is on and no playback is queued/draining
///   for this sender — also folded into that sender's capture via
///   [`parrot_capture`].
#[allow(clippy::too_many_arguments)]
fn handle_packet(
    socket: &UdpSocket,
    buf: &[u8],
    src: SocketAddr,
    clients: &mut HashMap<SocketAddr, ClientEntry>,
    parrot: bool,
    parrot_replay_delay: Duration,
    captures: &mut HashMap<SocketAddr, Capture>,
    playbacks: &mut HashMap<SocketAddr, Playback>,
) {
    let now = Instant::now();

    if let Some((own_module, dest_module)) = parse_link_request(buf) {
        if dest_module == BLANK_MODULE {
            let _ = socket.send_to(DISCONNECTED, src);
            clients.remove(&src);
            captures.remove(&src);
            playbacks.remove(&src);
            return;
        }

        let already_linked = clients.contains_key(&src);
        let room = already_linked || clients.len() < MAX_LINKED_CLIENTS;
        if dest_module.is_ascii_uppercase() && room {
            clients.insert(
                src,
                ClientEntry {
                    module: dest_module,
                    last_seen: now,
                    last_keepalive_sent: now,
                },
            );
            let _ = socket.send_to(&ack_nak_bytes(own_module, dest_module, true), src);
        } else {
            let _ = socket.send_to(&ack_nak_bytes(own_module, dest_module, false), src);
        }
        return;
    }

    // Everything below requires an already-linked client; unknown senders
    // are silently ignored.
    let Some(client) = clients.get_mut(&src) else {
        return;
    };
    client.last_seen = now;
    let module = client.module;

    if buf.len() == KEEPALIVE_LEN && buf[8] == 0x00 {
        // Liveness-refresh only (`last_seen` is already updated above) —
        // deliberately NOT echoed. See the module docs' "Keepalive
        // direction (no echo)" section: the periodic `tick` ping already
        // covers reflector→client, and echoing an inbound keepalive here
        // would ping-pong forever against a real client (or
        // `DextraFsm::on_packet`), which correctly answers every keepalive
        // it receives.
        return;
    }

    if let Ok(pkt) = DsvtPacket::parse(buf) {
        for (addr, c) in clients.iter() {
            if *addr != src && c.module == module {
                let _ = socket.send_to(buf, *addr);
            }
        }
        if parrot && !playbacks.contains_key(&src) {
            parrot_capture(
                captures,
                playbacks,
                src,
                &pkt,
                buf,
                parrot_replay_delay,
                now,
            );
        }
    }
    // Anything else (a stray/garbage buffer, or an unrecognized DSVT
    // shape): last_seen is already refreshed above; nothing further to do.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_nak_bytes_have_the_expected_wire_shape() {
        let ack = ack_nak_bytes(b' ', b'B', true);
        assert_eq!(ack.len(), ACK_NAK_LEN);
        assert_eq!(&ack[0..8], &REFLECTOR_CALLSIGN);
        assert_eq!(ack[8], b' ');
        assert_eq!(ack[9], b'B');
        assert_eq!(&ack[10..14], b"ACK\0");

        let nak = ack_nak_bytes(b' ', b'5', false);
        assert_eq!(&nak[10..14], b"NAK\0");
    }

    #[test]
    fn keepalive_bytes_have_the_expected_wire_shape() {
        let ka = keepalive_bytes();
        assert_eq!(ka.len(), KEEPALIVE_LEN);
        assert_eq!(&ka[0..8], &REFLECTOR_CALLSIGN);
        assert_eq!(ka[8], 0x00);
    }

    #[test]
    fn parse_link_request_rejects_wrong_length() {
        assert_eq!(parse_link_request(&[0u8; 9]), None);
        assert_eq!(parse_link_request(&[0u8; 14]), None);
    }
}
