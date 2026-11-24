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

//! The 88-bit IMBE frame as it travels in MMDVMHost and G4KLX's P25 gateway
//! software: the eight code vectors û0..û7 packed into 11 bytes, û0..û3 at 12
//! bits each, û4..û6 at 11 bits, û7 at 7 bits, every field most significant
//! bit first. This file is astar's addition; upstream works on the `u[8]`
//! vectors directly.

use crate::FRAME_BYTES;

/// The width in bits of each code vector, in order.
const WIDTHS: [u32; 8] = [12, 12, 12, 12, 11, 11, 11, 7];

/// Splits an 11-byte frame into the code vectors û0..û7.
pub fn unpack(frame: &[u8; FRAME_BYTES]) -> [i16; 8] {
    let mut u = [0i16; 8];
    let mut bit = 0usize;
    for (v, &w) in u.iter_mut().zip(&WIDTHS) {
        let mut x = 0i16;
        for _ in 0..w {
            x = (x << 1) | i16::from((frame[bit / 8] >> (7 - bit % 8)) & 1);
            bit += 1;
        }
        *v = x;
    }
    u
}

/// Packs the code vectors û0..û7 into an 11-byte frame. Bits above each
/// field's width are ignored.
pub fn pack(u: &[i16; 8]) -> [u8; FRAME_BYTES] {
    let mut out = [0u8; FRAME_BYTES];
    let mut bit = 0usize;
    for (&v, &w) in u.iter().zip(&WIDTHS) {
        for b in (0..w).rev() {
            if (v >> b) & 1 != 0 {
                out[bit / 8] |= 0x80 >> (bit % 8);
            }
            bit += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_null_frame_unpacks_and_repacks() {
        let null = [
            0x04, 0x0C, 0xFD, 0x7B, 0xFB, 0x7D, 0xF2, 0x7B, 0x3D, 0x9E, 0x45,
        ];
        let u = unpack(&null);
        assert_eq!(u[0], 0x040);
        assert_eq!(pack(&u), null);
    }
}
