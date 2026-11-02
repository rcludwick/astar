// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Server-Sent-Events for live console state (iax-dd42 Phase 2). `sse_frame`
//! formats one event; `next_frame_json` is the payload `GET /events` emits at
//! ~30 Hz.

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

/// The next `/events` payload, as JSON: the session snapshot, the parrot
/// signals and the node status. `last` is the previous good snapshot.
#[must_use]
pub fn next_frame_json(state: &ServerState, last: &mut ConsoleState) -> String {
    // Never block the live stream on the session lock. While connect /
    // disconnect hold it for their (slow) audio open/teardown,
    // `try_snapshot` returns None and we re-emit the last known state; the
    // next tick picks up the fresh state once the lock frees. This keeps the
    // UI responsive (it updates to "idle" right after teardown) instead of
    // freezing. The station shares the same session, so this is the same
    // non-blocking `try_lock`+snapshot the harness did inline before.
    if let Some(snap) = state.station.try_snapshot() {
        *last = snap;
    }
    let mut v = serde_json::to_value(&*last).unwrap_or_else(|_| serde_json::json!({}));
    v["parrot"] = parrot_view(&state.parrot_shared);
    // Node-mode status (iax-64b6) read from the shared NodeStatus the event
    // pump owns. `mode` is already in ConsoleState; echoed here for the UI.
    let node = &state.node_status;
    v["node"] = serde_json::json!({
        "mode": last.mode,
        "listening": *node.listening.lock().unwrap(),
        "incoming_from": *node.incoming_from.lock().unwrap(),
        "register": *node.register.lock().unwrap(),
    });
    v.to_string()
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
