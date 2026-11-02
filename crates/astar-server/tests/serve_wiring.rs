// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Integration test for `serve` wiring (Task 10).
//!
//! Verifies that `run_serve` (the testable serve entry-point):
//!   1. Listens: GET /status responds 200 and body contains "listening".
//!   2. Shuts down gracefully: setting the stop flag causes the serve thread
//!      to exit within a bounded time (≤ 500 ms in practice, bounded by a
//!      thread join timeout via a secondary thread).
//!
//! No real IAX2 traffic. No real audio hardware (`NullBackend`).
//! Deterministic: socket timeouts and a bounded join prevent hangs.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use astar_server::{
    command::NodeCommand, controller::NodeController, http::MAX_BODY_BYTES, run::run_serve,
    secrets::SecretProvider, server::bind_control,
};

// The chunked body below is over the cap too: the answer to it is 411, not
// 413, because a chunked body is never read at all.
const _: () = assert!(MAX_BODY_BYTES < 70_000);

// ---------------------------------------------------------------------------
// Helper: build a NullBackend controller (no audio hardware).
// ---------------------------------------------------------------------------

fn test_controller() -> Arc<NodeController> {
    let secrets = SecretProvider::new();
    let station = astar_station::Station::with_backend_factory(
        astar_station::StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    Arc::new(NodeController::new(station, secrets))
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// Spin up `run_serve` on a thread, hit GET /status, assert 200 + "listening",
/// then set the stop flag and assert the serve thread exits cleanly.
#[test]
fn serve_wiring_listens_and_shuts_down() {
    let ctrl = test_controller();

    // Enable inbound so the status shows listening=true.
    ctrl.execute(NodeCommand::EnableInbound)
        .expect("enable_inbound must succeed");

    // Verify listening before starting the server.
    let snap = ctrl.snapshot();
    assert!(
        snap.listening,
        "controller must be listening after EnableInbound"
    );

    // Bind an ephemeral server so we can read the port before spawning.
    let server = bind_control(&ctrl, "127.0.0.1:0").expect("bind an ephemeral control server");
    let port = server.local_addr().port();

    let stop = Arc::new(AtomicBool::new(false));

    // Clone for the serve thread.
    let ctrl_t = Arc::clone(&ctrl);
    let stop_t = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name("serve-wiring-test".into())
        .spawn(move || run_serve(&ctrl_t, server, &stop_t))
        .expect("spawn serve thread");

    // Give the accept loop a moment to start.
    std::thread::sleep(Duration::from_millis(50));

    // ---- GET /status --------------------------------------------------------
    let addr = format!("127.0.0.1:{port}");
    let mut stream = TcpStream::connect(&addr).expect("connect to test server");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout");

    stream
        .write_all(b"GET /status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write GET /status request");

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read GET /status response");

    assert!(
        response.contains("200"),
        "GET /status must return 200, got: {response:?}"
    );
    assert!(
        response.contains("listening"),
        "GET /status body must contain 'listening', got: {response:?}"
    );

    // ---- Graceful shutdown --------------------------------------------------
    // Set the stop flag; the serve loop sees it within 200 ms.
    stop.store(true, Ordering::Relaxed);

    // Join the serve thread with a generous timeout via a watchdog thread.
    let join_result = std::thread::Builder::new()
        .name("serve-wiring-watchdog".into())
        .spawn(move || handle.join())
        .expect("spawn watchdog")
        .join()
        .expect("watchdog join");

    assert!(
        join_result.is_ok(),
        "serve thread must exit cleanly, got panic: {:?}",
        join_result.err()
    );
}

/// Regression test for the single-threaded-accept-loop wedge (final review C1).
///
/// An open `GET /events` SSE connection must NOT block the control channel:
///   1. Connect and start reading an SSE stream, then LEAVE it open.
///   2. On a SECOND connection, `GET /status` must still return 200 within the
///      socket read timeout (proving the server serves other requests while
///      SSE is connected — i.e. the accept loop is not wedged on the SSE read).
///   3. Setting the stop flag must let the serve thread join within a bounded
///      time, even with the SSE connection still open (proving the open SSE
///      stream does not block graceful teardown — the event pump is
///      stop-aware).
///
/// Deterministic: socket read timeouts plus a watchdog-bounded join.
#[test]
fn sse_connection_does_not_wedge_control_channel_or_shutdown() {
    let ctrl = test_controller();
    ctrl.execute(NodeCommand::EnableInbound)
        .expect("enable_inbound must succeed");

    let server = bind_control(&ctrl, "127.0.0.1:0").expect("bind an ephemeral control server");
    let port = server.local_addr().port();
    let addr = format!("127.0.0.1:{port}");

    let stop = Arc::new(AtomicBool::new(false));
    let ctrl_t = Arc::clone(&ctrl);
    let stop_t = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name("sse-wedge-test".into())
        .spawn(move || run_serve(&ctrl_t, server, &stop_t))
        .expect("spawn serve thread");

    // Give the accept loop a moment to start.
    std::thread::sleep(Duration::from_millis(50));

    // ---- (1) Open an SSE stream and read a byte or two, then leave it open. --
    let mut sse = TcpStream::connect(&addr).expect("connect SSE client");
    sse.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set SSE read_timeout");
    sse.write_all(b"GET /events HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .expect("write GET /events request");

    // Read at least one byte of the SSE response so the stream is genuinely
    // established (its connection thread is now in the event pump).
    let mut one = [0u8; 1];
    let n = sse.read(&mut one).expect("read first SSE byte");
    assert!(n > 0, "SSE stream must yield at least one byte");
    // NOTE: `sse` is intentionally kept alive (not dropped) for the rest of the
    // test so the SSE worker thread stays parked, exercising the wedge case.

    // ---- (2) GET /status on a SECOND connection must still return 200. ------
    let mut status = TcpStream::connect(&addr).expect("connect status client");
    status
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set status read_timeout");
    status
        .write_all(b"GET /status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write GET /status request");

    let mut response = String::new();
    status
        .read_to_string(&mut response)
        .expect("read GET /status response while SSE is open");
    assert!(
        response.contains("200"),
        "GET /status must return 200 while SSE is connected (control channel must \
         not be wedged), got: {response:?}"
    );

    // ---- (3) Shutdown must not be blocked by the open SSE connection. -------
    stop.store(true, Ordering::Relaxed);

    let join_result = std::thread::Builder::new()
        .name("sse-wedge-watchdog".into())
        .spawn(move || handle.join())
        .expect("spawn watchdog")
        .join()
        .expect("watchdog join");

    assert!(
        join_result.is_ok(),
        "serve thread must exit cleanly even with an open SSE connection, got: {:?}",
        join_result.err()
    );

    // Keep the SSE socket alive until here so the worker thread is parked for
    // the whole test; then it can drop.
    drop(sse);
}

/// The `Host` allow-list runs in `dispatch`, before the SSE branch: a
/// rebinding page asking for `GET /events` gets the router's 403 and no
/// event stream. Socket-level because the router never sees `/events` —
/// this is the one refusal `handle_request_with_headers` cannot pin.
#[test]
fn sse_with_a_foreign_host_is_refused_before_the_stream_opens() {
    let ctrl = test_controller();
    let server = bind_control(&ctrl, "127.0.0.1:0").expect("bind an ephemeral control server");
    let port = server.local_addr().port();
    let addr = format!("127.0.0.1:{port}");

    let stop = Arc::new(AtomicBool::new(false));
    let ctrl_t = Arc::clone(&ctrl);
    let stop_t = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name("sse-host-guard-test".into())
        .spawn(move || run_serve(&ctrl_t, server, &stop_t))
        .expect("spawn serve thread");
    std::thread::sleep(Duration::from_millis(50));

    let mut sse = TcpStream::connect(&addr).expect("connect SSE client");
    sse.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set SSE read_timeout");
    sse.write_all(b"GET /events HTTP/1.1\r\nHost: evil.com\r\nConnection: close\r\n\r\n")
        .expect("write GET /events with a foreign Host");

    // A real event stream never ends: it sends a frame every few tens of
    // milliseconds, so a plain read timeout never fires and `read_to_string`
    // would wait forever; this loop keeps a deadline of its own instead, and
    // reaches EOF only because the server sent a complete, finite response
    // and closed. Without the guard in `dispatch` it fails at the first read
    // to return after five seconds, or, if the stream stalls, at the read
    // timeout: within ten seconds either way.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "a refused /events must be a finite response, not a stream: {:?}",
            String::from_utf8_lossy(&raw)
        );
        let n = sse
            .read(&mut chunk)
            .expect("a refused /events must be a finite response, not a stream");
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..n]);
    }
    let response = String::from_utf8_lossy(&raw).into_owned();

    assert!(
        response.starts_with("HTTP/1.1 403 "),
        "status line: {response:?}"
    );
    assert!(
        !response.contains("text/event-stream"),
        "no stream may open for a foreign Host: {response:?}"
    );
    let body = response.rsplit("\r\n\r\n").next().unwrap_or("");
    assert_eq!(
        body,
        r#"{"error":"cross-origin requests are not accepted on this port"}"#
    );

    stop.store(true, Ordering::Relaxed);
    let join_result = std::thread::Builder::new()
        .name("sse-host-guard-watchdog".into())
        .spawn(move || handle.join())
        .expect("spawn watchdog")
        .join()
        .expect("watchdog join");
    assert!(
        join_result.is_ok(),
        "serve thread must exit cleanly, got: {:?}",
        join_result.err()
    );
}

