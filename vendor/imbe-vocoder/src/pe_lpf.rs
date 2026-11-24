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

//! The low-pass filter in front of the pitch estimator. Port of `pe_lpf.cc`.

use crate::basic_op::*;

/// Order of the pitch estimation low-pass filter.
pub const PE_LPF_ORD: usize = 21;

const LPF_COEF: [i16; PE_LPF_ORD] = [
    -94, -92, 185, 543, 288, -883, -1834, -495, 3891, 9141, 11512, 9141, 3891, -495, -1834, -883,
    288, 543, 185, -92, -94,
];

/// Filters `sigin` into `sigout` (the same length), carrying the filter's
/// delay line in `mem` from one call to the next.
pub fn pe_lpf(sigin: &[i16], sigout: &mut [i16], mem: &mut [i16; PE_LPF_ORD]) {
    for (o, &s) in sigout.iter_mut().zip(sigin) {
        mem.copy_within(1.., 0);
        mem[PE_LPF_ORD - 1] = s;

        let mut l_sum: i32 = 0;
        for (&m, &c) in mem.iter().zip(&LPF_COEF) {
            l_sum = l_mac(l_sum, m, c);
        }
        *o = round(l_sum);
    }
}
