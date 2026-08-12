// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Server-Sent-Events stream for the astar-server control channel (iax-35b1).
//!
//! `sse_frame` formats one `NodeEvent` as an SSE data line; `SseReader` is a
//! blocking `Read` that drains the controller's event subscription and emits a
//! periodic `Snapshot` heartbeat when no event is pending.

use std::io::{self, Read};
use std::sync::{Arc, mpsc::Receiver};
use std::time::Duration;

use crate::{command::NodeEvent, controller::NodeController};

/// Format a `NodeEvent` as one SSE frame: `data: <json>\n\n`.
///
/// Credentials never appear in `NodeEvent` (the type is secret-free by
/// design), so the returned string is always credential-free.
#[must_use]
pub fn sse_frame(ev: &NodeEvent) -> String {
    let json = serde_json::to_string(ev).unwrap_or_else(|_| "{}".to_string());
    format!("data: {json}\n\n")
}

/// A blocking `Read` that yields SSE frames from the controller's event
/// subscription, with a heartbeat `Snapshot` when no event is pending.
///
/// `tiny_http` (Task 8) drives this by repeatedly calling `read`.  During
/// normal operation it never returns 0 — the stream ends only when the client
/// disconnects and the write side errors.  When the controller's stop flag is
/// set (shutdown), `read` returns `Ok(0)` (EOF) so `respond` finishes and the
/// SSE worker thread exits; otherwise an open SSE stream would keep its worker
/// thread alive forever and block graceful teardown.
pub struct SseReader {
    rx: Receiver<NodeEvent>,
    ctrl: Arc<NodeController>,
    residual: Vec<u8>,
    pos: usize,
    tick: Duration,
}

impl SseReader {
    /// Build an `SseReader` from the controller's subscriber channel and an
    /// `Arc` to the controller for heartbeat snapshots.
    ///
    /// `rx` should come from `NodeController::subscribe()`.
    #[must_use]
    pub fn new(rx: Receiver<NodeEvent>, ctrl: Arc<NodeController>) -> Self {
        Self {
            rx,
            ctrl,
            residual: Vec::new(),
            pos: 0,
            tick: Duration::from_millis(33),
        }
    }

    /// Build with an explicit tick (useful for tests to avoid sleeping).
    #[cfg(test)]
    pub fn with_tick(rx: Receiver<NodeEvent>, ctrl: Arc<NodeController>, tick: Duration) -> Self {
        Self {
            rx,
            ctrl,
            residual: Vec::new(),
            pos: 0,
            tick,
        }
    }

    /// Produce the next SSE frame.
    ///
    /// If an event is pending in `rx`, format it immediately.  Otherwise emit
    /// a heartbeat `Snapshot` so the stream never stalls.
    fn next_frame(&self) -> String {
        match self.rx.try_recv() {
            Ok(ev) => sse_frame(&ev),
            Err(_) => sse_frame(&NodeEvent::Snapshot(self.ctrl.snapshot())),
        }
    }
}

impl Read for SseReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.residual.len() {
            // On shutdown, return EOF so `respond` finishes and this SSE worker
            // thread exits.  Returning 0 only when stop is set is correct: it
            // closes the stream at teardown, and during normal operation the
            // reader still never returns 0.
            if self.ctrl.should_stop() {
                return Ok(0);
            }
            // Pace the stream, then produce the next frame.
            std::thread::sleep(self.tick);
            self.residual = self.next_frame().into_bytes();
            self.pos = 0;
        }
        let n = (self.residual.len() - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.residual[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{controller::NodeController, secrets::SecretProvider};

    fn test_controller() -> Arc<NodeController> {
        let secrets = SecretProvider::new();
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        Arc::new(NodeController::new(station, secrets))
    }

    // ---- required test from the brief -----------------------------------------

    #[test]
    fn sse_frame_format_and_secret_free() {
        let f = sse_frame(&NodeEvent::IncomingCall {
            from: "55553".into(),
        });
        assert!(f.starts_with("data: ") && f.ends_with("\n\n"));
        for bad in ["secret", "password"] {
            assert!(!f.contains(bad), "frame must not contain '{bad}': {f:?}");
        }
    }

    // ---- additional coverage --------------------------------------------------

    #[test]
    fn sse_frame_is_valid_json_payload() {
        let f = sse_frame(&NodeEvent::Registered);
        assert!(f.starts_with("data: "), "SSE prefix: {f:?}");
        assert!(f.ends_with("\n\n"), "SSE terminator: {f:?}");
        let json_part = f.trim_start_matches("data: ").trim_end();
        let v: serde_json::Value = serde_json::from_str(json_part).expect("valid JSON payload");
        assert_eq!(v["event"], "registered");
    }

    #[test]
    fn sse_reader_emits_valid_sse_frame() {
        let ctrl = test_controller();
        let rx = ctrl.subscribe();
        // Use a zero tick so the test doesn't sleep.
        let mut reader = SseReader::with_tick(rx, Arc::clone(&ctrl), Duration::from_nanos(0));

        // Read just enough bytes for at least one heartbeat frame.
        let mut buf = vec![0u8; 1024];
        let n = reader.read(&mut buf).expect("read should not fail");
        assert!(n > 0, "SseReader must return > 0 bytes");
        let text = std::str::from_utf8(&buf[..n]).expect("frame must be UTF-8");
        assert!(
            text.starts_with("data: "),
            "must start with SSE prefix: {text:?}"
        );
        assert!(
            text.ends_with("\n\n") || text.contains("\n\n"),
            "must contain SSE terminator: {text:?}"
        );
    }
}
