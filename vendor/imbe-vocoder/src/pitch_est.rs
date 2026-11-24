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

//! Initial pitch estimation, with look-back and look-ahead tracking over the
//! current and two future frames. Port of `pitch_est.cc`.

use crate::basic_op::*;
use crate::math_sub::l_mpy_ls;
use crate::tables::WI;
use crate::{FRAME, ImbeParam};

/// Pitch estimation frame size.
pub const PITCH_EST_FRAME: usize = 301;
/// Candidate pitch periods: 21 to 122 samples in half-sample steps.
const NUM_P: usize = 203;

const CNST_0_8717_Q1_15: i16 = 0x6F94;
const CNST_0_0031_Q1_15: i16 = 0x0064;
const CNST_0_48_Q4_12: i16 = 0x07AE;
const CNST_1_00_Q4_12: i16 = 0x1000;
const CNST_0_85_Q4_12: i16 = 0x0D9B;
const CNST_0_4_Q4_12: i16 = 0x0666;
const CNST_0_05_Q4_12: i16 = 0x00CD;
const CNST_0_5882_Q1_15: i16 = 0x4B4B;
const CNST_0_2857_Q1_15: i16 = 0x2492;

/// For each candidate index, the range of indices the next frame's pitch
/// may move to: the first in the high byte, the last in the low byte.
const MIN_MAX_TBL: [u16; NUM_P] = [
    0x0008, 0x0009, 0x000a, 0x000c, 0x000d, 0x000e, 0x000f, 0x0010, 0x0012, 0x0013, 0x0014, 0x0115,
    0x0216, 0x0218, 0x0319, 0x041a, 0x051b, 0x061c, 0x061e, 0x071f, 0x0820, 0x0921, 0x0a22, 0x0a24,
    0x0b25, 0x0c26, 0x0d27, 0x0e28, 0x0e2a, 0x0f2b, 0x102c, 0x112d, 0x122e, 0x1230, 0x1331, 0x1432,
    0x1533, 0x1634, 0x1636, 0x1737, 0x1838, 0x1939, 0x1a3a, 0x1a3c, 0x1b3d, 0x1c3e, 0x1d3f, 0x1e40,
    0x1e42, 0x1f43, 0x2044, 0x2145, 0x2246, 0x2248, 0x2349, 0x244a, 0x254b, 0x264c, 0x264e, 0x274f,
    0x2850, 0x2951, 0x2a52, 0x2a54, 0x2b55, 0x2c56, 0x2d57, 0x2e58, 0x2e5a, 0x2f5b, 0x305c, 0x315d,
    0x325e, 0x3260, 0x3361, 0x3462, 0x3563, 0x3664, 0x3666, 0x3767, 0x3868, 0x3969, 0x3a6a, 0x3a6c,
    0x3b6d, 0x3c6e, 0x3d6f, 0x3e70, 0x3e72, 0x3f73, 0x4074, 0x4175, 0x4276, 0x4278, 0x4379, 0x447a,
    0x457b, 0x467c, 0x467e, 0x477f, 0x4880, 0x4981, 0x4a82, 0x4a84, 0x4b85, 0x4c86, 0x4d87, 0x4e88,
    0x4e8a, 0x4f8b, 0x508c, 0x518d, 0x528e, 0x5290, 0x5391, 0x5492, 0x5593, 0x5694, 0x5696, 0x5797,
    0x5898, 0x5999, 0x5a9a, 0x5a9c, 0x5b9d, 0x5c9e, 0x5d9f, 0x5ea0, 0x5ea2, 0x5fa3, 0x60a4, 0x61a5,
    0x62a6, 0x62a8, 0x63a9, 0x64aa, 0x65ab, 0x66ac, 0x66ae, 0x67af, 0x68b0, 0x69b1, 0x6ab2, 0x6ab4,
    0x6bb5, 0x6cb6, 0x6db7, 0x6eb8, 0x6eba, 0x6fbb, 0x70bc, 0x71bd, 0x72be, 0x72c0, 0x73c1, 0x74c2,
    0x75c3, 0x76c4, 0x76c6, 0x77c7, 0x78c8, 0x79c9, 0x7aca, 0x7aca, 0x7bca, 0x7cca, 0x7dca, 0x7eca,
    0x7eca, 0x7fca, 0x80ca, 0x81ca, 0x82ca, 0x82ca, 0x83ca, 0x84ca, 0x85ca, 0x86ca, 0x86ca, 0x87ca,
    0x88ca, 0x89ca, 0x8aca, 0x8aca, 0x8bca, 0x8cca, 0x8dca, 0x8eca, 0x8eca, 0x8fca, 0x90ca, 0x91ca,
    0x92ca, 0x92ca, 0x93ca, 0x94ca, 0x95ca, 0x96ca, 0x96ca, 0x97ca, 0x98ca, 0x99ca, 0x9aca,
];

fn hi_byte(a: u16) -> usize {
    usize::from((a >> 8) & 0xFF)
}

