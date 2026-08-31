// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! What does the neural denoise stage cost per frame, on THIS machine?
//!
//! `docs/design/noise-suppression.md` quotes 36.5 µs/frame on Apple silicon
//! and is explicit that scaling that to a Pi is a guess. This exists so the
//! number can be measured on the target instead:
//!
//! ```text
//! cargo run --release -p astar-audio --example denoise_bench
//! ```
//!
//! Release mode matters — a debug build measures the wrong thing by an order
//! of magnitude. The budget is 10 ms per frame, because that is how much
//! audio a frame is.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    // Sample counts and rates crossing into f32/f64 for signal generation and
    // timing arithmetic — the same class `resample.rs` allows module-wide.
)]

use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Instant;

use astar_audio::{RNNOISE_FRAME, RNNOISE_RATE, RnnoiseStage};

fn main() {
    if cfg!(debug_assertions) {
        eprintln!("warning: debug build — rerun with --release for a meaningful number\n");
    }

    // Speech-ish: a 200 Hz fundamental with harmonics, plus broadband hiss,
    // so the network has both something to keep and something to remove.
    let frames = 2_000;
    let n = frames * RNNOISE_FRAME;
    let mut seed = 0x2545_F491_4F6C_DD1D_u64;
    let input: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f32 / RNNOISE_RATE as f32;
            let voice = (0..4)
                .map(|h| {
                    let f = 200.0 * (h + 1) as f32;
                    (std::f32::consts::TAU * f * t).sin() / (h + 1) as f32
                })
                .sum::<f32>()
                * 0.25;
            // xorshift, so the hiss is deterministic across runs and targets.
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let hiss = ((seed >> 40) as f32 / 8_388_608.0 - 1.0) * 0.05;
            voice + hiss
        })
        .collect();

    let mut stage = RnnoiseStage::new(Arc::new(AtomicU32::new(0)));

    // Warm up off the clock: first call builds the FFT planner and the
    // thread-local scratch caches.
    let mut warm = input[..RNNOISE_FRAME * 2].to_vec();
    stage.process(&mut warm);

    let mut buf = Vec::with_capacity(RNNOISE_FRAME);
    let start = Instant::now();
    for chunk in input.chunks_exact(RNNOISE_FRAME) {
        buf.clear();
        buf.extend_from_slice(chunk);
        stage.process(&mut buf);
    }
    let elapsed = start.elapsed();

    let per_frame = elapsed.as_secs_f64() / frames as f64;
    let budget = f64::from(RNNOISE_FRAME as u32) / f64::from(RNNOISE_RATE);
    println!("frames:      {frames}");
    println!("total:       {:.1} ms", elapsed.as_secs_f64() * 1e3);
    println!("per frame:   {:.1} µs", per_frame * 1e6);
    println!("budget:      {:.1} ms (one frame of audio)", budget * 1e3);
    println!("one core:    {:.3} %", per_frame / budget * 100.0);
}
