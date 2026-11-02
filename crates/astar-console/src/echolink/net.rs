// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The only place `EchoLink` code opens a socket.
//!
//! Every bind, connect, send and server lookup goes through one of these
//! functions, and each of them calls [`check`] on the address first. In a
//! build with the `echolink-loopback-only` feature (every test build of this
//! crate, through its own dev-dependency) `check` refuses any address that is
//! not loopback with [`io::ErrorKind::PermissionDenied`], and
//! [`resolve_server`] refuses a host name before any DNS lookup. So a test can
//! only ever reach a fake on 127.0.0.1, however it is written.
#![allow(dead_code, reason = "consumed by the presence and link code")]

use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use astar_echolink::DIRECTORY_PORT;
use astar_echolink::directory::MAX_RAW_LIST_BYTES;

#[cfg(all(test, not(feature = "echolink-loopback-only")))]
compile_error!("echolink unit tests must build with echolink-loopback-only");

const REFUSED: &str = "echolink: non-loopback address refused in a test build";
const NAME_REFUSED: &str = "echolink: host names are refused in a test build";

/// Refuse a non-loopback address in a loopback-only build; allow anything otherwise.
fn check(addr: SocketAddr) -> io::Result<()> {
    if super::LOOPBACK_ONLY && !addr.ip().is_loopback() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, REFUSED));
    }
    Ok(())
}

/// Bind a UDP socket.
pub(crate) fn bind_udp(addr: SocketAddr) -> io::Result<UdpSocket> {
    check(addr)?;
    UdpSocket::bind(addr)
}

/// Send one datagram. The socket's own address is checked as well as the
/// destination, so a socket that did not come from [`bind_udp`] is held to
/// the same rule.
pub(crate) fn send_to(sock: &UdpSocket, buf: &[u8], to: SocketAddr) -> io::Result<usize> {
    check(to)?;
    check(sock.local_addr()?)?;
    sock.send_to(buf, to)
}

/// Open a TCP connection, giving up after `timeout`.
pub(crate) fn connect_tcp(addr: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    check(addr)?;
    TcpStream::connect_timeout(&addr, timeout)
}

/// Turn a configured directory server into an address.
///
/// Takes `IP`, `IP:port`, `name` or `name:port`; the port defaults to 5200.
/// A loopback-only build takes only the two IP forms and refuses a name
/// without looking it up.
pub(crate) fn resolve_server(server: &str) -> io::Result<SocketAddr> {
    let server = server.trim();
    if server.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "echolink: the directory server is blank",
        ));
    }
    let literal = server.parse::<SocketAddr>().ok().or_else(|| {
        server
            .parse::<IpAddr>()
            .ok()
            .map(|ip| SocketAddr::new(ip, DIRECTORY_PORT))
    });
    if let Some(addr) = literal {
        check(addr)?;
        return Ok(addr);
    }
    if super::LOOPBACK_ONLY {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            NAME_REFUSED,
        ));
    }
    let (host, port) = match server.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse::<u16>().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "echolink: the directory server's port is not a number",
                )
            })?,
        ),
        None => (server, DIRECTORY_PORT),
    };
    let mut addrs: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
    // Directory servers are IPv4; prefer an IPv4 answer when there is one.
    addrs.sort_by_key(SocketAddr::is_ipv6);
    let addr = addrs.into_iter().next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "echolink: the directory server's name has no address",
        )
    })?;
    check(addr)?;
    Ok(addr)
}

/// One directory exchange: connect, write `request`, half-close, and read the
/// reply until the server closes.
///
/// `read_timeout` bounds the whole write-and-read, not each read, so a server
/// that trickles bytes cannot hold the exchange open. The reply is capped at
/// [`MAX_RAW_LIST_BYTES`]. `request` may hold the password; it is only ever
/// written to the socket, never copied or put in an error.
pub(crate) fn directory_exchange(
    server: SocketAddr,
    request: &[u8],
    connect_timeout: Duration,
    read_timeout: Duration,
) -> io::Result<Vec<u8>> {
    let mut stream = connect_tcp(server, connect_timeout)?;
    let deadline = Instant::now() + read_timeout;
    stream.set_write_timeout(Some(remaining(deadline)?))?;
    stream.write_all(request).map_err(timed_out)?;
    stream.shutdown(Shutdown::Write)?;
    let mut reply = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        // macOS refuses `setsockopt` (EINVAL) once the server has closed its
        // side; the timeout set on the previous turn, which is longer, then
        // still bounds the read, and the bytes already received are readable.
        match stream.set_read_timeout(Some(remaining(deadline)?)) {
            Err(e) if e.kind() != io::ErrorKind::InvalidInput => return Err(e),
            _ => {}
        }
        let n = match stream.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(timed_out(e)),
        };
        if n == 0 {
            return Ok(reply);
        }
        if reply.len() + n > MAX_RAW_LIST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "echolink: the directory's reply is too large",
            ));
        }
        reply.extend_from_slice(&buf[..n]);
    }
}

