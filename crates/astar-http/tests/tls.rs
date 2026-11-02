// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
#![cfg(feature = "tls")]
mod support;
use astar_http::{Response, Server, ServerOptions, TlsConfig};
use rustls_pki_types::{CertificateDer, ServerName};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::tls::{client_config, self_signed};
use support::{quick, send};

/// One request over TLS: the answer as text (or the handshake error), and
/// the certificate the server presented.
fn get_tls(
    addr: SocketAddr,
    cfg: &Arc<rustls::ClientConfig>,
    path: &str,
) -> Result<(String, Option<CertificateDer<'static>>), String> {
    let name = ServerName::try_from("test.invalid").unwrap();
    let conn = rustls::ClientConnection::new(Arc::clone(cfg), name).unwrap();
    let tcp = TcpStream::connect(addr).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut tls = rustls::StreamOwned::new(conn, tcp);
    tls.write_all(format!("GET {path} HTTP/1.1\r\nHost: test.invalid\r\n\r\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let read = tls.read_to_end(&mut out);
    let presented = tls
        .conn
        .peer_certificates()
        .and_then(|c| c.first())
        .map(|c| c.clone().into_owned());
    if out.is_empty() {
        return Err(read
            .err()
            .map_or_else(|| "no answer".into(), |e| e.to_string()));
    }
    // A clean end: the server said close_notify before closing.
    read.map_err(|e| format!("answer not closed cleanly: {e}"))?;
    Ok((String::from_utf8_lossy(&out).into_owned(), presented))
}

fn tls_server(cfg: &Arc<TlsConfig>) -> Server {
    Server::bind_with(
        "127.0.0.1:0",
        quick(),
        ServerOptions {
            tls: Some(Arc::clone(cfg)),
            ..ServerOptions::default()
        },
        |req| Response::text(200, format!("tls={}\n", req.tls())).into(),
    )
    .unwrap()
}

#[test]
fn https_and_http_share_a_port_and_hsts_is_tls_only() {
    let (key, root) = self_signed("test.invalid");
    let cfg = TlsConfig::new();
    cfg.install(key);
    assert!(cfg.ready());
    let s = tls_server(&cfg);

    let (out, _) = get_tls(s.local_addr(), &client_config(&[&root]), "/").unwrap();
    assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{out}");
    assert!(
        out.contains("Strict-Transport-Security: max-age=31536000\r\n"),
        "{out}"
    );
    assert!(out.ends_with("tls=true\n"), "{out}");

    let plain = send(
        s.local_addr(),
        b"GET / HTTP/1.1\r\nHost: test.invalid\r\n\r\n",
    );
    assert!(plain.starts_with("HTTP/1.1 200 OK\r\n"), "{plain}");
    assert!(!plain.contains("Strict-Transport-Security"), "{plain}");
    assert!(plain.ends_with("tls=false\n"), "{plain}");
    s.stop();
}

#[test]
fn a_clienthello_before_a_certificate_is_closed() {
    let (_, root) = self_signed("test.invalid");
    let cfg = TlsConfig::new();
    assert!(!cfg.ready());
    let s = tls_server(&cfg);

    let started = Instant::now();
    let r = get_tls(s.local_addr(), &client_config(&[&root]), "/");
    assert!(r.is_err(), "a handshake with no certificate: {r:?}");

    // A real ClientHello on a bare socket: closed with nothing said, not
    // answered as a malformed plain-HTTP request.
    let mut hello = Vec::new();
    let name = ServerName::try_from("test.invalid").unwrap();
    rustls::ClientConnection::new(client_config(&[&root]), name)
        .unwrap()
        .write_tls(&mut hello)
        .unwrap();
    assert_eq!(hello[0], 0x16);
    let raw = send(s.local_addr(), &hello);
    assert_eq!(raw, "", "nothing is sent back to a ClientHello");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "closed at once, not left to the deadline: {:?}",
        started.elapsed()
    );

    let plain = send(
        s.local_addr(),
        b"GET / HTTP/1.1\r\nHost: test.invalid\r\n\r\n",
    );
    assert!(plain.starts_with("HTTP/1.1 200 OK\r\n"), "{plain}");
    assert!(plain.ends_with("tls=false\n"), "{plain}");
    s.stop();
}

#[test]
fn a_certificate_swapped_in_is_served_without_rebinding() {
    let (key_a, root_a) = self_signed("test.invalid");
    let (key_b, root_b) = self_signed("test.invalid");
    assert_ne!(root_a, root_b);
    let client = client_config(&[&root_a, &root_b]);
    let cfg = TlsConfig::new();
    cfg.install(key_a);
    let s = tls_server(&cfg);
    let addr = s.local_addr();

    let (out, seen) = get_tls(addr, &client, "/").unwrap();
    assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{out}");
    assert_eq!(seen.as_ref(), Some(&root_a), "the first certificate");

    cfg.install(key_b);
    let (out, seen) = get_tls(addr, &client, "/").unwrap();
    assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{out}");
    assert_eq!(seen.as_ref(), Some(&root_b), "the swapped-in certificate");
    assert_eq!(s.local_addr(), addr, "the same listener");
    s.stop();
}

#[test]
fn a_trickled_handshake_is_cut_off_at_the_deadline() {
    let (key, _root) = self_signed("test.invalid");
    let cfg = TlsConfig::new();
    cfg.install(key);
    // quick(): a 300 ms deadline for the handshake, head and body together.
    let s = tls_server(&cfg);
    let mut c = TcpStream::connect(s.local_addr()).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut w = c.try_clone().unwrap();
    // A handshake record header promising 512 bytes, then a byte every
    // 50 ms: rustls waits for the record, one short read at a time.
    let trickle = std::thread::spawn(move || {
        let _ = w.write_all(&[0x16, 0x03, 0x01, 0x02, 0x00]);
        for _ in 0..80 {
            if w.write_all(&[0]).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    let started = Instant::now();
    let mut out = Vec::new();
    let _ = c.read_to_end(&mut out);
    let took = started.elapsed();
    assert!(took < Duration::from_millis(1500), "held for {took:?}");
    assert!(
        took >= Duration::from_millis(250),
        "cut at the deadline, not before: {took:?}"
    );
    drop(c);
    let _ = trickle.join();
    s.stop();
}
