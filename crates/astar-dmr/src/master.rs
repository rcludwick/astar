// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A loopback MMDVM/homebrew **master**, plus an optional parrot mode — this
//! crate's ONLY I/O module. Everything else in `astar-dmr` is pure
//! encode/decode with no sockets and no threads.
//!
//! It exists because of a house rule: no test may transmit anywhere but
//! `127.0.0.1`. DMR sharpens that rule rather than softening it — a real
//! master authenticates a *registered radio ID*, so a stray packet from a
//! test run is attributable to somebody's licence. Without a master of our
//! own there is nowhere at all to exercise the link, so this is not a
//! nicety.
//!
//! # It performs the real handshake
//!
//! `astar_ysf::reflector` and `astar_nxdn::reflector` echo a poll; there is
//! nothing to get wrong. This one has to be strict, because the thing under
//! test is a four-step chain with a digest in the middle:
//!
//! * `RPTL` → register the peer, mint a **fresh random salt**, answer
//!   `RPTACK` + salt.
//! * `RPTK` → recompute `SHA256(salt ‖ password)` with
//!   [`crate::wire::auth_digest`] and compare. Match: `RPTACK` + id. Mismatch:
//!   `MSTNAK` + id, forget the peer, and count it
//!   ([`MasterHandle::auth_failures`]) — a fixture that accepted any digest
//!   would let a broken one ship.
//! * `RPTC` (302 bytes) → `RPTACK` + id, and the peer is connected.
//! * `RPTPING` → `MSTPONG`. `RPTCL` → drop the peer and, as `hblink.py`'s
//!   master does, `MSTNAK` the goodbye. That NAK is a receipt and a client
//!   must not read it as an error; answering it here is what keeps astar's
//!   own tolerance of it honest.
//! * `DMRD` → relayed **verbatim** to every other connected peer, and to
//!   nobody at all if the sender is not a connected peer at the address it
//!   registered from (`hblink.py`: `_peer_id in self._peers and
//!   CONNECTION == 'YES' and SOCKADDR == _sockaddr`).
//!
//! Verbatim is load-bearing: a master that re-encoded a frame would hide
//! exactly the framing bugs a bench fixture exists to catch.
//!
//! **The talkgroup and timeslot filter is not here, and that is not an
//! omission.** A homebrew master learns nothing about which room a peer is
//! listening to — `RPTC` carries no talkgroup and astar sends no `RPTO` — so
//! it fans a frame out to everyone and the *client* decides what is its
//! audio ([`crate::fsm::DmrFsm`] does exactly that). A fixture that filtered
//! by talkgroup would be a kinder master than any real one and would let a
//! client with no filter of its own pass.
//!
//! # Parrot mode
//!
//! With parrot on, a transmission is captured and played back to whoever
//! sent it, one burst every [`BURST_INTERVAL`] so the replay lands at the
//! pace real audio would. A capture ends at the terminator
//! (`DataSync { data_type: 0x02 }` — `DT_TERMINATOR_WITH_LC`), when the
//! stream id changes, or after a gap long enough that the sender has plainly
//! stopped without saying so.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::wire::{
    self, ACK_LEN, AUTH_LEN, CLOSE_LEN, CONFIG_LEN, FrameType, LOGIN_LEN, NAK_LEN, PING_LEN,
    PONG_LEN, Packet,
};

/// How long a peer may go unheard before it is forgotten.
///
/// `hblink3`'s master reaps at `LAST_PING + PING_TIME × MAX_MISSED`, both
/// from its config file, so there is no protocol constant to copy
/// (`docs/design/dmr-wire.md` §10). Sixty seconds matches the client's own
/// [`crate::fsm::LINK_TIMEOUT`], which is the symmetry a bench fixture wants.
pub const DEFAULT_PEER_TIMEOUT: Duration = Duration::from_secs(60);
/// Pause between a transmission ending and the parrot replaying it.
pub const DEFAULT_PARROT_REPLAY_DELAY: Duration = Duration::from_millis(150);
/// The biggest datagram this master will ever read or relay — `RPTC`, at 302
/// bytes — for callers sizing their own buffers.
pub const MAX_DATAGRAM: usize = CONFIG_LEN;
/// A buffer sized to `DATA_LEN` would truncate every `RPTC`, and the
/// handshake would stall at a step that looks like silence. Checked at
/// compile time so it cannot drift.
const _: () = assert!(MAX_DATAGRAM >= CONFIG_LEN);

