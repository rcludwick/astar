// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A loopback `NXDNReflector`, plus an optional "parrot" echo mode — this
//! crate's ONLY I/O module. Everything else in `astar-nxdn` is pure
//! encode/decode with no sockets and no threads.
//!
//! It exists because of a house rule: no test may transmit anywhere but
//! `127.0.0.1`. Without a reflector of our own there is no way to exercise
//! the link at all, so this is not a nicety — it is the only place the
//! session layer can be tested. `astar_ysf::reflector` is the same fixture
//! for System Fusion; this one differs only where NXDN does.
//!
//! # What it does
//!
//! * `NXDNP` registers a client (or refreshes it) **only when its talkgroup
//!   is this reflector's** — `NXDNReflector.cpp`: `unsigned short id =
//!   (buffer[15U] << 8) | buffer[16U]; if (id == tg)`. A poll for another
//!   talkgroup is silently dropped, with no reply at all.
//! * The reply is the client's own poll **echoed back verbatim**
//!   (`NXDNReflector.cpp`: `// Return the poll`). There is no acknowledgement
//!   packet in this protocol, so there is nothing else to send.
//! * `NXDNU` drops the client.
//! * `NXDND` is relayed **verbatim** to every other registered client when
//!   `group && dst_id == talkgroup`, the reflector's own relay test. Not
//!   re-encoded: a reflector that rewrote frames would hide exactly the
//!   framing bugs these tests exist to catch.
//! * A client heard from in longer than `client_timeout` is reaped —
//!   [`DEFAULT_CLIENT_TIMEOUT`] is the reference implementation's 120 s.
//!
//! # Parrot mode
//!
//! With parrot on, a transmission is also captured and played back to
//! whoever sent it, one frame every [`FRAME_INTERVAL`] so the playback lands
//! on a receiver at the pace real audio would. Playback starts
//! `parrot_replay_delay` after the frame with the end flag set, or after a
//! gap long enough that the sender has plainly stopped without saying so.

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::wire::{self, DATA_LEN, DataPacket, Packet};

/// How long a registered client may go unheard before it is forgotten.
///
/// `NXDNReflector.h: CNXDNRepeater::m_timer(1000U, 120U)` — 120 s, twenty-four
/// missed polls. Generous on purpose: the cost of reaping a client that is
/// merely behind a slow link is a hole in its audio.
pub const DEFAULT_CLIENT_TIMEOUT: Duration = Duration::from_secs(120);
/// Pause between a transmission ending and the parrot replaying it.
pub const DEFAULT_PARROT_REPLAY_DELAY: Duration = Duration::from_millis(150);
/// One NXDN network frame on the air: four 20 ms AMBE+2 frames, two in each
/// of the two 14-byte blocks (`MMDVMHost/NXDNAudio.cpp: CNXDNAudio::decode`
/// decodes at bit offsets 0 and 49 of each block).
pub const FRAME_INTERVAL: Duration = Duration::from_millis(80);
/// Silence after which a transmission is treated as over even without an
/// end-flagged frame. The reference reflector's own watchdog is 1.5 s
/// (`NXDNReflector.cpp: CTimer watchdogTimer(1000U, 0U, 1500U);`); a bench
/// parrot wants to answer sooner than that.
const TRANSMISSION_GAP: Duration = Duration::from_millis(500);
/// How long a socket read blocks before the loop rechecks its shutdown flag.
const SOCKET_POLL_TIMEOUT: Duration = Duration::from_millis(50);
/// Largest datagram worth reading; the biggest real one is 43 bytes.
const BUFFER_LEN: usize = 64;

/// The biggest datagram this reflector will ever relay, for callers sizing
/// their own buffers.
pub const MAX_DATAGRAM: usize = DATA_LEN;

struct Client {
    last_seen: Instant,
}

struct Parrot {
    /// Whose transmission is being captured, and where to play it back.
    owner: SocketAddr,
    frames: Vec<Vec<u8>>,
    last_frame: Instant,
    /// When to send the next frame back, once playing.
    play_at: Option<Instant>,
    next: usize,
}

/// An unbound-thread loopback `NXDNReflector`: [`Reflector::bind`] opens the
/// socket, [`Reflector::run`] starts the run-loop thread.
pub struct Reflector {
    socket: UdpSocket,
    local_addr: SocketAddr,
    talkgroup: u16,
    client_timeout: Duration,
    parrot: bool,
    parrot_replay_delay: Duration,
}