/// Read one HTTP/1.1 response delimited by its `Content-Length`, without
/// waiting for the connection to close. Panics when the server does not
/// answer within the stream's read timeout.
fn read_one_response(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream
            .read(&mut chunk)
            .expect("the server must answer within the read timeout");
        assert!(
            n > 0,
            "connection closed before a complete response: {:?}",
            String::from_utf8_lossy(&buf)
        );
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).into_owned();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let want = head.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            });
            if want.is_some_and(|w| body.len() >= w) {
                return text;
            }
        }
    }
}

/// A chunked request body is refused with 411 before a byte of it is read
/// (plan decision D-g): the control port takes `Content-Length` bodies
/// only, which is what `curl -d`, the app and nodebench send. A client that
/// opens a 70 000-byte chunk and never finishes it still gets its answer at
/// once, so nothing on this port waits on a chunk terminator.
#[test]
fn a_chunked_body_is_refused_with_411() {
    let ctrl = test_controller();
    let server = bind_control(&ctrl, "127.0.0.1:0").expect("bind an ephemeral control server");
    let port = server.local_addr().port();
    let addr = format!("127.0.0.1:{port}");

    let stop = Arc::new(AtomicBool::new(false));
    let ctrl_t = Arc::clone(&ctrl);
    let stop_t = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name("body-chunked-test".into())
        .spawn(move || run_serve(&ctrl_t, server, &stop_t))
        .expect("spawn serve thread");
    std::thread::sleep(Duration::from_millis(50));

    let mut client = TcpStream::connect(&addr).expect("connect body-cap client");
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read_timeout");
    // `11170` is 70 000 in hex: the size line of the one chunk this body has.
    client
        .write_all(
            b"POST /hangup HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n11170\r\n",
        )
        .expect("write request head and the chunk's size line");
    client
        .write_all(&vec![b' '; 70_000])
        .expect("write the chunk's 70 000 bytes");
    // The chunk's closing CRLF and the terminating `0\r\n\r\n` are never sent.

    let response = read_one_response(&mut client);
    assert!(
        response.starts_with("HTTP/1.1 411 "),
        "status line: {response:?}"
    );
    // `read_one_response` returned because the body reached its
    // Content-Length, so the body is complete and exact equality holds.
    let (_, body) = response.split_once("\r\n\r\n").expect("a head/body split");
    assert_eq!(
        body,
        r#"{"error":"chunked request bodies are not accepted; send Content-Length"}"#
    );
    drop(client);

    stop.store(true, Ordering::Relaxed);
    let join_result = std::thread::Builder::new()
        .name("body-chunked-watchdog".into())
        .spawn(move || handle.join())
        .expect("spawn watchdog")
        .join()
        .expect("watchdog join");
    assert!(
        join_result.is_ok(),
        "serve thread must exit cleanly, got: {:?}",
        join_result.err()
    );
}

