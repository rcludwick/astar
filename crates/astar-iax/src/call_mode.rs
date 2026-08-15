// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Call-shape selection for the high-level client (iax-3fca).
//!
//! `CallMode` is the `AllStarLink`-aware sugar; it lowers to the mechanical
//! [`astar_iax_core::session::CallProfile`] consumed by the core FSM. Keeping
//! the `AllStar` concept here keeps `astar-iax-core` protocol-only.

use astar_iax_core::session::{CallProfile, CodecPolicy};

/// How the outbound call's NEW frame is shaped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CallMode {
    /// Standard IAX2: `dial(dest)` sets `CALLED_NUMBER`; `CAPABILITY` advertised.
    #[default]
    Standard,
    /// `AllStarLink` web-transceiver guest path. `CALLED_NUMBER` is forced to
    /// `"s"`; `node`/`name` populate `CALLING_NUMBER` / `CALLING_NAME`; no
    /// `CAPABILITY` IE. Pair with `caller_id("allstar-public")` and the guest
    /// `secret("allstar")`.
    WebTransceiver { node: String, name: String },
}

impl CallMode {
    /// Lower to the core [`CallProfile`] plus an optional forced `CALLED_NUMBER`
    /// that overrides the `dial()` argument (the WT path always calls `"s"`).
    pub(crate) fn resolve(&self) -> (CallProfile, Option<&'static str>) {
        match self {
            CallMode::Standard => (CallProfile::default(), None),
            CallMode::WebTransceiver { node, name } => (
                CallProfile {
                    calling_number: Some(node.clone()),
                    calling_name: Some(name.clone()),
                    send_capability: false,
                    codec_policy: CodecPolicy::default(),
                },
                Some("s"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_lowers_to_default_profile_and_no_forced_dest() {
        let (profile, dest) = CallMode::Standard.resolve();
        assert_eq!(profile, CallProfile::default());
        assert_eq!(dest, None);
    }

    #[test]
    fn web_transceiver_forces_s_and_wt_profile() {
        let mode = CallMode::WebTransceiver {
            node: "55553".into(),
            name: "astar".into(),
        };
        let (profile, dest) = mode.resolve();
        assert_eq!(dest, Some("s"));
        assert_eq!(profile.calling_number, Some("55553".into()));
        assert_eq!(profile.calling_name, Some("astar".into()));
        assert!(!profile.send_capability, "WT mode omits CAPABILITY");
    }

    #[test]
    fn default_call_mode_is_standard() {
        assert_eq!(CallMode::default(), CallMode::Standard);
    }
}
