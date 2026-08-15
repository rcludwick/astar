// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Command-and-event vocabulary for the astar-server daemon.
//!
//! Commands flow **inbound** (JSON → daemon); replies and events flow
//! **outbound** (daemon → JSON). `ProvideSecret` is inbound-only and is
//! never serialised or echoed.

/// Inbound commands from a client (stdin / HTTP / TUI).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum NodeCommand {
    Dial {
        node: String,
    },
    Hangup,
    Answer,
    Reject,
    Key,
    Unkey,
    EnableInbound,
    DisableInbound,
    Register,
    Deregister,
    SetDevices {
        input: Option<String>,
        output: Option<String>,
    },
    /// Inbound only — credentials supplied by the user. Never serialised/echoed.
    ProvideSecret {
        username: String,
        secret: String,
    },
    Status,
    Shutdown,
    Announce {
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        sample: Option<String>,
        #[serde(default)]
        destination: Option<String>,
        #[serde(default)]
        mixunder: Option<bool>,
        #[serde(default)]
        gain_db: Option<f32>,
    },
    IdNow,
    /// Node-to-node link control (iax-d829.1, `POST /link`) — the `AllStar`
    /// `ilink` surface. `connect` = transceive (two-way), `monitor` = RX-only,
    /// and `disconnect` tears the link to `node` down. `addr` optionally dials
    /// an EXPLICIT `host:port` (harness / localhost / NAT-hairpin), bypassing
    /// `AllStar` DNS; omitted means resolve `node` via DNS.
    Link {
        action: LinkAction,
        node: String,
        #[serde(default)]
        addr: Option<String>,
    },
    /// Re-wire the conference bridge live (iax-647d, `POST /bridge`). Omitted
    /// fields keep their current value, so a partial body is a partial update.
    SetBridge {
        /// `"handset"` | `"bridge"` | `"conference"` (case-insensitive).
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        mix_minus: Option<bool>,
        #[serde(default)]
        include_local_radio: Option<bool>,
    },
}

/// Which `ilink`-style link operation a [`NodeCommand::Link`] requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkAction {
    /// `*3<node>` — full two-way link (transceive).
    Connect,
    /// `*2<node>` — receive-only link (monitor).
    Monitor,
    /// `*1<node>` — tear the link down.
    Disconnect,
}

/// Synchronous replies sent in response to a `NodeCommand`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum NodeReply {
    Ok,
    Snapshot(NodeSnapshot),
}

/// Asynchronous events pushed to the client.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum NodeEvent {
    Snapshot(NodeSnapshot),
    IncomingCall {
        from: String,
    },
    Registered,
    RegisterFailed {
        reason: String,
    },
    Hangup {
        reason: String,
    },
    /// A voice/CW announcement has started. `kind` is a secret-free label:
    /// `"id"` for a periodic station-ID, `"event"` for an event-table trigger,
    /// `"command"` for a manual `Announce`/`IdNow` command.
    AnnouncementStarted {
        kind: String,
    },
    /// A voice/CW announcement has finished (reserved for future use when the
    /// station surfaces a completion event).
    AnnouncementFinished {
        kind: String,
    },
    /// A node-to-node link lifecycle edge (iax-d829.1), secret-free. `kind` is
    /// `"connected"` / `"disconnected"` / `"keyed"`; `reason` is set only for
    /// disconnects, `keyed` only for key changes.
    Link {
        kind: String,
        node: String,
        call: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        keyed: Option<bool>,
    },
    /// A DTMF digit received from a connected member (iax-2f5e), emitted for
    /// EVERY drained digit whether or not `[dtmf] enabled` maps it to a
    /// command — it is the only visibility operators have into whether a
    /// handset's tones are reaching the node at all. `command`, when set,
    /// reports the `*`-sequence this digit completed (e.g. `"connect 55553"`).
    Dtmf {
        call: u64,
        digit: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
    },
}

