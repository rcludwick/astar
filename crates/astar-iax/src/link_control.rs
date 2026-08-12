// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Link control API (iax-62cf): connect/disconnect by node, permanent links
//! with app-driven auto-reconnect, inbound-link allowlist validation, and an
//! aggregated link lifecycle event stream. Built on top of the iax-ad2e Link
//! layer and the iax-42e9 Manager pool. Vendor-neutral + secret-free: node→peer
//! resolution and secret re-supply are INJECTED callbacks, never `AllStar` DNS.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::Duration;

use astar_audio::OutputId;

use crate::call_mode::CallMode;
use crate::error::IaxError;
use crate::link::LinkMode;

/// Resolve a vendor-neutral node label to an IAX2 peer address. INJECTED by the
/// app (`AllStar` apps wire `astar-asl3`'s DNS resolver; plain-Asterisk apps
/// wire a static/host-table one). The library never bakes in any DNS.
pub type LinkResolver<'a> = dyn Fn(&str) -> Result<SocketAddr, IaxError> + 'a;

/// Re-supply a node's dial secret at (re)connect time. INJECTED so secrets stay
/// OUT of the permanent-link registry / config / events. Mirrors `Manager::apply`'s
/// `secret_of`.
pub type SecretResolver<'a> = dyn Fn(&str) -> String + 'a;

/// A request to bring a link up by node label in a chosen mode. The `secret` is
/// dial-time only: `connect_link` moves it straight into the `DialSpec` and never
/// stores it.
pub struct LinkSpec {
    /// Vendor-neutral node number / label.
    pub node: String,
    /// Desired link mode.
    pub mode: LinkMode,
    /// Output bus this link's RX mixes onto.
    pub output: OutputId,
    /// Username / caller-id presented to the peer.
    pub caller_id: String,
    /// Dial-time secret. Consumed immediately; never stored.
    pub secret: String,
    /// Dial target (e.g. node number); inert in WT mode.
    pub dest: String,
    /// Pre-built call shape (`Standard` or `WebTransceiver`).
    pub mode_shape: CallMode,
    /// If true, the link is registered as PERMANENT and auto-reconnects across
    /// drops when the app calls `Manager::tick`.
    pub permanent: bool,
}

/// Allowlist of node labels permitted to LINK (orthogonal to iax-8baf auth,
/// which proves identity). Secret-free.
#[derive(Debug, Clone, Default)]
pub struct KnownNodes {
    nodes: HashSet<String>,
}

impl KnownNodes {
    /// Build from any iterator of node labels.
    pub fn from_iter_labels<I, S>(labels: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            nodes: labels.into_iter().map(Into::into).collect(),
        }
    }

    /// Add a node label to the allowlist.
    pub fn allow(&mut self, node: impl Into<String>) {
        self.nodes.insert(node.into());
    }

    /// Whether `node` is on the allowlist.
    #[must_use]
    pub fn contains(&self, node: &str) -> bool {
        self.nodes.contains(node)
    }

    /// Whether the allowlist has no entries. An empty allowlist is treated by
    /// admission callers (iax-91c9) as "allow all" — a configured-but-empty
    /// list is the same as no list.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// The outcome of validating an inbound link request against the allowlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkAdmission {
    /// The node is allowed to link; the app should `answer()` + `adopt` + `add_link`.
    Accepted,
    /// The node is not allowed; the app should `IncomingCall::reject(Some(cause))`.
    Rejected { cause: String },
}

/// Validates an inbound link request: the node must be on the `KnownNodes`
/// allowlist. Composes ON TOP OF iax-8baf credential auth (which has already run
/// in the Listener by the time the app sees an `IncomingCall`).
#[derive(Debug, Clone, Default)]
pub struct LinkValidator {
    known: KnownNodes,
}

impl LinkValidator {
    /// Build a validator over an allowlist.
    #[must_use]
    pub fn new(known: KnownNodes) -> Self {
        Self { known }
    }

    /// Decide whether `node` may link. An empty/unknown node label is Rejected.
    #[must_use]
    pub fn validate(&self, node: &str) -> LinkAdmission {
        if self.known.contains(node) {
            LinkAdmission::Accepted
        } else {
            LinkAdmission::Rejected {
                cause: "node not in known-nodes allowlist".to_string(),
            }
        }
    }
}

