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

//! Voiced speech synthesis, port of `v_synt.cc`.
//!
//! Phases are unsigned Q0.32 fractions of a turn, so their sums wrap by
//! design; that arithmetic is spelled `wrapping_*` here.

use crate::basic_op::*;
use crate::math_sub::{cos_fxp, l_mpy_ls};
use crate::rand_gen::RandGen;
use crate::tables::WS;
use crate::{FRAME, ImbeParam, NUM_HARMS_MAX};

const CNST_0_1_Q1_15: i16 = 0x0CCD;

/// The voiced synthesiser's memory of the previous frame.
#[derive(Clone, Debug)]
pub struct VSynt {
    ph_mem: [u32; NUM_HARMS_MAX],
    vu_dsn_prev: [i16; NUM_HARMS_MAX],
    sa_prev3: [i16; NUM_HARMS_MAX],
    num_harms_prev3: i16,
    fund_freq_prev: i32,
}

/// `((x >> s) * k) << s`: a phase times an integer, modulo one turn.
#[inline]
fn mul_mod(x: i32, s: u32, k: i32) -> i32 {
    (x >> s).wrapping_mul(k).wrapping_shl(s)
}

impl VSynt {
    /// Upstream's `v_synt_init`: random initial phases.
    pub fn new(rng: &mut RandGen) -> Self {
        let mut ph_mem = [0u32; NUM_HARMS_MAX];
        for ph in ph_mem.iter_mut() {
            *ph = l_deposit_h(rng.rand_gen()) as u32;
        }
        VSynt {
            ph_mem,
            vu_dsn_prev: [0; NUM_HARMS_MAX],
            sa_prev3: [0; NUM_HARMS_MAX],
            num_harms_prev3: 0,
            fund_freq_prev: 0,
        }
    }

