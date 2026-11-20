// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A test SSTV signal: a Robot 36 picture of colour bars, made by the `sstv`
//! crate's encoder. That crate is a dev-dependency only, so it never reaches
//! a shipped build: astar decodes SSTV and never sends it.
//!
//! Shared by the session tests (`src/session.rs` includes this file by path)
//! and the end-to-end tests.

#![allow(dead_code, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

/// Robot 36's picture size, as the encoder sends it.
const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;

/// The test picture's pixel at (`x`, `y`): seven colour bars over a grey ramp.
fn pixel(x: u32, y: u32) -> sstv::RgbPixel {
    const BARS: [[u8; 3]; 7] = [
        [191, 191, 191],
        [191, 191, 0],
        [0, 191, 191],
        [0, 191, 0],
        [191, 0, 191],
        [191, 0, 0],
        [0, 0, 191],
    ];
    if y < HEIGHT * 2 / 3 {
        let [red, green, blue] = BARS[(x * 7 / WIDTH) as usize];
        sstv::RgbPixel::new(red, green, blue)
    } else {
        let v = (x * 255 / (WIDTH - 1)) as u8;
        sstv::RgbPixel::new(v, v, v)
    }
}

/// The whole Robot 36 transmission, header included, as 16-bit samples at
/// `rate`.
#[must_use]
pub fn robot36(rate: u32) -> Vec<i16> {
    let pixels: Vec<sstv::RgbPixel> = (0..HEIGHT)
        .flat_map(|y| (0..WIDTH).map(move |x| pixel(x, y)))
        .collect();
    let encoder = sstv::Encoder::new(sstv::modes::ROBOT_36, pixels.into_iter()).expect("encoder");
    sstv::Synthesizer::new(encoder, rate).collect()
}

/// How many milliseconds Robot 36's calibration header and VIS code last.
#[must_use]
pub fn robot36_header_ms() -> u64 {
    let ns: u64 = sstv::modes::ROBOT_36
        .header_tones()
        .map(|t| t.duration.ns())
        .sum();
    ns / 1_000_000
}
