// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Shared userspace single-peer `WireGuard` stack (iax-8516).
//!
//! [`WgStack`] replaces what kernel+TUN did, entirely in userspace: one
//! boringtun `Tunn` session to a single peer, an underlay [`UdpTransport`],
//! and a background I/O thread with a 250 ms timer tick (handshake retry,
//! rekey, persistent keepalive — same cadence as [`crate::WgTunnel`]).
//!
//! Multiple [`WgSocket`]s bind tunnel-inner UDP ports on the stack; the recv
//! path demuxes decrypted inner IPv4/UDP packets by destination port to the
//! owning socket's queue. Malformed inner packets (non-IPv4, non-UDP, bad
//! checksum) and packets for unbound ports are counted and dropped — the
//! stack never panics on wire input.

use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use boringtun::noise::{Tunn, TunnResult};

use crate::config::{SecretResolver, WgConfigError, WgLinkConfig};
use crate::io::UdpTransport;
use crate::packet::{build_udp4, parse_udp4};

/// First port of the ephemeral range used by [`WgStack::bind`] with port 0
/// (mirrors the IANA dynamic range).
const EPHEMERAL_START: u16 = 49152;

/// Wake callback registered on a [`WgSocket`]: invoked from the stack's I/O
/// thread whenever a datagram is queued for that socket. Kept generic (not a
/// `mio::Waker`) so this crate stays engine-agnostic — an engine-side adapter
/// wraps its waker in the closure.
pub type WakeCallback = Box<dyn Fn() + Send>;

/// Counters and handshake age reported by [`WgStack::status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WgStackStatus {
    /// Time since the last completed handshake; `None` before the first one.
    pub last_handshake_age: Option<Duration>,
    /// Inner datagrams successfully encapsulated and sent to the underlay.
    pub tx_packets: u64,
    /// Inner datagrams delivered to a bound socket.
    pub rx_packets: u64,
    /// Inner packets dropped: malformed (non-IPv4 / non-UDP / bad checksum /
    /// truncated) or addressed to a port no socket has bound.
    pub dropped_packets: u64,
}

/// Lock a mutex, ignoring poisoning: the guarded state (`Tunn`, transport,
/// port table) stays internally consistent even if a holder panicked, and the
/// stack must never panic on wire input.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn stack_stopped() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotConnected,
        "wireguard stack thread stopped",
    )
}

/// Per-socket state shared between the stack's I/O thread (producer) and the
/// [`WgSocket`] handle (consumer).
struct SocketShared {
    /// Received inner datagrams: payload + inner source address.
    queue: Mutex<VecDeque<(Vec<u8>, SocketAddrV4)>>,
    /// Wake callback, invoked after a push. Separate lock from `queue` so a
    /// callback may call [`WgSocket::recv_from`] without deadlocking.
    waker: Mutex<Option<WakeCallback>>,
}

/// Bound inner ports. `next_ephemeral` is the search cursor for port-0 binds.
struct PortTable {
    map: HashMap<u16, Arc<SocketShared>>,
    next_ephemeral: u16,
}

struct StackInner {
    tunn: Mutex<Tunn>,
    transport: Mutex<Box<dyn UdpTransport>>,
    endpoint: SocketAddr,
    tunnel_ip: Ipv4Addr,
    ports: Mutex<PortTable>,
    /// Set by [`WgStack::drop`]; the I/O thread exits on observing it.
    shutdown: AtomicBool,
    /// Set by the I/O thread on exit (clean or fatal); socket ops then error.
    thread_dead: AtomicBool,
    tx_packets: AtomicU64,
    rx_packets: AtomicU64,
    dropped_packets: AtomicU64,
}

impl StackInner {
    fn running(&self) -> bool {
        !self.shutdown.load(Ordering::SeqCst) && !self.thread_dead.load(Ordering::SeqCst)
    }

    /// Send one raw `WireGuard` datagram to the endpoint.
    fn underlay_send(&self, data: &[u8]) -> std::io::Result<usize> {
        lock(&self.transport).send_to(data, self.endpoint)
    }

