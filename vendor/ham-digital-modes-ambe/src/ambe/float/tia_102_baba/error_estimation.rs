//! Error estimation, frame repeat, and frame muting (TIA-102.BABA_2003.pdf sections 7.6-7.8,
//! Eq. 95-98) -- the decoder-side logic that tracks how many bit errors the Golay/Hamming decoders
//! ([`super::fec::golay_decode`]/[`super::fec::hamming_decode`]) actually corrected each frame, and
//! decides whether the frame is trustworthy enough to use.
//!
//! Transcribed from a 600 DPI render of TIA-102.BABA_2003.pdf pages 61-63 (self-numbered pages
//! 45-47), following the same discipline as every other equation in this spec. Every numeric
//! constant below (`0.95`, `0.000365`, `2`, `10`, `40`, `.0875`) was independently cross-checked
//! against `kchmck/imbe.rs`'s own MIT-licensed `enhance.rs` (`EnhanceErrors::new`,
//! `should_repeat`, `should_mute`) -- an unrelated, working, third-party IMBE decoder -- and matches
//! exactly, in addition to the usual 600 DPI re-render check. Structure/naming below still follows
//! this codebase's own established conventions rather than that crate's, since the cross-check is
//! for correctness, not for style.
//!
//! `u_hat_7` carries no FEC (see `fec.rs`'s own doc comment), so only `u_0..u_6`'s own seven
//! Golay/Hamming decodes ever contribute an error count here -- there is no `epsilon_7`.

/// The per-frame error-tracking state (Eq. 95-96): `total` is `epsilon_T`, this frame's own total
/// corrected-bit-error count across all seven FEC-protected code vectors; `rate` is `epsilon_R(0)`,
/// a slow-moving frame-to-frame average of that count (Eq. 41-style exponential update, unrelated
/// to `vuv::update_xi_max` despite the similar shape -- a coincidence of two different EWMA trackers
/// in this spec, not shared logic); `golay_init` is `epsilon_0`, the error count for `u_hat_0`
/// specifically (the one code vector bit modulation never touches, per `modulation.rs`'s own doc
/// comment, so its own error count is a meaningful signal on its own, not just folded into the
/// total); `hamming_init` is `epsilon_4`, the error count for `u_hat_4` (the first Hamming-coded
/// vector) -- used by `super::enhancement`'s own Eq. 112 threshold, not by anything in this module.
pub struct FrameErrors {
    pub total: u32,
    pub rate: f64,
    pub golay_init: u32,
    pub hamming_init: u32,
}

/// Computes this frame's own [`FrameErrors`] (Eq. 95-96) from the seven corrected-error counts
/// `epsilon_0..epsilon_6` (`fec::golay_decode`/`fec::hamming_decode`'s own second return value, one
/// per FEC-protected code vector `u_hat_0..u_hat_6`, in order) and the previous frame's own
/// `epsilon_R(-1)`.
// [@ANCHOR: estimate_errors]
pub fn estimate_errors(corrected_error_counts: &[u32; 7], previous_rate: f64) -> FrameErrors {
    let total: u32 = corrected_error_counts.iter().sum();
    let rate = 0.95 * previous_rate + 0.000365 * total as f64;
    FrameErrors {
        total,
        rate,
        golay_init: corrected_error_counts[0],
        hamming_init: corrected_error_counts[4],
    }
}

/// Whether the current frame should be discarded and the previous frame's own model parameters
/// repeated instead (Eq. 97-98, section 7.7): both `epsilon_0 >= 2` and `epsilon_T >= 10 +
/// 40*epsilon_R` must hold. The spec's own stated intent: these two conditions together detect the
/// "incorrect bit demodulation which results if there are uncorrectable bit errors in `c_hat_0`" --
/// `u_hat_0`'s own Golay code can only reliably correct up to 3 errors, so a high `epsilon_0`
/// specifically (not just a high total) is real evidence the demodulation seed itself
/// (`modulation.rs`'s own `u_hat_0`-seeded pseudo-random sequence) was decoded wrong, which would
/// desynchronize every other code vector's own demodulation too.
pub fn should_repeat_frame(errors: &FrameErrors) -> bool {
    errors.golay_init >= 2 && errors.total as f64 >= 10.0 + 40.0 * errors.rate
}

/// Whether the current frame's own severity calls for muting to comfort noise entirely rather than
/// a frame repeat (section 7.8): `epsilon_R > .0875`. The spec's own stated intent: repeats are a
/// reasonable substitute for one or a few bad frames, but a persistently high error *rate* means
/// "reliable communication cannot be supported," so the decoder gives up on synthesizing real speech
/// and squelches to noise instead (this module does not itself generate that noise -- see section
/// 11's own eventual synthesis work for that; this function only makes the yes/no call).
pub fn should_mute_frame(errors: &FrameErrors) -> bool {
    errors.rate > 0.0875
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Tests [@ANCHOR: estimate_errors]
    fn estimate_errors_matches_eq95_96_at_a_hand_computed_value() {
        // Cross-checked against kchmck/imbe.rs's own test_errors: EnhanceErrors::new(&[1,2,3,4,5,6,7],
        // 0.5) -> total=28, rate ~ 0.48522, golay_init=1, hamming_init=5.
        let counts = [1u32, 2, 3, 4, 5, 6, 7];
        let errors = estimate_errors(&counts, 0.5);
        assert_eq!(errors.total, 28);
        assert!(
            (errors.rate - 0.485_22).abs() < 1e-5,
            "expected rate ~0.48522, got {}",
            errors.rate
        );
        assert_eq!(errors.golay_init, 1);
        assert_eq!(errors.hamming_init, 5);
    }

    #[test]
    fn estimate_errors_of_all_zero_counts_and_zero_previous_rate_is_all_zero() {
        let errors = estimate_errors(&[0u32; 7], 0.0);
        assert_eq!(errors.total, 0);
        assert_eq!(errors.rate, 0.0);
        assert_eq!(errors.golay_init, 0);
        assert_eq!(errors.hamming_init, 0);
    }

    #[test]
    fn should_repeat_frame_requires_both_eq97_and_eq98() {
        // golay_init < 2: never repeats, no matter how high the total.
        let errors = FrameErrors {
            total: 1000,
            rate: 10.0,
            golay_init: 1,
            hamming_init: 0,
        };
        assert!(!should_repeat_frame(&errors));

        // golay_init >= 2 but the total is below threshold: doesn't repeat.
        let errors = FrameErrors {
            total: 5,
            rate: 0.0,
            golay_init: 2,
            hamming_init: 0,
        };
        assert!(!should_repeat_frame(&errors));

        // Both conditions true: repeats. total=10 >= 10 + 40*0 = 10 (boundary, inclusive).
        let errors = FrameErrors {
            total: 10,
            rate: 0.0,
            golay_init: 2,
            hamming_init: 0,
        };
        assert!(should_repeat_frame(&errors));
    }

    #[test]
    fn should_mute_frame_matches_the_075_threshold_strictly() {
        assert!(!should_mute_frame(&FrameErrors {
            total: 0,
            rate: 0.0875,
            golay_init: 0,
            hamming_init: 0,
        }));
        assert!(should_mute_frame(&FrameErrors {
            total: 0,
            rate: 0.0876,
            golay_init: 0,
            hamming_init: 0,
        }));
    }
}
