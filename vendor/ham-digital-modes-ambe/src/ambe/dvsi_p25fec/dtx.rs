// SPDX-License-Identifier: LGPL-3.0-or-later
//! The real DVSI chip's `DTX_ENABLE` (Discontinuous Transmission / Voice Activity Detection)
//! silence-classification behavior within RATET(27), determined by feeding pure digital silence and
//! loud voiced content to the live chip with `ECMODE_IN`'s `DTX_ENABLE` bit forced on -- see
//! `AMBE_CHIP_VALIDATION_FINDINGS.md` sections 26, 28, and 31 for the full experimental record.
//!
//! **Corrected from an earlier version of this module, which is worth recording rather than
//! silently fixing**: an initial single-frame check found `g0`, `g2`, and `c7` all reading clean
//! constants for confirmed DTX-silence, and this module originally required all three to match. A
//! fresh, more careful 10-frame live check (`examples/ambe_chip_validate_dvsi_p25fec/dtx.rs`) found
//! `g2` and `c7` are *not* reliably constant during confirmed silence (`g2` took 7 different values,
//! `c7` took 3, across just 10 frames) -- only `g0` is: exactly `3841` in 10/10 fresh silence
//! frames and exactly `1597` in 10/10 fresh loud-tone frames, zero overlap. **`g0` alone is the
//! real, robust discriminator; `g2`/`c7` were not.**
//!
//! This module implements only [`is_dtx_silence_frame`], a direct classifier for *genuine,
//! near-zero-noise silence* specifically -- it is deliberately narrower than "the chip currently
//! considers this frame inactive/non-speech" (that broader classification is the chip's own
//! `VOICE_ACTIVE` status flag, exposed only via `PKT_CHANFMT`'s `ECMODE_OUT` field, not decodable
//! from the ordinary 144 wire bits alone).
//!
//! **Ground-truth validation and the two open questions above were both resolved together** by
//! `examples/p25_ratet27_dtx_ground_truth_and_adaptive_check.rs`, which reads `g0` and the chip's own
//! `VOICE_ACTIVE` flag on the same frame:
//! - Across a noise-peak sweep from 0 through 50 (all confirmed `VOICE_ACTIVE=0`), `g0` read exactly
//!   `3841` on every single frame -- full agreement, real ground truth rather than stimulus
//!   inference.
//! - At peak 75 and peak 100 -- still `VOICE_ACTIVE=0` (below the roughly-50-to-75 activation
//!   threshold located in section 28/33) but noticeably noisier than near-silence -- `g0` read
//!   `3844`-`3845` and `3856`-`3857` respectively, rising smoothly with the actual noise level while
//!   still well above the (much lower) values seen once `VOICE_ACTIVE` flips to `1`. **This is a real, positive
//!   confirmation of DVSI's own "background noise level" manual claim**, previously tested and found
//!   inconclusive in section 26 -- `g0` genuinely does encode a continuous noise-floor reading when
//!   the chip judges the frame inactive, it just doesn't hold exactly `3841` outside of true silence.
//!   [`is_dtx_silence_frame`] therefore only catches the near-zero-noise case correctly; it is not a
//!   general "is this frame inactive" classifier, and was never claimed to decode `VOICE_ACTIVE`
//!   itself from wire bits alone (which isn't possible without `ECMODE_OUT`).
//! - The same abrupt-switch protocol section 32 used to find `VOICE_ACTIVE` adaptive also moved `g0`
//!   in lock-step: after settling at peak 100 (`g0=3857`, `VOICE_ACTIVE=0`), the frame immediately
//!   after an abrupt switch to a loud, unrelated tone flipped both `VOICE_ACTIVE` to `1` and `g0` to
//!   a much lower value within a single frame -- no separate lag between the two.
//! - **The history-dependence question is now answered, not just untested**: 600 consecutive frames
//!   of sustained peak-100 noise never once produced `g0=3841` -- the elevated noise-floor reading is
//!   stable under sustained exposure, not a slow drift back toward the true-silence constant. So
//!   while `VOICE_ACTIVE` is confirmed adaptive/contrast-based (section 32), `g0`'s own noise-floor
//!   reading in this test was not -- it tracked genuine noise level consistently, and
//!   [`DTX_SILENCE_G0`]'s exact-match behavior is safe to rely on for real near-silence without
//!   worrying that a merely-quiet-but-sustained signal will eventually also read as `3841`.
//!
//! **Do not broaden [`is_dtx_silence_frame`] to a range check (e.g. `g0 >= DTX_SILENCE_G0`) without
//! re-reading `AMBE_CHIP_VALIDATION_FINDINGS.md` section 35's own verification first** -- that
//! broadening was considered, checked exhaustively against every other committed capture dataset via
//! `examples/ratet27_verify_dtx_g0_threshold.rs`, and falsified for three separate reasons: DTMF and
//! forced-tone frames use an unrelated `g0` encoding that overlaps this exact range by construction
//! (section 25), a full-amplitude 60Hz tone reads `g0=3945` despite being genuinely active content,
//! and several un-converged pure sine tones intermittently spike into this range mid-stream. The
//! exact-match-only scope here is deliberate, not an oversight to fix later.

/// `g0`'s confirmed, robust constant value for a genuine DTX-silence frame -- verified stable
/// across 10 fresh live frames with zero exceptions, and zero overlap with voiced-frame values.
pub const DTX_SILENCE_G0: u16 = 3841;

/// Classifies a decoded RATET(27) frame (with `DTX_ENABLE` on) as a genuine DTX-silence frame by
/// matching [`DTX_SILENCE_G0`] exactly. `g2`/`c7` are deliberately not checked -- an earlier version
/// of this classifier required them too, but they were found unreliable during confirmed silence
/// (see this module's own doc comment); `g0` alone is the field actually shown robust.
pub fn is_dtx_silence_frame(g0: u16) -> bool {
    g0 == DTX_SILENCE_G0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_dtx_silence_frame_matches_the_confirmed_constant() {
        assert!(is_dtx_silence_frame(DTX_SILENCE_G0));
    }

    #[test]
    fn is_dtx_silence_frame_rejects_other_values() {
        assert!(!is_dtx_silence_frame(DTX_SILENCE_G0 + 1));
        assert!(!is_dtx_silence_frame(0));
    }

    /// Real chip-captured `g0` values from a fresh 10-frame live check
    /// (`examples/ambe_chip_validate_dvsi_p25fec/dtx.rs`): exactly `3841` for every one of 10 silence
    /// frames, exactly `1597` for every one of 10 loud-tone frames, zero exceptions either way.
    #[test]
    fn matches_every_real_captured_silence_frame_and_rejects_every_voiced_one() {
        for _ in 0..10 {
            assert!(is_dtx_silence_frame(3841));
        }
        for _ in 0..10 {
            assert!(!is_dtx_silence_frame(1597));
        }
    }
}