/// Aggregated, secret-free link lifecycle event. DERIVED from the canonical
/// per-call `CallEvent`/`CallSnapshot` — NOT a second source of truth. One
/// channel for all links so a node-controller consumes a single stream.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum LinkEvent {
    /// The link's call reached Active (answered).
    Connected { node: String, call: u64 },
    /// The link's call ended / dropped. `reason` is a human string (secret-free).
    Disconnected {
        node: String,
        call: u64,
        reason: String,
    },
    /// The link's local PTT changed (keyed = transmitting).
    Keyed {
        node: String,
        call: u64,
        keyed: bool,
    },
}

/// Backoff schedule for permanent-link reconnection. Exponential with a cap; a
/// successful reconnect resets attempts to zero. Pure (a function of `attempts`)
/// so it is unit-testable without a clock.
#[must_use]
pub(crate) fn backoff_delay(attempts: u32) -> Duration {
    const BASE_MS: u64 = 1_000;
    const CAP_MS: u64 = 30_000;
    let shifted = BASE_MS.saturating_mul(1u64.checked_shl(attempts).unwrap_or(u64::MAX));
    Duration::from_millis(shifted.min(CAP_MS))
}

/// The secret-free re-dial recipe for a permanent link, plus its backoff state.
/// Stores NEITHER the secret NOR the resolver — both are passed into
/// `Manager::tick`. `desired` is set false by `disconnect_link` so a deliberate
/// teardown is never re-dialed.
pub(crate) struct PermanentLink {
    pub(crate) node: String,
    pub(crate) mode: LinkMode,
    pub(crate) output: OutputId,
    pub(crate) caller_id: String,
    pub(crate) dest: String,
    pub(crate) mode_shape: CallMode,
    pub(crate) desired: bool,
    pub(crate) attempts: u32,
    pub(crate) next_attempt_at: Option<std::time::Instant>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_spec_carries_node_mode_and_permanence_without_storing_secret_anywhere_public() {
        let spec = LinkSpec {
            node: "55553".into(),
            mode: LinkMode::Monitor,
            output: OutputId::new("out:s"),
            caller_id: "1999".into(),
            secret: "hunter2".into(),
            dest: "55553".into(),
            mode_shape: CallMode::Standard,
            permanent: true,
        };
        assert_eq!(spec.node, "55553");
        assert_eq!(spec.mode, LinkMode::Monitor);
        assert!(spec.permanent);
    }

    #[test]
    fn validator_accepts_known_node_and_rejects_unknown() {
        let known = KnownNodes::from_iter_labels(["55553", "1999"]);
        let v = LinkValidator::new(known);
        assert_eq!(v.validate("55553"), LinkAdmission::Accepted);
        match v.validate("40000") {
            LinkAdmission::Rejected { cause } => assert!(cause.contains("allowlist")),
            LinkAdmission::Accepted => panic!("unknown node must be rejected"),
        }
    }

    #[test]
    fn known_nodes_is_empty_tracks_membership() {
        let mut k = KnownNodes::default();
        assert!(k.is_empty(), "default allowlist is empty");
        k.allow("55553");
        assert!(!k.is_empty(), "non-empty after allow()");
        assert!(KnownNodes::from_iter_labels(Vec::<String>::new()).is_empty());
        assert!(!KnownNodes::from_iter_labels(["1001"]).is_empty());
    }

    #[test]
    fn validator_rejects_empty_node_label() {
        let v = LinkValidator::new(KnownNodes::default());
        assert!(matches!(v.validate(""), LinkAdmission::Rejected { .. }));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn link_event_serializes_without_secrets() {
        let ev = LinkEvent::Connected {
            node: "55553".into(),
            call: 1,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("55553"));
        assert!(!json.to_lowercase().contains("secret"));
        assert!(!json.contains("password"));
        assert!(!json.contains("hunter2"));
    }

    #[test]
    fn backoff_is_exponential_and_capped_at_30s() {
        assert_eq!(backoff_delay(0), Duration::from_secs(1));
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(2), Duration::from_secs(4));
        // 2^5 * 1s = 32s -> capped at 30s.
        assert_eq!(backoff_delay(5), Duration::from_secs(30));
        // Large attempt counts must not panic / overflow; stay capped.
        assert_eq!(backoff_delay(64), Duration::from_secs(30));
    }
}
