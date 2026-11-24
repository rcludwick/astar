// Project 25 IMBE Encoder/Decoder Fixed-Point implementation
// Developed by Pavel Yazev E-mail: pyazev@gmail.com
// Version 1.0 (c) Copyright 2009
//
// Rust port (c) 2026 Rob Ludwick
//
// This is free software; you can redistribute it and/or modify it under the
// terms of the GNU General Public License as published by the Free Software
// Foundation; either version 3, or (at your option) any later version. It is
// distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
// without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
// PARTICULAR PURPOSE. See the GNU General Public License (COPYING) for more
// details.
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! The fixed-point basic operators the vocoder is written in.
//!
//! Upstream carries the ETSI/ITU-T GSM-AMR reference operators
//! (`basicop2.cc`). These are written fresh from the operators' definitions:
//! 16-bit (`i16`) and 32-bit (`i32`) two's-complement words, saturating where
//! the definition saturates. Upstream's `Overflow` and `Carry` globals are not
//! kept: nothing in the vocoder outside `basicop2.cc` reads them.

pub const MAX_16: i16 = i16::MAX;
pub const MIN_16: i16 = i16::MIN;
pub const MAX_32: i32 = i32::MAX;
pub const MIN_32: i32 = i32::MIN;

/// Clamps a 32-bit value into 16 bits.
#[inline]
pub fn saturate(l: i32) -> i16 {
    l.clamp(i32::from(MIN_16), i32::from(MAX_16)) as i16
}

/// 16-bit add with saturation.
#[inline]
pub fn add(a: i16, b: i16) -> i16 {
    a.saturating_add(b)
}

/// 16-bit subtract with saturation.
#[inline]
pub fn sub(a: i16, b: i16) -> i16 {
    a.saturating_sub(b)
}

/// 16-bit negate; `negate(-32768)` is 32767.
#[inline]
pub fn negate(a: i16) -> i16 {
    a.saturating_neg()
}

/// 16-bit arithmetic shift left with saturation; a negative count shifts
/// right.
pub fn shl(a: i16, n: i16) -> i16 {
    if n < 0 {
        return shr(a, -(n.max(-16)));
    }
    if a == 0 {
        return 0;
    }
    if n > 15 {
        return if a > 0 { MAX_16 } else { MIN_16 };
    }
    let r = i32::from(a) << n;
    if r != i32::from(r as i16) {
        if a > 0 { MAX_16 } else { MIN_16 }
    } else {
        r as i16
    }
}

/// 16-bit arithmetic shift right; a negative count shifts left with
/// saturation.
pub fn shr(a: i16, n: i16) -> i16 {
    if n < 0 {
        return shl(a, -(n.max(-16)));
    }
    if n >= 15 {
        if a < 0 { -1 } else { 0 }
    } else {
        a >> n
    }
}

/// 16-bit shift right with rounding.
pub fn shr_r(a: i16, n: i16) -> i16 {
    if n > 15 {
        return 0;
    }
    let mut r = shr(a, n);
    if n > 0 && (a & (1i16 << (n - 1))) != 0 {
        r = r.wrapping_add(1);
    }
    r
}

/// Q15 multiply: `(a * b) >> 15`, saturated.
#[inline]
pub fn mult(a: i16, b: i16) -> i16 {
    saturate((i32::from(a) * i32::from(b)) >> 15)
}

/// Q15 multiply with rounding.
#[inline]
pub fn mult_r(a: i16, b: i16) -> i16 {
    saturate((i32::from(a) * i32::from(b) + 0x4000) >> 15)
}

/// `a * b * 2` as 32 bits, saturated (only `-32768 * -32768` saturates).
#[inline]
pub fn l_mult(a: i16, b: i16) -> i32 {
    let p = i32::from(a) * i32::from(b);
    if p == 0x4000_0000 { MAX_32 } else { p * 2 }
}

/// The upper 16 bits of a 32-bit word.
#[inline]
pub fn extract_h(l: i32) -> i16 {
    (l >> 16) as i16
}

/// The lower 16 bits of a 32-bit word.
#[inline]
pub fn extract_l(l: i32) -> i16 {
    l as i16
}

/// Rounds a 32-bit word to its upper 16 bits, saturating.
#[inline]
pub fn round(l: i32) -> i16 {
    extract_h(l_add(l, 0x8000))
}

/// 32-bit add with saturation.
#[inline]
pub fn l_add(a: i32, b: i32) -> i32 {
    a.saturating_add(b)
}

/// 32-bit subtract with saturation.
#[inline]
pub fn l_sub(a: i32, b: i32) -> i32 {
    a.saturating_sub(b)
}

/// 32-bit absolute value; `l_abs(MIN_32)` is `MAX_32`.
#[inline]
pub fn l_abs(a: i32) -> i32 {
    a.saturating_abs()
}

/// Multiply-accumulate: `acc + l_mult(a, b)`, saturated.
#[inline]
pub fn l_mac(acc: i32, a: i16, b: i16) -> i32 {
    l_add(acc, l_mult(a, b))
}

/// Multiply-subtract: `acc - l_mult(a, b)`, saturated.
#[inline]
pub fn l_msu(acc: i32, a: i16, b: i16) -> i32 {
    l_sub(acc, l_mult(a, b))
}