/// One burst on the air. `MMDVMHost/DMRDefines.h`: `DMR_SLOT_TIME = 60U`,
/// and `hblink3/playback.py` replays a recorded stream with `sleep(0.06)`
/// between datagrams.
pub const BURST_INTERVAL: Duration = Duration::from_millis(60);

/// Silence after which a transmission is treated as over even without a
/// terminator burst. A bench parrot wants to answer sooner than a repeater's
/// watchdog would.
const TRANSMISSION_GAP: Duration = Duration::from_millis(500);
/// How long a socket read blocks before the loop rechecks its shutdown flag.
const SOCKET_POLL_TIMEOUT: Duration = Duration::from_millis(50);
/// `DT_TERMINATOR_WITH_LC` (`MMDVMHost/DMRDefines.h`): the data type that
/// closes a stream, and what `hblink3/playback.py` stops recording on.
const DT_TERMINATOR_WITH_LC: u8 = 0x02;

/// A peer the master has heard from, and how far through the chain it got.
struct Peer {
    id: u32,
    salt: [u8; 4],
    /// The digest checked out. Set by `RPTK`, required before `RPTC`.
    authenticated: bool,
    /// `RPTC` accepted: this peer may send and receive `DMRD`.
    connected: bool,
    last_seen: Instant,
}

/// One transmission being recorded for playback.
struct Capture {
    owner: SocketAddr,
    stream_id: [u8; 4],
    frames: Vec<Vec<u8>>,
    last_frame: Instant,
    /// When the transmission ended; playback is timed from here.
    ended_at: Option<Instant>,
    next: usize,
}

/// A homebrew/MMDVM master that performs the REAL handshake — salt, digest
/// check, config, ping/pong — and relays `DMRD` between its peers. It exists
/// so that a session-level suite can run against `127.0.0.1` and nothing
/// else.
pub struct Master {
    socket: UdpSocket,
    local_addr: SocketAddr,
    password: String,
    peer_timeout: Duration,
    parrot: bool,
    replay_delay: Duration,
}

impl Master {
    /// Binds a relay-only master to `addr`.
    ///
    /// # Errors
    /// Whatever [`UdpSocket::bind`] or `set_read_timeout` returns.
    pub fn bind(addr: SocketAddr, password: &str) -> io::Result<Master> {
        Self::bind_inner(
            addr,
            password,
            DEFAULT_PEER_TIMEOUT,
            false,
            DEFAULT_PARROT_REPLAY_DELAY,
        )
    }

    /// [`Master::bind`] with a caller-supplied peer timeout — lets a test
    /// shorten the window rather than waiting out the real minute.
    ///
    /// # Errors
    /// Whatever [`UdpSocket::bind`] or `set_read_timeout` returns.
    pub fn bind_with_timeout(
        addr: SocketAddr,
        password: &str,
        peer_timeout: Duration,
    ) -> io::Result<Master> {
        Self::bind_inner(
            addr,
            password,
            peer_timeout,
            false,
            DEFAULT_PARROT_REPLAY_DELAY,
        )
    }

    /// Binds a PARROT master: everything [`Master::bind`] does, plus playing
    /// each transmission back to whoever sent it.
    ///
    /// # Errors
    /// Whatever [`UdpSocket::bind`] or `set_read_timeout` returns.
    pub fn bind_parrot(addr: SocketAddr, password: &str) -> io::Result<Master> {
        Self::bind_inner(
            addr,
            password,
            DEFAULT_PEER_TIMEOUT,
            true,
            DEFAULT_PARROT_REPLAY_DELAY,
        )
    }

    /// [`Master::bind_parrot`] with a caller-supplied peer timeout and replay
    /// delay.
    ///
    /// # Errors
    /// Whatever [`UdpSocket::bind`] or `set_read_timeout` returns.
    pub fn bind_parrot_with_timeouts(
        addr: SocketAddr,
        password: &str,
        peer_timeout: Duration,
        replay_delay: Duration,
    ) -> io::Result<Master> {
        Self::bind_inner(addr, password, peer_timeout, true, replay_delay)
    }