    /// Drive boringtun timers (handshake retry, rekey, persistent keepalive).
    /// Returns `false` on a fatal error (session expired) — mirror of
    /// [`crate::WgTunnel::tick`].
    fn tick(&self, scratch: &mut [u8]) -> bool {
        let mut tunn = lock(&self.tunn);
        match tunn.update_timers(scratch) {
            TunnResult::WriteToNetwork(p) => {
                let owned = p.to_vec();
                drop(tunn);
                if let Err(e) = self.underlay_send(&owned) {
                    tracing::warn!("wireguard stack: timer send failed: {e}");
                }
                true
            }
            TunnResult::Err(e) => {
                tracing::error!("wireguard stack: timer update fatal error: {e:?}");
                false
            }
            _ => true,
        }
    }

    /// Decapsulate one underlay datagram and route the result: protocol
    /// follow-ups back to the network (draining boringtun's queue per its API
    /// contract), decrypted inner packets to the demux.
    fn handle_datagram(&self, datagram: &[u8], src: SocketAddr) {
        let mut out = vec![0u8; 65535];
        let mut tunn = lock(&self.tunn);
        match tunn.decapsulate(Some(src.ip()), datagram, &mut out) {
            TunnResult::WriteToNetwork(p) => {
                let mut pending = vec![p.to_vec()];
                // Drain queued packets per boringtun's API contract ("repeat
                // with an empty datagram until Done").
                loop {
                    let mut more = vec![0u8; 65535];
                    match tunn.decapsulate(None, &[], &mut more) {
                        TunnResult::WriteToNetwork(p) => pending.push(p.to_vec()),
                        _ => break,
                    }
                }
                drop(tunn);
                for p in pending {
                    if let Err(e) = self.underlay_send(&p) {
                        tracing::warn!("wireguard stack: send failed: {e}");
                    }
                }
            }
            TunnResult::WriteToTunnelV4(pkt, _) => {
                let owned = pkt.to_vec();
                drop(tunn);
                self.deliver(&owned);
            }
            TunnResult::WriteToTunnelV6(..) => {
                // IPv4-only inside the tunnel for v1.
                self.dropped_packets.fetch_add(1, Ordering::Relaxed);
            }
            TunnResult::Done => {}
            TunnResult::Err(e) => tracing::warn!("wireguard stack: decapsulate: {e:?}"),
        }
    }

    /// Parse a decrypted inner packet and demux it by destination port to the
    /// bound socket. Malformed or unroutable packets are counted and dropped.
    fn deliver(&self, inner: &[u8]) {
        let parsed = match parse_udp4(inner) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("wireguard stack: dropping malformed inner packet: {e}");
                self.dropped_packets.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        let Some(shared) = lock(&self.ports).map.get(&parsed.dst.port()).cloned() else {
            tracing::debug!(
                "wireguard stack: dropping packet for unbound port {}",
                parsed.dst.port()
            );
            self.dropped_packets.fetch_add(1, Ordering::Relaxed);
            return;
        };
        lock(&shared.queue).push_back((parsed.payload.to_vec(), parsed.src));
        self.rx_packets.fetch_add(1, Ordering::Relaxed);
        if let Some(wake) = lock(&shared.waker).as_ref() {
            wake();
        }
    }

