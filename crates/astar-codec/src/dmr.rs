// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! DMR voice: the AMBE+2 frames inside an MMDVM/homebrew burst, and the
//! AMBE-3000 rate word that configures a dongle to produce them.
//!
//! **DMR's vocoder is neither D-Star's nor YSF's**, and the difference is
//! one 17-byte control packet sent once, during init. `docs/design/
//! dmr-wire.md` §9 has the reading; the short version is
//! `DroidStar serialambe.cpp: SerialAMBE::config_ambe`:
//!
//! | protocol | RATEP constant | `packet_size` |
//! |---|---|---|
//! | DMR | `AMBE3000_2450_1150` | 9 |
//! | YSF, NXDN | `AMBE3000_2450_0000` | 7 |
//! | D-Star (AMBE-3000) | `AMBE2000_2400_1200` | 9 |
//!
//! DMR and D-Star share the channel width — 72 bits, nine bytes, the
//! hard-coded `0x48` in `ambe_thumbdv::channel_in` — and share nothing
//! else. Reusing [`crate::ambe::VocoderMode::Dstar`] for DMR would tell the
//! chip 2400 + 1200 and then hand it 2450 + 1150 frames, and the chip does
//! not complain: it returns confident noise. That is a working link that
//! sounds like static, which is the single most expensive way to be wrong
//! here, so DMR gets a mode of its own.
//!
//! The 72 bits are 2450 bit/s of voice plus 1150 bit/s of the chip's own
//! FEC, and they go into the burst unchanged:
//!
//! * **No re-FEC.** `MMDVMHost AMBEFEC.cpp: CAMBEFEC::regenerateDMR` is an
//!   *air-interface* repair, for bits that got hit over RF. Frames taken
//!   clean off a dongle and put in a UDP packet have nothing to regenerate.
//! * **No permutation.** `DroidStar` applies `dvsi_interleave` in `ysf.cpp`
//!   (and `nxdn.cpp`'s copy of the same table) and never in `dmr.cpp`,
//!   which `::memcpy`s the nine bytes into the burst and back out again.

/// Rate parameters for DMR: AMBE+2 at 2450 bit/s of voice plus 1150 bit/s of
/// FEC — 72 bits per 20 ms frame, FEC included, which is what goes into the
/// burst unchanged.
///
/// `DroidStar serialambe.cpp`'s `AMBE3000_2450_1150`, sent when
/// `m_protocol == "DMR"` with `packet_size = 9`. It shares the
/// `0x04 0x31 0x07 0x54` prefix with [`crate::ysf::ratep_dn`] — both are
/// 2450 bit/s of voice — and differs from it in the FEC word (`0x24 0x00`
/// where DN has none) and the trailing checksum. It shares nothing with
/// `ambe_thumbdv::packet::ratep_dstar`, whose 2400 + 1200 differs in five of
/// the six rate words.
#[must_use]
pub fn ratep_dmr() -> Vec<u8> {
    crate::ysf::dvsi_packet(
        0,
        &[
            0x0A, // RATEP field ID
            0x04, 0x31, // e
            0x07, 0x54, // u
            0x24, 0x00, // v -- 1150 bit/s of FEC; DN has 0x00 0x00 here
            0x00, 0x00, // w
            0x00, 0x00, // x
            0x6F, 0x48, // y
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `AMBE2000_2400_1200` from the same `serialambe.cpp` table — D-Star's
    /// word, spelled out here so the "not D-Star's" assertion holds without
    /// the `ambe-hw` feature. `ratep_dstar_really_is_that_word` below pins
    /// this literal against the vendored driver's own function.
    const RATEP_DSTAR: &str = "61 00 0D 00 0A 01 30 07 63 40 00 00 00 00 00 00 48";

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).expect("a hex byte"))
            .collect()
    }

    #[test]
    fn ratep_dmr_is_the_2450_plus_1150_word() {
        // DroidStar serialambe.cpp: AMBE3000_2450_1150, sent for
        // `m_protocol == "DMR"` with packet_size = 9.
        assert_eq!(
            ratep_dmr(),
            hex("61 00 0D 00 0A 04 31 07 54 24 00 00 00 00 00 6F 48")
        );
    }

    #[test]
    fn ratep_dmr_is_not_ratep_dstar() {
        // The whole ruling in one assertion: same 72-bit channel width, a
        // different rate word, and a dongle told the wrong one returns
        // confident noise rather than an error.
        assert_ne!(ratep_dmr(), hex(RATEP_DSTAR));
        assert_ne!(ratep_dmr(), crate::ysf::ratep_dn());
    }

    /// The vendored driver is only compiled under `ambe-hw`, so the
    /// assertion above compares against a literal. This is the test that
    /// keeps the literal honest.
    #[cfg(feature = "ambe-hw")]
    #[test]
    fn ratep_dstar_really_is_that_word() {
        assert_eq!(ambe_thumbdv::packet::ratep_dstar(), hex(RATEP_DSTAR));
    }
}
