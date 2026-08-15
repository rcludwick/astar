// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Server-Sent-Events for live console state (iax-dd42 Phase 2). `sse_frame`
//! formats one event; `SseReader` is a blocking `Read` that emits a frame at
//! ~30 Hz for a long-lived `GET /events` response.

use std::io::{self, Read};
use std::sync::Arc;
use std::time::Duration;

use astar_console::ConsoleState;
use astar_console::{ParrotShared, peak_to_dbfs};

use crate::server::ServerState;

/// Format a console state as one SSE event: `data: <json>\n\n`.
#[must_use]
pub fn sse_frame(state: &ConsoleState) -> String {
    let json = serde_json::to_string(state).unwrap_or_else(|_| "{}".to_string());
    format!("data: {json}\n\n")
}

/// JSON view of the parrot signals for the SSE frame.
#[must_use]
pub(crate) fn parrot_view(shared: &ParrotShared) -> serde_json::Value {
    let phase = shared.phase();
    serde_json::json!({
        "phase": phase.label(),
        "running": phase != astar_console::ParrotPhase::Stopped,
        "tx_db": peak_to_dbfs(shared.tx.get()),
    })
}

/// A blocking `Read` that yields one `ConsoleState` SSE frame per ~33 ms tick.
/// `tiny_http` drives it by repeatedly calling `read`; it never returns 0 (the
/// stream ends only when the client disconnects and the write side errors).
pub struct SseReader {
    state: Arc<ServerState>,
    /// Last successful snapshot, re-emitted when the session lock is busy so
    /// the stream never blocks (see `next_frame`).
    last: ConsoleState,
    residual: Vec<u8>,
    pos: usize,
    tick: Duration,
}

impl SseReader {
    #[must_use]
    pub fn new(state: Arc<ServerState>) -> Self {
        Self {
            state,
            last: ConsoleState::default(),
            residual: Vec::new(),
            pos: 0,
            tick: Duration::from_millis(33),
        }
    }

    fn next_frame(&mut self) -> String {
        // Never block the live stream on the session lock. While connect /
        // disconnect hold it for their (slow) audio open/teardown,
        // `try_snapshot` returns None and we re-emit the last known state; the
        // next tick picks up the fresh state once the lock frees. This keeps the
        // UI responsive (it updates to "idle" right after teardown) instead of
        // freezing. The station shares the same session, so this is the same
        // non-blocking `try_lock`+snapshot the harness did inline before.
        if let Some(snap) = self.state.station.try_snapshot() {
            self.last = snap;
        }
        let mut v = serde_json::to_value(&self.last).unwrap_or_else(|_| serde_json::json!({}));
        v["parrot"] = parrot_view(&self.state.parrot_shared);
        // Node-mode status (iax-64b6) read from the shared NodeStatus the event
        // pump owns. `mode` is already in ConsoleState; echoed here for the UI.
        let node = &self.state.node_status;
        v["node"] = serde_json::json!({
            "mode": self.last.mode,
            "listening": *node.listening.lock().unwrap(),
            "incoming_from": *node.incoming_from.lock().unwrap(),
            "register": *node.register.lock().unwrap(),
        });
        format!("data: {v}\n\n")
    }
}

impl Read for SseReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.residual.len() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use astar_console::CallStatus;

    #[test]
    fn frame_has_sse_prefix_and_terminator_and_json() {
        let state = ConsoleState {
            status: CallStatus::Answered,
            rtt_ms: Some(41),
            ..ConsoleState::default()
        };
        let frame = sse_frame(&state);
        assert!(frame.starts_with("data: "), "SSE data prefix: {frame:?}");
        assert!(frame.ends_with("\n\n"), "SSE terminator: {frame:?}");
        let json = frame.trim_start_matches("data: ").trim_end();
        let v: serde_json::Value = serde_json::from_str(json).expect("valid json payload");
        assert_eq!(v["status"]["kind"], "answered");
        assert_eq!(v["rtt_ms"], 41);
    }

    #[test]
    fn idle_default_frame_serializes() {
        let frame = sse_frame(&ConsoleState::default());
        let json = frame.trim_start_matches("data: ").trim_end();
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(v["status"]["kind"], "idle");
        assert_eq!(v["ptt"], false);
    }

    #[test]
    fn parrot_view_default_is_stopped() {
        let v = super::parrot_view(&astar_console::ParrotShared::new());
        assert_eq!(v["phase"], "stopped");
        assert_eq!(v["running"], false);
        assert_eq!(v["tx_db"], -60.0);
    }
}
