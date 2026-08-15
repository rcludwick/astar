// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Mode-varying parameters for the outbound NEW frame (iax-3fca).
//!
//! Standard IAX2 by default; the `AllStarLink` web-transceiver shape sets the
//! `CALLING_*` IEs and omits `CAPABILITY`. The high-level `astar-iax` crate's
//! `CallMode` enum lowers to this mechanical struct, keeping AllStar-specific
//! concepts out of the core.

use super::CodecPolicy;

/// Parameters that differ between a standard IAX2 NEW and the web-transceiver
/// guest NEW. Held by the [`crate::session::fsm::Fsm`] and consumed by
/// `build_new` at every NEW (re)transmit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallProfile {
    /// `CALLING_NUMBER` IE. `None` ⇒ the IE is omitted (standard mode).
    pub calling_number: Option<String>,
    /// `CALLING_NAME` IE. `None` ⇒ the IE is omitted (standard mode).
    pub calling_name: Option<String>,
    /// When `false`, the `CAPABILITY` IE is omitted entirely (web-transceiver).
    pub send_capability: bool,
    /// Codec negotiation policy: what we advertise in CAPABILITY and prefer
    /// in FORMAT (iax-31f7). Default `UlawOnly` keeps legacy wire behavior.
    pub codec_policy: CodecPolicy,
}

impl Default for CallProfile {
    /// Standard IAX2: no CALLING_* IEs, CAPABILITY advertised. (Hand-written
    /// because the derived `bool` default is `false`; `send_capability` must
    /// default to `true`.)
    fn default() -> Self {
        Self {
            calling_number: None,
            calling_name: None,
            send_capability: true,
            codec_policy: CodecPolicy::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_standard_shape() {
        let p = CallProfile::default();
        assert_eq!(p.calling_number, None);
        assert_eq!(p.calling_name, None);
        assert!(p.send_capability, "standard mode advertises CAPABILITY");
        assert_eq!(p.codec_policy, CodecPolicy::UlawOnly);
    }
}
