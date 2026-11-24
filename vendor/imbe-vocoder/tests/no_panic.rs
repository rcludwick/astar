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

//! Hostile input never panics. P25 frames arrive from the network, so a panic
//! in `decode` would be a remote crash; these sweeps run in the test profile,
//! which keeps overflow checks and debug assertions on, so an arithmetic
//! overflow or an out-of-range index anywhere in the port fails them.

use imbe_vocoder::{FRAME_BYTES, FRAME_SAMPLES, ImbeDecoder, ImbeEncoder, frame};

/// Random frames for the decoder sweep.
const RANDOM_FRAMES: usize = 200_000;
/// Random and extreme input frames for the encoder sweep.
const RANDOM_PCM_FRAMES: usize = 4_000;

/// xorshift64*, so the sweeps are the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn bytes(&mut self) -> [u8; FRAME_BYTES] {
        let mut f = [0u8; FRAME_BYTES];
        for b in &mut f {
            *b = (self.next() >> 56) as u8;
        }
        f
    }
}

/// `frame` with its 8-bit pitch index b0 set to `b0`. b0 lives in the top
/// six bits of û0 and bits 2..1 of û7.
fn with_pitch(frame: [u8; FRAME_BYTES], b0: u8) -> [u8; FRAME_BYTES] {
    let mut u = frame::unpack(&frame);
    u[0] = (u[0] & 0x03F) | (i16::from(b0 >> 2) << 6);
    u[7] = (u[7] & !0x6) | (i16::from(b0 & 3) << 1);
    frame::pack(&u)
}

#[test]
fn the_sweeps_run_with_overflow_checks_on() {
    // The test profile is optimised (Cargo.toml) but must keep the checks
    // that make these sweeps mean something.
    let max = std::hint::black_box(i16::MAX);
    assert!(
        std::panic::catch_unwind(|| max + 1).is_err(),
        "overflow checks"
    );
    let no = std::hint::black_box(false);
    assert!(
        std::panic::catch_unwind(|| debug_assert!(no)).is_err(),
        "debug assertions"
    );
}

#[test]
fn with_pitch_sets_b0() {
    for b0 in 0..=255u8 {
        let u = frame::unpack(&with_pitch([0xA5; FRAME_BYTES], b0));
        let got = ((u[0] >> 4) & 0xFC) | ((u[7] >> 1) & 0x3);
        assert_eq!(got, i16::from(b0));
    }
}

#[test]
fn decode_returns_for_every_pitch_with_the_harmonic_count_stepped() {
    // b0 0 is the shortest pitch period, so the fewest harmonics (9); b0 207
    // the longest, so the most (56); past 207 is invalid and repeats a frame.
    // Every b0 is decoded straight after each extreme, so the harmonic count
    // jumps both ways into every value, with the other bits all clear, all
    // set and random.
    let mut rng = Rng(0x1D5E_A5E5_0000_0001);
    let mut dec = ImbeDecoder::new();
    for fill in 0..3 {
        for b0 in 0..=255u8 {
            for prev in [0u8, 207] {
                let body = match fill {
                    0 => [0x00; FRAME_BYTES],
                    1 => [0xFF; FRAME_BYTES],
                    _ => rng.bytes(),
                };
                let _ = dec.decode(&with_pitch(body, prev));
                let _ = dec.decode(&with_pitch(body, b0));
                let _ = dec.decode(&with_pitch(rng.bytes(), prev));
                let _ = dec.decode(&with_pitch(rng.bytes(), b0));
            }
        }
    }
}

#[test]
fn decode_returns_for_a_large_random_sweep() {
    let mut rng = Rng(0x00C0_FFEE_0000_0002);
    let mut dec = ImbeDecoder::new();
    for i in 0..RANDOM_FRAMES {
        let f = rng.bytes();
        // Most random b0 values are valid, but force half the frames valid
        // and a few to the extremes so long runs of real frames occur.
        let f = match i % 4 {
            0 => with_pitch(f, (rng.next() % 208) as u8),
            1 if i % 64 == 1 => with_pitch(f, if i % 128 == 1 { 0 } else { 207 }),
            _ => f,
        };
        let _ = dec.decode(&f);
    }
}

/// A frame of `f(n)` for the frame's sample numbers.
fn pcm(f: impl FnMut(usize) -> i16) -> [i16; FRAME_SAMPLES] {
    std::array::from_fn(f)
}

#[test]
fn encode_returns_for_extreme_and_random_pcm() {
    let mut rng = Rng(0x0BAD_F00D_0000_0003);
    let mut enc = ImbeEncoder::new();
    let mut dec = ImbeDecoder::new();
    let mut run = |frame: [i16; FRAME_SAMPLES]| {
        // Feed the encoder's output on to a decoder too: a real frame the
        // decoder has never seen should not trip it either.
        let _ = dec.decode(&enc.encode(&frame));
    };

    let extremes: Vec<[i16; FRAME_SAMPLES]> = vec![
        [i16::MAX; FRAME_SAMPLES],
        [i16::MIN; FRAME_SAMPLES],
        [0; FRAME_SAMPLES],
        pcm(|n| if n % 2 == 0 { i16::MAX } else { i16::MIN }),
        pcm(|n| if n == 0 { i16::MAX } else { 0 }),
        pcm(|n| if n == 0 { i16::MIN } else { 0 }),
        pcm(|n| if n < 80 { i16::MIN } else { i16::MAX }),
        pcm(|n| if n % 3 == 0 { i16::MIN } else { i16::MAX }),
        pcm(|n| (n as i16).wrapping_mul(409).wrapping_mul(5)),
        [1; FRAME_SAMPLES],
        [-1; FRAME_SAMPLES],
    ];
    // Each extreme alone for a while, then every pair back to back.
    for e in &extremes {
        for _ in 0..8 {
            run(*e);
        }
    }
    for a in &extremes {
        for b in &extremes {
            run(*a);
            run(*b);
        }
    }

    // Full-scale square waves at every period from 2 to 160 samples, which
    // walks the pitch estimator across its whole range.
    for period in 2..=160usize {
        for k in 0..3 {
            run(pcm(|n| {
                if (n + k * FRAME_SAMPLES) % period < period / 2 {
                    i16::MAX
                } else {
                    i16::MIN
                }
            }));
        }
    }

    // Random frames at random levels, with the odd silent or extreme frame.
    for i in 0..RANDOM_PCM_FRAMES {
        let shift = (rng.next() % 16) as u32;
        let frame = match i % 16 {
            0 => [0; FRAME_SAMPLES],
            1 => extremes[(rng.next() % extremes.len() as u64) as usize],
            _ => pcm(|_| ((rng.next() >> 48) as u16 as i16) >> shift),
        };
        run(frame);
    }
}
