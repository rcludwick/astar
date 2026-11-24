// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A loopback `P25Reflector`, plus a parrot mode that behaves like G4KLX's
//! `P25Parrot`. This is the crate's only I/O module. Everything else in
//! `astar-p25` is pure encode and decode, with no sockets and no threads.
//!
//! It exists because of a house rule: no test may transmit anywhere but
//! `127.0.0.1`. Without a reflector of our own there is no way to exercise
//! the link at all. `astar_nxdn::reflector` is the same fixture for NXDN;
//! this one differs only where P25 does.
//!
//! # Relay mode
//!
//! The behaviour is read out of `nostar/DVReflectors`,
//! `P25Reflector/P25Reflector.cpp`:
//!
//! * A poll (`F0`) from any address registers that peer, or refreshes it,
//!   and is echoed back verbatim (`// Return the poll`). There is no other
//!   acknowledgement in this protocol.
//! * An unlink (`F1`) removes the peer. Nothing is sent back.
//! * A peer heard from in longer than `client_timeout` is dropped.
//!   [`DEFAULT_CLIENT_TIMEOUT`] is the reference's 120 s.
//! * One talker at a time. The first registered peer to send a voice record
//!   becomes the talker. Its records and its `0x80` go verbatim to every
//!   other registered peer, never back to it. Records from anyone else are
//!   dropped until the talker sends `0x80` or is silent for
//!   [`TALKER_WATCHDOG`]. Datagrams from an address that never polled are
//!   dropped, as the reference drops "data received from an unknown
//!   source".
//!
//! Relay is verbatim, never re-encoded: a reflector that rewrote records
//! would hide exactly the framing bugs these tests exist to catch.
//!
//! # Parrot mode
//!
//! All of the above, plus `P25Clients/P25Parrot`'s echo. The current
//! talker's voice records are recorded, up to [`PARROT_RECORD_CAP`] (the
//! rest are dropped). `replay_delay` after its `0x80`, or after the
//! watchdog gives up on it, the recording is played back to that talker,
//! one record every [`REPLAY_INTERVAL`], ending with an `0x80` (one is
//! appended if the over had none). Only the last over is kept: a new talker
//! replaces the recording, even one still playing.
//!
//! The live relay still runs in parrot mode, so a second client linked to
//! the same port hears the over as it happens.

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::wire::{self, Packet};

/// How long a registered peer may go unheard before it is dropped
/// (`P25Reflector.h`, `CP25Repeater::m_timer(1000U, 120U)`).
pub const DEFAULT_CLIENT_TIMEOUT: Duration = Duration::from_secs(120);
/// Silence after which the talker loses the floor without an `0x80`
/// (`P25Reflector.cpp` and `P25Parrot.cpp`, `CTimer watchdogTimer(1000U,
/// 0U, 1500U)`).
pub const TALKER_WATCHDOG: Duration = Duration::from_millis(1_500);
/// Pause between an over ending and the parrot replaying it
/// (`P25Parrot.cpp`, `CTimer turnaroundTimer(1000U, 2U)`).
pub const DEFAULT_PARROT_REPLAY_DELAY: Duration = Duration::from_secs(2);
/// One record every 20 ms on replay (`P25Parrot.cpp`: `// A frame every
/// 20ms`), the pace of IMBE audio.
pub const REPLAY_INTERVAL: Duration = Duration::from_millis(20);
/// The most records the parrot keeps of one over: 180 s of 20 ms records
/// (`P25Parrot.cpp`, `CParrot parrot(180U)`).
pub const PARROT_RECORD_CAP: usize = 9_000;

/// How long a socket read blocks, at most, before the loop rechecks its
/// shutdown flag and its timers.
const SOCKET_POLL_TIMEOUT: Duration = Duration::from_millis(50);
/// The shortest read timeout the loop asks for while a timer is close.
const MIN_POLL_TIMEOUT: Duration = Duration::from_millis(1);
/// Largest datagram worth reading; the biggest real one is 22 bytes.
const BUFFER_LEN: usize = 64;