    fn bind_inner(
        addr: SocketAddr,
        password: &str,
        peer_timeout: Duration,
        parrot: bool,
        replay_delay: Duration,
    ) -> io::Result<Master> {
        let socket = UdpSocket::bind(addr)?;
        socket.set_read_timeout(Some(SOCKET_POLL_TIMEOUT))?;
        let local_addr = socket.local_addr()?;
        Ok(Master {
            socket,
            local_addr,
            password: password.to_string(),
            peer_timeout,
            parrot,
            replay_delay,
        })
    }

    /// The address this master is bound to (useful when `addr`'s port was
    /// `0` — the OS picks an ephemeral one).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Starts the run-loop thread and returns a handle to control it.
    ///
    /// # Panics
    /// If the OS refuses to spawn the thread.
    #[must_use]
    pub fn run(self) -> MasterHandle {
        let shutdown = Arc::new(AtomicBool::new(false));
        let peer_count = Arc::new(AtomicUsize::new(0));
        let auth_failures = Arc::new(AtomicUsize::new(0));
        let mut session = Session {
            password: self.password,
            peer_timeout: self.peer_timeout,
            parrot: self.parrot,
            replay_delay: self.replay_delay,
            peers: HashMap::new(),
            capture: None,
            peer_count: Arc::clone(&peer_count),
            auth_failures: Arc::clone(&auth_failures),
        };
        let socket = self.socket;
        let thread_shutdown = Arc::clone(&shutdown);
        let thread = std::thread::Builder::new()
            .name("astar-dmr-master".to_string())
            .spawn(move || session.run_loop(&socket, &thread_shutdown))
            .expect("spawn astar-dmr-master thread");
        MasterHandle {
            shutdown,
            thread: Some(thread),
            peer_count,
            auth_failures,
        }
    }
}

/// A handle to a running [`Master`]'s thread. [`MasterHandle::shutdown`] (or
/// dropping the handle) requests the thread stop and joins it, bounded by the
/// 50 ms socket read timeout.
pub struct MasterHandle {
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    peer_count: Arc<AtomicUsize>,
    auth_failures: Arc<AtomicUsize>,
}

impl MasterHandle {
    /// Requests the run-loop thread stop and joins it.
    pub fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// How many peers have completed the whole chain and are connected.
    #[must_use]
    pub fn peer_count(&self) -> usize {
        self.peer_count.load(Ordering::Relaxed)
    }

    /// How many logins were refused for a bad digest. The auth test asserts
    /// on this rather than on a log line.
    #[must_use]
    pub fn auth_failures(&self) -> usize {
        self.auth_failures.load(Ordering::Relaxed)
    }
}

impl Drop for MasterHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Everything the run loop owns. A struct rather than a dozen parameters
/// threaded through every handler.
struct Session {
    password: String,
    peer_timeout: Duration,
    parrot: bool,
    replay_delay: Duration,
    peers: HashMap<SocketAddr, Peer>,
    capture: Option<Capture>,
    peer_count: Arc<AtomicUsize>,
    auth_failures: Arc<AtomicUsize>,
}

