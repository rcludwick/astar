// SPDX-License-Identifier: LGPL-3.0-or-later
//! General Q16.16 arithmetic helpers -- multiplication, division, square root, and a couple of
//! constants -- needed throughout the MBE dequantize/synthesis chain (`super::mbe_speech`) on top of
//! the transcendental primitives in [`super::trig`]/[`super::explog`].

use super::isqrt::isqrt_u64;

/// `round(pi * 65536)`. `TIA-102.BABA_2003.pdf`'s own equations and mbelib's real dequantize code
/// both use `pi`/`2*pi` directly as plain scalars (not as a turn/phase), so this is kept as an
/// ordinary Q16.16 constant rather than folded into [`super::trig`]'s phase convention.
pub const PI_Q16_16: i32 = 205887;
/// `round(2 * pi * 65536)` -- computed directly rather than `2 * PI_Q16_16`, so this constant isn't
/// carrying `PI_Q16_16`'s own rounding error doubled.
pub const TWO_PI_Q16_16: i32 = 411775;

/// `a * b` for two Q16.16 values, rounding to nearest rather than truncating (adds half an LSB
/// before the final shift, the same idiom [`super::explog::exp2_q16`] uses).
pub fn mul_q16(a: i32, b: i32) -> i32 {
    let product = (a as i64) * (b as i64);
    let rounded = if product >= 0 {
        product + (1 << 15)
    } else {
        product - (1 << 15)
    };
    (rounded >> 16) as i32
}

