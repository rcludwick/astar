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

//! Channel decoding: the code vectors û0..û7 back into the parameter
//! vector b and the voiced/unvoiced decisions. Port of `ch_decode.cc`.

use crate::aux_sub::get_bit_allocation;
use crate::basic_op::*;
use crate::{B_NUM, ImbeParam, NUM_BANDS_MAX, NUM_HARMS_MAX};

/// 0.9254 in unsigned Q0.16.
const CNST_0_9254_Q0_16: u32 = 60647;
/// 1/3 in unsigned Q0.16.
const CNST_0_33_Q0_16: u32 = 0x5556;

/// Bits b3..b(L+1) carry, plus the six of û0 and û7 that join them.
const BIT_STREAM_LEN: usize = 3 + 3 * 12 + 3 * 11 + 3;

/// Rebuilds `b_vec`, `fund_freq`, `num_harms`, `num_bands` and `bit_alloc`
/// from the code vectors. A pitch index past 207 leaves every parameter as
/// the previous frame left it, so that frame repeats.
pub fn decode_frame_vector(p: &mut ImbeParam, frame_vector: &[i16; 8]) {
    let fv = frame_vector;
    let mut bit_stream = [0i16; BIT_STREAM_LEN];

    p.b_vec[0] = (shr(fv[0], 4) & 0xFC) | (shr(fv[7], 1) & 0x3);

    if p.b_vec[0] < 0 || p.b_vec[0] > 207 {
        return; // frame repeat: keep the previous frame's parameters
    }

    // b0 in unsigned Q15.1, plus 39.5.
    let mut tmp: i16 = ((p.b_vec[0] & 0xFF) << 1) + 0x4F;

    // The fundamental frequency, 4 / (b0 + 39.5), to higher precision.
    let shift = norm_s(tmp);
    let tmp1 = tmp << shift;

    let mut tmp2 = div_s(0x4000, tmp1);
    p.fund_freq = l_shr(l_deposit_h(tmp2), 11 - shift);

    let mut l_tmp = l_sub(0x4000_0000, l_mult(tmp1, tmp2));
    tmp2 = div_s(extract_l(l_shr(l_tmp, 2)), tmp1);
    l_tmp = l_shr(l_deposit_l(tmp2), 11 - shift - 2);
    p.fund_freq = l_add(p.fund_freq, l_tmp);

    tmp = (tmp + 0x2) >> 3; // (b0 + 39.5 + 1) / 4
    p.num_harms = ((CNST_0_9254_Q0_16 * tmp as u32) >> 16) as i16;

    p.num_bands = if p.num_harms <= 36 {
        // fix((L + 2) / 3)
        extract_h(((p.num_harms + 2) as u32 * CNST_0_33_Q0_16) as i32)
    } else {
        NUM_BANDS_MAX as i16
    };

    // The input vector, b3 to b(L+1), as one bit stream.
    bit_stream[0] = i16::from(fv[0] & 0x4 != 0);
    bit_stream[1] = i16::from(fv[0] & 0x2 != 0);
    bit_stream[2] = i16::from(fv[0] & 0x1 != 0);

    bit_stream[BIT_STREAM_LEN - 3] = i16::from(fv[7] & 0x40 != 0);
    bit_stream[BIT_STREAM_LEN - 2] = i16::from(fv[7] & 0x20 != 0);
    bit_stream[BIT_STREAM_LEN - 1] = i16::from(fv[7] & 0x10 != 0);

    let mut index0 = 3 + 3 * 12 - 1;
    for vec_num in (1..=3).rev() {
        let mut t = fv[vec_num];
        for _ in 0..12 {
            bit_stream[index0] = i16::from(t & 0x1 != 0);
            t >>= 1;
            index0 -= 1;
        }
    }

    let mut index0 = 3 + 3 * 12 + 3 * 11 - 1;
    for vec_num in (4..=6).rev() {
        let mut t = fv[vec_num];
        for _ in 0..11 {
            bit_stream[index0] = i16::from(t & 0x1 != 0);
            t >>= 1;
            index0 -= 1;
        }
    }

    // Rebuild b1.
    let mut index0 = 3 + 3 * 12;
    let mut t: i16 = 0;
    for _ in 0..p.num_bands {
        t = (t << 1) | bit_stream[index0];
        index0 += 1;
    }
    p.b_vec[1] = t;

    // Rebuild b2.
    let mut t: i16 = 0;
    t |= bit_stream[index0] << 1;
    index0 += 1;
    t |= bit_stream[index0];
    index0 += 1;
    p.b_vec[2] = (fv[0] & 0x38) | (t << 1) | (shr(fv[7], 3) & 0x01);

    // Shift the rest of the sequence down.
    let sh = (p.num_bands + 2) as usize;
    while index0 < BIT_STREAM_LEN {
        bit_stream[index0 - sh] = bit_stream[index0];
        index0 += 1;
    }

    // Priority rescanning.
    p.b_vec[3..3 + B_NUM].fill(0);
    p.bit_alloc[..B_NUM].fill(0);

    get_bit_allocation(p.num_harms, &mut p.bit_alloc);

    let mut index0 = 0usize;
    let mut bit_thr: i16 = if p.num_harms == 0xb {
        9
    } else {
        p.bit_alloc[0]
    };
    let n = p.num_harms as usize - 1;

    while index0 < BIT_STREAM_LEN - p.num_bands as usize - 2 {
        for i in 0..n {
            if bit_thr != 0 && bit_thr <= p.bit_alloc[i] {
                // Upstream would read past the stream here if a table ever
                // allocated more bits than the frame carries; read 0 instead.
                let bit = bit_stream.get(index0).copied().unwrap_or(0);
                p.b_vec[3 + i] = (p.b_vec[3 + i] << 1) | bit;
                index0 += 1;
            }
        }
        bit_thr -= 1;
    }

    // Synchronisation bit.
    p.b_vec[p.num_harms as usize + 2] = fv[7] & 1;

    debug_assert!(p.num_harms as usize <= NUM_HARMS_MAX);
}

/// The voiced/unvoiced decision for each harmonic, from b1: three
/// harmonics per band, the last band taking the rest.
pub fn v_uv_decode(p: &mut ImbeParam) {
    let mut num_harms = p.num_harms;
    let mut num_bands = p.num_bands;
    let vu_vec = p.b_vec[1];

    let mut mask: i16 = 1 << (num_bands - 1);

    p.v_uv_dsn.fill(0);

    let mut k = 0usize;
    let mut i = 0;
    let mut uv_cnt: i16 = 0;
    while num_harms > 0 {
        num_harms -= 1;
        if vu_vec & mask != 0 {
            p.v_uv_dsn[k] = 1;
        } else {
            p.v_uv_dsn[k] = 0;
            uv_cnt += 1;
        }
        k += 1;

        i += 1;
        if i == 3 {
            if num_bands > 1 {
                num_bands -= 1;
                mask >>= 1;
            }
            i = 0;
        }
    }
    p.l_uv = uv_cnt;
}
