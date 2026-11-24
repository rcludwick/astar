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

//! Quantiser helpers, port of `qnt_sub.cc`.

use crate::basic_op::*;

/// 0.5 in unsigned Q0.16.
const CNST_0_5_Q0_16: i32 = 0x8000;

/// Dequantises `qval`, a `bit_num`-bit index, by `step_size` (unsigned
/// Q0.16). The result is in signed (`bit_num`).16.
pub fn deqnt_by_step(qval: i16, step_size: u16, bit_num: i16) -> i32 {
    if bit_num == 0 {
        return 0;
    }
    let step = i32::from(step_size);
    let res = step * (i32::from(qval) - (1i32 << (bit_num - 1)));
    l_add(res, (step * CNST_0_5_Q0_16) >> 16)
}

/// Quantises `val` (Q5.10) by `step_size` (unsigned Q0.16) to an unsigned
/// `bit_num`-bit index, clamping at both ends.
///
/// `bit_num` is a 4-bit bit-allocation field. Upstream evaluates
/// `1 << (bit_num - 1)` even for 0, which is undefined behaviour in C. On
/// arm64 and x86-64, which take shift counts modulo 32, it comes out as an
/// index of 0 whatever the value, so the port returns 0 for it directly.
pub fn qnt_by_step(val: i16, step_size: u16, bit_num: i16) -> i16 {
    if bit_num <= 0 {
        return 0;
    }
    let shift = norm_s(step_size as i16);

    // To get the result in Qxx.16, tmp would need a left shift of shift + 3.
    let tmp = div_s(0x4000, shl(step_size as i16, shift));
    let q_index = shr_r(mult(val, tmp), sub(9, shift)); // rounded to the nearest integer

    let max_val = (1i32 << (bit_num - 1)) as i16;
    let min_val = negate(max_val);

    if q_index < min_val {
        0
    } else if q_index >= max_val {
        ((1i32 << bit_num) - 1) as i16
    } else {
        (i32::from(max_val) + i32::from(q_index)) as i16
    }
}

/// The index of the entry of the ascending table `q_tbl` nearest to `val`;
/// a tie goes to the higher entry.
pub fn tbl_quant(val: i16, q_tbl: &[i16]) -> i16 {
    let mut min_index = 0usize;
    let mut max_index = q_tbl.len() - 1;

    if val >= q_tbl[max_index] {
        return max_index as i16;
    }
    if val <= q_tbl[min_index] {
        return min_index as i16;
    }

    while max_index - min_index != 1 {
        let index = min_index + ((max_index - min_index) >> 1);
        if q_tbl[index] > val {
            max_index = index;
        } else {
            min_index = index;
        }
    }

    let (val, hi, lo) = (
        i32::from(val),
        i32::from(q_tbl[max_index]),
        i32::from(q_tbl[min_index]),
    );
    if hi - val <= val - lo {
        max_index as i16
    } else {
        min_index as i16
    }
}
