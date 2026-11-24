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

//! Spectral amplitude decoding, port of `sa_decode.cc`.

use crate::basic_op::*;
use crate::dsp_sub::idct;
use crate::math_sub::{l_mpy_ls, pow2};
use crate::qnt_sub::deqnt_by_step;
use crate::tables::{
    GAIN_QNT_TBL, GAIN_STEP_SIZE_TBL, HI_ORD_STD_TBL, HI_ORD_STEP_SIZE_TBL, LMPRBL_TBL,
};
use crate::{ImbeParam, MAX_BLOCK_LEN, NUM_HARMS_MAX, NUM_HARMS_MIN, NUM_PRED_RES_BLKS};

const CNST_ONE_Q8_24: u32 = 0x0100_0000;
const CNST_0_7_Q1_15: i16 = 0x599A;
const CNST_0_4_Q1_15: i16 = 0x3333;
const CNST_0_03_Q1_15: i16 = 0x03D7;
const CNST_0_05_Q1_15: i16 = 0x0666;

/// The spectral amplitude decoder's memory of the previous frame.
#[derive(Clone, Debug)]
pub struct SaDecoder {
    num_harms_prev1: i16,
    /// Previous log amplitudes in Q10.22, 1-based, padded past the end.
    sa_prev1: [i32; NUM_HARMS_MAX + 2],
}

impl SaDecoder {
    /// Upstream's `sa_decode_init`.
    pub fn new() -> Self {
        SaDecoder {
            num_harms_prev1: 30,
            sa_prev1: [0; NUM_HARMS_MAX + 2],
        }
    }

    /// Decodes `p.sa` from `p.b_vec` and `p.bit_alloc`, and sets
    /// `p.div_one_by_num_harm` and its shift for the synthesisers.
    pub fn sa_decode(&mut self, p: &mut ImbeParam) {
        let mut gain_vec = [0i16; 6];
        let mut gain_r = [0i16; 6];
        let mut t_vec = [0i16; NUM_HARMS_MAX];
        let mut sa_tmp = [0i32; NUM_HARMS_MAX];

        let num_harms = p.num_harms;
        let nh = num_harms as usize;
        let index = (num_harms - NUM_HARMS_MIN) as usize;
        let mut ba = 0usize; // into p.bit_alloc
        let mut bv = 2usize; // into p.b_vec

        // The gain vector, in signed Q5.11.
        let gss = &GAIN_STEP_SIZE_TBL[index * 5..index * 5 + 5];
        gain_vec[0] = GAIN_QNT_TBL[p.b_vec[bv] as usize];
        bv += 1;
        for i in 1..6 {
            gain_vec[i] = extract_l(l_shr(
                deqnt_by_step(p.b_vec[bv], gss[i - 1], p.bit_alloc[ba]),
                5,
            ));
            bv += 1;
            ba += 1;
        }
        idct(&gain_vec, NUM_PRED_RES_BLKS, NUM_PRED_RES_BLKS, &mut gain_r);

        // The higher-order DCT coefficients.
        let mut lmprbl_item = LMPRBL_TBL[index];
        let mut t = 0usize;
        for &gain in gain_r.iter().take(NUM_PRED_RES_BLKS) {
            let bl_len = ((lmprbl_item >> 28) & 0xF) as usize;
            lmprbl_item <<= 4;
            let mut c_vec = [0i16; MAX_BLOCK_LEN];
            c_vec[0] = gain;
            for j in 1..bl_len {
                let num_bits = p.bit_alloc[ba];
                ba += 1;
                if num_bits != 0 {
                    let product = (u32::from(HI_ORD_STD_TBL[j - 1])
                        * u32::from(HI_ORD_STEP_SIZE_TBL[num_bits as usize - 1]))
                        << 1;
                    let step_size = extract_h(product as i32);
                    c_vec[j] = extract_l(l_shr(
                        deqnt_by_step(p.b_vec[bv], step_size as u16, num_bits),
                        5,
                    ));
                } else {
                    c_vec[j] = 0;
                }
                bv += 1;
            }
            idct(&c_vec, bl_len, bl_len, &mut t_vec[t..]);
            t += bl_len;
        }

        // num_harms_prev / num_harms in unsigned Q8.24.
        let prev = self.num_harms_prev1;
        let k_coef: u32 = if num_harms == prev {
            CNST_ONE_Q8_24
        } else if num_harms > prev {
            (div_s(prev << 9, num_harms << 9) as u32) << 9
        } else {
            let mut k = 0u32;
            let mut tmp = prev;
            while tmp > num_harms {
                tmp -= num_harms;
                k += CNST_ONE_Q8_24;
            }
            k + ((div_s(tmp << 9, num_harms << 9) as u32) << 9)
        };

        let ro_coef: i16 = if num_harms <= 15 {
            CNST_0_4_Q1_15
        } else if num_harms <= 24 {
            num_harms * CNST_0_03_Q1_15 - CNST_0_05_Q1_15
        } else {
            CNST_0_7_Q1_15
        };

        let mut k_acc = k_coef;
        let mut sum: i32 = 0;

        let last = self.sa_prev1[prev as usize];
        self.sa_prev1[prev as usize + 1..].fill(last);

        for i in 0..nh {
            let index = (k_acc >> 24) as usize; // integer part
            let si_coef = ((k_acc - ((index as u32) << 24)) >> 9) as i16; // fractional part

            if si_coef == 0 {
                // sa_prev1 is in Q10.22
                let tmp_word32 = l_mpy_ls(self.sa_prev1[index], ro_coef);
                sa_tmp[i] = l_add(l_shr(l_deposit_h(t_vec[i]), 5), tmp_word32);
                sum = l_add(sum, self.sa_prev1[index]);
            } else {
                let mut tmp_word32 = l_mpy_ls(self.sa_prev1[index], sub(0x7FFF, si_coef));
                sum = l_add(sum, tmp_word32);
                sa_tmp[i] = l_add(
                    l_shr(l_deposit_h(t_vec[i]), 5),
                    l_mpy_ls(tmp_word32, ro_coef),
                );

                tmp_word32 = l_mpy_ls(self.sa_prev1[index + 1], si_coef);
                sum = l_add(sum, tmp_word32);
                sa_tmp[i] = l_add(sa_tmp[i], l_mpy_ls(tmp_word32, ro_coef));
            }

            k_acc += k_coef;
        }

        // 1/num_harms, scaled for precision, kept for the synthesisers.
        let tmp = norm_s(num_harms);
        p.div_one_by_num_harm_sh = tmp;
        let tmp1 = div_s(0x4000, num_harms << tmp);
        p.div_one_by_num_harm = tmp1;

        sum = l_shr(l_mpy_ls(l_mpy_ls(sum, ro_coef), tmp1), 14 - tmp);

        for i in 1..=nh {
            self.sa_prev1[i] = l_sub(sa_tmp[i - 1], sum);
            p.sa[i - 1] = pow2(self.sa_prev1[i]);
        }

        self.num_harms_prev1 = num_harms;
    }
}