impl Reflector {
    /// Binds a relay-only reflector for `talkgroup` to `addr`, with the
    /// reference implementation's 120 s client timeout.
    ///
    /// # Errors
    /// Whatever [`UdpSocket::bind`] or `set_read_timeout` returns.
    pub fn bind(addr: SocketAddr, talkgroup: u16) -> io::Result<Reflector> {
        Self::bind_inner(
            addr,
            talkgroup,
            DEFAULT_CLIENT_TIMEOUT,
            false,
            DEFAULT_PARROT_REPLAY_DELAY,
        )
    }

    /// [`Reflector::bind`] with a caller-supplied client timeout — lets a
    /// test shorten the window rather than waiting out the real 120 s.
    ///
    /// # Errors
    /// Whatever [`UdpSocket::bind`] or `set_read_timeout` returns.
    pub fn bind_with_timeout(
        addr: SocketAddr,
        talkgroup: u16,
        client_timeout: Duration,
    ) -> io::Result<Reflector> {
        Self::bind_inner(
            addr,
            talkgroup,
            client_timeout,
            false,
            DEFAULT_PARROT_REPLAY_DELAY,
        )
    }

    /// Binds a PARROT reflector: everything [`Reflector::bind`] does, plus
    /// echoing each transmission back to its sender.
    ///
    /// # Errors
    /// Whatever [`UdpSocket::bind`] or `set_read_timeout` returns.
    pub fn bind_parrot(addr: SocketAddr, talkgroup: u16) -> io::Result<Reflector> {
        Self::bind_inner(
            addr,
            talkgroup,
            DEFAULT_CLIENT_TIMEOUT,
            true,
            DEFAULT_PARROT_REPLAY_DELAY,
        )
    }

    /// [`Reflector::bind_parrot`] with a caller-supplied client timeout and
    /// replay delay.
    ///
    /// # Errors
    /// Whatever [`UdpSocket::bind`] or `set_read_timeout` returns.
    pub fn bind_parrot_with_timeouts(
        addr: SocketAddr,
        talkgroup: u16,
        client_timeout: Duration,
        parrot_replay_delay: Duration,
    ) -> io::Result<Reflector> {
        Self::bind_inner(addr, talkgroup, client_timeout, true, parrot_replay_delay)
    }

    fn bind_inner(
        addr: SocketAddr,
        talkgroup: u16,
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
            talkgroup,
            client_timeout,
            parrot,
            parrot_replay_delay,
        })
    }

    /// The address this reflector is bound to (useful when `addr`'s port was
    /// `0` — the OS picks an ephemeral one).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The talkgroup this reflector answers for.
    #[must_use]
    pub fn talkgroup(&self) -> u16 {
        self.talkgroup
    }

    /// Starts the run-loop thread and returns a handle to control it.
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
            talkgroup,
            client_timeout,
            parrot,
            parrot_replay_delay,
            ..
        } = self;
        let thread = std::thread::Builder::new()
            .name("astar-nxdn-refl".to_string())
            .spawn(move || {
                run_loop(
                    &socket,
                    talkgroup,
                    client_timeout,
                    parrot,
                    parrot_replay_delay,
                    &thread_shutdown,
                    &thread_client_count,
                );
            })
            .expect("spawn astar-nxdn-refl thread");
        ReflectorHandle {
            shutdown,
            thread: Some(thread),
            client_count,
        }
    }
}

/// A handle to a running [`Reflector`]'s thread. [`ReflectorHandle::shutdown`]
/// (or dropping the handle) requests the thread stop and joins it, bounded
/// by the 50 ms socket read timeout.
pub struct ReflectorHandle {
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    client_count: Arc<AtomicUsize>,
}