/// Point-in-time view of the daemon state.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NodeSnapshot {
    /// This node's own ID (the register username, e.g. `"77777"`); `None`
    /// when registration is unconfigured. Page/UI title (iax-24e2).
    pub node_id: Option<String>,
    pub listening: bool,
    pub registered: bool,
    pub calls: Vec<astar_iax::CallSnapshot>,
    /// Live node-to-node links (iax-d829.1) — the `app_rpt` `RPT_LINKS`
    /// analogue, secret-free.
    pub links: Vec<astar_iax::LinkSnapshot>,
}

/// Error payload sent to the client — never contains credentials.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NodeError {
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_deserializes_from_json() {
        let c: NodeCommand = serde_json::from_str(r#"{"cmd":"dial","node":"55553"}"#).unwrap();
        assert!(matches!(c, NodeCommand::Dial { node } if node == "55553"));
        let s: NodeCommand = serde_json::from_str(r#"{"cmd":"status"}"#).unwrap();
        assert!(matches!(s, NodeCommand::Status));
    }

    #[test]
    fn dtmf_event_serializes_secret_free_with_optional_command() {
        let bare = NodeEvent::Dtmf {
            call: 7,
            digit: "5".into(),
            command: None,
        };
        let j = serde_json::to_string(&bare).unwrap();
        assert!(j.contains(r#""event":"dtmf""#), "{j}");
        assert!(j.contains(r#""digit":"5""#), "{j}");
        assert!(!j.contains("command"), "omitted when absent: {j}");

        let done = NodeEvent::Dtmf {
            call: 7,
            digit: "#".into(),
            command: Some("connect 55553".into()),
        };
        let j = serde_json::to_string(&done).unwrap();
        assert!(j.contains(r#""command":"connect 55553""#), "{j}");
        for banned in ["secret", "password", "token"] {
            assert!(!j.contains(banned), "{banned} must not appear: {j}");
        }
    }

    #[test]
    fn link_command_deserializes_with_action_and_optional_addr() {
        let c: NodeCommand =
            serde_json::from_str(r#"{"cmd":"link","action":"connect","node":"55553"}"#).unwrap();
        assert!(
            matches!(c, NodeCommand::Link { action: LinkAction::Connect, ref node, addr: None } if node == "55553")
        );
        let m: NodeCommand =
            serde_json::from_str(r#"{"cmd":"link","action":"monitor","node":"42"}"#).unwrap();
        assert!(matches!(
            m,
            NodeCommand::Link {
                action: LinkAction::Monitor,
                ..
            }
        ));
        let d: NodeCommand = serde_json::from_str(
            r#"{"cmd":"link","action":"disconnect","node":"42","addr":"127.0.0.1:4569"}"#,
        )
        .unwrap();
        assert!(matches!(
            d,
            NodeCommand::Link {
                action: LinkAction::Disconnect,
                addr: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn snapshot_with_links_serializes_secret_free() {
        let snap = NodeSnapshot {
            node_id: None,
            listening: true,
            registered: false,
            calls: vec![],
            links: vec![],
        };
        let j = serde_json::to_string(&snap).unwrap();
        assert!(j.contains("\"links\":[]"), "links field present: {j}");
    }

    #[test]
    fn announce_command_deserializes() {
        let c: NodeCommand = serde_json::from_str(
            r#"{"cmd":"announce","text":"net in five minutes","destination":"to_air"}"#,
        )
        .unwrap();
        assert!(matches!(c, NodeCommand::Announce { text: Some(_), .. }));
        let c2: NodeCommand = serde_json::from_str(r#"{"cmd":"id_now"}"#).unwrap();
        assert!(matches!(c2, NodeCommand::IdNow));
    }

    #[test]
    fn snapshot_serializes_and_is_secret_free() {
        let snap = NodeSnapshot {
            node_id: None,
            listening: true,
            registered: false,
            calls: vec![],
            links: vec![],
        };
        let j = serde_json::to_string(&snap).unwrap();
        assert!(j.contains("\"listening\":true"));
        for bad in ["secret", "password", "token"] {
            assert!(
                !j.contains(bad),
                "serialised snapshot contains forbidden word: {bad}"
            );
        }
    }
}
