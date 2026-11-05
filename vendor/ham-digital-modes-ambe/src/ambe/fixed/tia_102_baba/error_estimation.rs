// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point port of `ambe::float::tia_102_baba::error_estimation` (Eq. 95-98) -- a simple linear
//! recursion plus two threshold comparisons, needing only `mul_q16`.

use crate::ambe::fixed::general::fixed_ops::mul_q16;

/// `round(0.95 * 65536)`.
const POINT_95_Q16_16: i32 = 62259;
/// `round(0.000365 * 65536)`.
const POINT_000365_Q16_16: i32 = 24;
/// `round(0.0875 * 65536)`.
const POINT_0875_Q16_16: i32 = 5734;

/// The fixed-point equivalent of `FrameErrors` -- `total`/`golay_init`/`hamming_init` are already
/// plain integer counts in the float sibling, unchanged here; only `rate` becomes Q16.16.
pub struct FrameErrorsQ16 {
    pub total: u32,
    pub rate_q16: i32,
    pub golay_init: u32,
    pub hamming_init: u32,
}

/// The fixed-point equivalent of `estimate_errors` (Eq. 95-96).
pub fn estimate_errors_q16(
    corrected_error_counts: &[u32; 7],
    previous_rate_q16: i32,
) -> FrameErrorsQ16 {
    let total: u32 = corrected_error_counts.iter().sum();
    let rate_q16 = mul_q16(POINT_95_Q16_16, previous_rate_q16)
        + mul_q16(POINT_000365_Q16_16, (total as i32) << 16);
    FrameErrorsQ16 {
        total,
        rate_q16,
        golay_init: corrected_error_counts[0],
        hamming_init: corrected_error_counts[4],
    }
}

/// The fixed-point equivalent of `should_repeat_frame` (Eq. 97-98).
pub fn should_repeat_frame_q16(errors: &FrameErrorsQ16) -> bool {
    let total_q16 = (errors.total as i64) << 16;
    let threshold_q16 = (10i64 << 16) + 40 * (errors.rate_q16 as i64);
    errors.golay_init >= 2 && total_q16 >= threshold_q16
}

/// The fixed-point equivalent of `should_mute_frame` (section 7.8).
pub fn should_mute_frame_q16(errors: &FrameErrorsQ16) -> bool {
    errors.rate_q16 > POINT_0875_Q16_16
}
