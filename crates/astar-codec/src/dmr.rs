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
//!
//! So the voice lift below is a rearrangement and nothing more. Where the
//! three frames sit inside the 33 bytes is [`astar_dmr::frame`]'s answer —
//! the nibble-aligned 108/48/108 split of `docs/design/dmr-wire.md` §5, with
//! frame 2 straddling the middle field — and this module only tags what comes
//! back as `ChannelFrame::Dmr` so a caller cannot hand DMR frames to a chip
//! configured for D-Star.
//!
//! **Nothing here was copied.** The layout was read out of the deployed
//! reference implementations as a specification of the wire, the same way the
//! rest of this crate's protocol code was, and every constant carries the file
//! and function it came from; `docs/design/dmr-wire.md` records the fetch and
//! how the references' disagreements were settled.

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

/// AMBE+2 frames one DMR burst carries: three 20 ms frames, so 60 ms of
/// audio in the burst that fills one 30 ms TDMA timeslot.
///
/// One number, one home — [`astar_dmr::frame::AMBE_FRAMES`] is where the
/// burst layout counts them, and this is that count under the name the
/// vocoder side reads it by.
pub const FRAMES_PER_BURST: usize = astar_dmr::frame::AMBE_FRAMES;

/// Lift the three 20 ms AMBE+2 frames out of one 33-byte burst.
///
/// Infallible by construction: every burst has 216 information bits and this
/// only rearranges them. Whether they are *voice* is the `DMRD` bits byte's
/// answer, not this function's — which is why a signalling burst has no error
/// case here and is refused one layer up.
///
/// Gated with [`crate::ambe`] because [`crate::ambe::ChannelFrame`] is: the
/// tag exists to stop DMR frames reaching a chip configured for D-Star, and
/// with no chip compiled in there is no chip to protect. The layout itself is
/// under test on every run, in `astar-dmr`'s `frame` module and in
/// [`pack_voice`] below.
#[cfg(feature = "ambe-hw")]
#[must_use]
pub fn unpack_voice(
    burst: &[u8; astar_dmr::frame::BURST_LEN],
) -> [crate::ambe::ChannelFrame; FRAMES_PER_BURST] {
    astar_dmr::frame::ambe(burst).map(crate::ambe::ChannelFrame::Dmr)
}

/// The inverse. Public because the session tests build bursts with it; the
/// signalling builders are Task 12.
///
/// Writes only the two information halves, so a burst that already carries a
/// sync pattern keeps it — `astar_dmr::frame::write_ambe` is nibble-exact
/// about the two bytes the halves and the middle field share. Not gated on
/// `ambe-hw`: nine bytes in and 33 out needs no dongle, and a session test
/// that has to build a burst runs under `just ci`.
#[must_use]
pub fn pack_voice(frames: &[[u8; 9]; FRAMES_PER_BURST]) -> [u8; astar_dmr::frame::BURST_LEN] {
    let mut burst = [0u8; astar_dmr::frame::BURST_LEN];
    astar_dmr::frame::write_ambe(&mut burst, frames);
    burst
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

    #[test]
    fn a_packed_burst_puts_the_frames_where_the_burst_layout_says() {
        // The ungated half of the same claim: pack_voice is write_ambe and
        // nothing else, so astar-dmr finds the frames exactly where it put
        // them, and the 27 bytes land in the two information halves rather
        // than over the middle field.
        let frames: [[u8; 9]; FRAMES_PER_BURST] = [
            [0xA5, 0x3C, 0x71, 0x0E, 0xC3, 0x96, 0x5A, 0x18, 0x81],
            [0x5A, 0xC3, 0x8E, 0xF1, 0x3C, 0x69, 0xA5, 0xE7, 0x7E],
            [0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00],
        ];
        let mut burst = pack_voice(&frames);
        assert_eq!(astar_dmr::frame::ambe(&burst), frames);
        astar_dmr::frame::write_sync(&mut burst, astar_dmr::frame::Sync::MsAudio);
        assert_eq!(astar_dmr::frame::ambe(&burst), frames);
    }

    #[cfg(feature = "ambe-hw")]
    #[test]
    fn three_channel_frames_come_out_of_a_burst_in_wire_order() {
        let frames: [[u8; 9]; FRAMES_PER_BURST] =
            core::array::from_fn(|i| [u8::try_from(i + 1).expect("small"); 9]);
        let burst = pack_voice(&frames);
        assert_eq!(
            unpack_voice(&burst),
            [
                crate::ambe::ChannelFrame::Dmr(frames[0]),
                crate::ambe::ChannelFrame::Dmr(frames[1]),
                crate::ambe::ChannelFrame::Dmr(frames[2]),
            ]
        );
    }

    #[cfg(feature = "ambe-hw")]
    #[test]
    fn packing_voice_leaves_the_sync_field_alone() {
        // pack_voice writes only the two information halves. A burst built by
        // the session is sync-then-voice, and voice must not undo it.
        let frames: [[u8; 9]; FRAMES_PER_BURST] = [[0xFF; 9]; 3];
        let mut burst = pack_voice(&frames);
        astar_dmr::frame::write_sync(&mut burst, astar_dmr::frame::Sync::MsAudio);
        assert_eq!(
            astar_dmr::frame::sync_of(&burst),
            astar_dmr::frame::Sync::MsAudio
        );
        assert_eq!(
            unpack_voice(&burst)[0],
            crate::ambe::ChannelFrame::Dmr([0xFF; 9])
        );
    }

    #[cfg(feature = "ambe-hw")]
    #[test]
    fn the_frames_are_nine_bytes_each_and_the_chip_gets_them_unchanged() {
        // DroidStar dmr.cpp copies the chip's nine bytes into the burst with
        // memcpy and reads them back the same way; the dvsi_interleave that
        // ysf.cpp applies is never applied here. Nothing in this function
        // permutes anything, and that is the assertion.
        let frames: [[u8; 9]; FRAMES_PER_BURST] = [
            [0xA5, 0x3C, 0x71, 0x0E, 0xC3, 0x96, 0x5A, 0x18, 0x81],
            [0x5A, 0xC3, 0x8E, 0xF1, 0x3C, 0x69, 0xA5, 0xE7, 0x7E],
            [0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF, 0x00],
        ];
        let burst = pack_voice(&frames);
        for (i, got) in unpack_voice(&burst).iter().enumerate() {
            assert_eq!(got.as_dmr(), Some(frames[i]));
        }
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
