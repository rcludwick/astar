// SPDX-License-Identifier: LGPL-3.0-or-later
//! Fixed-point D-STAR encode side. The bit layout (`pack_raw_parameters`, `build_frame`, `pack_tone_parameters`,
//! `build_tone_frame`) is already pure integer in the float sibling (checked: no `f32`/`f64` anywhere in those four
//! functions or in `RawParameters`), so it is re-exported unchanged instead of duplicated. What this module adds is
//! the fixed-point speech quantizer's D-STAR table set.

use super::tables_q16::{
    DG_Q16_16, HOC_B5_Q16_16, HOC_B6_Q16_16, HOC_B7_Q16_16, HOC_B8_Q16_16, PRBA24_Q16_16,
    PRBA58_Q16_16,
};
use crate::ambe::fixed::general::explog::log2_q16_i64;
use crate::ambe::fixed::general::mbe_encode::ModeTables;
use crate::ambe::float::dstar::tables::{LMPRBL, VUV};

pub use crate::ambe::float::dstar::decode::RawParameters;
pub use crate::ambe::float::dstar::encode::{
    build_frame, build_tone_frame, pack_raw_parameters, pack_tone_parameters,
};

/// D-STAR's Q16.16 quantizer tables (`b8` even-only).
pub fn mode_tables() -> ModeTables<'static> {
    ModeTables {
        vuv: &VUV,
        dg_q16: &DG_Q16_16,
        prba24_q16: &PRBA24_Q16_16,
        prba58_q16: &PRBA58_Q16_16,
        lmprbl: &LMPRBL,
        hoc_q16: [
            &HOC_B5_Q16_16,
            &HOC_B6_Q16_16,
            &HOC_B7_Q16_16,
            &HOC_B8_Q16_16,
        ],
        hoc_b8_even_only: true,
        rho_q16: crate::ambe::fixed::general::mbe_speech::POINT_80_Q16_16,
        gamma_scale_q16: crate::ambe::fixed::general::mbe_speech::GAMMA_SCALE_2_Q16_16,
        gamma_memory_q16: crate::ambe::fixed::general::mbe_speech::GAMMA_MEMORY_ZERO_Q16_16,
    }
}

/// `2*pi * 2^32`, rounded.
pub const TWO_PI_Q32: i64 = 26_986_075_409;

/// `b0` for a pitch period of `p8 / 8` samples: the integer port of the float `quantize_pitch` (inverse of the chip's
/// pitch map `f0 = 2^(-4.258618 - 0.021766 * b0)`, rounded to nearest and clamped to `0..=125`). Since
/// `f0 = 8 / p8`, `b0 = (log2(p8) - 3 - 4.258618) / 0.021766`, evaluated with the table-interpolated `log2`.
pub fn quantize_pitch_p8(p8: u32) -> u32 {
    const OFFSET_Q16: i64 = (7_258_618i64 * 65536 + 500_000) / 1_000_000;
    const INV_STEP_Q16: i64 = (65536i64 * 1_000_000 + 10_883) / 21_766;
    let lg = log2_q16_i64((p8.max(1) as i64) << 16) as i64;
    let b0_q16 = ((lg - OFFSET_Q16) * INV_STEP_Q16) >> 16;
    ((b0_q16 + 32768) >> 16).clamp(0, 125) as u32
}

/// The table pitch of `b0` as the fixed decoder uses it: `(f0 in cycles/sample Q16.16, omega0 in radians/sample Q32)`.
pub fn table_pitch(b0: u32) -> (i32, i64) {
    (
        super::tables_q16::W0_TABLE_Q16_16[b0 as usize],
        super::tables_q16::W0_TABLE_Q32[b0 as usize],
    )
}