/// The biggest datagram this reflector will ever relay, for callers sizing
/// their own buffers.
pub const MAX_DATAGRAM: usize = 22;

struct Talker {
    addr: SocketAddr,
    last_heard: Instant,
}

struct Parrot {
    /// Whose over this is, and where it is played back to.
    owner: SocketAddr,
    records: Vec<Vec<u8>>,
    /// The over's own `0x80`, or a fresh one if it stopped dead.
    end: Vec<u8>,
    /// When the first record goes back, once the over has ended.
    replay_at: Option<Instant>,
    /// How many of `records` plus `end` have been sent back.
    sent: usize,
}

impl Parrot {
    fn total(&self) -> usize {
        self.records.len() + 1
    }

    fn item(&self, i: usize) -> &[u8] {
        self.records.get(i).map_or(&self.end, Vec::as_slice)
    }
}

/// A loopback `P25Reflector`: [`Reflector::bind`] opens the socket,
/// [`Reflector::run`] starts the run-loop thread.
pub struct Reflector {
    socket: UdpSocket,
    local_addr: SocketAddr,
    config: Config,
}

#[derive(Clone, Copy)]
struct Config {
    client_timeout: Duration,
    parrot: bool,
    replay_delay: Duration,
    replay_interval: Duration,
}

impl Reflector {
    /// Binds a relay-only reflector to `addr`, with the reference's 120 s
    /// client timeout.
    ///
    /// # Errors
    /// Whatever [`UdpSocket::bind`] or `set_read_timeout` returns.
    pub fn bind(addr: SocketAddr) -> io::Result<Reflector> {
        Self::bind_inner(
            addr,
            DEFAULT_CLIENT_TIMEOUT,
            false,
            DEFAULT_PARROT_REPLAY_DELAY,
        )
    }

    /// [`Reflector::bind`] with a caller-supplied client timeout, so a test
    /// need not wait out the real 120 s.
    ///
    /// # Errors
    /// Whatever [`UdpSocket::bind`] or `set_read_timeout` returns.
    pub fn bind_with_timeout(addr: SocketAddr, client_timeout: Duration) -> io::Result<Reflector> {
        Self::bind_inner(addr, client_timeout, false, DEFAULT_PARROT_REPLAY_DELAY)
    }

    /// Binds a parrot: everything [`Reflector::bind`] does, plus replaying
    /// each over to its sender [`DEFAULT_PARROT_REPLAY_DELAY`] after it ends.
    ///
    /// # Errors
    /// Whatever [`UdpSocket::bind`] or `set_read_timeout` returns.
    pub fn bind_parrot(addr: SocketAddr) -> io::Result<Reflector> {
        Self::bind_inner(
            addr,
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
        client_timeout: Duration,
        replay_delay: Duration,
    ) -> io::Result<Reflector> {
        Self::bind_inner(addr, client_timeout, true, replay_delay)
    }

    /// Replaces the parrot's [`REPLAY_INTERVAL`]. Only for tests that would
    /// otherwise wait minutes for a long over to play back; the real pace is
    /// the 20 ms of the audio. Zero sends the whole over at once.
    #[must_use]
    pub fn with_replay_interval(mut self, interval: Duration) -> Reflector {
        self.config.replay_interval = interval;
        self
    }

    fn bind_inner(
        addr: SocketAddr,
        client_timeout: Duration,
        parrot: bool,
        replay_delay: Duration,
    ) -> io::Result<Reflector> {
        let socket = UdpSocket::bind(addr)?;
        socket.set_read_timeout(Some(SOCKET_POLL_TIMEOUT))?;
        let local_addr = socket.local_addr()?;
        Ok(Reflector {
            socket,
            local_addr,
            config: Config {
                client_timeout,
                parrot,
                replay_delay,
                replay_interval: REPLAY_INTERVAL,
            },
        })
    }

    /// The address this reflector is bound to (useful when `addr`'s port
    /// was `0` and the OS picked one).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Starts the run-loop thread and returns a handle to control it.
    ///
    /// # Panics
    /// If the OS refuses to spawn the thread.
    #[must_use]
    pub fn run(self) -> ReflectorHandle {
        let shutdown = Arc::new(AtomicBool::new(false));
        let client_count = Arc::new(AtomicUsize::new(0));
        let parrot_recorded = Arc::new(AtomicUsize::new(0));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread_client_count = Arc::clone(&client_count);
        let thread_parrot_recorded = Arc::clone(&parrot_recorded);
        let Reflector { socket, config, .. } = self;
        let thread = std::thread::Builder::new()
            .name("astar-p25-refl".to_string())
            .spawn(move || {
                Loop {
                    socket: &socket,
                    config,
                    clients: HashMap::new(),
                    talker: None,
                    parrot: None,
                    client_count: &thread_client_count,
                    parrot_recorded: &thread_parrot_recorded,
                }
                .run(&thread_shutdown);
            })
            .expect("spawn astar-p25-refl thread");
        ReflectorHandle {
            shutdown,
            thread: Some(thread),
            client_count,
            parrot_recorded,
        }
    }
}

/// A handle to a running [`Reflector`]'s thread. [`ReflectorHandle::shutdown`]
/// (or dropping the handle) asks the thread to stop and joins it, which
/// takes at most the 50 ms socket read timeout.
pub struct ReflectorHandle {
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    client_count: Arc<AtomicUsize>,
    parrot_recorded: Arc<AtomicUsize>,
}

impl ReflectorHandle {
    /// Asks the run-loop thread to stop and joins it.
    pub fn shutdown(mut self) {
        self.stop();
    }

