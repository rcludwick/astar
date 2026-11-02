// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
mod support;
use astar_http::{Reply, Response, Server, ServerOptions, StreamClass};
use std::io::Write;
use support::{quick, send};

#[test]
fn security_headers_on_every_answer_and_no_hsts_in_plain_http() {
    let s = Server::bind_with(
        "127.0.0.1:0",
        quick(),
        ServerOptions {
            csp: Some("default-src 'self'"),
            ..ServerOptions::default()
        },
        |req| {
            if req.path() == "/own" {
                Response::text(200, "x")
                    .header("Content-Security-Policy", "default-src 'none'")
                    .into()
            } else {
                Response::text(404, "no").into()
            }
        },
    )
    .unwrap();
    for (path, csp) in [("/x", "default-src 'self'"), ("/own", "default-src 'none'")] {
        let r = send(
            s.local_addr(),
            format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes(),
        );
        for h in [
            "X-Content-Type-Options: nosniff",
            "X-Frame-Options: DENY",
            "Referrer-Policy: no-referrer",
            &format!("Content-Security-Policy: {csp}"),
        ] {
            assert!(r.contains(&format!("{h}\r\n")), "{path}: {h}: {r}");
        }
        assert_eq!(r.matches("Content-Security-Policy").count(), 1, "{r}");
        assert!(!r.contains("Strict-Transport-Security"), "{r}");
    }
    // A crate-level refusal carries them too.
    let r = send(s.local_addr(), b"GET / HTTP/1.1\r\n\r\n");
    assert!(
        r.starts_with("HTTP/1.1 400 ") && r.contains("X-Frame-Options: DENY"),
        "{r}"
    );
    s.stop();
}

#[test]
fn a_response_cannot_set_its_own_framing_headers() {
    let s = Server::bind("127.0.0.1:0", quick(), |_| {
        Response::text(200, "hello")
            .header("Content-Length", "99")
            .header("Connection", "keep-alive")
            .header("Transfer-Encoding", "chunked")
            .into()
    })
    .unwrap();
    let r = send(s.local_addr(), b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(r.contains("\r\nContent-Length: 5\r\n"), "{r}");
    assert!(r.contains("\r\nConnection: close\r\n"), "{r}");
    assert_eq!(r.matches("Content-Length").count(), 1, "{r}");
    assert!(
        !r.contains("keep-alive") && !r.contains("Transfer-Encoding"),
        "{r}"
    );
    assert!(r.ends_with("\r\n\r\nhello"), "{r}");
    s.stop();
}

#[test]
fn a_header_that_would_split_the_response_is_dropped() {
    let s = Server::bind("127.0.0.1:0", quick(), |req| {
        if req.path() == "/stream" {
            Reply::Stream {
                class: StreamClass::PerClientOnly,
                run: Box::new(|mut conn| {
                    let _ = conn.write_head(
                        200,
                        &[
                            ("X", "a\r\nSet-Cookie: s=1"),
                            ("Bad Name", "v"),
                            ("X-Nul", "a\0b"),
                            ("X-Ok", "fine"),
                        ],
                    );
                    let _ = conn.write_all(b"data");
                }),
            }
        } else {
            Response::text(200, "hello")
                .header("X", "a\r\nSet-Cookie: s=1")
                .header("X-Lf", "a\nContent-Length: 0")
                .header("Bad Name", "v")
                .header("X-Nul", "a\0b")
                .header("X-Ok", "fine")
                .into()
        }
    })
    .unwrap();
    for (path, body) in [("/", "hello"), ("/stream", "data")] {
        let r = send(
            s.local_addr(),
            format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes(),
        );
        assert!(r.starts_with("HTTP/1.1 200 "), "{path}: {r}");
        assert!(
            !r.contains("Set-Cookie") && !r.contains("Bad Name"),
            "{path}: {r}"
        );
        assert!(!r.contains('\0') && !r.contains("X-Nul"), "{path}: {r}");
        assert!(r.contains("\r\nX-Ok: fine\r\n"), "{path}: {r}");
        // Still well framed: one head, then exactly the body.
        let (head, rest) = r.split_once("\r\n\r\n").unwrap();
        assert_eq!(rest, body, "{path}: {r}");
        assert!(
            head.split("\r\n").skip(1).all(|l| l.contains(": ")),
            "{path}: {r}"
        );
        assert!(head.matches("Content-Length").count() <= 1, "{path}: {r}");
    }
    s.stop();
}

#[test]
fn a_dropped_header_does_not_displace_a_default() {
    let s = Server::bind_with(
        "127.0.0.1:0",
        quick(),
        ServerOptions {
            csp: Some("default-src 'self'"),
            ..ServerOptions::default()
        },
        |_| {
            Response::text(200, "x")
                .header("Content-Security-Policy", "a\r\nX-Evil: 1")
                .header("X-Frame-Options", "SAMEORIGIN\n")
                .into()
        },
    )
    .unwrap();
    let r = send(s.local_addr(), b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(
        r.contains("\r\nContent-Security-Policy: default-src 'self'\r\n"),
        "{r}"
    );
    assert!(r.contains("\r\nX-Frame-Options: DENY\r\n"), "{r}");
    assert!(!r.contains("X-Evil") && !r.contains("SAMEORIGIN"), "{r}");
    s.stop();
}