    /// Synthesises the voiced part of one frame.
    pub fn v_synt(&mut self, p: &ImbeParam, rng: &mut RandGen) -> [i16; FRAME] {
        let fund_freq = p.fund_freq;
        let fund_freq_prev = self.fund_freq_prev;
        let num_harms = p.num_harms;
        let vu_dsn = &p.v_uv_dsn;
        let sa = &p.sa;
        let num_harms_inv = p.div_one_by_num_harm;
        let num_harms_sh = p.div_one_by_num_harm_sh;
        let num_uv = p.l_uv;
        let sa_prev3 = &self.sa_prev3;

        let mut l_snd = [0i32; FRAME];
        let mut ph_mem_prev = [0u32; NUM_HARMS_MAX];
        let mut dph = [0u32; NUM_HARMS_MAX];

        // Advance the phases to the frame boundary (integer multiply, mod 1).
        let l_tmp = (fund_freq_prev.wrapping_add(fund_freq) >> 7)
            .wrapping_mul(FRAME as i32)
            .wrapping_div(2)
            .wrapping_shl(7);

        let mut l_ph_acc: i32 = 0;
        for i in 0..NUM_HARMS_MAX {
            ph_mem_prev[i] = self.ph_mem[i];
            l_ph_acc = l_ph_acc.wrapping_add(l_tmp);
            self.ph_mem[i] = self.ph_mem[i].wrapping_add(l_ph_acc as u32);
        }

        let num_harms_max = num_harms.max(self.num_harms_prev3);
        let num_harms_max_4 = num_harms_max >> 2;

        let freq_flag =
            l_abs(l_sub(fund_freq, fund_freq_prev)) >= l_mpy_ls(fund_freq, CNST_0_1_Q1_15);

        let mut l_ph_step: i32 = 0;
        let mut l_ph_step_prev: i32 = 0;
        for i in 0..num_harms_max as usize {
            l_ph_step = l_ph_step.wrapping_add(fund_freq);
            l_ph_step_prev = l_ph_step_prev.wrapping_add(fund_freq_prev);

            if i as i16 > num_harms_max_4 {
                if num_uv == num_harms {
                    dph[i] = l_deposit_h(rng.rand_gen()) as u32;
                } else {
                    let l_tmp = l_mult(rng.rand_gen(), num_harms_inv);
                    dph[i] = l_shr(l_tmp, 15 - num_harms_sh).wrapping_mul(i32::from(num_uv)) as u32;
                }
                self.ph_mem[i] = self.ph_mem[i].wrapping_add(dph[i]);
            }

            let (now, before) = (vu_dsn[i], self.vu_dsn_prev[i]);

            if now == 0 && before == 0 {
                continue;
            }

            if now == 1 && before == 0 {
                // unvoiced => voiced: fade the new harmonic in.
                let mut l_ph_acc =
                    self.ph_mem[i].wrapping_sub(mul_mod(l_ph_step, 7, 104) as u32) as i32;
                for (k, j) in (56..=104).enumerate() {
                    let mut t = l_mult(WS[k], sa[i]);
                    t = l_mpy_ls(t, cos_fxp(extract_h(l_ph_acc)));
                    t = l_shr(t, 1);
                    l_snd[j] = l_add(l_snd[j], t);
                    l_ph_acc = l_ph_acc.wrapping_add(l_ph_step);
                }
                for s in l_snd[105..=159].iter_mut() {
                    let mut t = l_mult(sa[i], cos_fxp(extract_h(l_ph_acc)));
                    t = l_shr(t, 1);
                    *s = l_add(*s, t);
                    l_ph_acc = l_ph_acc.wrapping_add(l_ph_step);
                }
                continue;
            }

            if now == 0 && before == 1 {
                // voiced => unvoiced: fade the old harmonic out.
                let mut l_ph_acc = ph_mem_prev[i] as i32;
                for s in l_snd[0..=55].iter_mut() {
                    let mut t = l_mult(sa_prev3[i], cos_fxp(extract_h(l_ph_acc)));
                    t = l_shr(t, 1);
                    *s = l_add(*s, t);
                    l_ph_acc = l_ph_acc.wrapping_add(l_ph_step_prev);
                }
                for (k, j) in (56..=104).enumerate() {
                    let mut t = l_mult(WS[48 - k], sa_prev3[i]);
                    t = l_mpy_ls(t, cos_fxp(extract_h(l_ph_acc)));
                    t = l_shr(t, 1);
                    l_snd[j] = l_add(l_snd[j], t);
                    l_ph_acc = l_ph_acc.wrapping_add(l_ph_step_prev);
                }
                continue;
            }

            if i >= 7 || freq_flag {
                // Cross-fade the old harmonic out and the new one in.
                let mut l_ph_acc_aux = ph_mem_prev[i] as i32;
                let mut l_ph_acc =
                    self.ph_mem[i].wrapping_sub(mul_mod(l_ph_step, 7, 104) as u32) as i32;

                for s in l_snd[0..=55].iter_mut() {
                    let mut t = l_mult(sa_prev3[i], cos_fxp(extract_h(l_ph_acc_aux)));
                    t = l_shr(t, 1);
                    *s = l_add(*s, t);
                    l_ph_acc_aux = l_ph_acc_aux.wrapping_add(l_ph_step_prev);
                }

                for (k, j) in (56..=104).enumerate() {
                    let mut t = l_mult(WS[48 - k], sa_prev3[i]);
                    t = l_mpy_ls(t, cos_fxp(extract_h(l_ph_acc_aux)));
                    t = l_shr(t, 1);
                    l_snd[j] = l_add(l_snd[j], t);

                    let mut t = l_mult(WS[k], sa[i]);
                    t = l_mpy_ls(t, cos_fxp(extract_h(l_ph_acc)));
                    t = l_shr(t, 1);
                    l_snd[j] = l_add(l_snd[j], t);

                    l_ph_acc_aux = l_ph_acc_aux.wrapping_add(l_ph_step_prev);
                    l_ph_acc = l_ph_acc.wrapping_add(l_ph_step);
                }

                for s in l_snd[105..=159].iter_mut() {
                    let mut t = l_mult(sa[i], cos_fxp(extract_h(l_ph_acc)));
                    t = l_shr(t, 1);
                    *s = l_add(*s, t);
                    l_ph_acc = l_ph_acc.wrapping_add(l_ph_step);
                }
                continue;
            }

            // A low, steady harmonic: interpolate amplitude and phase across
            // the whole frame.
            // (sa[i] - sa_prev3[i]) / 160, as 1/160 = 0.1/16
            let l_amp_step = l_mpy_ls(
                l_shr(l_deposit_h(sub(sa[i], sa_prev3[i])), 4 + 1),
                CNST_0_1_Q1_15,
            );
            let mut l_amp_acc = l_shr(l_deposit_h(sa_prev3[i]), 1);

            // (fund_freq - fund_freq_prev) / (2 * 160)
            let mut l_ph_step_aux = l_mpy_ls(
                l_shr(fund_freq.wrapping_sub(fund_freq_prev), 4 + 1),
                CNST_0_1_Q1_15,
            );
            l_ph_step_aux = mul_mod(l_ph_step_aux, 7, i as i32 + 1);

            let mut l_ph_acc = ph_mem_prev[i] as i32;

            let l_tmp1 = l_mpy_ls(l_shr(dph[i] as i32, 4), CNST_0_1_Q1_15); // dph[i] / 160

            for (j, s) in l_snd.iter_mut().enumerate() {
                let mut l_ph_acc_aux = mul_mod(l_ph_step_aux, 9, j as i32);
                l_ph_acc_aux = mul_mod(l_ph_acc_aux, 9, j as i32);

                let t = l_mpy_ls(
                    l_amp_acc,
                    cos_fxp(extract_h(l_ph_acc.wrapping_add(l_ph_acc_aux))),
                );
                *s = l_add(*s, t);

                l_amp_acc = l_add(l_amp_acc, l_amp_step);
                l_ph_acc = l_ph_acc.wrapping_add(l_ph_step_prev);
                l_ph_acc = l_ph_acc.wrapping_add(l_tmp1);
            }
        }

        let mut snd = [0i16; FRAME];
        for (o, &l) in snd.iter_mut().zip(&l_snd) {
            *o = extract_h(l);
        }

        let n = num_harms as usize;
        self.vu_dsn_prev.fill(0);
        self.vu_dsn_prev[..n].copy_from_slice(&p.v_uv_dsn[..n]);
        self.sa_prev3[..n].copy_from_slice(&p.sa[..n]);

        self.num_harms_prev3 = num_harms;
        self.fund_freq_prev = fund_freq;
        snd
    }
}
