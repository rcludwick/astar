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

//! Fixed-point mathematics: 2^x, cosine, sine, square root and log2, port
//! of `math_sub.cc`.

use crate::basic_op::*;

/// 0.5 in Q15.
pub const X05_Q15: i16 = 16384;
/// 1.0 (as 32767) in Q15.
pub const ONE_Q15: i16 = 32767;

/// 2^(-1..0) for `pow2`.
const POW2_TABLE: [i16; 33] = [
    16384, 16743, 17109, 17484, 17867, 18258, 18658, 19066, 19484, 19911, 20347, 20792, 21247,
    21713, 22188, 22674, 23170, 23678, 24196, 24726, 25268, 25821, 26386, 26964, 27554, 28158,
    28774, 29405, 30048, 30706, 31379, 32066, 32767,
];

/// `pow(2.0, x)`: `x` in signed Q10.22, the result in signed Q14.2.
/// Upstream's `Pow2`.
pub fn pow2(x: i32) -> i16 {
    let mut exponent = extract_h(l_shr(x, 6));
    if exponent < 0 {
        exponent = add(exponent, 1);
    }
    let mut fraction = extract_l(l_shr(l_sub(x, l_shl(l_deposit_l(exponent), 6 + 16)), 7));
    if x < 0 {
        fraction = negate(fraction);
    }

    let mut l_x = l_mult(fraction, 32); // fraction << 6
    let i = extract_h(l_x) as usize; // b10..b16 of fraction
    l_x = l_shr(l_x, 1);
    let a = extract_l(l_x) & 0x7fff; // b0..b9 of fraction

    l_x = l_deposit_h(POW2_TABLE[i]);
    let tmp = sub(POW2_TABLE[i], POW2_TABLE[i + 1]);
    l_x = l_msu(l_x, tmp, a);

    if x < 0 {
        l_x = l_deposit_h(div_s(0x4000, extract_h(l_x))); // 1/fraction
        exponent = sub(exponent, 1);
    }

    let exp = sub(12, exponent);
    l_x = l_shr_r(l_x, exp);
    extract_h(l_x)
}

/// Multiplies a 32-bit number by a 16-bit one, truncating `l_var2` to 31
/// bits first. Upstream's `L_mpy_ls`.
pub fn l_mpy_ls(l_var2: i32, var1: i16) -> i32 {
    let swtemp = shr(extract_l(l_var2), 1) & 32767;
    let mut out = l_mult(var1, swtemp);
    out = l_shr(out, 15);
    l_mac(out, var1, extract_h(l_var2))
}

/// cos(pi * x / 2) table for `cos_fxp`.
const COS_TABLE: [i16; 129] = [
    32767, 32766, 32758, 32746, 32729, 32706, 32679, 32647, 32610, 32568, 32522, 32470, 32413,
    32352, 32286, 32214, 32138, 32058, 31972, 31881, 31786, 31686, 31581, 31471, 31357, 31238,
    31114, 30986, 30853, 30715, 30572, 30425, 30274, 30118, 29957, 29792, 29622, 29448, 29269,
    29086, 28899, 28707, 28511, 28311, 28106, 27897, 27684, 27467, 27246, 27020, 26791, 26557,
    26320, 26078, 25833, 25583, 25330, 25073, 24812, 24548, 24279, 24008, 23732, 23453, 23170,
    22884, 22595, 22302, 22006, 21706, 21403, 21097, 20788, 20475, 20160, 19841, 19520, 19195,
    18868, 18538, 18205, 17869, 17531, 17190, 16846, 16500, 16151, 15800, 15447, 15091, 14733,
    14373, 14010, 13646, 13279, 12910, 12540, 12167, 11793, 11417, 11039, 10660, 10279, 9896, 9512,
    9127, 8740, 8351, 7962, 7571, 7180, 6787, 6393, 5998, 5602, 5205, 4808, 4410, 4011, 3612, 3212,
    2811, 2411, 2009, 1608, 1206, 804, 402, 0,
];