/// 32-bit arithmetic shift left with saturation; a negative count shifts
/// right.
pub fn l_shl(a: i32, n: i16) -> i32 {
    if n <= 0 {
        return l_shr(a, -(n.max(-32)));
    }
    if a == 0 {
        return 0;
    }
    // Saturates as soon as a doubling would leave the 32-bit range.
    let lz = if a > 0 {
        a.leading_zeros()
    } else {
        (!a).leading_zeros()
    };
    if n as u32 >= lz {
        if a > 0 { MAX_32 } else { MIN_32 }
    } else {
        a << n
    }
}

/// 32-bit arithmetic shift right; a negative count shifts left with
/// saturation.
pub fn l_shr(a: i32, n: i16) -> i32 {
    if n < 0 {
        return l_shl(a, -(n.max(-32)));
    }
    if n >= 31 {
        if a < 0 { -1 } else { 0 }
    } else {
        a >> n
    }
}

/// 32-bit shift right with rounding.
pub fn l_shr_r(a: i32, n: i16) -> i32 {
    if n > 31 {
        return 0;
    }
    let mut r = l_shr(a, n);
    if n > 0 && (a & (1i32 << (n - 1))) != 0 {
        r = r.wrapping_add(1);
    }
    r
}

/// A 16-bit word into the upper half of a 32-bit word.
#[inline]
pub fn l_deposit_h(a: i16) -> i32 {
    i32::from(a) << 16
}

/// A 16-bit word, sign-extended to 32 bits.
#[inline]
pub fn l_deposit_l(a: i16) -> i32 {
    i32::from(a)
}

/// The left shift that normalises a 16-bit word (0 for 0, 15 for -1).
pub fn norm_s(a: i16) -> i16 {
    match a {
        0 => 0,
        -1 => 15,
        _ => {
            let v = if a < 0 { !a } else { a };
            v.leading_zeros() as i16 - 1
        }
    }
}

/// The left shift that normalises a 32-bit word (0 for 0, 31 for -1).
pub fn norm_l(a: i32) -> i16 {
    match a {
        0 => 0,
        -1 => 31,
        _ => {
            let v = if a < 0 { !a } else { a };
            v.leading_zeros() as i16 - 1
        }
    }
}

/// Fractional division `num / den` in Q15, for `0 <= num <= den`, `den > 0`.
///
/// Upstream calls `abort()` on any other input. The port does not, in any
/// build: the decoder's input comes off the network, so an abort (or a
/// panic) would let one malformed frame take astar down. It saturates
/// instead, to 32767 when `num > den` with both positive and to 0 for a
/// negative or zero operand. No golden vector reaches that path, so the
/// change never alters what a valid frame decodes to.
pub fn div_s(num: i16, den: i16) -> i16 {
    if num < 0 || den <= 0 {
        return 0;
    }
    if num > den {
        return MAX_16;
    }
    if num == 0 {
        return 0;
    }
    if num == den {
        return MAX_16;
    }
    let (mut n, d) = (i32::from(num), i32::from(den));
    let mut out: i16 = 0;
    for _ in 0..15 {
        out <<= 1;
        n <<= 1;
        if n >= d {
            n -= d;
            out += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_ops_saturate_as_etsi_defines() {
        assert_eq!(add(32767, 1), 32767);
        assert_eq!(sub(-32768, 1), -32768);
        assert_eq!(mult(-32768, -32768), 32767);
        assert_eq!(l_shl(1, 31), i32::MAX);
        assert_eq!(l_mult(-32768, -32768), i32::MAX);
        assert_eq!(norm_s(0), 0);
        assert_eq!(norm_l(1), 30);
        assert_eq!(div_s(1, 2), 16384);
    }

    #[test]
    fn shifts_match_the_loop_definitions() {
        // l_shl saturates the step before a doubling would pass 0x3fffffff.
        assert_eq!(l_shl(0x3fff_ffff, 1), 0x7fff_fffe);
        assert_eq!(l_shl(0x4000_0000, 1), MAX_32);
        assert_eq!(l_shl(-0x4000_0000, 1), MIN_32);
        assert_eq!(l_shl(-0x4000_0001, 1), MIN_32);
        assert_eq!(l_shl(-1, 31), MIN_32);
        assert_eq!(l_shl(-1, 30), -0x4000_0000);
        assert_eq!(l_shr(-5, 1), -3);
        assert_eq!(l_shr(-5, -1), -10);
        assert_eq!(shl(0x4000, 1), MAX_16);
        assert_eq!(shl(-0x4000, 1), MIN_16);
        assert_eq!(shl(-1, 15), MIN_16);
        assert_eq!(shl(-1, 16), MIN_16);
        assert_eq!(shl(1, 16), MAX_16);
        assert_eq!(shr(-1, 20), -1);
        assert_eq!(shr_r(3, 1), 2);
        assert_eq!(l_shr_r(3, 1), 2);
        assert_eq!(norm_s(-2), 14);
        assert_eq!(norm_s(0x4000), 0);
        assert_eq!(norm_l(-0x4000_0000), 1);
        assert_eq!(round(0x7fff_ffff), MAX_16);
    }

    #[test]
    fn div_s_saturates_where_upstream_aborts() {
        assert_eq!(div_s(5, 3), MAX_16);
        assert_eq!(div_s(i16::MAX, 1), MAX_16);
        assert_eq!(div_s(-1, 3), 0);
        assert_eq!(div_s(1, 0), 0);
        assert_eq!(div_s(0, 0), 0);
        assert_eq!(div_s(1, -2), 0);
        assert_eq!(div_s(i16::MIN, i16::MIN), 0);
    }
}