    /// I/O thread body: drain underlay datagrams, tick timers every 250 ms,
    /// nap briefly when idle. Exits on shutdown or fatal timer error.
    fn run(&self) {
        let mut last_tick = std::time::Instant::now();
        let mut udp = vec![0u8; 65535];
        let mut scratch = vec![0u8; 65535];
        while !self.shutdown.load(Ordering::SeqCst) {
            let mut did_work = false;
            loop {
                let recv = lock(&self.transport).recv_from(&mut udp);
                match recv {
                    Ok((n, src)) => {
                        did_work = true;
                        self.handle_datagram(&udp[..n], src);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        tracing::warn!("wireguard stack: recv_from failed: {e}");
                        break;
                    }
                }
            }
            if last_tick.elapsed() >= Duration::from_millis(250) {
                if !self.tick(&mut scratch) {
                    tracing::error!("wireguard stack: session expired; stopping I/O thread");
                    break;
                }
                last_tick = std::time::Instant::now();
            }
            if !did_work {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        self.thread_dead.store(true, Ordering::SeqCst);
    }
}

/// Shared userspace single-peer `WireGuard` tunnel. Bind inner UDP ports with
/// [`WgStack::bind`]; drop the stack to stop and join its I/O thread.
pub struct WgStack {
    inner: Arc<StackInner>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl WgStack {
    /// Build the stack and start its I/O thread.
    ///
    /// The private key is resolved from `cfg`'s reference via `resolver` here,
    /// exactly once, and handed straight to boringtun — it is never stored in
    /// the config or the stack (house secret rule). Persistent keepalive is
    /// taken from `cfg` (0 disables it).
    pub fn new(
        cfg: &WgLinkConfig,
        resolver: &SecretResolver,
        transport: Box<dyn UdpTransport>,
    ) -> Result<Self, WgConfigError> {
        let private_key = cfg.resolve_private_key(resolver)?;
        let keepalive = cfg.keepalive();
        let tunn = Tunn::new(
            private_key,
            cfg.peer_public_key(),
            None,
            (keepalive != 0).then_some(keepalive),
            0,
            None,
        )
        .map_err(|e| WgConfigError::Key(e.to_string()))?;
        let inner = Arc::new(StackInner {
            tunn: Mutex::new(tunn),
            transport: Mutex::new(transport),
            endpoint: cfg.endpoint(),
            tunnel_ip: cfg.tunnel_ip(),
            ports: Mutex::new(PortTable {
                map: HashMap::new(),
                next_ephemeral: EPHEMERAL_START,
            }),
            shutdown: AtomicBool::new(false),
            thread_dead: AtomicBool::new(false),
            tx_packets: AtomicU64::new(0),
            rx_packets: AtomicU64::new(0),
            dropped_packets: AtomicU64::new(0),
        });
        let thread_inner = Arc::clone(&inner);
        let join = std::thread::Builder::new()
            .name("wg-stack".into())
            .spawn(move || thread_inner.run())
            .expect("spawn wg-stack thread");
        Ok(Self {
            inner,
            join: Some(join),
        })
    }

    /// Bind a tunnel-inner UDP port. Port 0 allocates an ephemeral port from
    /// the 49152+ range (mirroring OS semantics); binding a port that is
    /// already bound fails with `AddrInUse`. Dropping the returned socket
    /// frees its port.
    pub fn bind(&self, port: u16) -> std::io::Result<WgSocket> {
        if !self.inner.running() {
            return Err(stack_stopped());
        }
        let shared = Arc::new(SocketShared {
            queue: Mutex::new(VecDeque::new()),
            waker: Mutex::new(None),
        });
        let mut ports = lock(&self.inner.ports);
        let port = if port == 0 {
            let span = usize::from(u16::MAX - EPHEMERAL_START) + 1;
            let mut candidate = ports.next_ephemeral;
            let mut found = None;
            for _ in 0..span {
                if !ports.map.contains_key(&candidate) {
                    found = Some(candidate);
                    break;
                }
                candidate = if candidate == u16::MAX {
                    EPHEMERAL_START
                } else {
                    candidate + 1
                };
            }
            let Some(p) = found else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "no free ephemeral ports",
                ));
            };
            ports.next_ephemeral = if p == u16::MAX {
                EPHEMERAL_START
            } else {
                p + 1
            };
            p
        } else {
            if ports.map.contains_key(&port) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!("inner port {port} already bound"),
                ));
            }
            port
        };
        ports.map.insert(port, Arc::clone(&shared));
        drop(ports);
        Ok(WgSocket {
            inner: Arc::clone(&self.inner),
            shared,
            port,
        })
    }

    /// The stack's tunnel-inner IPv4 address (the source address of every
    /// inner packet it sends).
    #[must_use]
    pub fn tunnel_ip(&self) -> Ipv4Addr {
        self.inner.tunnel_ip
    }

    /// Handshake age and traffic counters, for periodic operational logging.
    #[must_use]
    pub fn status(&self) -> WgStackStatus {
        let last_handshake_age = lock(&self.inner.tunn).time_since_last_handshake();
        WgStackStatus {
            last_handshake_age,
            tx_packets: self.inner.tx_packets.load(Ordering::Relaxed),
            rx_packets: self.inner.rx_packets.load(Ordering::Relaxed),
            dropped_packets: self.inner.dropped_packets.load(Ordering::Relaxed),
        }
    }

    // ---- test seams (cfg(test) only) ------------------------------------

    /// Drain the datagrams the stack has sent to the underlay (`FakeTransport`).
    #[cfg(test)]
    pub(crate) fn drain_underlay_sent(&self) -> Vec<(Vec<u8>, SocketAddr)> {
        let mut transport = lock(&self.inner.transport);
        let ft = (&mut **transport as &mut dyn std::any::Any)
            .downcast_mut::<crate::io::FakeTransport>()
            .expect("test transport is FakeTransport");
        std::mem::take(&mut ft.sent)
    }

    /// Queue an underlay datagram for the stack's I/O thread to receive.
    #[cfg(test)]
    pub(crate) fn push_underlay_inbound(&self, data: Vec<u8>, src: SocketAddr) {
        let mut transport = lock(&self.inner.transport);
        let ft = (&mut **transport as &mut dyn std::any::Any)
            .downcast_mut::<crate::io::FakeTransport>()
            .expect("test transport is FakeTransport");
        ft.push_inbound(data, src);
    }
}

