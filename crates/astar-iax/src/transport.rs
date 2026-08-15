// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Engine transport seam (iax-b6f5): socket-factory traits that decouple the
//! run loops (`runtime`, `listener`, `registration`) from `mio::net::UdpSocket`.
//!
//! [`NetStack`] is the factory ("give me a datagram socket"); [`LinkSocket`] is
//! the socket itself. [`OsNetStack`] is the only implementation today and
//! reproduces the pre-seam behavior exactly: a `mio` UDP socket registered
//! READABLE with the consumer's `Poll`. A future tunnel stack (e.g. `WireGuard`)
//! implements the same pair, storing the consumer's `Waker` in
//! [`LinkSocket::register`] and waking it on packet arrival instead of
//! registering an OS fd.
//!
//! The run loops therefore treat readiness uniformly: on ANY poll wakeup they
//! attempt a non-blocking drain (`recv_from` until `WouldBlock`) — for OS UDP
//! that is behaviorally identical to draining only when the socket token fired.

use std::io;
use std::net::{SocketAddr, SocketAddrV4};
use std::sync::Arc;

use astar_wireguard::{WgSocket, WgStack};
use mio::net::UdpSocket;
use mio::{Interest, Registry, Token, Waker};

/// Socket factory: the one seam a transport implementation plugs into.
pub trait NetStack: Send + Sync {
    /// Bind a datagram socket on `addr` (port 0 allocates an ephemeral port,
    /// mirroring OS semantics).
    fn bind(&self, addr: SocketAddr) -> io::Result<Box<dyn LinkSocket>>;
}

/// A bound datagram socket. `Sync` because the listener shares one socket
/// (`Arc<dyn LinkSocket>`) between its demux thread and every leg thread.
///
/// All I/O is non-blocking: `recv_from` returns `WouldBlock` when nothing is
/// pending, exactly like a `mio` UDP socket.
pub trait LinkSocket: Send + Sync {
    /// Connect the socket to `peer`: subsequent [`LinkSocket::send`]s go to
    /// `peer`, and (for the OS impl) the kernel filters inbound datagrams to
    /// that source — the per-call outgoing runtime relies on both.
    fn connect(&mut self, peer: SocketAddr) -> io::Result<()>;

    /// Send on a connected socket (see [`LinkSocket::connect`]).
    fn send(&self, buf: &[u8]) -> io::Result<usize>;

    /// Send to an explicit destination (unconnected/shared-socket path).
    fn send_to(&self, buf: &[u8], dst: SocketAddr) -> io::Result<usize>;

    /// Non-blocking receive; `WouldBlock` when nothing is pending.
    fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)>;

    /// The actually-bound local address (resolves an ephemeral `:0` port).
    fn local_addr(&self) -> io::Result<SocketAddr>;

    /// Hook the socket into the consumer's readiness loop. The OS impl
    /// registers its `mio` socket READABLE on `registry` under `token` (and
    /// ignores `waker`); a waker-driven impl stores `waker` instead and wakes
    /// it on packet arrival.
    fn register(&mut self, registry: &Registry, token: Token, waker: Arc<Waker>) -> io::Result<()>;
}

/// The OS UDP transport: wraps `mio::net::UdpSocket`, byte-identical to the
/// engine's pre-seam behavior.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsNetStack;

impl NetStack for OsNetStack {
    fn bind(&self, addr: SocketAddr) -> io::Result<Box<dyn LinkSocket>> {
        Ok(Box::new(OsLinkSocket {
            sock: UdpSocket::bind(addr)?,
        }))
    }
}

/// A bound OS UDP socket (see [`OsNetStack`]).
struct OsLinkSocket {
    sock: UdpSocket,
}

impl LinkSocket for OsLinkSocket {
    fn connect(&mut self, peer: SocketAddr) -> io::Result<()> {
        self.sock.connect(peer)
    }

    fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.sock.send(buf)
    }

    fn send_to(&self, buf: &[u8], dst: SocketAddr) -> io::Result<usize> {
        self.sock.send_to(buf, dst)
    }

    fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.sock.recv_from(buf)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.sock.local_addr()
    }

    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        _waker: Arc<Waker>,
    ) -> io::Result<()> {
        registry.register(&mut self.sock, token, Interest::READABLE)
    }
}

/// The `WireGuard` transport (iax-927a): adapts the shared userspace
/// [`WgStack`] (one tunnel, one peer) to the engine seam. Every socket bound
/// here is a tunnel-inner UDP port on the SAME stack — the Manager hands one
/// `Arc<WgNetStack>` to every dial runtime, the registrar, and the listener.
///
/// The tunnel-inner network is IPv4-only (v1), so IPv6 addresses are rejected
/// with `InvalidInput` at `bind`/`connect`/`send_to`.
pub struct WgNetStack {
    stack: Arc<WgStack>,
}

impl WgNetStack {
    /// Adapt `stack`. The stack stays shared: clones of the `Arc` may be held
    /// elsewhere (e.g. by the Manager, for `status()` and teardown ordering).
    #[must_use]
    pub fn new(stack: Arc<WgStack>) -> Self {
        Self { stack }
    }
}

impl std::fmt::Debug for WgNetStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WgNetStack")
            .field("stack", &self.stack)
            .finish()
    }
}

/// Reject an IPv6 address at the seam (the tunnel-inner network is IPv4-only).
fn require_v4(addr: SocketAddr, what: &str) -> io::Result<SocketAddrV4> {
    match addr {
        SocketAddr::V4(v4) => Ok(v4),
        SocketAddr::V6(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{what}: the wireguard inner network is IPv4-only, got {addr}"),
        )),
    }
}

impl NetStack for WgNetStack {
    fn bind(&self, addr: SocketAddr) -> io::Result<Box<dyn LinkSocket>> {
        // Mirror OS semantics: the IP part of a bind address is a local
        // interface selection; the stack has exactly one inner address
        // (its tunnel IP), so only the port matters. Port 0 = ephemeral.
        let v4 = require_v4(addr, "bind")?;
        let sock = self.stack.bind(v4.port())?;
        Ok(Box::new(WgLinkSocket {
            tunnel_ip: SocketAddrV4::new(self.stack.tunnel_ip(), sock.local_port()),
            sock,
            peer: None,
        }))
    }
}

/// A tunnel-inner UDP socket (see [`WgNetStack`]).
struct WgLinkSocket {
    sock: WgSocket,
    /// The resolved local address: tunnel IP + bound inner port.
    tunnel_ip: SocketAddrV4,
    /// Connected peer (set by [`LinkSocket::connect`]): the default `send`
    /// destination AND the inbound source filter, mirroring what the kernel
    /// does for a connected OS UDP socket.
    peer: Option<SocketAddrV4>,
}

impl LinkSocket for WgLinkSocket {
    fn connect(&mut self, peer: SocketAddr) -> io::Result<()> {
        self.peer = Some(require_v4(peer, "connect")?);
        Ok(())
    }

