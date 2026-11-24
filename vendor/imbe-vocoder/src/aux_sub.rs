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

//! Vector helpers and the bit allocation lookup, port of `aux_sub.cc`.
//! Upstream's `v_zap` and `v_equ` are slice `fill` and `copy_from_slice`
//! at their call sites.

use crate::NUM_HARMS_MIN;
use crate::basic_op::*;
use crate::tables::{BIT_ALLOCATION_OFFSET_TBL, BIT_ALLOCATION_TBL};

/// The offset in `BIT_ALLOCATION_TBL` of the allocation for `num_harms`.
pub fn get_bit_allocation_arr(num_harms: i16) -> usize {
    if num_harms == NUM_HARMS_MIN {
        0
    } else {
        let index = num_harms - NUM_HARMS_MIN - 1;
        let offset =
            BIT_ALLOCATION_OFFSET_TBL[(index >> 2) as usize] + (3 + (index >> 2)) * (index & 0x3);
        offset as usize
    }
}

/// Unpacks the bit allocation for `num_harms` into `out`, one entry per
/// coefficient, four per table word.
pub fn get_bit_allocation(num_harms: i16, out: &mut [i16]) {
    let mut bat = get_bit_allocation_arr(num_harms);
    let mut p = 0usize;
    let mut i = 0;
    while i < num_harms - 1 {
        let mut tmp = BIT_ALLOCATION_TBL[bat];
        bat += 1;
        out[p + 3] = (tmp & 0xF) as i16;
        tmp >>= 4;
        out[p + 2] = (tmp & 0xF) as i16;
        tmp >>= 4;
        out[p + 1] = (tmp & 0xF) as i16;
        tmp >>= 4;
        out[p] = (tmp & 0xF) as i16;
        p += 4;
        i += 4;
    }
}

/// Sum of squares of `vec`, with saturation: `L_v_magsq`.
pub fn l_v_magsq(vec: &[i16]) -> i32 {
    vec.iter().fold(0, |acc, &v| l_mac(acc, v, v))
}

/// `dst[i] = shr(src[i], scale)` for the first `n` entries.
pub fn v_equ_shr(dst: &mut [i16], src: &[i16], scale: i16, n: usize) {
    for (d, &s) in dst[..n].iter_mut().zip(&src[..n]) {
        *d = shr(s, scale);
    }
}
