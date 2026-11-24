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

//! Spectral amplitude enhancement, port of `sa_enh.cc`.

use crate::aux_sub::{l_v_magsq, v_equ_shr};
use crate::basic_op::*;
use crate::math_sub::{cos_fxp, l_mpy_ls, sqrt_l_exp};
use crate::{ImbeParam, NUM_HARMS_MAX};

const CNST_0_9898_Q1_15: i16 = 0x7EB3;
const CNST_0_5_Q2_14: i16 = 0x2000;
const CNST_1_2_Q2_14: i16 = 0x4CCC;

/// Enhances `p.sa` in place, using `p.num_harms` and `p.fund_freq`.
pub fn sa_enh(p: &mut ImbeParam) {
    let num_harm = p.num_harms as usize;
    let sa = &mut p.sa;
    let mut sa_tmp = [0i16; NUM_HARMS_MAX];
    let mut cos_w = [0i16; NUM_HARMS_MAX];

    sa_tmp[..num_harm].copy_from_slice(&sa[..num_harm]);

    let mut rm0 = l_v_magsq(&sa[..num_harm]);

    if rm0 == 0 {
        return;
    }

    let mut nm = norm_l(rm0);
    if rm0 == MAX_32 {
        nm = 1;
        v_equ_shr(&mut sa_tmp, &sa[..], nm, num_harm);
        rm0 = l_v_magsq(&sa_tmp[..num_harm]);
    } else if nm > 2 {
        nm = -(nm >> 1);
        v_equ_shr(&mut sa_tmp, &sa[..], nm, num_harm);
        rm0 = l_v_magsq(&sa_tmp[..num_harm]);
    }

    let w0 = p.fund_freq;

    let mut cos_acc: i32 = 0;
    let mut rm1: i32 = 0;
    for i in 0..num_harm {
        cos_acc = l_add(cos_acc, w0);

        cos_w[i] = cos_fxp(extract_h(cos_acc));
        rm1 = l_add(rm1, l_mpy_ls(l_mult(sa_tmp[i], sa_tmp[i]), cos_w[i]));
    }

    let rm0_s = extract_h(rm0);
    let rm1_s = extract_h(rm1);

    let l_rm0_2 = l_mult(rm0_s, rm0_s);
    let l_rm1_2 = l_mult(rm1_s, rm1_s);
    let mut l_den = l_sub(l_rm0_2, l_rm1_2);
    l_den = l_mult(extract_h(l_den), rm0_s);
    let mut nm1 = norm_l(l_den);
    l_den = l_shl(l_den, nm1);

    let mut nm2 = norm_l(w0);
    l_den = l_mpy_ls(l_den, extract_h(l_shl(w0, nm2))); // w0 * Rm0 * (Rm0^2 - Rm1^2)
    nm1 += nm2; // the denominator's total shift

    if l_den < 1 {
        return; // upstream's guard against an endless loop on invalid input
    }

    let l_sum_rm02_rm12 = l_add(l_shr(l_rm0_2, 2), l_shr(l_rm1_2, 2));
    let rm0rm1 = shr(mult_r(rm0_s, rm1_s), 1);

    for i in 0..num_harm {
        if ((i + 1) << 3) > num_harm && sa_tmp[i] != 0x0000 {
            let mut l_num = l_sub(l_sum_rm02_rm12, l_mult(rm0rm1, cos_w[i]));
            let mut tot_nm = norm_l(l_num);
            l_num = l_shl(l_num, tot_nm);
            while l_num >= l_den {
                l_num = l_shr(l_num, 1);
                tot_nm -= 1;
            }

            let mut tmp = div_s(extract_h(l_num), extract_h(l_den));
            tot_nm -= nm1;

            let mut l_tmp = l_mult(sa_tmp[i], sa_tmp[i]);
            nm2 = norm_l(l_tmp);
            l_tmp = l_shl(l_tmp, nm2);
            l_tmp = l_mult(extract_h(l_tmp), tmp);
            tot_nm += nm2;
            tot_nm -= 2;

            if tot_nm <= 0 {
                l_tmp = l_shr(l_tmp, add(8, tot_nm));
                (l_tmp, tot_nm) = sqrt_l_exp(l_tmp);
                l_tmp = l_shr(l_tmp, tot_nm);
                (l_tmp, tot_nm) = sqrt_l_exp(l_tmp);
                l_tmp = l_shr(l_tmp, tot_nm);
                l_tmp = l_mult(extract_h(l_tmp), CNST_0_9898_Q1_15);
                tmp = extract_h(l_shl(l_tmp, 1));
            } else if tot_nm <= 8 {
                l_tmp = l_shr(l_tmp, tot_nm);
                (l_tmp, tot_nm) = sqrt_l_exp(l_tmp);
                l_tmp = l_shr(l_tmp, tot_nm);
                (l_tmp, tot_nm) = sqrt_l_exp(l_tmp);
                l_tmp = l_shr(l_tmp, tot_nm + 1);
                tmp = mult(extract_h(l_tmp), CNST_0_9898_Q1_15);
            } else {
                // As upstream: this branch reuses nm1, so the harmonics after
                // it see the changed denominator shift.
                nm1 = tot_nm & !1;
                l_tmp = l_shr(l_tmp, tot_nm - nm1);
                (l_tmp, tot_nm) = sqrt_l_exp(l_tmp);
                l_tmp = l_shr(l_tmp, tot_nm);
                tot_nm = nm1 >> 1;

                nm1 = tot_nm & !1;
                l_tmp = l_shr(l_tmp, tot_nm - nm1);
                (l_tmp, tot_nm) = sqrt_l_exp(l_tmp);
                l_tmp = l_shr(l_tmp, tot_nm);
                l_tmp = l_mult(extract_h(l_tmp), CNST_0_9898_Q1_15);
                tot_nm = nm1 >> 1;
                tmp = extract_h(l_shr(l_tmp, tot_nm + 1));
            }

            if tmp > CNST_1_2_Q2_14 {
                sa[i] = extract_h(l_shl(l_mult(sa[i], CNST_1_2_Q2_14), 1));
            } else if tmp < CNST_0_5_Q2_14 {
                sa[i] = shr(sa[i], 1);
            } else {
                sa[i] = extract_h(l_shl(l_mult(sa[i], tmp), 1));
            }
        }
    }

    // The scale factor that restores the frame's energy.
    v_equ_shr(&mut sa_tmp, &sa[..], nm, num_harm);
    let l_sum_mod = l_v_magsq(&sa_tmp[..num_harm]);
    if l_sum_mod > rm0 {
        let tmp = div_s(extract_h(rm0), extract_h(l_sum_mod));
        let (l_tmp, tot_nm) = sqrt_l_exp(l_deposit_h(tmp));
        let tmp = shr(extract_h(l_tmp), tot_nm);
        for s in sa[..num_harm].iter_mut() {
            *s = mult_r(*s, tmp);
        }
    }
}