/// `a / b` for two Q16.16 values, returning a Q16.16 result. Saturates to `i32::MAX`/`i32::MIN`
/// (matching the sign of the true result) on division by zero, rather than panicking -- this crate's
/// own established "never panic" convention, extended to a case `f64` would represent as `inf`/`-inf`.
pub fn div_q16(a: i32, b: i32) -> i32 {
    if b == 0 {
        return if a >= 0 { i32::MAX } else { i32::MIN };
    }
    let numerator = (a as i64) << 16;
    let denominator = b as i64;
    let quotient = numerator / denominator;
    quotient.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// `a * b` for an `i64` value `a` with 16 fractional bits (`a_real = a / 65536`, the same "wide
/// container, same convention" idea [`super::explog::log2_q16_i64`] documents) and an ordinary Q16.16
/// `i32` scalar `b`, returning the product with the same 16-fractional-bit `i64` convention. Uses an
/// `i128` intermediate rather than `i64` -- `a` can be large enough (TIA-102.BABA enhancement's own
/// `R_M0`/`S_E`) that `a * b` alone can exceed `i64::MAX` before the final shift, and an `i128`
/// product costs nothing extra here (it is not a floating-point type; this crate's own "no floating
/// point whatsoever" rule is about `f32`/`f64`, not integer width).
pub fn mul_q16_i64(a: i64, b_q16: i32) -> i64 {
    let product = (a as i128) * (b_q16 as i128);
    let rounded = if product >= 0 {
        product + (1i128 << 15)
    } else {
        product - (1i128 << 15)
    };
    (rounded >> 16) as i64
}

/// `a / b` for two `i64` values sharing the same 16-fractional-bit convention [`mul_q16_i64`]/
/// [`super::explog::log2_q16_i64`] use, returning a Q16.16 `i32` ratio -- for computing a
/// dimensionless ratio (TIA-102.BABA enhancement's own `k = R_M1/R_M0`, bounded to `[-1,1]` by
/// Cauchy-Schwarz, or its final `gamma = sqrt(R_M0/E_enh)` rescale) from two values whose own
/// individual magnitudes may not fit an `i32`, even though their *ratio* always will. Normalizes
/// both operands by the same right-shift first so `numerator << 16` cannot overflow `i64` regardless
/// of `a`/`b`'s own magnitude, then divides exactly as [`div_q16`] does. Saturates to `i32::MAX`/
/// `i32::MIN` on division by zero (before or after normalizing), matching [`div_q16`]'s own
/// never-panic convention.
pub fn div_q16_i64(a: i64, b: i64) -> i32 {
    if b == 0 {
        return if a >= 0 { i32::MAX } else { i32::MIN };
    }
    let max_abs = a.unsigned_abs().max(b.unsigned_abs());
    let bits = if max_abs == 0 {
        0
    } else {
        64 - max_abs.leading_zeros() as i32
    };
    let shift = (bits - 47).max(0);
    let a_s = a >> shift;
    let b_s = b >> shift;
    if b_s == 0 {
        return if a_s >= 0 { i32::MAX } else { i32::MIN };
    }
    let numerator = a_s << 16;
    let quotient = numerator / b_s;
    quotient.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// `sqrt(x)` for a non-negative Q16.16 `x`, returning a Q16.16 result -- exact to the nearest
/// representable Q16.16 value (built directly on [`isqrt_u64`]'s own exact integer result, not an
/// iterative approximation). Returns `0` for a negative `x` (matching this crate's own "never panic"
/// convention; `f64::sqrt` of a negative number is `NaN`, which has no fixed-point representation).
///
/// Derivation: `sqrt(x_real) = sqrt(x_q16 / 65536) = sqrt(x_q16) / 256`. To get that result *as*
/// Q16.16 (i.e. multiplied back up by 65536): `sqrt(x_q16)/256 * 65536 = sqrt(x_q16) * 256 =
/// sqrt(x_q16 * 256^2) = sqrt(x_q16 * 65536)` -- so a single `isqrt_u64` call on `x_q16 * 65536`
/// gives the Q16.16 result directly.
pub fn sqrt_q16(x: i32) -> i32 {
    if x < 0 {
        return 0;
    }
    isqrt_u64((x as u64) * 65536) as i32
}

/// `sqrt(x)` for a non-negative, wide (`i64`) Q16.16 `x` whose own real magnitude may be far outside
/// a plain `i32` Q16.16's `~32767` ceiling (e.g. `unvoiced_synthesis`'s own windowed-noise-DFT power,
/// which can reach the hundreds of thousands in real terms) -- and whose *square root*, unlike
/// [`sqrt_q16`]'s own typical caller, may **also** be too wide for a plain `i32` Q16.16 result. Returns
/// an `i64` Q16.16 result on the same wide convention `mul_q16_i64`/`div_q16_i64` use, `0` for a
/// negative `x`.
///
/// **Why this exists alongside [`sqrt_q16`], not as a drop-in replacement**: the natural derivation
/// (`sqrt_q16`'s own: `result_q16 = isqrt(x_q16 * 65536)`) needs `x_q16 * 65536` computed exactly
/// first -- for a wide `x_q16` (say `~4.7e16`, a real `unvoiced_synthesis` value), that product
/// (`~3.1e21`) overflows even `u64` (`~1.8e19`), so [`isqrt_u64`] alone can't take it directly.
///
/// **Derivation**: widen to `u128` first (`target = x_q16 << 16`, exact, `u128` has ample headroom).
/// If `target` still doesn't fit `u64`, drop the *lowest* `2k` bits for the smallest `k` that brings
/// it back into `u64` range, take the exact integer square root of what remains, then shift the root
/// left by `k` -- exploiting `sqrt(a * 4^k) = sqrt(a) * 2^k` so the dropped low bits (a relative error
/// on the order of `2^(2k) / target`, negligible at these magnitudes -- at most a few bits out of a
/// 70+ bit number for every real caller) don't need to be reasoned about bit-by-bit, only bounded.
pub fn sqrt_wide_q16(x_q16: i64) -> i64 {
    if x_q16 <= 0 {
        return 0;
    }
    let target = (x_q16 as u128) << 16;
    let bits = 128 - target.leading_zeros();
    let excess_bits = bits.saturating_sub(63);
    let k = excess_bits.div_ceil(2);
    let shifted = (target >> (2 * k)) as u64;
    let root = isqrt_u64(shifted);
    (root << k) as i64
}
