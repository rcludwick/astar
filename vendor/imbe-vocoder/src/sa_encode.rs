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

//! Spectral amplitude encoding: the log amplitudes are predicted from the
//! previous frame's, and the residual is split into six blocks, each coded
//! by a DCT. The encoder then decodes its own result, as the decoder will,
//! so the next frame predicts from what the far end actually has. Port of
//! `sa_encode.cc`.

use crate::aux_sub::get_bit_allocation;
use crate::basic_op::*;
use crate::dsp_sub::{dct, idct};
use crate::math_sub::{l_mpy_ls, log2};
use crate::qnt_sub::{deqnt_by_step, qnt_by_step, tbl_quant};
use crate::tables::{
    GAIN_QNT_TBL, GAIN_STEP_SIZE_TBL, HI_ORD_STD_TBL, HI_ORD_STEP_SIZE_TBL, LMPRBL_TBL,
};
use crate::{ImbeParam, MAX_BLOCK_LEN, NUM_HARMS_MAX, NUM_HARMS_MIN, NUM_PRED_RES_BLKS};

const CNST_ONE_Q8_24: u32 = 0x0100_0000;
const CNST_0_7_Q1_15: i16 = 0x599A;
const CNST_0_4_Q1_15: i16 = 0x3333;
const CNST_0_03_Q1_15: i16 = 0x03D7;
const CNST_0_05_Q1_15: i16 = 0x0666;

/// Upstream's `d_gain_adjust`, a gain offset (in log2 units, Q10.22) its
/// owner may set and nothing in OP25's IMBE path does. It stays at its
/// default of 0 and is not exposed; see `VENDORED.md`.
const D_GAIN_ADJUST_Q10_22: i32 = 0;

/// The step size for a higher-order DCT coefficient: coefficient `j` (from
/// 1) coded in `num_bits` bits.
fn hi_ord_step_size(j: usize, num_bits: i16) -> u16 {
    let product = (u32::from(HI_ORD_STD_TBL[j - 1])
        * u32::from(HI_ORD_STEP_SIZE_TBL[num_bits as usize - 1]))
        << 1;
    extract_h(product as i32) as u16
}

#[derive(Clone, Debug)]
pub struct SaEncoder {
    num_harms_prev2: u16,
    /// Previous log amplitudes in Q10.22, 1-based, padded past the end.
    sa_prev2: [i32; NUM_HARMS_MAX + 2],
}

impl SaEncoder {
    /// Upstream's `sa_encode_init`.
    pub fn new() -> Self {
        SaEncoder {
            num_harms_prev2: 30,
            sa_prev2: [0; NUM_HARMS_MAX + 2],
        }
    }

    /// Pads `sa_prev2` past the previous frame's harmonics with its last
    /// amplitude.
    fn pad_prev(&mut self) {
        let prev = usize::from(self.num_harms_prev2);
        let last = self.sa_prev2[prev];
        self.sa_prev2[prev + 1..].fill(last);
    }

