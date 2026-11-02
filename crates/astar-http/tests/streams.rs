// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
mod support;
use astar_http::{Limits, Reply, Server, StreamClass, StreamGuard, sse};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::quick;

fn open(addr: std::net::SocketAddr) -> BufReader<TcpStream> {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"GET /s HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    BufReader::new(s)
}

fn wait_for(what: impl Fn() -> bool) -> bool {
    let t = Instant::now();
    while !what() && t.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
    }
    what()
}

#[test]
fn the_sse_writer_frames_events_and_sends_keepalives() {
    // Assertions inside the stream would be swallowed by the server's
    // catch_unwind: the stream reports, the client side asserts.
    let due = Arc::new(Mutex::new(None::<(bool, bool)>));
    let d2 = Arc::clone(&due);
    let s = Server::bind("127.0.0.1:0", quick(), move |_| {
        let d3 = Arc::clone(&d2);
        Reply::Stream {
            class: StreamClass::Counted,
            run: Box::new(move |mut conn| {
                conn.write_head(
                    200,
                    &[
                        ("Content-Type", "text/event-stream"),
                        ("Cache-Control", "no-store"),
                    ],
                )
                .unwrap();
                let mut w = sse::Writer::with_keepalive(conn, Duration::from_millis(50));
                let early = w.keepalive_if_due().unwrap();
                w.event(Some("status"), "{\"a\":1}\nsecond").unwrap();
                w.data("plain").unwrap();
                std::thread::sleep(Duration::from_millis(120));
                let late = w.keepalive_if_due().unwrap();
                *d3.lock().unwrap() = Some((early, late));
            }),
        }
    })
    .unwrap();
    let mut r = open(s.local_addr());
    let mut all = String::new();
    let _ = r.read_to_string(&mut all);
    assert!(all.starts_with("HTTP/1.1 200 OK\r\n"), "{all}");
    assert!(
        all.contains("event: status\ndata: {\"a\":1}\ndata: second\n\ndata: plain\n\n"),
        "{all}"
    );
    assert!(all.contains(": keepalive\n\n"), "{all}");
    assert!(!all.contains("Content-Length"), "{all}");
    assert!(all.contains("Connection: close\r\n"), "{all}");
    assert!(all.contains("X-Content-Type-Options: nosniff\r\n"), "{all}");
    assert_eq!(
        *due.lock().unwrap(),
        Some((false, true)),
        "not due before the interval, due after it"
    );
    s.stop();
}

#[test]
fn sse_frames_cannot_be_split_by_the_data_or_the_name() {
    let mut w = sse::Writer::new(Vec::new());
    w.event(Some("a\r\nevent: forged"), "x\r\ny\rz").unwrap();
    let out = String::from_utf8(w.into_inner()).unwrap();
    assert_eq!(out, "event: aevent: forged\ndata: x\ndata: y\ndata: z\n\n");
}

