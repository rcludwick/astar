// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The D-Star vocoder choice (astar-dstar-perens): the AMBE dongle, or the
//! software vocoder.
//!
//! **D-Star only, by construction.** D-Star's AMBE 3600x2400 is out of patent,
//! so a software implementation may ship. DMR, NXDN and System Fusion use
//! AMBE+2, which still reads on US 8,359,197 until 2028-05-20, so they are
//! hardware-only and there is deliberately no type here that could carry a
//! choice for them. The opener enforces the same rule a second time
//! ([`crate::ambe::software_backend_permitted`]).
//!
//! Compiled into every build — the enum is how configuration names the
//! choice — whether or not the software vocoder itself
//! (`dstar-soft-ambe`) is.

/// Which vocoder a D-Star session runs its voice through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum DstarVocoder {
    /// The `ThumbDV` / `DVstick` dongle. The default, everywhere.
    #[default]
    Dongle,
    /// The software vocoder (Bruce Perens' AMBE 3600x2400). Usable only in a
    /// build with the `dstar-soft-ambe` feature; see
    /// [`software_dstar_vocoder_compiled`].
    Software,
}

impl DstarVocoder {
    /// A stable lowercase name: the `node.toml` value, the C-ABI string and
    /// the macOS preference value. Treat these as fixed.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dongle => "dongle",
            Self::Software => "software",
        }
    }

    /// Parse [`Self::as_str`]'s spelling, exactly. Anything else is `None`:
    /// a misspelt choice must be an error, not a silent dongle.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dongle" => Some(Self::Dongle),
            "software" => Some(Self::Software),
            _ => None,
        }
    }
}

/// `true` when this build contains the software D-Star vocoder (the
/// `dstar-soft-ambe` feature of `astar-codec`). On in every shipped build —
/// the macOS app and astar-server's release — and off in a plain
/// `cargo build`.
#[must_use]
pub const fn software_dstar_vocoder_compiled() -> bool {
    cfg!(feature = "dstar-soft-ambe")
}

/// What a D-Star connect says when the software vocoder was chosen but this
/// build does not contain it. Never a silent fallback to the dongle.
pub const SOFTWARE_DSTAR_VOCODER_NOT_COMPILED: &str = "The software D-Star vocoder isn't in this build of astar (the dstar-soft-ambe feature is off). Choose the dongle in Settings → Vocoders.";

/// What a D-Star connect with the dongle chosen and NOTHING trusted says, in
/// a build that has the software vocoder: the generic
/// [`NO_TRUSTED_VOCODER`](crate::vocoder_trust::NO_TRUSTED_VOCODER) sentence,
/// plus the other way to get D-Star going. It starts with the same words, so
/// a front-end that offers "Open Vocoder Settings…" on that sentence still
/// does. Never an automatic switch: the operator chooses.
pub const NO_TRUSTED_VOCODER_DSTAR: &str = "No trusted vocoder — add one in Settings → Vocoders, or switch D-Star to the software vocoder there";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dongle_is_the_default() {
        assert_eq!(DstarVocoder::default(), DstarVocoder::Dongle);
    }

    #[test]
    fn names_round_trip_and_nothing_else_parses() {
        for v in [DstarVocoder::Dongle, DstarVocoder::Software] {
            assert_eq!(DstarVocoder::parse(v.as_str()), Some(v));
        }
        for bad in [
            "", "Dongle", "SOFTWARE", "hardware", "auto", "thumbdv", " dongle",
        ] {
            assert_eq!(DstarVocoder::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_dstar_refusal_extends_the_generic_one() {
        assert!(NO_TRUSTED_VOCODER_DSTAR.starts_with(crate::vocoder_trust::NO_TRUSTED_VOCODER));
        assert!(NO_TRUSTED_VOCODER_DSTAR.contains("software vocoder"));
    }

    #[test]
    fn compiled_flag_follows_the_feature() {
        assert_eq!(
            software_dstar_vocoder_compiled(),
            cfg!(feature = "dstar-soft-ambe")
        );
    }
}