fn lo_byte(a: u16) -> usize {
    usize::from(a & 0xFF)
}

/// The tracker's memory of the last two frames.
#[derive(Clone, Debug)]
pub struct PitchEst {
    prev_pitch: usize,
    #[allow(dead_code)] // upstream keeps it up to date and never reads it
    prev_prev_pitch: usize,
    prev_e_p: i16,
    prev_prev_e_p: i16,
}

impl PitchEst {
    /// Upstream's `pitch_est_init`.
    pub fn new() -> Self {
        PitchEst {
            prev_pitch: 158,
            prev_prev_pitch: 158,
            prev_e_p: 0,
            prev_prev_e_p: 0,
        }
    }

    /// Sets `p.pitch` (Q15.1) and `p.e_p` from `frames_buf`, which holds the
    /// current frame followed by the two future ones.
    pub fn pitch_est(&mut self, p: &mut ImbeParam, frames_buf: &[i16]) {
        let mut e_p_arr0 = [0i16; NUM_P];
        let mut e_p_arr1 = [0i16; NUM_P];
        let mut e_p_arr2 = [0i16; NUM_P];
        let mut e1p1_e2p2_est_save = [0i16; NUM_P];
        let mut e_p_arr2_min = [0i16; NUM_P];

        // E(p) for the current frame.
        e_p(&frames_buf[..PITCH_EST_FRAME], &mut e_p_arr0);

        // Look-back pitch tracking.
        let min_index = hi_byte(MIN_MAX_TBL[self.prev_pitch]);
        let max_index = lo_byte(MIN_MAX_TBL[self.prev_pitch]);

        let mut pb = min_index;
        let mut e_p_cur = e_p_arr0[min_index];
        for (q, &e) in e_p_arr0
            .iter()
            .enumerate()
            .take(max_index + 1)
            .skip(min_index + 1)
        {
            if e < e_p_cur {
                e_p_cur = e;
                pb = q;
            }
        }
        let ceb = add(e_p_cur, add(self.prev_e_p, self.prev_prev_e_p));

        if ceb <= CNST_0_48_Q4_12 {
            self.prev_prev_pitch = self.prev_pitch;
            self.prev_pitch = pb;
            self.prev_prev_e_p = self.prev_e_p;
            self.prev_e_p = e_p_arr0[pb];

            p.pitch = pb as i16 + 42; // Q15.1
            p.e_p = self.prev_e_p;
            return;
        }

        // Look-ahead pitch tracking.
        e_p(&frames_buf[FRAME..FRAME + PITCH_EST_FRAME], &mut e_p_arr1);
        e_p(
            &frames_buf[2 * FRAME..2 * FRAME + PITCH_EST_FRAME],
            &mut e_p_arr2,
        );

        // Upstream adds these as `int` and stores the sum in a Word16.
        let mut p0_est = 0usize;
        let mut cef_est =
            (i32::from(e_p_arr0[0]) + i32::from(e_p_arr1[0]) + i32::from(e_p_arr2[0])) as i16;

        for p1 in 0..NUM_P {
            let mut s_tmp = e_p_arr2[p1];
            for &e in &e_p_arr2[hi_byte(MIN_MAX_TBL[p1])..=lo_byte(MIN_MAX_TBL[p1])] {
                if e < s_tmp {
                    s_tmp = e;
                }
            }
            e_p_arr2_min[p1] = s_tmp;
        }
        for p0 in 0..NUM_P {
            let mut e1p1_e2p2_est = (i32::from(e_p_arr1[p0]) + i32::from(e_p_arr2_min[p0])) as i16;
            for p1 in hi_byte(MIN_MAX_TBL[p0])..=lo_byte(MIN_MAX_TBL[p0]) {
                let s = add(e_p_arr1[p1], e_p_arr2_min[p1]);
                if s < e1p1_e2p2_est {
                    e1p1_e2p2_est = s;
                }
            }
            e1p1_e2p2_est_save[p0] = e1p1_e2p2_est;
            let cef = add(e_p_arr0[p0], e1p1_e2p2_est);
            if cef < cef_est {
                cef_est = cef;
                p0_est = p0;
            }
        }

        let mut pf = p0_est;
        // Sub-multiples analysis.
        if pf >= 42 {
            let count = if pf < 84 {
                1
            } else if pf < 126 {
                2
            } else if pf < 168 {
                3
            } else {
                4
            };

            // The estimate's period (Q15.1) shifted up by 8, unsigned.
            let p_fp: u16 = ((pf + 42) << 8) as u16;

            for i in (0..count).rev() {
                let tmp: u16 = match i {
                    0 => p_fp >> 1,                                 // P_est/2
                    1 => ((u32::from(p_fp) * 0x5555) >> 16) as u16, // P_est/3
                    2 => p_fp >> 2,                                 // P_est/4
                    _ => ((u32::from(p_fp) * 0x3333) >> 16) as u16, // P_est/5
                };

                // Back to an array index, rounded.
                let p_index = ((usize::from(tmp) + 0x0080) >> 8) - 42;

                let cef = add(e_p_arr0[p_index], e1p1_e2p2_est_save[p_index]);

                if cef <= CNST_0_85_Q4_12 && mult_r(cef, CNST_0_5882_Q1_15) <= cef_est {
                    pf = p_index; // 1/1.7 = 0.5882
                    break;
                }
                if cef <= CNST_0_4_Q4_12 && mult_r(cef, CNST_0_2857_Q1_15) <= cef_est {
                    pf = p_index; // 1/3.5 = 0.2857
                    break;
                }
                if cef <= CNST_0_05_Q4_12 {
                    pf = p_index;
                    break;
                }
            }
        }

        let cef = add(e_p_arr0[pf], e1p1_e2p2_est_save[pf]);
        let chosen = if ceb <= cef { pb } else { pf };

        self.prev_prev_pitch = self.prev_pitch;
        self.prev_pitch = chosen;
        self.prev_prev_e_p = self.prev_e_p;
        self.prev_e_p = e_p_arr0[chosen];

        p.pitch = chosen as i16 + 42; // Q15.1
        p.e_p = self.prev_e_p;
    }
}