impl Session {
    fn run_loop(&mut self, socket: &UdpSocket, shutdown: &AtomicBool) {
        let mut buf = [0u8; MAX_DATAGRAM];
        while !shutdown.load(Ordering::Relaxed) {
            match socket.recv_from(&mut buf) {
                Ok((len, from)) => {
                    let datagram: Vec<u8> = buf[..len].to_vec();
                    self.handle(socket, &datagram, from);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {}
                Err(_) => break,
            }

            let now = Instant::now();
            self.peers
                .retain(|_, p| now.duration_since(p.last_seen) < self.peer_timeout);
            self.publish_peer_count();

            if self.parrot {
                self.drive_parrot(socket, now);
            }
        }
    }

    /// Connected peers only: a peer part-way through the chain is not one a
    /// test should be able to see as linked.
    fn publish_peer_count(&self) {
        let connected = self.peers.values().filter(|p| p.connected).count();
        self.peer_count.store(connected, Ordering::Relaxed);
    }

    fn handle(&mut self, socket: &UdpSocket, datagram: &[u8], from: SocketAddr) {
        let now = Instant::now();
        // `DMRD` first, and `RPTCL` before `RPTC`: hblink.py matches commands
        // on four bytes and has to re-test `_data[:5]` to separate the two,
        // which is exactly the ambiguity this ordering removes.
        if datagram.starts_with(b"DMRD") {
            self.on_data(socket, datagram, from, now);
        } else if datagram.starts_with(b"RPTL") && datagram.len() == LOGIN_LEN {
            self.on_login(socket, datagram, from, now);
        } else if datagram.starts_with(b"RPTK") && datagram.len() == AUTH_LEN {
            self.on_auth(socket, datagram, from, now);
        } else if datagram.starts_with(b"RPTCL") && datagram.len() == CLOSE_LEN {
            self.on_goodbye(socket, datagram, from);
        } else if datagram.starts_with(b"RPTC") && datagram.len() == CONFIG_LEN {
            self.on_config(socket, datagram, from, now);
        } else if datagram.starts_with(b"RPTPING") && datagram.len() == PING_LEN {
            self.on_ping(socket, datagram, from, now);
        }
        // Anything else — RPTO, a trunking command, noise — is dropped in
        // silence, which is what a real master does with what it has no
        // branch for.
    }

    fn on_login(&mut self, socket: &UdpSocket, datagram: &[u8], from: SocketAddr, now: Instant) {
        let id = be_u32(&datagram[4..8]);
        let salt = mint_salt(from);
        // A second RPTL replaces the peer outright, salt and all — hblink.py
        // does the same, and it is why a client must send exactly one.
        self.peers.insert(
            from,
            Peer {
                id,
                salt,
                authenticated: false,
                connected: false,
                last_seen: now,
            },
        );
        self.publish_peer_count();
        let _ = socket.send_to(&tagged(b"RPTACK", salt, ACK_LEN), from);
    }

    fn on_auth(&mut self, socket: &UdpSocket, datagram: &[u8], from: SocketAddr, now: Instant) {
        let id = be_u32(&datagram[4..8]);
        let Some(peer) = self.peers.get_mut(&from).filter(|p| p.id == id) else {
            let _ = socket.send_to(&tagged(b"MSTNAK", id.to_be_bytes(), NAK_LEN), from);
            return;
        };
        let expected = wire::auth_digest(peer.salt, &self.password);
        if datagram[8..] == expected {
            peer.authenticated = true;
            peer.last_seen = now;
            let _ = socket.send_to(&tagged(b"RPTACK", id.to_be_bytes(), ACK_LEN), from);
        } else {
            // Forget the peer, not just the attempt: hblink.py deletes it,
            // so a client cannot retry a digest against the same salt.
            self.peers.remove(&from);
            self.publish_peer_count();
            self.auth_failures.fetch_add(1, Ordering::Relaxed);
            let _ = socket.send_to(&tagged(b"MSTNAK", id.to_be_bytes(), NAK_LEN), from);
        }
    }

    fn on_config(&mut self, socket: &UdpSocket, datagram: &[u8], from: SocketAddr, now: Instant) {
        let id = be_u32(&datagram[4..8]);
        let Some(peer) = self
            .peers
            .get_mut(&from)
            .filter(|p| p.id == id && p.authenticated)
        else {
            let _ = socket.send_to(&tagged(b"MSTNAK", id.to_be_bytes(), NAK_LEN), from);
            return;
        };
        peer.connected = true;
        peer.last_seen = now;
        // Published before the reply goes out, not after: a client holding
        // its RPTACK is connected, and a test that asks the moment it
        // arrives must not race the run loop's own bookkeeping.
        self.publish_peer_count();
        let _ = socket.send_to(&tagged(b"RPTACK", id.to_be_bytes(), ACK_LEN), from);
    }

    fn on_ping(&mut self, socket: &UdpSocket, datagram: &[u8], from: SocketAddr, now: Instant) {
        let id = be_u32(&datagram[7..11]);
        let Some(peer) = self
            .peers
            .get_mut(&from)
            .filter(|p| p.id == id && p.connected)
        else {
            let _ = socket.send_to(&tagged(b"MSTNAK", id.to_be_bytes(), NAK_LEN), from);
            return;
        };
        peer.last_seen = now;
        let _ = socket.send_to(&tagged(b"MSTPONG", id.to_be_bytes(), PONG_LEN), from);
    }

    fn on_goodbye(&mut self, socket: &UdpSocket, datagram: &[u8], from: SocketAddr) {
        let id = be_u32(&datagram[5..9]);
        self.peers.remove(&from);
        self.publish_peer_count();
        // hblink.py's master answers its own peer's goodbye with MSTNAK. It
        // is a receipt, not a refusal, and sending it here is what keeps the
        // client's tolerance of it under test.
        let _ = socket.send_to(&tagged(b"MSTNAK", id.to_be_bytes(), NAK_LEN), from);
    }

    fn on_data(&mut self, socket: &UdpSocket, datagram: &[u8], from: SocketAddr, now: Instant) {
        let Some(Packet::Data(data)) = wire::parse(datagram) else {
            return;
        };
        // The gate hblink.py applies: a connected peer, at the address it
        // registered from, carrying the id it registered with. Without it a
        // client would pass a test no real master would let it pass.
        let Some(peer) = self
            .peers
            .get_mut(&from)
            .filter(|p| p.connected && p.id == data.peer_id)
        else {
            return;
        };
        peer.last_seen = now;

        for (&addr, other) in &self.peers {
            if addr != from && other.connected {
                let _ = socket.send_to(datagram, addr);
            }
        }

        if self.parrot {
            self.capture(datagram, &data, from, now);
        }
    }

    /// Adds one burst to the capture, starting a fresh one when the speaker
    /// changes, the stream id changes, or the last transmission plainly
    /// ended.
    fn capture(
        &mut self,
        datagram: &[u8],
        data: &wire::DataPacket,
        from: SocketAddr,
        now: Instant,
    ) {
        let stale = self.capture.as_ref().is_some_and(|c| {
            c.owner != from
                || c.stream_id != data.stream_id
                || c.ended_at.is_some()
                || now.duration_since(c.last_frame) > TRANSMISSION_GAP
        });
        if stale {
            self.capture = None;
        }
        let capture = self.capture.get_or_insert_with(|| Capture {
            owner: from,
            stream_id: data.stream_id,
            frames: Vec::new(),
            last_frame: now,
            ended_at: None,
            next: 0,
        });
        capture.frames.push(datagram.to_vec());
        capture.last_frame = now;
        if data.frame_type
            == (FrameType::DataSync {
                data_type: DT_TERMINATOR_WITH_LC,
            })
        {
            capture.ended_at = Some(now);
        }
    }

    fn drive_parrot(&mut self, socket: &UdpSocket, now: Instant) {
        let replay_delay = self.replay_delay;
        let Some(capture) = self.capture.as_mut() else {
            return;
        };
        // A sender that stopped without a terminator still stopped.
        if capture.ended_at.is_none() && now.duration_since(capture.last_frame) > TRANSMISSION_GAP {
            capture.ended_at = Some(capture.last_frame + TRANSMISSION_GAP);
        }
        let Some(ended_at) = capture.ended_at else {
            return;
        };
        let due = ended_at
            + replay_delay
            + BURST_INTERVAL * u32::try_from(capture.next).unwrap_or(u32::MAX);
        if now < due {
            return;
        }
        if let Some(frame) = capture.frames.get(capture.next) {
            let _ = socket.send_to(frame, capture.owner);
            capture.next += 1;
        }
        if capture.next >= capture.frames.len() {
            self.capture = None;
        }
    }
}

/// A tag and its four trailing bytes, in a buffer of exactly `len`.
fn tagged(tag: &[u8], trailer: [u8; 4], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(tag);
    out.extend_from_slice(&trailer);
    debug_assert_eq!(out.len(), len, "tag + four bytes is the whole datagram");
    out
}

fn be_u32(bytes: &[u8]) -> u32 {
    let mut four = [0u8; 4];
    four.copy_from_slice(bytes);
    u32::from_be_bytes(four)
}

/// A fresh four-byte salt.
///
/// `hblink.py` uses `randint(0, 0xFFFFFFFF)`. This needs no `rand`
/// dependency to be right for what it is: `RandomState` is seeded from the
/// OS and re-keyed per instance, so two peers — and two runs — get different
/// salts. It is a loopback fixture, not a credential store.
fn mint_salt(from: SocketAddr) -> [u8; 4] {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    hasher.write(from.to_string().as_bytes());
    let bytes = hasher.finish().to_be_bytes();
    [bytes[0], bytes[1], bytes[2], bytes[3]]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_peers_get_two_different_salts() {
        // A constant salt would make one recorded digest authenticate
        // forever, and would hide a client that ignored the salt entirely.
        let a: SocketAddr = "127.0.0.1:60001".parse().expect("v4");
        let b: SocketAddr = "127.0.0.1:60002".parse().expect("v4");
        assert_ne!(mint_salt(a), mint_salt(b));
        assert_ne!(mint_salt(a), mint_salt(a));
    }
}