impl std::fmt::Debug for WgStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The Tunn (which holds key material) is deliberately not printed.
        f.debug_struct("WgStack")
            .field("endpoint", &self.inner.endpoint)
            .field("tunnel_ip", &self.inner.tunnel_ip)
            .finish_non_exhaustive()
    }
}

impl Drop for WgStack {
    fn drop(&mut self) {
        // Flag the I/O thread down and join it. The thread polls (5 ms naps
        // between idle sweeps), so it observes the flag promptly; no separate
        // wake channel is needed.
        self.inner.shutdown.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// A tunnel-inner UDP socket bound on a [`WgStack`]. Non-blocking: `recv_from`
/// returns `WouldBlock` when the queue is empty. Dropping the socket frees its
/// port.
pub struct WgSocket {
    inner: Arc<StackInner>,
    shared: Arc<SocketShared>,
    port: u16,
}

impl WgSocket {
    /// The bound tunnel-inner port.
    #[must_use]
    pub fn local_port(&self) -> u16 {
        self.port
    }

    /// Register (or clear) the wake callback invoked from the stack's I/O
    /// thread whenever a datagram is queued for this socket.
    pub fn set_wake(&self, wake: Option<WakeCallback>) {
        *lock(&self.shared.waker) = wake;
    }

    /// Encapsulate `payload` as an inner IPv4/UDP datagram from this socket's
    /// port to `dst` and send it to the peer, inline on the caller's thread.
    ///
    /// Underlay send errors surface here. Before the handshake completes,
    /// boringtun queues/replaces the payload and emits a handshake initiation
    /// instead — the datagram may be delivered late or dropped, which upper
    /// layers (IAX2 retransmission) already tolerate.
    pub fn send_to(&self, payload: &[u8], dst: SocketAddrV4) -> std::io::Result<usize> {
        if !self.inner.running() {
            return Err(stack_stopped());
        }
        let src = SocketAddrV4::new(self.inner.tunnel_ip, self.port);
        let inner_pkt = build_udp4(src, dst, payload)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        // boringtun requires dst to be at least max(src.len() + 32, 148) bytes
        // (the 148-byte minimum covers the handshake initiation message).
        let mut dst_buf = vec![0u8; (inner_pkt.len() + 32).max(148)];
        let mut tunn = lock(&self.inner.tunn);
        match tunn.encapsulate(&inner_pkt, &mut dst_buf) {
            TunnResult::WriteToNetwork(p) => {
                let owned = p.to_vec();
                drop(tunn);
                self.inner.underlay_send(&owned)?;
            }
            TunnResult::Err(e) => {
                return Err(std::io::Error::other(format!(
                    "wireguard encapsulate: {e:?}"
                )));
            }
            _ => {}
        }
        self.inner.tx_packets.fetch_add(1, Ordering::Relaxed);
        Ok(payload.len())
    }

    /// Pop the next received datagram into `buf`, returning its length and the
    /// inner source address. Returns `WouldBlock` when the queue is empty and
    /// an error if the stack has stopped.
    pub fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddrV4)> {
        if let Some((payload, src)) = lock(&self.shared.queue).pop_front() {
            let n = payload.len().min(buf.len());
            buf[..n].copy_from_slice(&payload[..n]);
            return Ok((n, src));
        }
        if !self.inner.running() {
            return Err(stack_stopped());
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "no data",
        ))
    }
}