/// Time left before `deadline`, or `TimedOut` once it has passed.
fn remaining(deadline: Instant) -> io::Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "echolink: the directory did not answer in time",
        ));
    }
    Ok(left)
}

/// A socket timeout reads as `WouldBlock` on some platforms and `TimedOut` on
/// others; report both as `TimedOut`.
fn timed_out(e: io::Error) -> io::Error {
    if e.kind() == io::ErrorKind::WouldBlock {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "echolink: the directory did not answer in time",
        )
    } else {
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(
        clippy::assertions_on_constants,
        reason = "a test that fails loudly if a test build ever drops the feature"
    )]
    fn test_builds_are_loopback_only() {
        assert!(super::super::LOOPBACK_ONLY);
    }

    #[test]
    fn guard_refuses_a_public_tcp_address() {
        let e = connect_tcp("192.0.2.1:5200".parse().unwrap(), Duration::from_secs(1)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn guard_refuses_a_public_udp_destination() {
        let sock = bind_udp("127.0.0.1:0".parse().unwrap()).unwrap();
        let e = send_to(&sock, b"x", "198.51.100.7:5199".parse().unwrap()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            e.to_string(),
            "echolink: non-loopback address refused in a test build"
        );
    }

    #[test]
    fn guard_refuses_mapped_and_unspecified_destinations() {
        let sock = bind_udp("127.0.0.1:0".parse().unwrap()).unwrap();
        for to in ["[::ffff:198.51.100.7]:5199", "0.0.0.0:5199", "[::]:5199"] {
            let to: SocketAddr = to.parse().unwrap();
            let e = send_to(&sock, b"x", to).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "send_to {to}");
            let e = connect_tcp(to, Duration::from_secs(1)).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "connect {to}");
        }
    }

    #[test]
    fn guard_refuses_a_wildcard_bind() {
        let e = bind_udp("0.0.0.0:5198".parse().unwrap()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        let e = bind_udp("[::]:0".parse().unwrap()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn guard_refuses_a_host_name_without_dns() {
        let e = resolve_server("example.invalid").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        // `localhost` is a name too: it would need a lookup, so it is refused.
        let e = resolve_server("localhost:5200").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn guard_allows_loopback() {
        let a = bind_udp("127.0.0.1:0".parse().unwrap()).unwrap();
        let b = bind_udp("127.0.0.1:0".parse().unwrap()).unwrap();
        a.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        assert_eq!(send_to(&b, b"hi", a.local_addr().unwrap()).unwrap(), 2);
        let mut buf = [0u8; 8];
        let (n, from) = a.recv_from(&mut buf).unwrap();
        assert_eq!((&buf[..n], from), (&b"hi"[..], b.local_addr().unwrap()));
    }

    #[test]
    fn resolve_server_takes_ip_and_ip_port() {
        assert_eq!(
            resolve_server("127.0.0.1").unwrap(),
            "127.0.0.1:5200".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            resolve_server(" 127.0.0.1:40123 ").unwrap(),
            "127.0.0.1:40123".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            resolve_server("::1").unwrap(),
            "[::1]:5200".parse::<SocketAddr>().unwrap()
        );
        let e = resolve_server("198.51.100.7:5200").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        let e = resolve_server("198.51.100.7").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            resolve_server("").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn directory_exchange_refuses_a_public_server() {
        let e = directory_exchange(
            "192.0.2.1:5200".parse().unwrap(),
            b"s",
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn directory_exchange_reaches_a_loopback_fake() {
        use astar_echolink::directory::LIST_REQUEST;
        use astar_echolink::fake::{FakeDirectory, FakeLogin};
        let dir = FakeDirectory::start(FakeLogin::Ok, Vec::new());
        let server = resolve_server(&dir.addr().to_string()).unwrap();
        let reply = directory_exchange(
            server,
            LIST_REQUEST,
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(reply, b"@@@\n0\n+++\n");
        assert_eq!(dir.lines(), vec![LIST_REQUEST.to_vec()]);
    }

    #[test]
    fn directory_exchange_times_out_on_a_silent_directory() {
        use astar_echolink::fake::{FakeDirectory, FakeLogin};
        let dir = FakeDirectory::start(FakeLogin::Silent, Vec::new());
        let started = Instant::now();
        let e = directory_exchange(
            dir.addr(),
            b"lKX7ABC",
            Duration::from_secs(2),
            Duration::from_millis(150),
        )
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
