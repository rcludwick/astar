// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Station-level codec negotiation policy (iax-31f7).
//!
//! Selects which codecs we advertise in CAPABILITY and in what order we
//! prefer them. `UlawOnly` (the default) reproduces pre-slin wire behavior
//! exactly; the other variants opt a station into slin (16-bit linear,
//! ~128 kbps) where bandwidth is cheap and companding noise matters.

use std::str::FromStr;

use super::CodecMask;
use crate::subclass::VoiceFormat;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CodecPolicy {
    /// G.711 only — wire behavior identical to pre-slin builds.
    #[default]
    UlawOnly,
    /// Advertise slin but keep µ-law preferred; slin is used only when the
    /// peer prefers it.
    AllowSlin,
    /// Advertise and prefer slin; fall back to µ-law.
    PreferSlin,
    /// Advertise and prefer slin16 wideband (16 kHz, ~256 kbps); fall back to slin,
    /// then µ-law. Requires a 16 kHz station pipeline — see `max_sample_rate`.
    PreferSlin16,
}

impl CodecPolicy {
    /// Codecs advertised in the CAPABILITY IE and accepted from peers.
    #[must_use]
    pub fn capability_mask(self) -> CodecMask {
        self.preference_order().iter().copied().collect()
    }

    /// Our codec preference, best first. The first entry is what FORMAT IEs
    /// name; the order breaks ties in codec selection.
    #[must_use]
    pub fn preference_order(self) -> &'static [VoiceFormat] {
        match self {
            Self::UlawOnly => &[VoiceFormat::G711U, VoiceFormat::G711A],
            Self::AllowSlin => &[VoiceFormat::G711U, VoiceFormat::G711A, VoiceFormat::Slin],
            Self::PreferSlin => &[VoiceFormat::Slin, VoiceFormat::G711U, VoiceFormat::G711A],
            Self::PreferSlin16 => &[
                VoiceFormat::Slin16,
                VoiceFormat::Slin,
                VoiceFormat::G711U,
                VoiceFormat::G711A,
            ],
        }
    }

    /// The single format we name in FORMAT IEs (top preference).
    #[must_use]
    pub fn preferred(self) -> VoiceFormat {
        self.preference_order()[0]
    }

    /// Whether this policy ASSERTS its own codec preference over a caller's
    /// stated FORMAT when the caller is capable of ours (iax-d0cc — Asterisk
    /// callee-preference semantics). `Prefer*` policies do: the point of
    /// "prefer slin/slin16" is to get wideband whenever the caller can,
    /// even if the caller stated a lesser preference. `UlawOnly`/`AllowSlin`
    /// defer to the caller's FORMAT — `AllowSlin`'s contract is explicitly
    /// "slin only when the peer prefers it".
    #[must_use]
    pub fn asserts_preference(self) -> bool {
        matches!(self, Self::PreferSlin | Self::PreferSlin16)
    }

    /// Highest audio sample rate this policy can negotiate. Pins the station
    /// pipeline rate (iax-4348): 16 kHz iff slin16 is offerable, else 8 kHz.
    #[must_use]
    pub fn max_sample_rate(self) -> u32 {
        self.preference_order()
            .iter()
            .filter_map(|f| f.sample_rate())
            .max()
            .unwrap_or(8000)
    }

    /// The policy actually usable on a station whose bus runs at `bus_rate`:
    /// wideband formats are dropped when the bus cannot carry them (upsampled
    /// narrowband in 16 kHz frames wastes bandwidth for zero quality).
    #[must_use]
    pub fn capped_to_rate(self, bus_rate: u32) -> Self {
        if self.max_sample_rate() > bus_rate {
            match self {
                Self::PreferSlin16 => Self::PreferSlin,
                other => other,
            }
        } else {
            self
        }
    }

    /// Whether the media path can encode/decode `f` at all, independent of
    /// policy. Guards against a peer `ACCEPT`ing a format we never offered.
    #[must_use]
    pub fn is_encodable(f: VoiceFormat) -> bool {
        matches!(
            f,
            VoiceFormat::G711U | VoiceFormat::G711A | VoiceFormat::Slin | VoiceFormat::Slin16
        )
    }
}