#[test]
fn stream_caps_and_slots_freed_on_every_exit_panics_included() {
    let runs = Arc::new(AtomicUsize::new(0));
    let r2 = Arc::clone(&runs);
    let limits = Limits {
        streams_per_client: 1,
        max_counted_streams: 8,
        ..quick()
    };
    let s = Server::bind("127.0.0.1:0", limits, move |req| {
        let n = r2.fetch_add(1, Ordering::SeqCst);
        let hold = req.path() == "/hold";
        Reply::Stream {
            class: StreamClass::Counted,
            run: Box::new(move |mut c| {
                c.write_head(200, &[("Content-Type", "text/plain")])
                    .unwrap();
                if hold {
                    std::thread::sleep(Duration::from_millis(600));
                } else if n.is_multiple_of(2) {
                    panic!("stream bug")
                }
            }),
        }
    })
    .unwrap();
    let a = s.local_addr();
    let held =
        std::thread::spawn(move || support::send(a, b"GET /hold HTTP/1.1\r\nHost: x\r\n\r\n"));
    std::thread::sleep(Duration::from_millis(200));
    let r = support::send(a, b"GET /x HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(
        r.starts_with("HTTP/1.1 429 ") && r.contains("Retry-After: 10\r\n"),
        "{r}"
    );
    assert!(r.contains("Cache-Control: no-store\r\n"), "{r}");
    assert!(held.join().unwrap().starts_with("HTTP/1.1 200 "));
    // Panicking and normal streams both give their slot back.
    for _ in 0..4 {
        let r = support::send(a, b"GET /x HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(r.starts_with("HTTP/1.1 200 "), "{r}");
    }
    s.stop();
}

#[test]
fn the_counted_cap_is_global_and_a_shared_guard_spans_servers() {
    let guard = StreamGuard::new(8, 1);
    let opts = || astar_http::ServerOptions {
        streams: Some(Arc::clone(&guard)),
        ..astar_http::ServerOptions::default()
    };
    let handler = |req: &mut astar_http::Request<'_>| {
        let class = if req.path() == "/audio" {
            StreamClass::PerClientOnly
        } else {
            StreamClass::Counted
        };
        Reply::Stream {
            class,
            run: Box::new(|mut c| {
                let _ = c.write_head(200, &[("Content-Type", "text/plain")]);
                std::thread::sleep(Duration::from_millis(600));
            }),
        }
    };
    let s1 = Server::bind_with("127.0.0.1:0", quick(), opts(), handler).unwrap();
    let s2 = Server::bind_with("127.0.0.1:0", quick(), opts(), handler).unwrap();
    let a1 = s1.local_addr();
    let held =
        std::thread::spawn(move || support::send(a1, b"GET /events HTTP/1.1\r\nHost: x\r\n\r\n"));
    assert!(wait_for(|| guard.held("127.0.0.1".parse().unwrap()) == 1));
    let r = support::send(s2.local_addr(), b"GET /events HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(
        r.starts_with("HTTP/1.1 503 ") && r.contains("Retry-After: 10\r\n"),
        "{r}"
    );
    let r = support::send(s2.local_addr(), b"GET /audio HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 200 "), "audio is not counted: {r}");
    assert!(held.join().unwrap().starts_with("HTTP/1.1 200 "));
    assert!(wait_for(|| guard.held("127.0.0.1".parse().unwrap()) == 0));
    s1.stop();
    s2.stop();
}

#[test]
fn a_stream_ends_when_the_client_leaves() {
    let ended = Arc::new(AtomicUsize::new(0));
    let e2 = Arc::clone(&ended);
    let s = Server::bind("127.0.0.1:0", quick(), move |_| {
        let e3 = Arc::clone(&e2);
        Reply::Stream {
            class: StreamClass::Counted,
            run: Box::new(move |c| {
                let mut w = sse::Writer::new(c);
                while w.data("tick").is_ok() {
                    std::thread::sleep(Duration::from_millis(20));
                }
                e3.fetch_add(1, Ordering::SeqCst);
            }),
        }
    })
    .unwrap();
    let mut r = open(s.local_addr());
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    drop(r);
    assert!(wait_for(|| ended.load(Ordering::SeqCst) == 1));
    s.stop();
}

#[test]
fn a_stream_read_does_not_block_forever() {
    // The request deadline is over once the handler answers; a stream that
    // reads from a silent client still gets an error, after the write
    // stall, rather than hanging its thread.
    let failed = Arc::new(AtomicBool::new(false));
    let f2 = Arc::clone(&failed);
    let limits = Limits {
        write_stall: Duration::from_millis(400),
        ..quick()
    };
    let s = Server::bind("127.0.0.1:0", limits, move |_| {
        let f3 = Arc::clone(&f2);
        Reply::Stream {
            class: StreamClass::Counted,
            run: Box::new(move |mut c| {
                let _ = c.write_head(200, &[("Content-Type", "text/plain")]);
                let mut b = [0u8; 16];
                // Well past the 300 ms request deadline.
                std::thread::sleep(Duration::from_millis(500));
                if c.read(&mut b).is_err() {
                    f3.store(true, Ordering::SeqCst);
                }
            }),
        }
    })
    .unwrap();
    let r = open(s.local_addr());
    assert!(
        wait_for(|| failed.load(Ordering::SeqCst)),
        "the read never gave up"
    );
    drop(r);
    s.stop();
}

