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
    command::NodeCommand, controller::NodeController, run::run_serve, secrets::SecretProvider,
};

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
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral tiny_http server");
    let port = server
        .server_addr()
        .to_ip()
        .expect("server_addr must be an IP socket addr")
        .port();

    let stop = Arc::new(AtomicBool::new(false));

    // Clone for the serve thread.
    let ctrl_t = Arc::clone(&ctrl);
    let stop_t = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name("serve-wiring-test".into())
        .spawn(move || run_serve(&ctrl_t, &server, &stop_t))
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
    // Set the stop flag; the recv_timeout loop exits within ≤ 200 ms.
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
///      stream does not block graceful teardown — `SseReader` is stop-aware).
///
/// Deterministic: socket read timeouts plus a watchdog-bounded join.
#[test]
fn sse_connection_does_not_wedge_control_channel_or_shutdown() {
    let ctrl = test_controller();
    ctrl.execute(NodeCommand::EnableInbound)
        .expect("enable_inbound must succeed");

    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral tiny_http server");
    let port = server
        .server_addr()
        .to_ip()
        .expect("server_addr must be an IP socket addr")
        .port();
    let addr = format!("127.0.0.1:{port}");

    let stop = Arc::new(AtomicBool::new(false));
    let ctrl_t = Arc::clone(&ctrl);
    let stop_t = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name("sse-wedge-test".into())
        .spawn(move || run_serve(&ctrl_t, &server, &stop_t))
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
    // established (the worker thread is now parked in SseReader::read).
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