impl std::fmt::Debug for WgSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WgSocket")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl Drop for WgSocket {
    fn drop(&mut self) {
        lock(&self.inner.ports).map.remove(&self.port);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::FakeTransport;
    use base64::Engine as _;
    use boringtun::x25519::{PublicKey, StaticSecret};
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    const A_UNDERLAY: &str = "192.0.2.1:51820";
    const B_UNDERLAY: &str = "192.0.2.2:51820";
    const A_TUNNEL_IP: Ipv4Addr = Ipv4Addr::new(10, 99, 0, 1);
    const B_TUNNEL_IP: Ipv4Addr = Ipv4Addr::new(10, 99, 0, 2);

    fn b64(k: [u8; 32]) -> String {
        base64::engine::general_purpose::STANDARD.encode(k)
    }

    /// Build a stack with tunnel ip `ip`, key seed `seed`, peering with the
    /// public key of `peer_seed`, over a `FakeTransport`.
    fn stack(ip: Ipv4Addr, seed: u8, peer_seed: u8, endpoint: &str, keepalive: u16) -> WgStack {
        let private = StaticSecret::from([seed; 32]);
        let peer_pub = PublicKey::from(&StaticSecret::from([peer_seed; 32]));
        let cfg = WgLinkConfig::new(
            "TEST_KEY",
            &format!("{ip}/32"),
            &b64(peer_pub.to_bytes()),
            endpoint,
            &["10.99.0.0/24".to_string()],
            keepalive,
        )
        .expect("valid config");
        let resolver = move |_: &str| b64(private.to_bytes());
        WgStack::new(&cfg, &resolver, Box::new(FakeTransport::new())).expect("stack builds")
    }

    /// A pair of stacks wired back-to-back: A's underlay tx feeds B's rx and
    /// vice versa (via [`shuttle`]).
    fn pair(keepalive_a: u16) -> (WgStack, WgStack) {
        let a = stack(A_TUNNEL_IP, 1, 2, B_UNDERLAY, keepalive_a);
        let b = stack(B_TUNNEL_IP, 2, 1, A_UNDERLAY, 25);
        (a, b)
    }

    /// Move pending underlay datagrams in both directions. Returns the
    /// datagrams A sent (pre-delivery), for tests that inspect the wire.
    fn shuttle(a: &WgStack, b: &WgStack) -> Vec<Vec<u8>> {
        let from_a: Vec<Vec<u8>> = a
            .drain_underlay_sent()
            .into_iter()
            .map(|(data, _)| data)
            .collect();
        for data in &from_a {
            b.push_underlay_inbound(data.clone(), A_UNDERLAY.parse().unwrap());
        }
        for (data, _) in b.drain_underlay_sent() {
            a.push_underlay_inbound(data, B_UNDERLAY.parse().unwrap());
        }
        from_a
    }

    /// Wait-until polling helper (house style — no fixed sleeps).
    fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn recv_ready(sock: &WgSocket) -> Option<(Vec<u8>, SocketAddrV4)> {
        let mut buf = [0u8; 2048];
        match sock.recv_from(&mut buf) {
            Ok((n, src)) => Some((buf[..n].to_vec(), src)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
            Err(e) => panic!("recv_from: {e}"),
        }
    }

    #[test]
    fn handshake_completes_and_round_trips_both_directions() {
        let (a, b) = pair(25);
        let a_sock = a.bind(0).unwrap();
        let b_sock = b.bind(4569).unwrap();

        // A initiates: the first send triggers the handshake; boringtun queues
        // the payload and flushes it when the handshake completes.
        a_sock
            .send_to(b"hello", SocketAddrV4::new(B_TUNNEL_IP, 4569))
            .unwrap();
        let (payload, from) = wait_for("A->B delivery", || {
            shuttle(&a, &b);
            recv_ready(&b_sock)
        });
        assert_eq!(payload, b"hello");
        assert_eq!(from, SocketAddrV4::new(A_TUNNEL_IP, a_sock.local_port()));

        // Reply B -> A to the observed source address.
        b_sock.send_to(b"world", from).unwrap();
        let (payload, from) = wait_for("B->A delivery", || {
            shuttle(&a, &b);
            recv_ready(&a_sock)
        });
        assert_eq!(payload, b"world");
        assert_eq!(from, SocketAddrV4::new(B_TUNNEL_IP, 4569));

        // Both sides have a completed handshake and traffic counters.
        assert!(a.status().last_handshake_age.is_some());
        assert!(b.status().last_handshake_age.is_some());
        assert!(a.status().tx_packets >= 1);
        assert!(b.status().rx_packets >= 1);
    }

    #[test]
    fn demux_delivers_to_the_bound_port() {
        let (a, b) = pair(25);
        let a_sock = a.bind(0).unwrap();
        let b_one = b.bind(4569).unwrap();
        let b_two = b.bind(5060).unwrap();

        a_sock
            .send_to(b"for-4569", SocketAddrV4::new(B_TUNNEL_IP, 4569))
            .unwrap();
        a_sock
            .send_to(b"for-5060", SocketAddrV4::new(B_TUNNEL_IP, 5060))
            .unwrap();

        let (p1, p2) = wait_for("both ports served", || {
            shuttle(&a, &b);
            // Peek both queues without consuming until both have data.
            let q1 = lock(&b_one.shared.queue).front().cloned();
            let q2 = lock(&b_two.shared.queue).front().cloned();
            match (q1, q2) {
                (Some(p1), Some(p2)) => Some((p1, p2)),
                _ => None,
            }
        });
        assert_eq!(p1.0, b"for-4569");
        assert_eq!(p2.0, b"for-5060");
    }

    #[test]
    fn unknown_port_and_malformed_inner_are_counted_and_dropped() {
        // Raw boringtun peer against stack B, mirroring the WgTunnel tests, so
        // we can inject arbitrary (including malformed) inner packets.
        let b = stack(B_TUNNEL_IP, 2, 1, A_UNDERLAY, 25);
        let b_sock = b.bind(4569).unwrap();
        let peer_priv = StaticSecret::from([1u8; 32]);
        let b_pub = PublicKey::from(&StaticSecret::from([2u8; 32]));
        let mut peer = Tunn::new(peer_priv, b_pub, None, None, 7, None).expect("valid peer tunn");

        // Handshake: peer initiates by encapsulating a valid packet; the queued
        // packet is flushed to the stack once the handshake completes.
        let valid = build_udp4(
            SocketAddrV4::new(A_TUNNEL_IP, 40000),
            SocketAddrV4::new(B_TUNNEL_IP, 4569),
            b"ok",
        )
        .unwrap();
        let mut buf = vec![0u8; 2048];
        let TunnResult::WriteToNetwork(init) = peer.encapsulate(&valid, &mut buf) else {
            panic!("expected handshake initiation");
        };
        b.push_underlay_inbound(init.to_vec(), A_UNDERLAY.parse().unwrap());

        // Complete the handshake: B replies; feed the response to the peer and
        // forward the peer's queued data packets to B until B's socket sees the
        // valid payload.
        wait_for("handshake + valid delivery", || {
            for (data, _) in b.drain_underlay_sent() {
                let mut out = vec![0u8; 2048];
                if let TunnResult::WriteToNetwork(p) = peer.decapsulate(None, &data, &mut out) {
                    b.push_underlay_inbound(p.to_vec(), A_UNDERLAY.parse().unwrap());
                    // Drain queued packets per boringtun's API contract.
                    loop {
                        let mut more = vec![0u8; 2048];
                        match peer.decapsulate(None, &[], &mut more) {
                            TunnResult::WriteToNetwork(q) => {
                                b.push_underlay_inbound(q.to_vec(), A_UNDERLAY.parse().unwrap());
                            }
                            _ => break,
                        }
                    }
                }
            }
            recv_ready(&b_sock)
        });
        let dropped_before = b.status().dropped_packets;

        // 1) Unknown inner port: valid packet for a port nobody bound.
        let unknown = build_udp4(
            SocketAddrV4::new(A_TUNNEL_IP, 40000),
            SocketAddrV4::new(B_TUNNEL_IP, 9999),
            b"nobody",
        )
        .unwrap();
        // 2) Malformed: flip a payload bit so the UDP checksum fails.
        let mut bad_csum = valid.clone();
        let last = bad_csum.len() - 1;
        bad_csum[last] ^= 0x01;
        // 3) Malformed: non-UDP protocol (TCP) with a fixed-up IPv4 checksum.
        let mut tcp = valid.clone();
        tcp[9] = 6;
        tcp[10] = 0;
        tcp[11] = 0;
        let mut sum = 0u32;
        for w in tcp[..20].chunks_exact(2) {
            sum += u32::from(u16::from_be_bytes([w[0], w[1]]));
        }
        while sum > 0xFFFF {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        #[allow(clippy::cast_possible_truncation)]
        let csum = !(sum as u16);
        tcp[10..12].copy_from_slice(&csum.to_be_bytes());

        for pkt in [&unknown, &bad_csum, &tcp] {
            let mut buf = vec![0u8; 2048];
            let TunnResult::WriteToNetwork(enc) = peer.encapsulate(pkt, &mut buf) else {
                panic!("peer failed to encapsulate after handshake");
            };
            b.push_underlay_inbound(enc.to_vec(), A_UNDERLAY.parse().unwrap());
        }

        wait_for("three packets dropped", || {
            (b.status().dropped_packets >= dropped_before + 3).then_some(())
        });
        // Nothing further reached the bound socket.
        assert!(recv_ready(&b_sock).is_none());
    }

    #[test]
    fn keepalive_emitted_on_tick_after_handshake() {
        // A has a 1-second persistent keepalive; after the handshake, its
        // 250 ms tick must emit a keepalive (32-byte data-type message: 16-byte
        // header + 16-byte tag over an empty payload) without any socket send.
        let (a, b) = pair(1);
        let a_sock = a.bind(0).unwrap();
        let b_sock = b.bind(4569).unwrap();
        a_sock
            .send_to(b"x", SocketAddrV4::new(B_TUNNEL_IP, 4569))
            .unwrap();
        wait_for("handshake + delivery", || {
            shuttle(&a, &b);
            recv_ready(&b_sock)
        });

        wait_for("keepalive on the wire", || {
            let from_a = shuttle(&a, &b);
            from_a
                .iter()
                .find(|d| d.len() == 32 && d.first() == Some(&4))
                .map(|_| ())
        });
    }

    #[test]
    fn ephemeral_ports_allocate_skip_and_reuse_after_drop() {
        let a = stack(A_TUNNEL_IP, 1, 2, B_UNDERLAY, 25);
        let s1 = a.bind(0).unwrap();
        let s2 = a.bind(0).unwrap();
        assert!(s1.local_port() >= EPHEMERAL_START);
        assert!(s2.local_port() >= EPHEMERAL_START);
        assert_ne!(s1.local_port(), s2.local_port());

        // Explicitly binding a taken port errors...
        let err = a.bind(s1.local_port()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        // ...and an ephemeral allocation skips both taken ports.
        let s3 = a.bind(0).unwrap();
        assert_ne!(s3.local_port(), s1.local_port());
        assert_ne!(s3.local_port(), s2.local_port());

        // Dropping a socket frees its port for explicit rebinding.
        let freed = s1.local_port();
        drop(s1);
        let s4 = a.bind(freed).unwrap();
        assert_eq!(s4.local_port(), freed);
    }

    #[test]
    fn explicit_port_conflict_errors() {
        let a = stack(A_TUNNEL_IP, 1, 2, B_UNDERLAY, 25);
        let _held = a.bind(4569).unwrap();
        let err = a.bind(4569).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    }

    #[test]
    fn secret_ref_resolved_exactly_once_and_never_stored() {
        let calls = AtomicUsize::new(0);
        let seen = Mutex::new(Vec::new());
        let key_b64 = b64(StaticSecret::from([1u8; 32]).to_bytes());
        let peer_pub = PublicKey::from(&StaticSecret::from([2u8; 32]));
        let cfg = WgLinkConfig::new(
            "MY_KEY_REF",
            "10.99.0.1/32",
            &b64(peer_pub.to_bytes()),
            B_UNDERLAY,
            &[],
            25,
        )
        .unwrap();
        let resolver = |name: &str| {
            calls.fetch_add(1, Ordering::SeqCst);
            lock(&seen).push(name.to_string());
            key_b64.clone()
        };
        let _stack = WgStack::new(&cfg, &resolver, Box::new(FakeTransport::new())).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(*lock(&seen), vec!["MY_KEY_REF".to_string()]);
        // Config Debug carries the reference, never the material.
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains(&key_b64), "Debug leaked key material: {dbg}");
        assert!(dbg.contains("MY_KEY_REF"));
    }

    #[test]
    fn build_fails_when_resolver_returns_bad_material() {
        let peer_pub = PublicKey::from(&StaticSecret::from([2u8; 32]));
        let cfg = WgLinkConfig::new(
            "MY_KEY_REF",
            "10.99.0.1/32",
            &b64(peer_pub.to_bytes()),
            B_UNDERLAY,
            &[],
            25,
        )
        .unwrap();
        let err = WgStack::new(&cfg, &|_| String::new(), Box::new(FakeTransport::new()))
            .expect_err("empty secret must fail");
        assert!(err.to_string().contains("MY_KEY_REF"), "got: {err}");
    }

    #[test]
    fn wake_callback_fires_on_delivery() {
        let (a, b) = pair(25);
        let a_sock = a.bind(0).unwrap();
        let b_sock = b.bind(4569).unwrap();
        let woken = Arc::new(AtomicUsize::new(0));
        let woken2 = Arc::clone(&woken);
        b_sock.set_wake(Some(Box::new(move || {
            woken2.fetch_add(1, Ordering::SeqCst);
        })));

        a_sock
            .send_to(b"ding", SocketAddrV4::new(B_TUNNEL_IP, 4569))
            .unwrap();
        wait_for("wake callback", || {
            shuttle(&a, &b);
            (woken.load(Ordering::SeqCst) >= 1).then_some(())
        });
        // The queued datagram is there for the woken consumer.
        let (payload, _) = recv_ready(&b_sock).expect("data queued");
        assert_eq!(payload, b"ding");
    }

    #[test]
    fn drop_joins_thread_and_socket_ops_error_afterwards() {
        let a = stack(A_TUNNEL_IP, 1, 2, B_UNDERLAY, 25);
        let sock = a.bind(4569).unwrap();
        // Dropping the stack sets the shutdown flag and joins the I/O thread;
        // completing without hanging is the join assertion.
        drop(a);
        let err = sock
            .send_to(b"late", SocketAddrV4::new(B_TUNNEL_IP, 4569))
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotConnected);
        let mut buf = [0u8; 16];
        let err = sock.recv_from(&mut buf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotConnected);
    }
}