/// Autocorrelation of `sigin` at lag `shift`, each product scaled down by
/// `scale_shift`.
fn autocorr(sigin: &[i16], shift: usize, scale_shift: i16) -> i32 {
    let mut l_sum: i32 = 0;
    for i in 0..PITCH_EST_FRAME - shift {
        l_sum = l_add(
            l_sum,
            l_shr(l_mult(sigin[i], sigin[i + shift]), scale_shift),
        );
    }
    l_sum
}

/// The error function E(p) for every candidate period, in Q4.12.
fn e_p(sigin: &[i16], res_buf: &mut [i16; NUM_P]) {
    let mut sig_wndwed = [0i16; PITCH_EST_FRAME];
    let mut corr = [0i32; 259];

    // The input windowed by wi^2: s * wi^2.
    for ((w, &s), &wi) in sig_wndwed.iter_mut().zip(sigin).zip(&WI) {
        *w = mult_r(s, wi);
    }

    // sum(s^2 * wi^2)
    let mut l_sum: i32 = 0;
    for (&s, &wi) in sigin.iter().zip(&WI) {
        l_sum = l_add(l_sum, l_mpy_ls(l_mult(s, s), wi));
    }

    // On overflow, recalculate with scaling.
    let scale_shift: i16 = if l_sum == MAX_32 {
        l_sum = 0;
        for (&s, &wi) in sigin.iter().zip(&WI) {
            l_sum = l_add(l_sum, l_mpy_ls(l_shr(l_mult(s, s), 5), wi));
        }
        5
    } else {
        0
    };

    // sum(s^2 * wi^4)
    let mut l_e0: i32 = 0;
    for &w in &sig_wndwed {
        l_e0 = l_add(l_e0, l_shr(l_mult(w, w), scale_shift));
    }

    // The correlation at lags 21..150 in steps of 0.5: integer lags first,
    // then the half-sample lags between them.
    for (i, lag) in (21..=150).enumerate() {
        corr[2 * i] = autocorr(&sig_wndwed, lag, scale_shift);
    }
    for i in (1..258).step_by(2) {
        corr[i] = l_shr(l_add(corr[i - 1], corr[i + 1]), 1);
    }

    // For 1 - P * sum(wi^4) in the denominator.
    let mut den_part_acc = CNST_0_8717_Q1_15;

    l_e0 = l_shr(l_e0, 7); // divide by 64 to compensate the wi scaling

    // p = 21 .. 122 in steps of 0.5.
    for (index_beg, r) in res_buf.iter_mut().enumerate() {
        // 42 is 21 in Q15.1, so index_step is also p in Q15.1.
        let index_step = 42 + index_beg as i32;

        // sum(corr(n * p))
        let mut l_tmp: i32 = 0;
        let mut j = index_beg;
        while j <= 258 {
            l_tmp = l_add(l_tmp, corr[j]);
            j += index_step as usize;
        }

        l_tmp = l_shr(l_tmp, 6); // compensate the wi scaling
        l_tmp = l_add(l_tmp, l_e0); // for n = 0
        // A plain `int` multiply upstream; it wraps rather than saturates.
        l_tmp = l_tmp.wrapping_mul(index_step);
        let l_num = l_sub(l_sum, l_tmp);

        let l_den = l_mpy_ls(l_sum, den_part_acc);

        *r = if l_num < l_den && l_den != 0 {
            if l_num <= 0 {
                0
            } else {
                let tmp = norm_l(l_den);
                let q = div_s(extract_h(l_shl(l_num, tmp)), extract_h(l_shl(l_den, tmp)));
                shr(q, 3) // Q4.12
            }
        } else {
            CNST_1_00_Q4_12
        };

        den_part_acc = sub(den_part_acc, CNST_0_0031_Q1_15);
    }
}