    fn send(&self, buf: &[u8]) -> io::Result<usize> {
        let peer = self.peer.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "send() on unconnected socket")
        })?;
        self.sock.send_to(buf, peer)
    }

    fn send_to(&self, buf: &[u8], dst: SocketAddr) -> io::Result<usize> {
        self.sock.send_to(buf, require_v4(dst, "send_to")?)
    }

    fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        loop {
            let (n, src) = self.sock.recv_from(buf)?;
            // Connected filter: drop datagrams from any other source, exactly
            // as the kernel would on a connected UDP socket (the per-call
            // outgoing runtime relies on this).
            if let Some(peer) = self.peer
                && src != peer
            {
                continue;
            }
            return Ok((n, SocketAddr::V4(src)));
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(SocketAddr::V4(self.tunnel_ip))
    }

    fn register(
        &mut self,
        _registry: &Registry,
        _token: Token,
        waker: Arc<Waker>,
    ) -> io::Result<()> {
        // Waker-driven readiness: no OS fd to register. The stack's I/O thread
        // invokes the callback whenever a datagram is queued for this socket;
        // the consumer's run loop then drains on the wakeup (iax-b6f5).
        self.sock.set_wake(Some(Box::new(move || {
            let _ = waker.wake();
        })));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mio::{Events, Poll};
    use std::time::{Duration, Instant};

    fn localhost() -> SocketAddr {
        "127.0.0.1:0".parse().expect("valid addr")
    }

    /// Poll until the socket can deliver a datagram (wait-until, no fixed
    /// sleeps): drain-on-any-wakeup, exactly like the run loops.
    fn recv_wait(
        poll: &mut Poll,
        sock: &dyn LinkSocket,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr)> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut events = Events::with_capacity(8);
        loop {
            match sock.recv_from(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                other => return other,
            }
            assert!(Instant::now() < deadline, "timed out waiting for datagram");
            poll.poll(&mut events, Some(Duration::from_millis(20)))?;
        }
    }

    #[test]
    fn os_stack_bind_send_to_recv_round_trip() {
        let net = OsNetStack;
        let mut rx = net.bind(localhost()).expect("bind rx");
        let tx = net.bind(localhost()).expect("bind tx");
        let rx_addr = rx.local_addr().expect("local addr");
        assert_ne!(rx_addr.port(), 0, "ephemeral port resolved");

        let mut poll = Poll::new().expect("poll");
        let waker = Arc::new(Waker::new(poll.registry(), Token(1)).expect("waker"));
        rx.register(poll.registry(), Token(0), waker)
            .expect("register");

        tx.send_to(b"ping", rx_addr).expect("send_to");

        let mut buf = [0u8; 64];
        let (n, src) = recv_wait(&mut poll, rx.as_ref(), &mut buf).expect("recv");
        assert_eq!(&buf[..n], b"ping");
        assert_eq!(src, tx.local_addr().expect("tx local addr"));
    }

    #[test]
    fn os_stack_connected_send_reaches_the_peer() {
        // The outgoing-call runtime's shape: bind ephemeral, connect(peer),
        // then plain send() — the datagram must arrive from our bound port.
        let net = OsNetStack;
        let mut rx = net.bind(localhost()).expect("bind rx");
        let rx_addr = rx.local_addr().expect("rx local addr");
        let mut tx = net.bind(localhost()).expect("bind tx");
        tx.connect(rx_addr).expect("connect");

        let mut poll = Poll::new().expect("poll");
        let waker = Arc::new(Waker::new(poll.registry(), Token(1)).expect("waker"));
        rx.register(poll.registry(), Token(0), waker)
            .expect("register");

        tx.send(b"connected").expect("send");

        let mut buf = [0u8; 64];
        let (n, src) = recv_wait(&mut poll, rx.as_ref(), &mut buf).expect("recv");
        assert_eq!(&buf[..n], b"connected");
        assert_eq!(src, tx.local_addr().expect("tx local addr"));
    }

    // -- WgNetStack adapter (iax-927a) ------------------------------------

    mod wg {
        use super::*;
        use astar_wireguard::x25519::{PublicKey, StaticSecret};
        use astar_wireguard::{UdpTransport, WgLinkConfig};
        use base64::Engine as _;
        use std::collections::VecDeque;
        use std::net::Ipv4Addr;
        use std::sync::Mutex;

        const A_TUNNEL: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 1);
        const B_TUNNEL: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 2);

        type Queue = Arc<Mutex<VecDeque<Vec<u8>>>>;

        /// One end of a crossed in-memory underlay: `send_to` lands in the
        /// peer's queue, `recv_from` pops our own — datagrams flow between two
        /// stacks with no test-side pumping (their I/O threads poll).
        struct PairedTransport {
            rx: Queue,
            tx: Queue,
            peer: SocketAddr,
        }

        impl UdpTransport for PairedTransport {
            fn send_to(&mut self, data: &[u8], _dst: SocketAddr) -> io::Result<usize> {
                self.tx.lock().unwrap().push_back(data.to_vec());
                Ok(data.len())
            }
            fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
                match self.rx.lock().unwrap().pop_front() {
                    Some(d) => {
                        let n = d.len().min(buf.len());
                        buf[..n].copy_from_slice(&d[..n]);
                        Ok((n, self.peer))
                    }
                    None => Err(io::Error::new(io::ErrorKind::WouldBlock, "no data")),
                }
            }
        }

        fn b64(k: [u8; 32]) -> String {
            base64::engine::general_purpose::STANDARD.encode(k)
        }

        /// Two adapted stacks wired back-to-back over crossed in-memory
        /// underlays (A = key seed 1, B = key seed 2).
        fn wg_pair() -> (WgNetStack, WgNetStack) {
            let a_to_b: Queue = Arc::default();
            let b_to_a: Queue = Arc::default();
            let mk = |ip: Ipv4Addr, seed: u8, peer_seed: u8, rx: &Queue, tx: &Queue| {
                let private = StaticSecret::from([seed; 32]);
                let peer_pub = PublicKey::from(&StaticSecret::from([peer_seed; 32]));
                let cfg = WgLinkConfig::new(
                    "TEST_KEY",
                    &format!("{ip}/32"),
                    &b64(peer_pub.to_bytes()),
                    "192.0.2.9:51820",
                    &["10.77.0.0/24".to_string()],
                    25,
                )
                .expect("valid config");
                let resolver = move |_: &str| b64(private.to_bytes());
                let underlay = PairedTransport {
                    rx: Arc::clone(rx),
                    tx: Arc::clone(tx),
                    peer: "192.0.2.9:51820".parse().unwrap(),
                };
                WgNetStack::new(Arc::new(
                    astar_wireguard::WgStack::new(&cfg, &resolver, Box::new(underlay))
                        .expect("stack builds"),
                ))
            };
            let a = mk(A_TUNNEL, 1, 2, &b_to_a, &a_to_b);
            let b = mk(B_TUNNEL, 2, 1, &a_to_b, &b_to_a);
            (a, b)
        }

        fn v4(ip: Ipv4Addr, port: u16) -> SocketAddr {
            SocketAddr::V4(SocketAddrV4::new(ip, port))
        }

        /// Wait-until polling helper (house style — no fixed sleeps).
        fn wait_recv(sock: &dyn LinkSocket, buf: &mut [u8]) -> (usize, SocketAddr) {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match sock.recv_from(buf) {
                    Ok(v) => return v,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => panic!("recv_from: {e}"),
                }
                assert!(Instant::now() < deadline, "timed out waiting for datagram");
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        #[test]
        fn wg_bind_and_local_addr_resolve_to_the_tunnel_ip() {
            let (a, _b) = wg_pair();
            let sock = a.bind(v4(Ipv4Addr::UNSPECIFIED, 0)).expect("bind");
            let SocketAddr::V4(addr) = sock.local_addr().expect("local addr") else {
                panic!("wg local addr must be V4");
            };
            assert_eq!(*addr.ip(), A_TUNNEL, "local IP is the tunnel IP");
            assert_ne!(addr.port(), 0, "ephemeral inner port resolved");
        }

        #[test]
        fn wg_rejects_ipv6_at_bind_connect_and_send_to() {
            let (a, _b) = wg_pair();
            let six: SocketAddr = "[::1]:4569".parse().unwrap();
            let Err(err) = a.bind(six) else {
                panic!("bind must reject IPv6");
            };
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

            let mut sock = a.bind(v4(Ipv4Addr::UNSPECIFIED, 0)).expect("bind");
            let err = sock.connect(six).expect_err("connect must reject IPv6");
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
            let err = sock
                .send_to(b"x", six)
                .expect_err("send_to must reject IPv6");
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        }

        #[test]
        fn wg_send_on_unconnected_socket_errors() {
            let (a, _b) = wg_pair();
            let sock = a.bind(v4(Ipv4Addr::UNSPECIFIED, 0)).expect("bind");
            let err = sock.send(b"x").expect_err("send needs connect first");
            assert_eq!(err.kind(), io::ErrorKind::NotConnected);
        }

        #[test]
        fn wg_connected_socket_round_trips_and_filters_foreign_sources() {
            let (a, b) = wg_pair();
            // B: the "listener" port a connected caller talks to, plus a
            // second (foreign) socket that must be filtered out.
            let b_main = b.bind(v4(Ipv4Addr::UNSPECIFIED, 4569)).expect("bind 4569");
            let b_foreign = b.bind(v4(Ipv4Addr::UNSPECIFIED, 5060)).expect("bind 5060");
            // A: the outgoing-call runtime's shape — bind ephemeral, connect.
            let mut conn = a.bind(v4(Ipv4Addr::UNSPECIFIED, 0)).expect("bind conn");
            conn.connect(v4(B_TUNNEL, 4569)).expect("connect");
            let SocketAddr::V4(conn_addr) = conn.local_addr().expect("local") else {
                panic!("V4");
            };

            // connect()ed send() reaches B from A's bound inner port.
            conn.send(b"hello").expect("send");
            let mut buf = [0u8; 256];
            let (n, src) = wait_recv(b_main.as_ref(), &mut buf);
            assert_eq!(&buf[..n], b"hello");
            assert_eq!(
                src,
                SocketAddr::V4(SocketAddrV4::new(A_TUNNEL, conn_addr.port()))
            );

            // A foreign source (B's 5060 socket) then the real peer: the
            // connected socket must drop the foreign datagram and deliver the
            // peer's, exactly like a kernel-connected UDP socket.
            b_foreign
                .send_to(b"intruder", SocketAddr::V4(conn_addr))
                .expect("foreign send");
            b_main
                .send_to(b"legit", SocketAddr::V4(conn_addr))
                .expect("peer send");
            let (n, src) = wait_recv(conn.as_ref(), &mut buf);
            assert_eq!(&buf[..n], b"legit", "foreign-source datagram not dropped");
            assert_eq!(src, v4(B_TUNNEL, 4569));
            // And nothing else is pending (the intruder is gone, not queued).
            let err = conn.recv_from(&mut buf).expect_err("queue must be empty");
            assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        }

        #[test]
        fn wg_register_stores_a_waker_that_fires_on_delivery() {
            let (a, b) = wg_pair();
            let mut rx = a.bind(v4(Ipv4Addr::UNSPECIFIED, 4569)).expect("bind rx");
            let tx = b.bind(v4(Ipv4Addr::UNSPECIFIED, 0)).expect("bind tx");

            let mut poll = Poll::new().expect("poll");
            let waker = Arc::new(Waker::new(poll.registry(), Token(1)).expect("waker"));
            rx.register(poll.registry(), Token(0), waker)
                .expect("register");

            tx.send_to(b"ding", v4(A_TUNNEL, 4569)).expect("send_to");

            // A single long poll: only the stored waker can cut it short. If
            // register() didn't wire the waker, this would block the full 10 s
            // and the events assertion below would fail.
            let mut events = Events::with_capacity(8);
            poll.poll(&mut events, Some(Duration::from_secs(10)))
                .expect("poll");
            assert!(
                !events.is_empty(),
                "delivery must wake the registered waker"
            );
            let mut buf = [0u8; 64];
            let (n, src) = rx
                .recv_from(&mut buf)
                .expect("data queued for the woken consumer");
            assert_eq!(&buf[..n], b"ding");
            assert_eq!(src.ip().to_string(), B_TUNNEL.to_string());
        }
    }
}
