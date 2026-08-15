// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Secret-free routing configuration over the JSON/FFI boundary (iax-42e9).
//!
//! Secrets are NEVER serialized here — they stay a `dial()`-time argument.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Stable, secret-free identity for a connection. This IS the `CallId` identity
/// (Task 3.0) — the manager keys its pool and `apply()` reconciliation on this,
/// NOT on `node`, because two connections may target the same node (Q5).
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ConnectionId(String);
impl ConnectionId {
    /// Construct a stable connection identity from any string-like value.
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// Borrow the identity as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One node connection's static wiring. No secret field by design.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionSpec {
    /// Stable secret-free identity; the reconciliation/pool key (Q5).
    pub id: ConnectionId,
    /// Node number / label this connection targets (NOT the identity — two
    /// connections may share a node).
    pub node: String,
    /// The calling node / caller-id presented to the peer.
    pub calling_node: String,
    /// Human-friendly label for this connection.
    pub name: String,
    /// Output bus device id this call's RX mixes onto.
    pub output_device: String,
    /// Input (mic) device id routed to this call (`None` = monitor-only).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub input_device: Option<String>,
}

/// The whole desired pool. Secret-free.
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoutingConfig {
    /// Every connection in the desired pool, keyed (for reconciliation) by `id`.
    pub connections: Vec<ConnectionSpec>,
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;
    #[test]
    fn roundtrips_through_json_without_secrets() {
        let cfg = RoutingConfig {
            connections: vec![ConnectionSpec {
                id: ConnectionId::new("harness-55553"),
                node: "55553".into(),
                calling_node: "1999".into(),
                name: "harness".into(),
                output_device: "out:shared".into(),
                input_device: Some("in:a".into()),
            }],
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(
            !json.to_lowercase().contains("secret"),
            "no secret key in config"
        );
        assert!(!json.contains("allstar"), "no known secret value leaks");
        let back: RoutingConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.connections.len(), 1);
        assert_eq!(back.connections[0].node, "55553");
        assert_eq!(back.connections[0].id, ConnectionId::new("harness-55553"));
    }

    #[test]
    fn two_connections_may_target_the_same_node_with_distinct_ids() {
        // Q5: identity is `id`, not `node` — one node can have two connections.
        let cfg = RoutingConfig {
            connections: vec![
                ConnectionSpec {
                    id: ConnectionId::new("tx"),
                    node: "55553".into(),
                    calling_node: "1999".into(),
                    name: "tx".into(),
                    output_device: "out:s".into(),
                    input_device: Some("in:a".into()),
                },
                ConnectionSpec {
                    id: ConnectionId::new("mon"),
                    node: "55553".into(),
                    calling_node: "1999".into(),
                    name: "mon".into(),
                    output_device: "out:s".into(),
                    input_device: None,
                },
            ],
        };
        assert_ne!(cfg.connections[0].id, cfg.connections[1].id);
        assert_eq!(cfg.connections[0].node, cfg.connections[1].node);
    }

    #[test]
    fn connection_spec_has_no_secret_field() {
        // Compile-time guard: there is no `secret` field to set.
        let _ = ConnectionSpec {
            id: ConnectionId::new("x"),
            node: String::new(),
            calling_node: String::new(),
            name: String::new(),
            output_device: String::new(),
            input_device: None,
        };
    }
}