/// Cosine of `x`, where `x` is the angle in radians divided by pi, in Q15.
/// The result is in Q15.
pub fn cos_fxp(x: i16) -> i16 {
    let mut tx = if x < 0 { negate(x) } else { x };
    let mut sign = false;

    // If the angle is past pi/2, cos(angle) = -cos(pi - angle).
    if tx > X05_Q15 {
        tx = sub(ONE_Q15, tx);
        sign = true;
    }
    let index1 = shr(tx, 7);
    let index2 = add(index1, 1);

    if index1 == 128 {
        return 0;
    }

    let mut m = sub(tx, shl(index1, 7));
    m = shl(m, 8); // the remainder in Q15

    let mut temp = sub(COS_TABLE[index2 as usize], COS_TABLE[index1 as usize]);
    temp = mult(m, temp);
    let ty = add(COS_TABLE[index1 as usize], temp);

    if sign { negate(ty) } else { ty }
}

/// Sine of `x`, where `x` is the angle in radians divided by pi, in Q15.
pub fn sin_fxp(x: i16) -> i16 {
    let (mut tx, sign) = if x < 0 { (negate(x), true) } else { (x, false) };

    if tx > X05_Q15 {
        tx = sub(tx, X05_Q15);
    } else {
        tx = sub(X05_Q15, tx);
    }

    let ty = cos_fxp(tx);
    if sign { negate(ty) } else { ty }
}

/// sqrt((i + 16) * 2^-6) * 2^15 for `sqrt_l_exp`.
const SQRT_TABLE: [i16; 49] = [
    16384, 16888, 17378, 17854, 18318, 18770, 19212, 19644, 20066, 20480, 20886, 21283, 21674,
    22058, 22435, 22806, 23170, 23530, 23884, 24232, 24576, 24915, 25249, 25580, 25905, 26227,
    26545, 26859, 27170, 27477, 27780, 28081, 28378, 28672, 28963, 29251, 29537, 29819, 30099,
    30377, 30652, 30924, 31194, 31462, 31727, 31991, 32252, 32511, 32767,
];

/// sqrt(`l_x`) for `l_x` in Q1.31. Returns the result in Q1.31 and the right
/// shift still to be applied to it (upstream's `*exp` out-parameter).
pub fn sqrt_l_exp(l_x: i32) -> (i32, i16) {
    if l_x <= 0 {
        return (0, 0);
    }

    let e = norm_l(l_x) & !1; // the next lower even normalisation shift
    let mut l_x = l_shl(l_x, e); // now in [0.25, 1)
    let exp = e >> 1;

    l_x = l_shr(l_x, 9);
    let mut i = extract_h(l_x); // b25..b31, 16 <= i <= 63
    l_x = l_shr(l_x, 1);
    let a = extract_l(l_x) & 0x7fff; // b10..b24

    i = sub(i, 16);
    let i = i as usize;

    let mut l_y = l_deposit_h(SQRT_TABLE[i]);
    let tmp = sub(SQRT_TABLE[i], SQRT_TABLE[i + 1]);
    l_y = l_msu(l_y, tmp, a);

    (l_y, exp)
}

/// log2 table for `log2`.
const LOG_TABLE: [i16; 33] = [
    0, 1455, 2866, 4236, 5568, 6863, 8124, 9352, 10549, 11716, 12855, 13967, 15054, 16117, 17156,
    18172, 19167, 20142, 21097, 22033, 22951, 23852, 24735, 25603, 26455, 27291, 28113, 28922,
    29716, 30497, 31266, 32023, 32767,
];

/// log2(`x`) for `x` in Q14.2; the result in Q10.22. Upstream's `Log2`.
pub fn log2(x: i16) -> i32 {
    if x <= 0 {
        return 0;
    }

    let mut exp = norm_s(x);
    let x = shl(x, exp);

    let mut i = shr(x, 9); // b15..b9
    let a = shl(x & 0x1ff, 6); // b8..b0
    i = sub(i, 32);
    let i = i as usize;

    let mut l_y = l_deposit_h(LOG_TABLE[i]);
    let tmp = sub(LOG_TABLE[i], LOG_TABLE[i + 1]);
    l_y = l_msu(l_y, tmp, a);

    l_y = l_shr(l_y, 9);

    exp = sub(12, exp);
    l_add(l_y, l_deposit_h(shl(exp, 6)))
}
