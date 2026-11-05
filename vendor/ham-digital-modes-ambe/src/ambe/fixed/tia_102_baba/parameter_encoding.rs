// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point fundamental-frequency dequantization and voicing-decision decoding -- the decoder's
//! own first stage (`tia_102_baba::decode`'s step 6), matching
//! [`crate::ambe::float::tia_102_baba::parameter_encoding`]'s own real-numbered functions bit for bit
//! where the decode path only ever needs a discrete lookup (see [`b0_table`]'s own doc comment for
//! why a table, not a general fixed-point division, is exact here rather than an approximation).
//!
//! Voicing-decision decoding (`decode_voicing_decisions`/`decode_voicing_decisions_per_harmonic`) and
//! `K~`'s own formula (`frequency_bands_count`) are already pure integer arithmetic in the
//! floating-point sibling -- reused directly via `pub use` rather than duplicated, the same
//! reasoning [`super::super::general`] applies to the FEC layer.

mod b0_table;

use b0_table::{B0_COUNT, L_HAT_FROM_B0, OMEGA0_TILDE_Q16_16, OMEGA0_TILDE_Q32};

pub use crate::ambe::float::tia_102_baba::parameter_encoding::{
    decode_voicing_decisions, decode_voicing_decisions_per_harmonic, encode_voicing_decisions,
};
pub use crate::ambe::float::tia_102_baba::vuv::frequency_bands_count;

/// `omega0_tilde` (Eq. 46) in Q16.16, for a received `b_hat_0` -- an exact table lookup (not an
/// approximation) since `b_hat_0` only has [`B0_COUNT`] possible values. Clamps to the nearest valid
/// entry rather than panicking on an out-of-spec `b_hat_0` (the spec's own stated range is
/// `0..=207`; a corrupted frame could in principle carry a larger 8-bit value up to 255), matching
/// this crate's own established "never panic on any input" convention.
pub fn dequantize_fundamental_frequency_q16(b0_tilde: u32) -> i32 {
    OMEGA0_TILDE_Q16_16[(b0_tilde as usize).min(B0_COUNT - 1)]
}

/// `omega0_tilde` (Eq. 46) in Q32 radians/sample (`2^32` per radian): the same table lookup at the
/// wider precision synthesis needs (a harmonic's phase is `l * omega0 * n`, so Q16.16's 1.5e-5 rad
/// resolution is amplified by the harmonic number and sample index).
pub fn dequantize_fundamental_frequency_q32(b0_tilde: u32) -> i64 {
    OMEGA0_TILDE_Q32[(b0_tilde as usize).min(B0_COUNT - 1)]
}

/// `L~` (Eq. 47) for a received `b_hat_0` -- likewise an exact table lookup. Takes `b_hat_0`
/// directly rather than a dequantized `omega0_tilde`, unlike the floating-point sibling's
/// `harmonics_count`, since every real decoder call site only ever has `b_hat_0` and an
/// already-dequantized `omega0_tilde` derived from it -- see [`b0_table`]'s own doc comment for why
/// this is exact for the decode path specifically, not a general-purpose replacement for
/// `harmonics_count` (which the *encoder*'s pitch-estimation path still needs at an arbitrary,
/// continuous frequency, not yet ported).
pub fn harmonics_count_from_b0(b0_tilde: u32) -> u32 {
    L_HAT_FROM_B0[(b0_tilde as usize).min(B0_COUNT - 1)]
}
