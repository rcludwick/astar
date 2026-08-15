// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `tiny_http` accept loop for the astar-server control channel (iax-35b1).
//!
//! `serve_http` binds to `bind`, then loops: `GET /events` → streaming
//! `SseReader`; all other requests → `handle_request` → plain response.
//! The loop exits when `stop` is set or `ctrl.should_stop()` returns `true`.

use std::io;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use crate::{controller::NodeController, http::handle_request, sse::SseReader};

// ---- public API -------------------------------------------------------------

/// Run the `tiny_http` accept loop.
///
/// Binds to `bind` (e.g. `"127.0.0.1:8080"` or `"127.0.0.1:0"` for an
/// ephemeral port).  Each request is handled inline (SSE responses are
/// long-lived; the caller should run this on a dedicated thread if the server
/// must stay responsive).
///
/// Returns `Ok(())` when `stop` is set to `true` or `ctrl.should_stop()`
/// returns `true`.
///
/// # Errors
/// Returns an error if the address cannot be bound.
pub fn serve_http(
    ctrl: &Arc<NodeController>,
    bind: &str,
    stop: &Arc<AtomicBool>,
) -> io::Result<()> {
    let server = tiny_http::Server::http(bind).map_err(|e| io::Error::other(e.to_string()))?;
    accept_loop(&server, ctrl, stop);
    Ok(())
}

/// Same as `serve_http` but accepts a pre-bound `tiny_http::Server`.
///
/// Useful in tests where the caller wants to inspect `server.server_addr()`
/// before handing the server to the serve loop.
pub fn serve_http_on(
    server: &tiny_http::Server,
    ctrl: &Arc<NodeController>,
    stop: &Arc<AtomicBool>,
) -> io::Result<()> {
    accept_loop(server, ctrl, stop);
    Ok(())
}

// ---- internal loop ----------------------------------------------------------

fn accept_loop(server: &tiny_http::Server, ctrl: &Arc<NodeController>, stop: &Arc<AtomicBool>) {
    loop {
        match server.recv_timeout(Duration::from_millis(200)) {
            Ok(Some(request)) => {
                // Spawn a worker thread per request so the accept loop stays
                // free to observe the stop flag.  Critically, `GET /events`
                // responds with a streaming `SseReader` that blocks until the
                // client disconnects (or stop is set); handling it inline would
                // wedge the whole control channel and prevent shutdown.
                let ctrl = Arc::clone(ctrl);
                std::thread::spawn(move || dispatch(request, &ctrl));
            }
            Ok(None) => {
                // Timeout — check stop flags.
                if stop.load(Ordering::Relaxed) || ctrl.should_stop() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

// ---- dispatch ---------------------------------------------------------------

fn dispatch(mut request: tiny_http::Request, ctrl: &Arc<NodeController>) {
    let method = request.method().as_str().to_string();
    let url = request.url().to_string();
    let path_no_query = url.split('?').next().unwrap_or(&url).to_string();

    // SSE: long-lived streaming response — drive it directly from SseReader.
    if method == "GET" && path_no_query == "/events" {
        let rx = ctrl.subscribe();
        let reader = SseReader::new(rx, Arc::clone(ctrl));
        let response = tiny_http::Response::new(
            tiny_http::StatusCode(200),
            vec![
                header("Content-Type", "text/event-stream"),
                header("Cache-Control", "no-cache"),
            ],
            reader,
            None,
            None,
        );
        let _ = request.respond(response);
        return;
    }

    // All other routes: read body → handle_request → respond.
    let mut body = Vec::new();
    let _ = request.as_reader().read_to_end(&mut body);
    let resp = handle_request(ctrl, &method, &url, &body);
    let response = tiny_http::Response::from_data(resp.body)
        .with_status_code(resp.status)
        .with_header(header("Content-Type", resp.content_type));
    let _ = request.respond(response);
}

fn header(key: &str, value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(key.as_bytes(), value.as_bytes()).expect("static header is valid")
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{controller::NodeController, secrets::SecretProvider};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::time::Duration;

    fn test_controller() -> Arc<NodeController> {
        let secrets = SecretProvider::new();
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        Arc::new(NodeController::new(station, secrets))
    }

    #[test]
    fn http_server_serves_status_over_a_socket() {
        let ctrl = test_controller();
        let stop = Arc::new(AtomicBool::new(false));

        // Bind an ephemeral server, read its port, then hand it to serve_http_on
        // (pre-bound variant so we can observe the port before spawning the loop).
        let server =
            Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("bind ephemeral server"));
        let port = server
            .server_addr()
            .to_ip()
            .expect("server_addr must be an IP socket addr")
            .port();

        // Clone Arcs for the server thread.
        let server_thread = Arc::clone(&server);
        let ctrl_thread = Arc::clone(&ctrl);
        let stop_thread = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            serve_http_on(&server_thread, &ctrl_thread, &stop_thread)
                .expect("serve_http_on must not error");
        });

        // Connect with a short timeout so the test cannot hang.
        let addr = format!("127.0.0.1:{port}");
        let mut stream = TcpStream::connect(&addr).expect("connect to test server");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        // Send a minimal HTTP/1.1 GET /status request.
        stream
            .write_all(b"GET /status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .expect("write request");

        // Read the full response.
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read response");

        // Assert the response includes 200 and the word "listening".
        assert!(
            response.contains("200"),
            "response must contain 200: {response:?}"
        );
        assert!(
            response.contains("listening"),
            "response body must contain 'listening': {response:?}"
        );

        // Signal the server to stop and join the thread.
        stop.store(true, Ordering::Relaxed);
        handle.join().expect("server thread must exit cleanly");
    }
}
