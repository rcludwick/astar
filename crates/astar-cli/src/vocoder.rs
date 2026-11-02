// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The vocoder trust list for the `*-listen` commands.
//!
//! These tools have no config file, so their trust list comes from
//! `ASTAR_VOCODER_SERIALS` (comma-separated USB serial numbers of enrolled
//! sticks). Unset means nothing is trusted and no AMBE port is opened.

use astar_station::{StationConfig, VocoderTrust};

/// A [`StationConfig`] whose vocoder trust comes from `ASTAR_VOCODER_SERIALS`.
pub fn listener_config() -> StationConfig {
    config_with(VocoderTrust::from_env())
}

fn config_with(trust: VocoderTrust) -> StationConfig {
    StationConfig {
        vocoder_trust: trust,
        ..StationConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_listener_config_carries_the_trust_list() {
        let c = config_with(VocoderTrust::parse_list("TESTSER1,TESTSER2"));
        assert_eq!(c.vocoder_trust.serials, vec!["TESTSER1", "TESTSER2"]);
        assert!(config_with(VocoderTrust::none()).vocoder_trust.is_empty());
    }
}