    /// The number of registered peers, so a test can see a link or a reap
    /// happen.
    #[must_use]
    pub fn client_count(&self) -> usize {
        self.client_count.load(Ordering::Relaxed)
    }

    /// How many voice records the parrot holds for the last over (its own
    /// `0x80` not counted), so a test can see the cap hold without counting
    /// datagrams through a socket buffer that may drop some. Always 0 for a
    /// relay-only reflector.
    #[must_use]
    pub fn parrot_recorded(&self) -> usize {
        self.parrot_recorded.load(Ordering::Relaxed)
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ReflectorHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Loop<'a> {
    socket: &'a UdpSocket,
    config: Config,
    /// Registered peers and when each was last heard.
    clients: HashMap<SocketAddr, Instant>,
    talker: Option<Talker>,
    parrot: Option<Parrot>,
    client_count: &'a AtomicUsize,
    parrot_recorded: &'a AtomicUsize,
}

impl Loop<'_> {
    fn run(mut self, shutdown: &AtomicBool) {
        let mut buf = [0u8; BUFFER_LEN];
        while !shutdown.load(Ordering::Relaxed) {
            let wait = self.next_wait(Instant::now());
            if self.socket.set_read_timeout(Some(wait)).is_err() {
                break;
            }
            match self.socket.recv_from(&mut buf) {
                Ok((len, from)) => self.handle(&buf[..len], from, Instant::now()),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => break,
            }

            let now = Instant::now();
            let timeout = self.config.client_timeout;
            self.clients
                .retain(|_, seen| now.duration_since(*seen) < timeout);
            self.publish_count();
            if self
                .talker
                .as_ref()
                .is_some_and(|t| now.duration_since(t.last_heard) >= TALKER_WATCHDOG)
            {
                self.release(None, now);
            }
            self.drive_parrot(now);
        }
    }

    fn publish_count(&self) {
        self.client_count
            .store(self.clients.len(), Ordering::Relaxed);
    }

    /// How long the next read may block: until the nearest timer, within
    /// [`MIN_POLL_TIMEOUT`]..=[`SOCKET_POLL_TIMEOUT`].
    fn next_wait(&self, now: Instant) -> Duration {
        let watchdog = self.talker.as_ref().map(|t| t.last_heard + TALKER_WATCHDOG);
        let replay = self.parrot.as_ref().and_then(|p| {
            p.replay_at.map(|at| {
                at + self.config.replay_interval * u32::try_from(p.sent).unwrap_or(u32::MAX)
            })
        });
        [watchdog, replay]
            .into_iter()
            .flatten()
            .min()
            .map_or(SOCKET_POLL_TIMEOUT, |due| {
                due.saturating_duration_since(now)
                    .clamp(MIN_POLL_TIMEOUT, SOCKET_POLL_TIMEOUT)
            })
    }

