// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
mod support;
use astar_http::{Limits, Response, Server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use support::{quick, send};

fn echo() -> Server {
    Server::bind("127.0.0.1:0", quick(), |req| {
        let path = req.path().to_string();
        match req.body() {
            Ok(b) => Response::text(200, format!("{path} {}", b.len())).into(),
            Err(e) => Response::text(e.status, "refused\n").into(),
        }
    })
    .unwrap()
}

/// Wait (generously) until the server holds `n` connections.
fn wait_open(s: &Server, n: usize) {
    let until = Instant::now() + Duration::from_secs(5);
    while s.open_connections() < n {
        assert!(Instant::now() < until, "only {} open", s.open_connections());
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn one_request_per_connection_then_close() {
    let s = echo();
    let r = send(
        s.local_addr(),
        b"GET /a HTTP/1.1\r\nHost: x\r\n\r\nGET /b HTTP/1.1\r\nHost: x\r\n\r\n",
    );
    assert!(r.starts_with("HTTP/1.1 200 "), "{r}");
    assert!(r.contains("\r\nConnection: close\r\n"), "{r}");
    assert!(r.ends_with("/a 0") && !r.contains("/b"), "{r}");
    s.stop();
}

#[test]
fn the_request_reports_what_was_sent() {
    let s = Server::bind("127.0.0.1:0", quick(), |req| {
        Response::text(
            200,
            format!(
                "{}|{}|{}|{}|{}|{:?}|{}|{}|{}|{}",
                req.method(),
                req.target(),
                req.path(),
                req.query(),
                req.version(),
                req.host(),
                req.forwarded(),
                req.tls(),
                req.peer().ip(),
                req.header("x-thing").unwrap_or("-"),
            ),
        )
        .into()
    })
    .unwrap();
    let r = send(
        s.local_addr(),
        b"GET /p/q?a=1&b=2 HTTP/1.1\r\nHost: Node.Example.NET.:8443\r\nX-Thing: yes\r\n\r\n",
    );
    assert!(
        r.ends_with(
            "GET|/p/q?a=1&b=2|/p/q|a=1&b=2|HTTP/1.1|Some(\"node.example.net\")|false|false|127.0.0.1|yes"
        ),
        "{r}"
    );
    for h in ["Forwarded: for=192.0.2.1", "X-Forwarded-For: 192.0.2.1"] {
        let r = send(
            s.local_addr(),
            format!("GET / HTTP/1.1\r\nHost: x\r\n{h}\r\n\r\n").as_bytes(),
        );
        assert!(r.contains("|true|false|127.0.0.1|"), "{h}: {r}");
    }
    s.stop();
}

#[test]
fn body_rules() {
    let s = echo();
    let a = s.local_addr();
    let post = |extra: &str, body: &str| {
        send(
            a,
            format!("POST /p HTTP/1.1\r\nHost: x\r\n{extra}\r\n{body}").as_bytes(),
        )
    };
    assert!(post("Content-Length: 3\r\n", "abc").ends_with("/p 3"));
    assert!(post("", "").ends_with("/p 0"));
    assert!(post("Content-Length: 0\r\n", "").ends_with("/p 0"));
    // Part of the body arriving with the head, the rest later, is still one body.
    assert!(post("Content-Length: 1024\r\n", &"y".repeat(1024)).ends_with("/p 1024"));
    let r = post("Transfer-Encoding: chunked\r\n", "3\r\nabc\r\n0\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 411 "), "{r}");
    // Both framings at once is a Transfer-Encoding request like any other.
    let r = post("Content-Length: 3\r\nTransfer-Encoding: chunked\r\n", "abc");
    assert!(r.starts_with("HTTP/1.1 411 "), "{r}");
    let r = post("Content-Length: 70000\r\n", "");
    assert!(r.starts_with("HTTP/1.1 413 "), "{r}");
    // A length past u64 is over any cap.
    let r = post("Content-Length: 99999999999999999999999999\r\n", "");
    assert!(r.starts_with("HTTP/1.1 413 "), "{r}");
    for bad in ["zz", "+1", "-1", "1, 1", "0x10", "1 1", "", "1.0"] {
        let r = post(&format!("Content-Length: {bad}\r\n"), "a");
        assert!(r.starts_with("HTTP/1.1 400 "), "{bad:?}: {r}");
    }
    // Cut short: the deadline passes before the rest of the body.
    let r = post("Content-Length: 9\r\n", "abc");
    assert!(r.starts_with("HTTP/1.1 400 "), "{r}");
    s.stop();
}

/// `Expect: 100-continue` (what `curl -d` sends for a body over 1 KiB): the
/// interim answer goes out once, before the body is read, so the client
/// sends it at once instead of waiting a second for a `100` that never
/// comes. Nothing interim for a body the server refuses, one the handler
/// never reads, or an HTTP/1.0 client.
#[test]
fn expect_100_continue_is_answered_before_the_body_is_read() {
    let s = echo();
    let a = s.local_addr();
    let mut c = TcpStream::connect(a).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    c.write_all(
        b"POST /p HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\nContent-Length: 5\r\n\r\n",
    )
    .unwrap();
    // The body is held back until the interim answer arrives: the request
    // deadline (300 ms here) would cut the request short without it.
    let want = b"HTTP/1.1 100 Continue\r\n\r\n";
    let mut got = vec![0u8; want.len()];
    c.read_exact(&mut got).unwrap();
    assert_eq!(String::from_utf8_lossy(&got), String::from_utf8_lossy(want));
    c.write_all(b"hello").unwrap();
    let mut rest = String::new();
    let _ = c.read_to_string(&mut rest);
    assert!(rest.starts_with("HTTP/1.1 200 "), "{rest}");
    assert!(rest.ends_with("/p 5"), "{rest}");
    assert!(!rest.contains(" 100 "), "sent once: {rest}");

    // Refused on the announcement: the final answer, nothing interim.
    let r = send(
        a,
        b"POST /p HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\nContent-Length: 70000\r\n\r\n",
    );
    assert!(r.starts_with("HTTP/1.1 413 "), "{r}");
    // HTTP/1.0 never gets a 1xx (RFC 9110 15.2).
    let r = send(
        a,
        b"POST /p HTTP/1.0\r\nHost: x\r\nExpect: 100-continue\r\nContent-Length: 2\r\n\r\nab",
    );
    assert!(r.starts_with("HTTP/1.1 200 ") && r.ends_with("/p 2"), "{r}");
    s.stop();

    // A handler that never reads the body sends no interim answer either.
    let s = Server::bind("127.0.0.1:0", quick(), |_| {
        Response::text(200, "no body\n").into()
    })
    .unwrap();
    let r = send(
        s.local_addr(),
        b"POST /p HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\nContent-Length: 2\r\n\r\n",
    );
    assert!(r.starts_with("HTTP/1.1 200 "), "{r}");
    s.stop();
}

#[test]
fn no_host_is_400_before_the_handler() {
    let s = Server::bind("127.0.0.1:0", quick(), |_| -> astar_http::Reply {
        panic!("the handler must not run")
    })
    .unwrap();
    for raw in [
        &b"GET / HTTP/1.0\r\n\r\n"[..],
        b"GET / HTTP/1.1\r\n\r\n",
        b"GET / HTTP/1.1\r\nHost:\r\n\r\n",
        b"GET / HTTP/1.1\r\nHost:   \r\n\r\n",
    ] {
        let r = send(s.local_addr(), raw);
        assert!(r.starts_with("HTTP/1.1 400 "), "{r}");
    }
    s.stop();
}

#[test]
fn a_bad_head_is_answered_with_its_status() {
    let s = echo();
    let r = send(s.local_addr(), b"GET / HTTP/2.0\r\nHost: x\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 400 "), "{r}");
    let big = format!(
        "GET / HTTP/1.1\r\nHost: x\r\nX-Big: {}\r\n\r\n",
        "a".repeat(9000)
    );
    let r = send(s.local_addr(), big.as_bytes());
    assert!(r.starts_with("HTTP/1.1 431 "), "{r}");
    s.stop();
}

#[test]
fn head_gets_the_length_and_no_body() {
    let s = Server::bind("127.0.0.1:0", quick(), |_| {
        Response::text(200, "hello").into()
    })
    .unwrap();
    let r = send(s.local_addr(), b"HEAD / HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(
        r.contains("Content-Length: 5\r\n") && r.ends_with("\r\n\r\n"),
        "{r}"
    );
    s.stop();
}

#[test]
fn a_handler_panic_is_a_500_and_the_server_lives() {
    let s = Server::bind("127.0.0.1:0", quick(), |req| {
        assert!(req.path() != "/boom", "handler bug");
        Response::text(200, "ok").into()
    })
    .unwrap();
    assert!(
        send(s.local_addr(), b"GET /boom HTTP/1.1\r\nHost: x\r\n\r\n").starts_with("HTTP/1.1 500 ")
    );
    assert!(
        send(s.local_addr(), b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").starts_with("HTTP/1.1 200 ")
    );
    s.stop();
}

#[test]
fn a_trickling_client_is_cut_off_at_the_deadline() {
    let s = echo(); // deadline 300 ms
    let mut client = TcpStream::connect(s.local_addr()).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut w = client.try_clone().unwrap();
    // One byte every 40 ms, for longer than the deadline, on its own thread.
    let trickle = std::thread::spawn(move || {
        for b in b"GET / HTTP/1.1\r\nHost: x\r\nX-Slow: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n" {
            if w.write_all(&[*b]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    });
    let started = Instant::now();
    let mut buf = [0u8; 64];
    let n = client.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "closed without an answer");
    // The trickle alone would take over 3.5 s.
    let took = started.elapsed();
    assert!(took < Duration::from_millis(2500), "{took:?}");
    let _ = trickle.join();
    s.stop();
}

/// Hold `held` idle connections, then a further one is closed unanswered.
fn cap_case(limits: Limits, held: usize) {
    let s = Server::bind("127.0.0.1:0", limits, |_| Response::text(200, "ok").into()).unwrap();
    let conns: Vec<TcpStream> = (0..held)
        .map(|_| TcpStream::connect(s.local_addr()).unwrap())
        .collect();
    wait_open(&s, held);
    let mut extra = TcpStream::connect(s.local_addr()).unwrap();
    extra
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let _ = extra.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
    let mut buf = [0u8; 16];
    assert_eq!(extra.read(&mut buf).unwrap_or(0), 0, "{limits:?}");
    assert_eq!(s.open_connections(), held);
    drop(conns);
    s.stop();
}

#[test]
fn the_global_and_per_client_caps_hold() {
    // A long deadline: the held connections must outlive the check.
    let base = Limits {
        deadline: Duration::from_secs(10),
        ..quick()
    };
    // Per client: two from one address, with room left overall.
    cap_case(
        Limits {
            max_connections: 3,
            conns_per_client: 2,
            ..base
        },
        2,
    );
    // Global: two in all, with room left for the address.
    cap_case(
        Limits {
            max_connections: 2,
            conns_per_client: 8,
            ..base
        },
        2,
    );
}

#[test]
fn a_freed_slot_is_given_to_the_next_client() {
    let limits = Limits {
        max_connections: 1,
        ..quick()
    };
    let s = Server::bind("127.0.0.1:0", limits, |_| Response::text(200, "ok").into()).unwrap();
    for _ in 0..3 {
        let r = send(s.local_addr(), b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(r.starts_with("HTTP/1.1 200 "), "{r}");
        // The lingering close holds the slot a moment after the answer.
        wait_closed(&s);
    }
    s.stop();
}

fn wait_closed(s: &Server) {
    let until = Instant::now() + Duration::from_secs(5);
    while s.open_connections() > 0 {
        assert!(
            Instant::now() < until,
            "still {} open",
            s.open_connections()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_refusal_reaches_a_client_still_sending_its_body() {
    // Review Focus #2: the 413 must not be lost to a reset.
    let s = Server::bind(
        "127.0.0.1:0",
        Limits {
            max_body_bytes: 1024,
            ..quick()
        },
        |req| match req.body() {
            Ok(_) => Response::text(200, "ok").into(),
            Err(e) => Response::text(e.status, "too big\n").into(),
        },
    )
    .unwrap();
    let mut c = TcpStream::connect(s.local_addr()).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    c.write_all(b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 200000\r\n\r\n")
        .unwrap();
    let _ = c.write_all(&vec![b'x'; 200_000]);
    let mut out = String::new();
    // EOF, not a reset: the server half-closed and is still reading.
    let read = c.read_to_string(&mut out);
    assert!(read.is_ok(), "{read:?}");
    assert!(out.starts_with("HTTP/1.1 413 "), "{out:?}");
    // The lingering close is still draining: the client may keep sending.
    c.write_all(b"more").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(s.open_connections(), 1, "the server is still draining");
    // The drain ends on the client's EOF, not at its 1 s bound.
    drop(c);
    let dropped = Instant::now();
    wait_closed(&s);
    let took = dropped.elapsed();
    assert!(took < Duration::from_millis(600), "{took:?}");
    s.stop();
}

#[test]
fn a_silent_client_is_let_go_when_the_drain_runs_out() {
    let s = echo();
    let mut c = TcpStream::connect(s.local_addr()).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let sent = Instant::now();
    c.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
    let mut out = String::new();
    let _ = c.read_to_string(&mut out);
    assert!(out.starts_with("HTTP/1.1 400 "), "{out:?}");
    // Open and silent: the server holds the slot for the drain's 1 s, then
    // lets it go.
    wait_closed(&s);
    let took = sent.elapsed();
    assert!(
        took >= Duration::from_millis(900) && took < Duration::from_secs(2),
        "{took:?}"
    );
    drop(c);
    s.stop();
}

#[test]
fn stop_returns_promptly() {
    let s = echo();
    let t = Instant::now();
    s.stop();
    assert!(t.elapsed() < Duration::from_secs(1));
}

#[test]
fn stop_closes_the_listener() {
    let s = echo();
    let a = s.local_addr();
    s.stop();
    // Nothing accepts any more: the connect is refused, or it is never answered.
    if let Ok(mut c) = TcpStream::connect(a) {
        c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let _ = c.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
        let mut buf = [0u8; 16];
        assert_eq!(c.read(&mut buf).unwrap_or(0), 0);
    }
}
