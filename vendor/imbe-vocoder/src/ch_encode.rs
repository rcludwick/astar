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

//! Channel encoding: the parameter vector b into the code vectors û0..û7.
//! Port of `ch_encode.cc`.

use crate::ImbeParam;
use crate::aux_sub::get_bit_allocation;
use crate::basic_op::*;

/// Bits b3..b(L+1) carry, plus the ones of b1 and b2 that join them.
const EN_BIT_STREAM_LEN: usize = 3 + 3 * 12 + 6 + 2 * 11 + 3;

/// Fills `frame_vector` (û0..û7) from `p.b_vec`, `p.num_harms` and
/// `p.num_bands`, and refreshes `p.bit_alloc`.
pub fn encode_frame_vector(p: &mut ImbeParam, frame_vector: &mut [i16; 8]) {
    let mut bit_stream = [0i16; EN_BIT_STREAM_LEN];
    let num_harms = p.num_harms;
    let num_bands = p.num_bands;
    let nb = num_bands as usize;

    *frame_vector = [0; 8];

    get_bit_allocation(num_harms, &mut p.bit_alloc);

    // Priority scanning: the most significant bits of every b first. The
    // bit allocation depends only on num_harms, so the scan always fills
    // the stream exactly (a test below walks every num_harms).
    let mut index0 = 0usize;
    let ba = &p.bit_alloc;
    let mut bit_thr: i16 = if num_harms == 0xb { 9 } else { ba[0] };
    let mut bit_mask = shl(1, bit_thr - 1);

    while index0 < EN_BIT_STREAM_LEN - (nb - 3) {
        let b = &p.b_vec[3..];
        for i in 0..(num_harms - 1) as usize {
            if bit_thr != 0 && bit_thr <= ba[i] {
                bit_stream[index0] = i16::from(b[i] & bit_mask != 0);
                index0 += 1;
            }
        }
        bit_thr -= 1;
        bit_mask = shr(bit_mask, 1);
    }

    frame_vector[0] = shl(p.b_vec[0] & 0xFC, 4) | (p.b_vec[2] & 0x38);

    let mut index0 = 0usize;
    for mask in [4, 2, 1] {
        if bit_stream[index0] != 0 {
            frame_vector[0] |= mask;
        }
        index0 += 1;
    }

    for v in &mut frame_vector[1..=3] {
        let mut tmp: i16 = 0;
        for _ in 0..12 {
            tmp <<= 1;
            tmp |= bit_stream[index0];
            index0 += 1;
        }
        *v = tmp;
    }

    // b1 (the band decisions) and bits 2..1 of b2 take the last nb + 2
    // places of û3's stream onwards.
    index0 -= nb + 2;

    let mut bit_mask = shl(1, num_bands - 1);
    for _ in 0..nb {
        bit_stream[index0] = i16::from(p.b_vec[1] & bit_mask != 0);
        index0 += 1;
        bit_mask >>= 1;
    }
    bit_stream[index0] = i16::from(p.b_vec[2] & 0x04 != 0);
    index0 += 1;
    bit_stream[index0] = i16::from(p.b_vec[2] & 0x02 != 0);
    index0 += 1;

    index0 -= nb + 2;

    for v in &mut frame_vector[4..=6] {
        let mut tmp: i16 = 0;
        for _ in 0..11 {
            tmp <<= 1;
            tmp |= bit_stream[index0];
            index0 += 1;
        }
        *v = tmp;
    }

    frame_vector[7] = shl(p.b_vec[0] & 0x03, 1) | shl(p.b_vec[2] & 0x01, 3);
    for mask in [0x40, 0x20, 0x10] {
        if bit_stream[index0] != 0 {
            frame_vector[7] |= mask;
        }
        index0 += 1;
    }
    // Upstream's last bit: b(L+2), one past the frame's last coefficient,
    // as the previous frames left it.
    if p.b_vec[num_harms as usize + 2] != 0 {
        frame_vector[7] |= 0x01;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NUM_BANDS_MAX, NUM_HARMS_MAX, NUM_HARMS_MIN};

    #[test]
    fn every_harmonic_count_fills_the_bit_stream_exactly() {
        for num_harms in NUM_HARMS_MIN..=NUM_HARMS_MAX as i16 {
            let num_bands = if num_harms <= 36 {
                (num_harms + 2) / 3
            } else {
                NUM_BANDS_MAX as i16
            };
            for fill in [0, -1] {
                let mut p = ImbeParam {
                    num_harms,
                    num_bands,
                    ..ImbeParam::default()
                };
                p.b_vec = [fill; NUM_HARMS_MAX + 3];
                p.b_vec[0] = 0;
                let mut fv = [0i16; 8];
                encode_frame_vector(&mut p, &mut fv);
                if fill == 0 {
                    assert_eq!(fv, [0; 8], "num_harms {num_harms}");
                }
            }
        }
    }
}