    fn handle(&mut self, datagram: &[u8], from: SocketAddr, now: Instant) {
        let Some(packet) = wire::parse(datagram) else {
            return;
        };
        match packet {
            Packet::Poll(_) => {
                self.clients.insert(from, now);
                // Published before the echo goes out: a client holding its
                // echo is registered, and a test asking at that moment must
                // not race the loop's own bookkeeping.
                self.publish_count();
                // "Return the poll": the client's own bytes, unaltered.
                let _ = self.socket.send_to(datagram, from);
            }
            Packet::Unlink(_) => {
                self.clients.remove(&from);
                self.publish_count();
            }
            Packet::Voice(_) | Packet::End | Packet::Unknown(_) => {
                let Some(seen) = self.clients.get_mut(&from) else {
                    // Never polled: dropped, as the reference drops it.
                    return;
                };
                *seen = now;
                match packet {
                    Packet::Voice(_) => self.voice(datagram, from, now),
                    Packet::End if self.talker.as_ref().is_some_and(|t| t.addr == from) => {
                        self.relay(datagram, from);
                        self.release(Some(datagram), now);
                    }
                    _ => {}
                }
            }
        }
    }

    fn voice(&mut self, datagram: &[u8], from: SocketAddr, now: Instant) {
        match &mut self.talker {
            Some(t) if t.addr == from => t.last_heard = now,
            Some(_) => return,
            None => {
                self.talker = Some(Talker {
                    addr: from,
                    last_heard: now,
                });
                if self.config.parrot {
                    // Only the last over is kept, even if one is playing.
                    self.parrot = Some(Parrot {
                        owner: from,
                        records: Vec::new(),
                        end: wire::end_record().to_vec(),
                        replay_at: None,
                        sent: 0,
                    });
                    self.parrot_recorded.store(0, Ordering::Relaxed);
                }
            }
        }
        self.relay(datagram, from);
        if let Some(p) = self.parrot.as_mut()
            && p.owner == from
            && p.replay_at.is_none()
            && p.records.len() < PARROT_RECORD_CAP
        {
            p.records.push(datagram.to_vec());
            self.parrot_recorded
                .store(p.records.len(), Ordering::Relaxed);
        }
    }

    /// Sends `datagram` to every registered peer except `from`.
    fn relay(&self, datagram: &[u8], from: SocketAddr) {
        for &peer in self.clients.keys() {
            if peer != from {
                let _ = self.socket.send_to(datagram, peer);
            }
        }
    }

    /// Ends the current over: by its own `0x80` (`end`), or by the
    /// watchdog (`None`). The parrot's replay clock starts here.
    fn release(&mut self, end: Option<&[u8]>, now: Instant) {
        let Some(talker) = self.talker.take() else {
            return;
        };
        if let Some(p) = self.parrot.as_mut()
            && p.owner == talker.addr
            && p.replay_at.is_none()
        {
            if let Some(end) = end {
                p.end = end.to_vec();
            }
            p.replay_at = Some(now + self.config.replay_delay);
        }
    }

    fn drive_parrot(&mut self, now: Instant) {
        let interval = self.config.replay_interval;
        let Some(p) = self.parrot.as_mut() else {
            return;
        };
        let Some(start) = p.replay_at else {
            return;
        };
        if now < start {
            return;
        }
        // Every record whose slot has come, so a late wake-up catches up
        // rather than stretching the over (as `P25Parrot.cpp` does).
        let wanted = if interval.is_zero() {
            p.total()
        } else {
            let slots = now.duration_since(start).as_nanos() / interval.as_nanos() + 1;
            usize::try_from(slots).unwrap_or(usize::MAX).min(p.total())
        };
        while p.sent < wanted {
            let _ = self.socket.send_to(p.item(p.sent), p.owner);
            p.sent += 1;
        }
        if p.sent >= p.total() {
            self.parrot = None;
        }
    }
}
