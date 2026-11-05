// SPDX-License-Identifier: LGPL-3.0-or-later
//! The chip's pitch index map at its `RATET(27)` / "P25 FEC" setting (not TIA-102.BABA's linear Eq. 45/46).

use std::f64::consts::PI;

/// The real DVSI chip's pitch index is not Eq. 45/46's linear map: measured by feeding harmonic signals of known
/// period `P` (samples at 8 kHz) to the chip's encoder (`examples/ratet27_calibrate_pitch_map.rs`, 381 periods
/// from 23 to 118), its `b0` follows `b0 = 92.02*log2(P) - 390.99` with residual std 1.06 and max 3.3 index
/// steps, and reaches 255 (past the TIA maximum of 207). Log-scale pitch, ~92 steps per octave.
pub const CHIP_B0_STEPS_PER_OCTAVE: f64 = 92.0194;
/// See [`CHIP_B0_STEPS_PER_OCTAVE`]: `b0 = STEPS*log2(P) + OFFSET`.
pub const CHIP_B0_OFFSET: f64 = -390.9867;

/// Encoder side of the chip's pitch map: `b0` for period `p_samples`.
pub fn quantize_fundamental_frequency_chip(p_samples: f64) -> u32 {
    (CHIP_B0_STEPS_PER_OCTAVE * p_samples.log2() + CHIP_B0_OFFSET)
        .round()
        .clamp(0.0, 255.0) as u32
}

/// Decoder side of the chip's pitch map: `omega0` for a received `b0` (see [`CHIP_B0_STEPS_PER_OCTAVE`]).
pub fn dequantize_fundamental_frequency_chip(b0: u32) -> f64 {
    let p = 2f64.powf((b0 as f64 - CHIP_B0_OFFSET) / CHIP_B0_STEPS_PER_OCTAVE);
    2.0 * PI / p
}
