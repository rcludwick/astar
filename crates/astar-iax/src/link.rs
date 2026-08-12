// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The Link layer (iax-ad2e): a thin, policy-free metadata view over a
//! [`crate::Manager`] connection.
//!
//! A [`Link`] is NOT a new runtime object and introduces NO parallel audio
//! mechanism. It is a named [`LinkMode`] + node label + key-state mirror sitting
//! on top of an existing pooled [`crate::CallId`]. The three modes are pure
//! functions of the iax-42e9 routing table:
//! - [`LinkMode::Transceive`]  => `tx[call] = Some(mic)`, `rx[call] = output`
//!   (mic routed + RX mixed). The Link layer does NOT pick the mic — that is a
//!   `Manager::route` decision; the Link only enforces the negative invariant
//!   for the monitor modes.
//! - [`LinkMode::Monitor`]     => `tx[call] = None`, `rx[call] = output`
//!   (RX mixed onward, no mic).
//! - [`LinkMode::LocalMonitor`] => `tx[call] = None`, `rx[call] = output`,
//!   [`LinkMode::relays_onward`]`() == false` (RX heard locally only; inert
//!   until iax-42ce consumes the flag).
//!
//! All route/unroute/key work DELEGATES to [`crate::Manager`]; this module only
//! adds mode bookkeeping + the read-only, secret-free [`LinkRoster`].

use crate::call::CallId;

/// How a link relates a local mic/speaker to a peer node.
///
/// Mechanism only — this models the *routing state*, not any command policy
/// that selects it. Maps 1:1 onto the iax-42e9 routing table:
/// - `Transceive`   => tx[call] = Some(mic), rx[call] = output  (mic routed + RX mixed)
/// - `Monitor`      => tx[call] = None,      rx[call] = output  (RX mixed, no mic)
/// - `LocalMonitor` => tx[call] = None,      rx[call] = output, relay = false
///   (RX heard locally only; never relayed onward to other links)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum LinkMode {
    /// Mic routed + RX mixed: the local operator can transmit to and hear the peer.
    Transceive,
    /// No mic + RX mixed onward: hear the peer, never transmit; relay-eligible.
    Monitor,
    /// No mic + RX mixed locally only: hear the peer here, never relayed onward
    /// (iax-42ce reads [`LinkMode::relays_onward`]). Inert until then.
    LocalMonitor,
}

impl LinkMode {
    /// True only for `Transceive`. Monitor/LocalMonitor never transmit.
    #[must_use]
    pub const fn is_transmit_capable(self) -> bool {
        matches!(self, LinkMode::Transceive)
    }

    /// Whether this link's RX may be relayed onward to other links.
    ///
    /// This flag is the SINGLE SOURCE OF TRUTH for relay-eligibility: iax-42ce
    /// MUST consume `LinkMode::relays_onward()` / the roster rather than
    /// inventing its own relay flag. Nothing in *this* crate relays; the flag is
    /// inert until iax-42ce reads it. `LocalMonitor` is local-only (heard here,
    /// never relayed).
    #[must_use]
    pub const fn relays_onward(self) -> bool {
        !matches!(self, LinkMode::LocalMonitor)
    }
}

/// Connection liveness as seen by the roster. Derived from the canonical
/// [`crate::CallSnapshot`] state, NOT a second source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum LinkState {
    /// Handshake in progress (dialed / inbound NEW received, not yet Active).
    Connecting,
    /// Call is Active; audio path is live.
    Up,
}

/// A live link: a Manager connection viewed through a mode.
///
/// `Link` owns NO runtime resources. The call thread, socket, FSM, and audio
/// streams all live in the Manager's `Connection` + `AudioRouter`. `Link` is a
/// metadata record keyed by [`CallId`].
#[derive(Debug, Clone)]
pub struct Link {
    /// Identifies the underlying Manager connection.
    pub call: CallId,
    /// AllStar/Asterisk node number (or other peer label). Vendor-neutral string.
    pub node: String,
    /// Current mode. Changing this is a routing-table mutation (see
    /// `Manager::set_link_mode`).
    pub mode: LinkMode,
    /// Resolved peer address this link dialed (`connect_link` records it);
    /// `None` for adopted inbound links (iax-24e2).
    pub addr: Option<String>,
    /// First instant the roster observed the call `Active`. `OnceLock` so the
    /// `&self` [`crate::Manager::link_roster`] read can set it exactly once.
    pub(crate) up_since: std::sync::OnceLock<std::time::Instant>,
}