impl ReflectorHandle {
    /// Requests the run-loop thread stop and joins it.
    pub fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// The number of registered clients — a test-visible escape hatch for
    /// asserting a client linked or was reaped without needing a second
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

fn run_loop(
    socket: &UdpSocket,
    talkgroup: u16,
    client_timeout: Duration,
    parrot_enabled: bool,
    parrot_replay_delay: Duration,
    shutdown: &AtomicBool,
    client_count: &AtomicUsize,
) {
    let mut clients: HashMap<SocketAddr, Client> = HashMap::new();
    let mut parrot: Option<Parrot> = None;
    let mut buf = [0u8; BUFFER_LEN];

    while !shutdown.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((len, from)) => {
                handle(
                    socket,
                    talkgroup,
                    &buf[..len],
                    from,
                    &mut clients,
                    client_count,
                    parrot_enabled.then_some(&mut parrot),
                );
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }

        let now = Instant::now();
        clients.retain(|_, c| now.duration_since(c.last_seen) < client_timeout);
        client_count.store(clients.len(), Ordering::Relaxed);

        if parrot_enabled {
            drive_parrot(socket, &mut parrot, parrot_replay_delay, now);
        }
    }
}

fn handle(
    socket: &UdpSocket,
    talkgroup: u16,
    datagram: &[u8],
    from: SocketAddr,
    clients: &mut HashMap<SocketAddr, Client>,
    client_count: &AtomicUsize,
    parrot: Option<&mut Option<Parrot>>,
) {
    let Some(packet) = wire::parse(datagram) else {
        return;
    };
    let now = Instant::now();
    match packet {
        Packet::Poll { talkgroup: tg, .. } => {
            // The talkgroup gate is the whole of registration, and a poll
            // for another one gets no reply — not even a refusal.
            if tg != talkgroup {
                return;
            }
            clients.insert(from, Client { last_seen: now });
            // Published before the reply goes out, not after: a client that
            // has its echo in hand is registered, and a test that asks the
            // moment it arrives must not race the run loop's own bookkeeping.
            client_count.store(clients.len(), Ordering::Relaxed);
            // "Return the poll": the client's own bytes, unaltered.
            let _ = socket.send_to(datagram, from);
        }
        Packet::Unlink { .. } => {
            // Not gated on the talkgroup: a client that says it is leaving
            // is leaving, and a reflector that argued about which talkgroup
            // it was leaving would keep sending audio into a closed socket.
            clients.remove(&from);
            client_count.store(clients.len(), Ordering::Relaxed);
        }
        Packet::Data(data) => {
            if !data.group || data.dst_id != talkgroup {
                return;
            }
            clients
                .entry(from)
                .and_modify(|c| c.last_seen = now)
                .or_insert(Client { last_seen: now });
            for &peer in clients.keys() {
                if peer != from {
                    let _ = socket.send_to(datagram, peer);
                }
            }
            if let Some(slot) = parrot {
                capture(slot, datagram, &data, from, now);
            }
        }
        Packet::Unknown(_) => {}
    }
}

/// Adds one frame to the parrot's capture, starting a fresh one when the
/// speaker changes or the last transmission plainly ended.
fn capture(
    slot: &mut Option<Parrot>,
    datagram: &[u8],
    data: &DataPacket,
    from: SocketAddr,
    now: Instant,
) {
    let stale = slot.as_ref().is_some_and(|p| {
        p.owner != from
            || p.play_at.is_some()
            || now.duration_since(p.last_frame) > TRANSMISSION_GAP
    });
    if stale {
        *slot = None;
    }
    let parrot = slot.get_or_insert_with(|| Parrot {
        owner: from,
        frames: Vec::new(),
        last_frame: now,
        play_at: None,
        next: 0,
    });
    parrot.frames.push(datagram.to_vec());
    parrot.last_frame = now;
    if data.end {
        // Marked as the last frame: start the clock on playback.
        parrot.play_at = Some(now);
    }
}

fn drive_parrot(
    socket: &UdpSocket,
    slot: &mut Option<Parrot>,
    replay_delay: Duration,
    now: Instant,
) {
    let Some(parrot) = slot.as_mut() else {
        return;
    };

    // A sender that stopped without an end flag still stopped.
    if parrot.play_at.is_none() && now.duration_since(parrot.last_frame) > TRANSMISSION_GAP {
        parrot.play_at = Some(parrot.last_frame + TRANSMISSION_GAP);
    }

    let Some(play_at) = parrot.play_at else {
        return;
    };
    let due = play_at + replay_delay + FRAME_INTERVAL * u32::try_from(parrot.next).unwrap_or(0);
    if now < due {
        return;
    }
    if let Some(frame) = parrot.frames.get(parrot.next) {
        let _ = socket.send_to(frame, parrot.owner);
        parrot.next += 1;
    }
    if parrot.next >= parrot.frames.len() {
        *slot = None;
    }
}