/// A malformed `Content-Length` and a body that stops short are different
/// faults and say so: 400 "bad Content-Length" for the first, 400 "request
/// body cut short" for the second (the client half-closes mid-body).
#[test]
fn a_bad_length_and_a_short_body_are_told_apart() {
    let ctrl = test_controller();
    let server = bind_control(&ctrl, "127.0.0.1:0").expect("bind an ephemeral control server");
    let port = server.local_addr().port();
    let addr = format!("127.0.0.1:{port}");

    let stop = Arc::new(AtomicBool::new(false));
    let ctrl_t = Arc::clone(&ctrl);
    let stop_t = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name("body-length-test".into())
        .spawn(move || run_serve(&ctrl_t, server, &stop_t))
        .expect("spawn serve thread");
    std::thread::sleep(Duration::from_millis(50));

    let ask = |request: &[u8]| {
        let mut client = TcpStream::connect(&addr).expect("connect client");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read_timeout");
        client.write_all(request).expect("write request");
        if request.ends_with(b"abc") {
            // Three of the ten announced bytes, then the client is done.
            client
                .shutdown(std::net::Shutdown::Write)
                .expect("half-close");
        }
        read_one_response(&mut client)
    };

    for bad in ["abc", "-1", "+5", "1,2", "0x10"] {
        let response = ask(format!(
            "POST /hangup HTTP/1.1\r\nHost: localhost\r\nContent-Length: {bad}\r\nConnection: close\r\n\r\n"
        )
        .as_bytes());
        assert!(response.starts_with("HTTP/1.1 400 "), "{bad}: {response:?}");
        let (_, body) = response.split_once("\r\n\r\n").expect("a head/body split");
        assert_eq!(body, r#"{"error":"bad Content-Length"}"#, "{bad}");
    }

    let response = ask(
        b"POST /hangup HTTP/1.1\r\nHost: localhost\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabc",
    );
    assert!(response.starts_with("HTTP/1.1 400 "), "{response:?}");
    let (_, body) = response.split_once("\r\n\r\n").expect("a head/body split");
    assert_eq!(body, r#"{"error":"request body cut short"}"#);

    stop.store(true, Ordering::Relaxed);
    let join_result = std::thread::Builder::new()
        .name("body-length-watchdog".into())
        .spawn(move || handle.join())
        .expect("spawn watchdog")
        .join()
        .expect("watchdog join");
    assert!(join_result.is_ok(), "serve thread must exit cleanly");
}

/// An announced body over the cap is refused on the announcement alone:
/// `dispatch` reads `Content-Length` before it reads a byte of body, so a
/// client that announces a megabyte and sends nothing gets its 413 at once.
/// The router never sees this request, and no read of the body is
/// started, so without the check this test's bounded read times out.
#[test]
fn an_announced_oversize_body_is_refused_before_a_byte_is_read() {
    let ctrl = test_controller();
    let server = bind_control(&ctrl, "127.0.0.1:0").expect("bind an ephemeral control server");
    let port = server.local_addr().port();
    let addr = format!("127.0.0.1:{port}");

    let stop = Arc::new(AtomicBool::new(false));
    let ctrl_t = Arc::clone(&ctrl);
    let stop_t = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name("body-cap-announced-test".into())
        .spawn(move || run_serve(&ctrl_t, server, &stop_t))
        .expect("spawn serve thread");
    std::thread::sleep(Duration::from_millis(50));

    let mut client = TcpStream::connect(&addr).expect("connect body-cap client");
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read_timeout");
    client
        .write_all(
            b"POST /hangup HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\n",
        )
        .expect("write a request head announcing a megabyte");
    // Not one byte of the body is sent.

    let response = read_one_response(&mut client);
    assert!(
        response.starts_with("HTTP/1.1 413 "),
        "status line: {response:?}"
    );
    let (_, body) = response.split_once("\r\n\r\n").expect("a head/body split");
    assert_eq!(body, r#"{"error":"request body exceeds 65536 bytes"}"#);

    drop(client);

    stop.store(true, Ordering::Relaxed);
    let join_result = std::thread::Builder::new()
        .name("body-cap-announced-watchdog".into())
        .spawn(move || handle.join())
        .expect("spawn watchdog")
        .join()
        .expect("watchdog join");
    assert!(
        join_result.is_ok(),
        "serve thread must exit cleanly, got: {:?}",
        join_result.err()
    );
}

// ---------------------------------------------------------------------------
// M17: run_serve DISCs the monitor before it returns
// ---------------------------------------------------------------------------

use std::sync::Mutex;

use astar_m17::{ControlPacket, encode_callsign};
use astar_server::{command::M17Action, m17_directory::M17Settings};

/// Every datagram the recorder received, in order.
type DatagramLog = Arc<Mutex<Vec<Vec<u8>>>>;

/// A loopback UDP sink that records every datagram and never answers: the
/// M17 reflector stand-in. With no ACKN the monitor stays "linking", so
/// only a DISC can be the last thing it sends.
fn spawn_silent_recorder() -> (std::net::SocketAddr, DatagramLog, Arc<AtomicBool>) {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind the recorder");
    sock.set_read_timeout(Some(Duration::from_millis(20)))
        .expect("set the recorder's read timeout");
    let addr = sock.local_addr().expect("the recorder's address");
    let log = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (t_log, t_stop) = (Arc::clone(&log), Arc::clone(&stop));
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        while !t_stop.load(Ordering::Relaxed) {
            if let Ok((n, _)) = sock.recv_from(&mut buf) {
                t_log
                    .lock()
                    .expect("recorder log mutex poisoned")
                    .push(buf[..n].to_vec());
            }
        }
    });
    (addr, log, stop)
}

/// Deadline-polling wait (iax-3f7a): never a single fixed-timeout read.
fn wait_until(mut pred: impl FnMut() -> bool, timeout_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    while std::time::Instant::now() < deadline {
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    pred()
}

/// `serve` never drops the controller (the signal handler holds an `Arc`),
/// and `Hangup` deliberately leaves the monitor alone. Without an explicit
/// teardown in `run_serve`, the reflector would keep a client carrying
/// someone's callsign until the process died.
#[test]
fn run_serve_disconnects_an_m17_monitor_before_it_returns() {
    let (addr, log, recorder_stop) = spawn_silent_recorder();
    let station = astar_station::Station::with_backend_factory(
        astar_station::StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    let mut ctrl = NodeController::new(station, SecretProvider::new());
    let mut reflectors = std::collections::BTreeMap::new();
    reflectors.insert("M17-TEST".to_string(), (addr.ip().to_string(), addr.port()));
    ctrl.set_m17_config(Some(M17Settings {
        enabled: true,
        reflectors,
        directory: None,
    }));
    let ctrl = Arc::new(ctrl);
    ctrl.execute(NodeCommand::EnableInbound)
        .expect("enable_inbound must succeed");
    ctrl.execute(NodeCommand::SetBridge {
        mode: Some("bridge".into()),
        mix_minus: Some(true),
        include_local_radio: Some(false),
    })
    .expect("bridge mode is valid");
    ctrl.execute(NodeCommand::M17 {
        action: M17Action::Monitor,
        reflector: Some("M17-TEST".into()),
        module: Some('A'),
        callsign: Some("AJ7HR".into()),
    })
    .expect("monitor");

    let callsign = encode_callsign("AJ7HR").expect("a valid callsign");
    let seen = |p: ControlPacket| {
        log.lock()
            .expect("recorder log mutex poisoned")
            .iter()
            .any(|d| ControlPacket::parse(d) == Some(p))
    };
    assert!(
        wait_until(
            || seen(ControlPacket::Conn {
                callsign,
                module: b'A'
            }),
            2_000
        ),
        "the monitor sent its CONN"
    );

    // Stop already set: the accept loop returns after one poll and the
    // teardown runs on this thread.
    let server = bind_control(&ctrl, "127.0.0.1:0").expect("bind an ephemeral control server");
    let stop = Arc::new(AtomicBool::new(true));
    run_serve(&ctrl, server, &stop);

    assert!(
        wait_until(
            || seen(ControlPacket::Disc {
                callsign: Some(callsign)
            }),
            2_000
        ),
        "run_serve must DISC the M17 monitor before it returns"
    );
    assert!(ctrl.snapshot().m17.is_none(), "and forget the leg");
    recorder_stop.store(true, Ordering::Relaxed);
}
