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

//! The high-pass filter that removes DC from the encoder's input. Port of
//! `dc_rmv.cc`.

use crate::basic_op::*;
use crate::math_sub::l_mpy_ls;

/// 0.99 in Q1.15.
const CNST_0_99_Q1_15: i16 = 0x7EB8;

/// Filters `sigin` into `sigout` (the same length), carrying the filter's
/// state in `mem` from one call to the next.
pub fn dc_rmv(sigin: &[i16], sigout: &mut [i16], mem: &mut i32) {
    let mut l_mem = *mem;
    for (o, &s) in sigout.iter_mut().zip(sigin) {
        let l_tmp = l_deposit_h(s);
        l_mem = l_add(l_mem, l_tmp);
        *o = round(l_mem);
        l_mem = l_mpy_ls(l_mem, CNST_0_99_Q1_15);
        l_mem = l_sub(l_mem, l_tmp);
    }
    *mem = l_mem;
}