impl FromStr for CodecPolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ulaw_only" => Ok(Self::UlawOnly),
            "allow_slin" => Ok(Self::AllowSlin),
            "prefer_slin" => Ok(Self::PreferSlin),
            "prefer_slin16" => Ok(Self::PreferSlin16),
            other => Err(format!(
                "unknown codec_policy '{other}' (expected ulaw_only | allow_slin | prefer_slin | prefer_slin16)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subclass::VoiceFormat;

    #[test]
    fn default_is_ulaw_only_and_matches_legacy_wire_behavior() {
        let p = CodecPolicy::default();
        assert_eq!(p, CodecPolicy::UlawOnly);
        assert_eq!(p.preferred(), VoiceFormat::G711U);
        let mask = p.capability_mask();
        assert!(mask.contains(VoiceFormat::G711U));
        assert!(mask.contains(VoiceFormat::G711A));
        assert!(!mask.contains(VoiceFormat::Slin));
    }

    #[test]
    fn allow_slin_offers_but_does_not_prefer() {
        let p = CodecPolicy::AllowSlin;
        assert!(p.capability_mask().contains(VoiceFormat::Slin));
        assert_eq!(p.preferred(), VoiceFormat::G711U);
    }

    #[test]
    fn prefer_slin_offers_and_prefers() {
        let p = CodecPolicy::PreferSlin;
        assert!(p.capability_mask().contains(VoiceFormat::Slin));
        assert_eq!(p.preferred(), VoiceFormat::Slin);
        assert_eq!(
            p.preference_order(),
            &[VoiceFormat::Slin, VoiceFormat::G711U, VoiceFormat::G711A]
        );
    }

    #[test]
    fn from_str_parses_config_values() {
        assert_eq!("ulaw_only".parse(), Ok(CodecPolicy::UlawOnly));
        assert_eq!("allow_slin".parse(), Ok(CodecPolicy::AllowSlin));
        assert_eq!("prefer_slin".parse(), Ok(CodecPolicy::PreferSlin));
        assert!("opus".parse::<CodecPolicy>().is_err());
    }

    #[test]
    fn encodable_covers_exactly_the_implemented_codecs() {
        assert!(CodecPolicy::is_encodable(VoiceFormat::G711U));
        assert!(CodecPolicy::is_encodable(VoiceFormat::G711A));
        assert!(CodecPolicy::is_encodable(VoiceFormat::Slin));
        assert!(CodecPolicy::is_encodable(VoiceFormat::Slin16));
    }

    #[test]
    fn prefer_slin16_offers_wideband_and_prefers_it() {
        let p = CodecPolicy::PreferSlin16;
        assert_eq!(
            p.preference_order(),
            &[
                VoiceFormat::Slin16,
                VoiceFormat::Slin,
                VoiceFormat::G711U,
                VoiceFormat::G711A
            ]
        );
        assert!(p.capability_mask().contains(VoiceFormat::Slin16));
        assert_eq!(p.preferred(), VoiceFormat::Slin16);
        assert_eq!("prefer_slin16".parse(), Ok(CodecPolicy::PreferSlin16));
    }

    #[test]
    fn max_sample_rate_is_16k_only_for_wideband() {
        assert_eq!(CodecPolicy::UlawOnly.max_sample_rate(), 8000);
        assert_eq!(CodecPolicy::AllowSlin.max_sample_rate(), 8000);
        assert_eq!(CodecPolicy::PreferSlin.max_sample_rate(), 8000);
        assert_eq!(CodecPolicy::PreferSlin16.max_sample_rate(), 16000);
    }

    #[test]
    fn capped_to_rate_strips_wideband_on_narrow_stations() {
        assert_eq!(
            CodecPolicy::PreferSlin16.capped_to_rate(8000),
            CodecPolicy::PreferSlin
        );
        assert_eq!(
            CodecPolicy::PreferSlin16.capped_to_rate(16000),
            CodecPolicy::PreferSlin16
        );
        assert_eq!(
            CodecPolicy::PreferSlin.capped_to_rate(8000),
            CodecPolicy::PreferSlin
        );
        assert_eq!(
            CodecPolicy::UlawOnly.capped_to_rate(16000),
            CodecPolicy::UlawOnly
        );
    }

    #[test]
    fn slin16_is_encodable() {
        assert!(CodecPolicy::is_encodable(VoiceFormat::Slin16));
    }
}
