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

//! The vocoder's pseudo-random generator, port of `rand_gen.cc`.
//!
//! Upstream keeps the seed in a file-static variable shared by every
//! vocoder in the process; here each owner has its own.

/// A Park-Miller minimal standard generator (multiplier 16807, modulus
/// 2^31 - 1), seeded with 1.
#[derive(Clone, Debug)]
pub struct RandGen {
    seed: u32,
}

impl RandGen {
    pub fn new() -> Self {
        RandGen { seed: 1 }
    }

    /// The next pseudo-random number, in signed Q1.15. Upstream's
    /// `rand_gen`.
    pub fn rand_gen(&mut self) -> i16 {
        let mut lo: u32 = 16807 * (self.seed & 0xFFFF);
        let hi: u32 = 16807 * (self.seed >> 16);

        lo += (hi & 0x7FFF) << 16;
        lo += hi >> 15;

        if lo > 0x7FFF_FFFF {
            lo -= 0x7FFF_FFFF;
        }

        self.seed = lo;
        lo as i16
    }
}
