// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
use std::net::SocketAddr;
use std::time::Duration;

use astar_asl3::PortalCredentials;
use astar_console::OperatingMode;
use astar_iax::{CodecPolicy, IncomingCallPolicy, KnownNodes};

/// How the node answers inbound calls. Re-exported from `astar-console` so
/// the station, the console session, and the FFI all share one `AnswerPolicy`.
pub use astar_console::AnswerPolicy;

/// Outbound node-registration recipe. **Secret-free** — the registrar password
/// comes from a runtime resolver hook ([`crate::Station::set_secret_resolver`]),
/// never from config.
#[derive(Clone, Debug)]
pub struct RegisterConfig {
    /// The upstream registrar's address (resolve the node→host:port yourself).
    pub peer: std::net::SocketAddr,
    /// The username/node id to register AS (e.g. `"77777"`).
    pub username: String,
    /// Requested refresh interval.
    pub refresh: Duration,
}

/// Node-mode configuration. **Secret-free** — inbound credentials (for
/// auth=Required) live in `policy.credentials`, and registration secrets
/// arrive via a runtime resolver hook ([`crate::Station::set_secret_resolver`]),
/// never here.
#[derive(Debug)]
pub struct NodeConfig {
    /// Listener bind address (default `0.0.0.0:4569`).
    pub bind: SocketAddr,
    /// Inbound-call policy (auth/calltoken/credentials). `decision` is forced
    /// to `AppDecide` internally so the engine gates every offer.
    pub policy: IncomingCallPolicy,
    /// Auto vs manual answer.
    pub answer: AnswerPolicy,
    /// Optional outbound registration: when `Some`, the node registers itself
    /// AS a node with an upstream registrar so callers can reach it by node
    /// number. Secret-free — the password is resolved at runtime. `None` = the
    /// node only listens, it does not register.
    pub register: Option<RegisterConfig>,
    /// Maximum simultaneous inbound calls (default `20`).
    pub max_calls: usize,
    /// Optional inbound node allowlist (iax-91c9). When `Some` and non-empty,
    /// callers whose node id is not on the list are rejected ("not authorized")
    /// at call-setup time. `None`/empty = admit all (backward compatible).
    pub allowlist: Option<KnownNodes>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:4569".parse().expect("valid default bind"),
            policy: IncomingCallPolicy::default(),
            answer: AnswerPolicy::default(),
            register: None,
            max_calls: 20,
            allowlist: None,
        }
    }
}

/// Station configuration. **Secret-free except `secret`**, which is a guest
/// secret (default `"allstar"`), never a portal password. No portal password
/// is stored in plain config — WT credentials live in [`PortalCredentials`],
/// which `astar-asl3` is responsible for zeroizing.
///
/// Deliberately **not** `#[derive(Debug)]`: it holds [`PortalCredentials`]
/// (which carries a portal password), and a derived `Debug` would risk leaking
/// it through logs. If a redacted view is ever needed, hand-write one.
pub struct StationConfig {
    /// Capture device substring; `None` = system default.
    pub input: Option<String>,
    /// Playback device substring; `None` = system default.
    pub output: Option<String>,
    /// Portal credentials for WT token minting; `None` = no WT path.
    pub portal: Option<PortalCredentials>,
    /// Guest secret (default `"allstar"`).
    pub secret: String,
    /// Operating mode applied at construction (default Wt). Flip live with [`crate::Station::set_mode`].
    pub mode: OperatingMode,
    /// Node-mode configuration (listener bind, inbound policy, answer mode).
    /// `None` = use [`NodeConfig::default`] when switching to Node mode.
    pub node: Option<NodeConfig>,
    /// Override for the portal base URL used by the WT token mint
    /// ([`crate::Station::connect_wt`] / [`crate::Station::test_mint_token`]).
    /// `None` = the live `AllStarLink` portal. Set only to point at a staging or
    /// stub portal (e.g. an offline test harness); not exposed across the C-ABI.
    pub portal_base: Option<String>,
    /// Codec negotiation policy for OUTBOUND calls placed by this station
    /// (iax-31f7). Default `UlawOnly`. **Outbound-only**: this field feeds
    /// `connect`/`connect_wt` only. Inbound calls take their codec policy from
    /// `node.policy.codec_policy` (or `NodeConfig::default()`'s
    /// `IncomingCallPolicy::default()`, also `UlawOnly`) — set that field
    /// directly if a caller wants asymmetric inbound/outbound policy.
    pub codec_policy: CodecPolicy,
}

impl Default for StationConfig {
    fn default() -> Self {
        Self {
            input: None,
            output: None,
            portal: None,
            secret: "allstar".to_string(),
            mode: OperatingMode::Wt,
            node: None,
            portal_base: None,
            codec_policy: CodecPolicy::default(),
        }
    }
}

/// Inbound-listener configuration — the public surface for always-on node
/// mode (iax-a1fb P2). [`NodeConfig`] is kept for the internal shim; this
/// struct is what consumers construct.
///
/// Note: `Clone` is implemented manually because [`IncomingCallPolicy`] does
/// not implement `Clone` (it holds `Arc<Secret>`; see `clone_policy`).
#[derive(Debug)]
pub struct InboundConfig {
    /// Listener bind address (default `0.0.0.0:4569`).
    pub bind: SocketAddr,
    /// Inbound-call policy (auth/calltoken/credentials).
    pub policy: IncomingCallPolicy,
    /// Auto vs manual answer.
    pub answer: AnswerPolicy,
    /// Maximum simultaneous inbound calls (default `20`).
    pub max_calls: usize,
    /// Optional inbound node allowlist (iax-91c9). When `Some` and non-empty,
    /// callers whose node id is not on the list are rejected ("not authorized")
    /// at call-setup time, BEFORE answer/adopt. `None`/empty = admit all
    /// (backward compatible). Orthogonal to `policy.auth`.
    pub allowlist: Option<KnownNodes>,
}

impl Clone for InboundConfig {
    fn clone(&self) -> Self {
        Self {
            bind: self.bind,
            policy: clone_policy(&self.policy),
            answer: self.answer,
            max_calls: self.max_calls,
            allowlist: self.allowlist.clone(),
        }
    }
}

impl Default for InboundConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:4569".parse().expect("valid default bind"),
            policy: IncomingCallPolicy::default(),
            answer: AnswerPolicy::Auto,
            max_calls: 20,
            allowlist: None,
        }
    }
}

/// Clone an [`IncomingCallPolicy`] field-by-field (it is not `Clone`; the
/// per-username credential map holds `Arc<Secret>`, so cloning it shares the
/// zeroizing secret rather than copying plaintext — iax-f755 L5).
pub(crate) fn clone_policy(p: &IncomingCallPolicy) -> IncomingCallPolicy {
    IncomingCallPolicy {
        calltoken: p.calltoken,
        auth: p.auth,
        decision: p.decision,
        auto_answer: p.auto_answer,
        accept_decision_timeout: p.accept_decision_timeout,
        allow_plaintext: p.allow_plaintext,
        credentials: p.credentials.clone(),
        // `Arc` clone — shares the resolver, copies no plaintext (iax-99cd).
        credential_resolver: p.credential_resolver.clone(),
        codec_policy: p.codec_policy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_config_defaults() {
        let c = InboundConfig::default();
        assert_eq!(c.bind, "0.0.0.0:4569".parse().unwrap());
        assert_eq!(c.max_calls, 20);
        assert_eq!(c.answer, AnswerPolicy::Auto);
    }
}
