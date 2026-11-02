// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Server-Sent-Events stream for the astar-server control channel (iax-35b1).
//!
//! `sse_frame` formats one `NodeEvent` as an SSE data line; `pump_events`
//! drains the controller's event subscription onto an
//! [`astar_http::sse::Writer`], with a `Snapshot` heartbeat whenever no
//! event is pending.

use std::io::{self, Write};
use std::sync::{Arc, mpsc::Receiver};
use std::time::Duration;

use crate::{command::NodeEvent, controller::NodeController};

/// How often an event stream sends a frame: an event when one is pending,
/// a snapshot heartbeat when not.
pub const EVENT_TICK: Duration = Duration::from_millis(33);

/// Format a `NodeEvent` as one SSE frame: `data: <json>\n\n`.
///
/// Credentials never appear in `NodeEvent` (the type is secret-free by
/// design), so the returned string is always credential-free.
#[must_use]
pub fn sse_frame(ev: &NodeEvent) -> String {
    format!("data: {}\n\n", event_json(ev))
}

fn event_json(ev: &NodeEvent) -> String {
    serde_json::to_string(ev).unwrap_or_else(|_| "{}".to_string())
}

/// The next event's JSON: the one pending in `rx`, or a heartbeat
/// `Snapshot` when none is, so the stream never stalls.
#[must_use]
pub fn next_event_json(rx: &Receiver<NodeEvent>, ctrl: &NodeController) -> String {
    match rx.try_recv() {
        Ok(ev) => event_json(&ev),
        Err(_) => event_json(&NodeEvent::Snapshot(ctrl.snapshot())),
    }
}

/// Copy the controller's events to `w`, one frame every `tick`, until
/// `keep_going` says no, the controller stops (so an open stream never
/// holds up a shutdown), or a write fails (the client left, or stalled past
/// the write timeout). Subscribes on entry.
///
/// # Errors
/// The write that failed.
pub fn pump_events<W: Write>(
    w: &mut astar_http::sse::Writer<W>,
    ctrl: &Arc<NodeController>,
    tick: Duration,
    mut keep_going: impl FnMut() -> bool,
) -> io::Result<()> {
    let rx = ctrl.subscribe();
    loop {
        if !keep_going() || ctrl.should_stop() {
            return Ok(());
        }
        std::thread::sleep(tick);
        w.data(&next_event_json(&rx, ctrl))?;
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
    fn pump_events_writes_a_valid_frame() {
        let ctrl = test_controller();
        let mut w = astar_http::sse::Writer::new(Vec::new());
        let mut once = true;
        pump_events(&mut w, &ctrl, Duration::ZERO, || std::mem::take(&mut once))
            .expect("a Vec never fails a write");
        let text = String::from_utf8(w.into_inner()).expect("frame must be UTF-8");
        assert!(
            text.starts_with("data: {"),
            "must start with SSE prefix: {text:?}"
        );
        assert!(text.ends_with("\n\n"), "one whole frame: {text:?}");
        assert_eq!(text.matches("\n\n").count(), 1, "exactly one: {text:?}");
        let v: serde_json::Value =
            serde_json::from_str(text.trim_start_matches("data: ").trim_end()).expect("JSON");
        assert_eq!(v["event"], "snapshot", "the heartbeat: {text}");
    }

    #[test]
    fn pump_events_ends_when_the_node_stops() {
        let ctrl = test_controller();
        let _ = ctrl.execute(crate::command::NodeCommand::Shutdown);
        assert!(ctrl.should_stop());
        let mut w = astar_http::sse::Writer::new(Vec::new());
        pump_events(&mut w, &ctrl, Duration::ZERO, || true).expect("ends cleanly");
        assert!(w.into_inner().is_empty(), "nothing after stop");
    }
}