/// Serde-friendly, secret-free per-link snapshot for a UI / FFI (iax-1075).
///
/// MUST NOT contain credentials, tokens, or anything not already public in the
/// iax-42e9 [`crate::ManagerSnapshot`] / [`crate::CallSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct LinkSnapshot {
    /// `CallId::as_raw()` — an opaque process-level id, not a secret.
    pub call: u64,
    /// Node number / peer label (mirrors the call snapshot's `node`).
    pub node: String,
    /// Current link mode.
    pub mode: LinkMode,
    /// Derived liveness (`Connecting` / `Up`) from the call snapshot's state.
    pub state: LinkState,
    /// Local PTT state for this link (true = keyed / transmitting). Mirrors the
    /// call snapshot's `keyed`; always false for Monitor / `LocalMonitor` (they
    /// never have a mic, so they never transmit).
    pub keyed: bool,
    /// Remote peer is keyed (talking) — mirrors the call snapshot's
    /// `remote_keyed` (iax-24e2, the "who's talking" column).
    pub rx_active: bool,
    /// Whole seconds since the roster first observed this link `Up`; `0`
    /// while connecting (iax-24e2).
    pub up_secs: u64,
    /// Resolved peer address dialed for this link; `None` for adopted
    /// inbound links (iax-24e2).
    pub addr: Option<String>,
}

/// The whole roster: `app_rpt` `RPT_LINKS` analogue. Secret-free + serde-friendly.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct LinkRoster {
    /// One snapshot per live link, sorted by `call` for stable UI diffing.
    pub links: Vec<LinkSnapshot>,
}

/// Errors from the Link layer (`Manager::add_link` / `set_link_mode` /
/// `remove_link` / `key_link` / `unkey_link`).
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    /// No link is registered for this call.
    #[error("no link for call {0:?}")]
    NoSuchLink(CallId),
    /// Tried to key a link that is not in `Transceive` mode, or a Transceive
    /// link with no mic routed (maps `IaxError::NotTransmitCapable` /
    /// `IaxError::NotRouted`).
    #[error("link is not transmit-capable in its current mode")]
    NotTransmitCapable,
    /// The underlying call is not pooled / has ended (maps
    /// `IaxError::NoActiveCall`).
    #[error("no active call for {0:?}")]
    NoActiveCall(CallId),
    /// The injected resolver failed to turn a node label into a peer address
    /// (e.g. DNS miss). The library never resolves itself — this surfaces the
    /// app-supplied resolver's failure.
    #[error("could not resolve node to a peer: {0}")]
    Resolve(String),
    /// An operation requiring a PERMANENT link was called for a `CallId` that is
    /// not registered as permanent.
    #[error("call {0:?} is not a permanent link")]
    NotPermanent(CallId),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_and_local_monitor_are_never_keyed_capable() {
        // Mechanism invariant: only Transceive can transmit.
        assert!(LinkMode::Transceive.is_transmit_capable());
        assert!(!LinkMode::Monitor.is_transmit_capable());
        assert!(!LinkMode::LocalMonitor.is_transmit_capable());
    }

    #[test]
    fn local_monitor_does_not_relay_others_do() {
        // LocalMonitor RX is heard locally only; the other two may be relayed onward.
        assert!(!LinkMode::LocalMonitor.relays_onward());
        assert!(LinkMode::Monitor.relays_onward());
        assert!(LinkMode::Transceive.relays_onward());
    }

    #[test]
    fn link_error_has_resolve_and_not_permanent_variants() {
        // Construction + Display smoke test for the new variants used by connect_link/tick.
        let e = LinkError::Resolve("no TXT record for 55553".to_string());
        assert!(format!("{e}").contains("resolve"));
        let p = LinkError::NotPermanent(CallId::from_raw(7));
        assert!(format!("{p}").contains("permanent"));
    }
}