    /// Codes `p.sa` into `p.b_vec[2..]`, filling `p.bit_alloc` and setting
    /// `p.div_one_by_num_harm` and its shift.
    pub fn sa_encode(&mut self, p: &mut ImbeParam) {
        let mut gain_vec = [0i16; 6];
        let mut gain_r = [0i16; 6];
        let mut t_vec = [0i16; NUM_HARMS_MAX];
        let mut c_vec = [0i16; MAX_BLOCK_LEN];
        let mut vec32_tmp = [0i32; NUM_HARMS_MAX];

        let num_harms = p.num_harms as u16;
        let nh = usize::from(num_harms);
        let prev = self.num_harms_prev2;

        // num_harms_prev2 / num_harms in unsigned Q8.24.
        let k_coef: u32 = if num_harms == prev {
            CNST_ONE_Q8_24
        } else if num_harms > prev {
            (div_s((prev << 9) as i16, (num_harms << 9) as i16) as u32) << 9
        } else {
            let mut k = 0u32;
            let mut tmp = prev;
            while tmp > num_harms {
                tmp -= num_harms;
                k += CNST_ONE_Q8_24;
            }
            k + ((div_s((tmp << 9) as i16, (num_harms << 9) as i16) as u32) << 9)
        };

        // The prediction coefficient.
        let ro_coef: i16 = if num_harms <= 15 {
            CNST_0_4_Q1_15
        } else if num_harms <= 24 {
            num_harms as i16 * CNST_0_03_Q1_15 - CNST_0_05_Q1_15
        } else {
            CNST_0_7_Q1_15
        };

        self.pad_prev();

        let mut k_acc = k_coef;
        let mut sum: i32 = 0;
        for i in 0..nh {
            let index = (k_acc >> 24) as usize; // integer part
            let si_coef = ((k_acc - ((index as u32) << 24)) >> 9) as i16; // fractional part

            if si_coef == 0 {
                // sa_prev2 is in Q10.22
                let tmp_word32 = l_mpy_ls(self.sa_prev2[index], ro_coef);
                vec32_tmp[i] = l_sub(log2(p.sa[i]), tmp_word32);
                sum = l_add(sum, self.sa_prev2[index]); // Q10.22
            } else {
                let mut tmp_word32 = l_mpy_ls(self.sa_prev2[index], sub(0x7FFF, si_coef));
                sum = l_add(sum, tmp_word32);
                vec32_tmp[i] = l_sub(log2(p.sa[i]), l_mpy_ls(tmp_word32, ro_coef));

                tmp_word32 = l_mpy_ls(self.sa_prev2[index + 1], si_coef);
                sum = l_add(sum, tmp_word32);
                vec32_tmp[i] = l_sub(vec32_tmp[i], l_mpy_ls(tmp_word32, ro_coef));
            }
            // Upstream subtracts the gain adjustment only when it is
            // non-zero; subtracting 0 is the same thing.
            vec32_tmp[i] = l_sub(vec32_tmp[i], D_GAIN_ADJUST_Q10_22);

            k_acc = k_acc.wrapping_add(k_coef);
        }

        // 1/num_harms, scaled for precision, kept for later use.
        let tmp = norm_s(num_harms as i16);
        p.div_one_by_num_harm_sh = tmp;
        let tmp1 = div_s(0x4000, (num_harms as i16) << tmp);
        p.div_one_by_num_harm = tmp1;

        sum = l_shr(l_mpy_ls(l_mpy_ls(sum, ro_coef), tmp1), 14 - tmp);

        for i in 0..nh {
            t_vec[i] = extract_h(l_shl(l_add(vec32_tmp[i], sum), 5)); // Q5.11
        }

        // -------- Encode the T vector --------
        let index = nh - NUM_HARMS_MIN as usize;

        get_bit_allocation(num_harms as i16, &mut p.bit_alloc);
        let mut lmprbl_item = LMPRBL_TBL[index];

        // The higher-order DCT coefficients.
        let mut t = 0usize;
        let mut bv = 8usize; // into p.b_vec
        let mut ba = 5usize; // into p.bit_alloc
        for g in gain_vec.iter_mut() {
            let bl_len = ((lmprbl_item >> 28) & 0xF) as usize;
            lmprbl_item <<= 4;

            dct(&t_vec[t..], bl_len, bl_len, &mut c_vec);
            *g = c_vec[0];

            for (j, &c) in c_vec.iter().enumerate().take(bl_len).skip(1) {
                let num_bits = p.bit_alloc[ba];
                ba += 1;
                p.b_vec[bv] = if num_bits != 0 {
                    qnt_by_step(c, hi_ord_step_size(j, num_bits), num_bits)
                } else {
                    0
                };
                bv += 1;
            }
            t += bl_len;
        }

        // The gain vector.
        dct(&gain_vec, NUM_PRED_RES_BLKS, NUM_PRED_RES_BLKS, &mut gain_r);

        let gss = &GAIN_STEP_SIZE_TBL[index * 5..index * 5 + 5];
        p.b_vec[2] = tbl_quant(gain_r[0], &GAIN_QNT_TBL);
        for j in 1..6 {
            p.b_vec[2 + j] = qnt_by_step(gain_r[j], gss[j - 1], p.bit_alloc[j - 1]);
        }

        // -------- Decode the T vector --------
        let mut ba = 0usize;
        let mut bv = 2usize;

        // The gain vector, in signed Q5.11.
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

        t_vec = [0; NUM_HARMS_MAX];
        lmprbl_item = LMPRBL_TBL[index];

        // The higher-order DCT coefficients.
        let mut t = 0usize;
        for &gain in &gain_r {
            let bl_len = ((lmprbl_item >> 28) & 0xF) as usize;
            lmprbl_item <<= 4;
            c_vec = [0; MAX_BLOCK_LEN];
            c_vec[0] = gain;
            for (j, c) in c_vec.iter_mut().enumerate().take(bl_len).skip(1) {
                let num_bits = p.bit_alloc[ba];
                ba += 1;
                *c = if num_bits != 0 {
                    extract_l(l_shr(
                        deqnt_by_step(p.b_vec[bv], hi_ord_step_size(j, num_bits), num_bits),
                        5,
                    ))
                } else {
                    0
                };
                bv += 1;
            }
            idct(&c_vec, bl_len, bl_len, &mut t_vec[t..]);
            t += bl_len;
        }

        // -------- Reconstruct the spectral amplitudes --------
        let mut k_acc = k_coef;
        self.pad_prev();

        for i in 0..nh {
            let index = (k_acc >> 24) as usize; // integer part
            let si_coef = ((k_acc - ((index as u32) << 24)) >> 9) as i16; // fractional part

            if si_coef == 0 {
                let tmp_word32 = l_mpy_ls(self.sa_prev2[index], ro_coef); // Q10.22
                // t_vec to Q10.22, plus the prediction.
                vec32_tmp[i] = l_add(l_shr(l_deposit_h(t_vec[i]), 5), tmp_word32);
            } else {
                let mut tmp_word32 = l_mpy_ls(self.sa_prev2[index], sub(0x7FFF, si_coef));
                vec32_tmp[i] = l_add(
                    l_shr(l_deposit_h(t_vec[i]), 5),
                    l_mpy_ls(tmp_word32, ro_coef),
                );

                tmp_word32 = l_mpy_ls(self.sa_prev2[index + 1], si_coef);
                vec32_tmp[i] = l_add(vec32_tmp[i], l_mpy_ls(tmp_word32, ro_coef));
            }

            k_acc = k_acc.wrapping_add(k_coef);
        }

        for i in 1..=nh {
            self.sa_prev2[i] = l_sub(vec32_tmp[i - 1], sum);
        }

        self.num_harms_prev2 = num_harms;
    }
}