#[test]
fn a_stream_that_refuses_answers_in_full_to_a_client_still_sending() {
    // A stream that turns the client away after taking its slot (a full
    // audio hub) answers with Content-Length and closes gently, so the
    // refusal is not lost to a reset while the client is still sending.
    let s = Server::bind("127.0.0.1:0", quick(), |_| Reply::Stream {
        class: StreamClass::PerClientOnly,
        run: Box::new(|conn| {
            conn.respond(
                &astar_http::Response::text(503, "Full, try again shortly\n")
                    .header("Retry-After", "10"),
            );
        }),
    })
    .unwrap();
    let mut c = TcpStream::connect(s.local_addr()).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    c.write_all(b"GET /s HTTP/1.1\r\nHost: x\r\nContent-Length: 200000\r\n\r\n")
        .unwrap();
    let _ = c.write_all(&vec![b'x'; 200_000]);
    let mut out = String::new();
    // EOF, not a reset: the server half-closed and is still reading.
    let read = c.read_to_string(&mut out);
    assert!(read.is_ok(), "{read:?}");
    assert!(out.starts_with("HTTP/1.1 503 "), "{out:?}");
    assert!(out.contains("\r\nContent-Length: 24\r\n"), "{out:?}");
    assert!(out.contains("\r\nRetry-After: 10\r\n"), "{out:?}");
    assert!(out.contains("\r\nX-Frame-Options: DENY\r\n"), "{out:?}");
    assert!(
        out.ends_with("\r\n\r\nFull, try again shortly\n"),
        "{out:?}"
    );
    // The lingering close is still draining: the client may keep sending.
    c.write_all(b"more").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(s.open_connections(), 1, "the server is still draining");
    drop(c);
    assert!(wait_for(|| s.open_connections() == 0), "drained");
    s.stop();
}

/// A stream answered with `run`, and the whole response one client read.
fn stream_once(run: impl Fn(astar_http::Conn) + Send + Sync + 'static) -> String {
    let run = Arc::new(run);
    let s = Server::bind("127.0.0.1:0", quick(), move |_| {
        let run = Arc::clone(&run);
        Reply::Stream {
            class: StreamClass::PerClientOnly,
            run: Box::new(move |conn| run(conn)),
        }
    })
    .unwrap();
    let mut r = open(s.local_addr());
    let mut all = Vec::new();
    let _ = r.read_to_end(&mut all);
    s.stop();
    String::from_utf8_lossy(&all).into_owned()
}

#[test]
fn a_sized_stream_announces_its_length() {
    let all = stream_once(|mut conn| {
        conn.write_head_sized(
            200,
            &[("Content-Type", "audio/flac"), ("Content-Length", "1")],
            5,
        )
        .unwrap();
        conn.write_all(b"hello").unwrap();
    });
    assert!(all.starts_with("HTTP/1.1 200 OK\r\n"), "{all}");
    assert!(all.contains("Content-Length: 5\r\n"), "{all}");
    assert_eq!(all.matches("Content-Length").count(), 1, "{all}");
    assert!(!all.contains("Transfer-Encoding"), "{all}");
    assert!(all.ends_with("\r\n\r\nhello"), "{all}");
}

#[test]
fn a_chunked_stream_frames_every_write_and_ends_with_the_terminator() {
    let all = stream_once(|mut conn| {
        conn.write_head_chunked(200, &[("Content-Type", "application/x-tar")])
            .unwrap();
        let mut w = astar_http::Chunked::new(conn);
        w.write_all(b"hello ").unwrap();
        w.write_all(b"").unwrap();
        w.write_all(b"world").unwrap();
        w.finish().unwrap();
    });
    assert!(all.contains("Transfer-Encoding: chunked\r\n"), "{all}");
    assert!(!all.contains("Content-Length"), "{all}");
    assert!(
        all.ends_with("\r\n\r\n6\r\nhello \r\n5\r\nworld\r\n0\r\n\r\n"),
        "{all:?}"
    );
}

#[test]
fn a_chunked_stream_abandoned_midway_never_sends_the_terminator() {
    let all = stream_once(|mut conn| {
        conn.write_head_chunked(200, &[("Content-Type", "application/x-tar")])
            .unwrap();
        let mut w = astar_http::Chunked::new(conn);
        w.write_all(b"partial").unwrap();
        // An error upstream: dropped, never finished.
        drop(w);
    });
    assert!(all.ends_with("7\r\npartial\r\n"), "{all:?}");
    assert!(!all.contains("0\r\n\r\n"), "{all:?}");
}
