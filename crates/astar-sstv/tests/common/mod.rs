// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! Shared helpers for the synthetic decode tests: the test picture, the
//! test-only encoder (the `sstv` crate, a dev-dependency that never ships), a
//! G.711 µ-law round trip, signal generators and an error measure.

#![allow(
    dead_code,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::many_single_char_names
)]

use astar_sstv::{Decoder, Event, Mode, Picture};

/// Width and height of the master test picture.
const MASTER: (u32, u32) = (320, 256);

/// The master picture's pixel at (`x`, `y`): seven colour bars over the top
/// two thirds, a horizontal grey ramp across the bottom third.
fn master_pixel(x: u32, y: u32) -> [u8; 3] {
    const BARS: [[u8; 3]; 7] = [
        [191, 191, 191],
        [191, 191, 0],
        [0, 191, 191],
        [0, 191, 0],
        [191, 0, 191],
        [191, 0, 0],
        [0, 0, 191],
    ];
    if y < MASTER.1 * 2 / 3 {
        BARS[(x * 7 / MASTER.0) as usize]
    } else {
        let g = (x * 255 / (MASTER.0 - 1)) as u8;
        [g, g, g]
    }
}

/// The test picture at `width` × `height`, nearest-neighbour resized from
/// the master, as row-major RGB bytes.
#[must_use]
pub fn test_image(width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            out.extend_from_slice(&master_pixel(x * MASTER.0 / width, y * MASTER.1 / height));
        }
    }
    out
}

/// The picture size astar reports for `mode` (display rows).
#[must_use]
pub fn expected_size(mode: Mode) -> (u32, u32) {
    match mode {
        Mode::Pd120 | Mode::Pd180 | Mode::Pd240 => (640, 496),
        // Robot 12/8 B/W send 120 rows, each shown twice.
        Mode::Robot24
        | Mode::Robot36
        | Mode::Robot72
        | Mode::Robot24Bw
        | Mode::Robot12Bw
        | Mode::Robot8Bw => (320, 240),
        _ => (320, 256),
    }
}

fn encoder_mode(mode: Mode) -> sstv::Mode {
    use sstv::modes;
    match mode {
        Mode::Pd120 => modes::PD_120,
        Mode::Pd180 => modes::PD_180,
        Mode::Pd240 => modes::PD_240,
        Mode::Robot36 => modes::ROBOT_36,
        Mode::Robot72 => modes::ROBOT_72,
        Mode::Martin1 => modes::MARTIN_1,
        Mode::Martin2 => modes::MARTIN_2,
        Mode::Scottie1 => modes::SCOTTIE_1,
        Mode::Scottie2 => modes::SCOTTIE_2,
        Mode::ScottieDx => modes::SCOTTIE_DX,
        Mode::Wraase180 => modes::WRASSE_SC2_180,
        other => panic!("the test encoder has no {other:?}"),
    }
}

/// The whole transmission of the test picture in `mode`, header included,
/// as 16-bit samples at `rate`.
#[must_use]
pub fn encode(mode: Mode, rate: u32) -> Vec<i16> {
    let m = encoder_mode(mode);
    let (w, h) = m.resolution();
    let pixels: Vec<sstv::RgbPixel> = test_image(w, h)
        .as_chunks::<3>()
        .0
        .iter()
        .map(|p| sstv::RgbPixel::new(p[0], p[1], p[2]))
        .collect();
    let encoder = sstv::Encoder::new(m, pixels.into_iter()).expect("encoder");
    sstv::Synthesizer::new(encoder, rate).collect()
}

/// Only the calibration header and VIS code of `mode`, at `rate`.
#[must_use]
pub fn header_only(mode: Mode, rate: u32) -> Vec<i16> {
    let m = encoder_mode(mode);
    sstv::Synthesizer::new(m.header_tones(), rate).collect()
}

/// How many samples at `rate` the header of `mode` lasts.
#[must_use]
pub fn header_len(mode: Mode, rate: u32) -> usize {
    let ns: u64 = encoder_mode(mode)
        .header_tones()
        .map(|t| t.duration.ns())
        .sum();
    (ns * u64::from(rate) / 1_000_000_000) as usize
}

/// Feed `samples` in 20 ms chunks and collect every event.
#[must_use]
pub fn run(decoder: &mut Decoder, samples: &[i16]) -> Vec<Event> {
    let chunk = (decoder.rate() / 50) as usize;
    let mut events = Vec::new();
    for c in samples.chunks(chunk) {
        events.extend(decoder.push(c));
    }
    events
}

/// Decode one whole transmission, with a second of zeros after it, and
/// return the finished picture.
#[must_use]
pub fn decode_one(samples: Vec<i16>, rate: u32) -> Picture {
    let mut decoder = Decoder::new(rate).expect("decoder");
    let mut padded = samples;
    padded.extend(std::iter::repeat_n(0, rate as usize));
    let events = run(&mut decoder, &padded);
    events
        .into_iter()
        .find_map(|e| match e {
            Event::Finished(p) => Some(p),
            _ => None,
        })
        .expect("a finished picture")
}

/// Mean absolute difference per channel between `pic` and the test
/// picture at `pic`'s size.
#[must_use]
pub fn mean_abs_error(pic: &Picture, mode: Mode) -> f64 {
    assert_eq!((pic.width, pic.height), expected_size(mode));
    let want = test_image(pic.width, pic.height);
    assert_eq!(want.len(), pic.rgb.len());
    let total: u64 = want
        .iter()
        .zip(&pic.rgb)
        .map(|(a, b)| u64::from(a.abs_diff(*b)))
        .sum();
    total as f64 / want.len() as f64
}

/// G.711 µ-law encode of one linear sample.
fn ulaw_encode(sample: i16) -> u8 {
    const BIAS: i32 = 0x84;
    const CLIP: i32 = 32_635;
    let mut s = i32::from(sample);
    let sign = if s < 0 {
        s = -s;
        0x80
    } else {
        0
    };
    s = s.min(CLIP) + BIAS;
    let mut exponent = 7;
    let mut mask = 0x4000;
    while exponent > 0 && s & mask == 0 {
        exponent -= 1;
        mask >>= 1;
    }
    let mantissa = (s >> (exponent + 3)) & 0x0F;
    !((sign | (exponent << 4) | mantissa) as u8)
}

/// G.711 µ-law decode of one byte.
fn ulaw_decode(byte: u8) -> i16 {
    let b = !byte;
    let sign = b & 0x80;
    let exponent = i32::from((b >> 4) & 0x07);
    let mantissa = i32::from(b & 0x0F);
    let magnitude = (((mantissa << 3) + 0x84) << exponent) - 0x84;
    if sign != 0 {
        -magnitude as i16
    } else {
        magnitude as i16
    }
}

/// Every sample through µ-law and back, as a telephone-grade link would.
#[must_use]
pub fn ulaw_round_trip(samples: &[i16]) -> Vec<i16> {
    samples
        .iter()
        .map(|&s| ulaw_decode(ulaw_encode(s)))
        .collect()
}

/// A steady tone of `hz` for `secs` at `rate`.
#[must_use]
pub fn tone(hz: f64, secs: f64, rate: u32) -> Vec<i16> {
    let n = (secs * f64::from(rate)) as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 / f64::from(rate);
            ((2.0 * std::f64::consts::PI * hz * t).sin() * 16_000.0) as i16
        })
        .collect()
}

/// A linear sweep from `from` to `to` Hz over `secs` at `rate`.
#[must_use]
pub fn sweep(from: f64, to: f64, secs: f64, rate: u32) -> Vec<i16> {
    let n = (secs * f64::from(rate)) as usize;
    let mut phase = 0.0_f64;
    (0..n)
        .map(|i| {
            let hz = from + (to - from) * i as f64 / n as f64;
            phase += 2.0 * std::f64::consts::PI * hz / f64::from(rate);
            (phase.sin() * 16_000.0) as i16
        })
        .collect()
}

/// White noise for `secs` at `rate`, from a fixed seed.
#[must_use]
pub fn noise(secs: f64, rate: u32, seed: u32) -> Vec<i16> {
    let n = (secs * f64::from(rate)) as usize;
    let mut x = seed.max(1);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            ((x >> 16) as u16 as i16) / 3
        })
        .collect()
}

/// The first `fraction` of the picture part of a transmission (the header
/// is always kept whole).
#[must_use]
pub fn cut(mode: Mode, samples: &[i16], rate: u32, fraction: f64) -> Vec<i16> {
    let header = header_len(mode, rate);
    let body = samples.len() - header;
    samples[..header + (body as f64 * fraction) as usize].to_vec()
}

/// Decode one whole transmission with re-timing switched `retime`, with a
/// second of zeros after it, and return the finished picture.
#[must_use]
pub fn decode_with(samples: &[i16], rate: u32, retime: astar_sstv::Retime) -> Picture {
    let mut decoder = Decoder::new(rate).expect("decoder");
    decoder.set_retime(retime);
    let mut padded = samples.to_vec();
    padded.extend(std::iter::repeat_n(0, rate as usize));
    run(&mut decoder, &padded)
        .into_iter()
        .find_map(|e| match e {
            Event::Finished(p) => Some(p),
            _ => None,
        })
        .expect("a finished picture")
}

/// One 20 ms audio frame at `rate`.
fn frame(rate: u32) -> usize {
    (rate / 50) as usize
}

/// Throw away `count` whole 20 ms frames, one every 535 ms from sample
/// `start`, as a jitter buffer shrinking in a stream does.
#[must_use]
pub fn drop_frames(samples: &[i16], rate: u32, start: usize, count: usize) -> Vec<i16> {
    drop_frames_every(samples, rate, start, count, 0.535)
}

/// Throw away `count` whole 20 ms frames, one every `secs` from sample
/// `start`.
#[must_use]
pub fn drop_frames_every(
    samples: &[i16],
    rate: u32,
    start: usize,
    count: usize,
    secs: f64,
) -> Vec<i16> {
    let spacing = (secs * f64::from(rate)) as usize;
    let mut out = Vec::with_capacity(samples.len());
    let mut at = 0;
    for i in 0..count {
        let drop_at = start + i * spacing;
        if drop_at + frame(rate) > samples.len() {
            break;
        }
        out.extend_from_slice(&samples[at..drop_at]);
        at = drop_at + frame(rate);
    }
    out.extend_from_slice(&samples[at..]);
    out
}

/// Bursts like a far node bridging two clocks: every 8.3 s from `start`,
/// fifteen 1 ms runs of exact zeros are inserted 21 ms apart, then the next
/// 20 ms of audio is lost.
#[must_use]
pub fn zero_bursts(samples: &[i16], rate: u32, start: usize) -> Vec<i16> {
    let ms = (rate / 1000) as usize;
    let every = (8.3 * f64::from(rate)) as usize;
    let mut out = Vec::with_capacity(samples.len() + samples.len() / 100);
    let mut at = 0;
    let mut burst = start;
    while burst + 400 * ms < samples.len() {
        for i in 0..15 {
            let gap = burst + i * 21 * ms;
            out.extend_from_slice(&samples[at..gap]);
            out.extend(std::iter::repeat_n(0, ms));
            at = gap;
        }
        let lost = burst + 15 * 21 * ms;
        out.extend_from_slice(&samples[at..lost]);
        at = lost + frame(rate);
        burst += every;
    }
    out.extend_from_slice(&samples[at..]);
    out
}

/// The audio as a sender whose clock runs `ppm` parts per million slow
/// would make it: stretched by `1 + ppm / 1e6`, linearly interpolated.
#[must_use]
pub fn clock_error(samples: &[i16], ppm: f64) -> Vec<i16> {
    let factor = 1.0 + ppm / 1e6;
    let n = (samples.len() as f64 * factor) as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 / factor;
            let k = t.floor() as usize;
            let frac = t - k as f64;
            let a = f64::from(samples[k.min(samples.len() - 1)]);
            let b = f64::from(samples[(k + 1).min(samples.len() - 1)]);
            (a + (b - a) * frac).round() as i16
        })
        .collect()
}

/// The horizontal shift, in whole pixels from -4 to 4, that best lines
/// display rows `rows` of `pic` up with the test picture.
#[must_use]
pub fn best_shift(pic: &Picture, rows: std::ops::Range<u32>) -> i32 {
    let want = test_image(pic.width, pic.height);
    let w = pic.width as i32;
    let mut best = (u64::MAX, 0);
    for dx in -4..=4_i32 {
        let mut total = 0_u64;
        for y in rows.clone() {
            for x in 8..w - 8 {
                for c in 0..3 {
                    let a = want[((y as i32 * w + x) * 3 + c) as usize];
                    let b = pic.rgb[((y as i32 * w + x + dx) * 3 + c) as usize];
                    total += u64::from(a.abs_diff(b));
                }
            }
        }
        if total < best.0 {
            best = (total, dx);
        }
    }
    best.1
}

/// `samples` with white noise (from [`noise`], seeded by `seed`) added at a
/// signal-to-noise ratio of `snr_db`, measured over the whole input.
#[must_use]
pub fn with_noise(samples: &[i16], rate: u32, seed: u32, snr_db: f64) -> Vec<i16> {
    let rms = |x: &[i16]| {
        (x.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
    };
    let n = noise(samples.len() as f64 / f64::from(rate) + 1.0, rate, seed);
    let gain = rms(samples) / rms(&n) / 10_f64.powf(snr_db / 20.0);
    samples
        .iter()
        .zip(&n)
        .map(|(&s, &v)| {
            (f64::from(s) + f64::from(v) * gain)
                .round()
                .clamp(-32768.0, 32767.0) as i16
        })
        .collect()
}

/// Where the sync pulse nearest sample `around` (within `radius`) is
/// centred: the 5 ms window with the fewest zero crossings, which is the
/// 1200 Hz tone, the lowest in the picture.
#[must_use]
pub fn sync_pulse_near(samples: &[i16], rate: u32, around: usize, radius: usize) -> usize {
    let w = (rate / 200) as usize;
    let crossings = |at: usize| {
        samples[at - w / 2..at + w / 2]
            .windows(2)
            .filter(|p| (p[0] < 0) != (p[1] < 0))
            .count()
    };
    (around - radius..around + radius)
        .min_by_key(|&at| crossings(at))
        .expect("a window")
}

/// One-row specks in `pic`: pixels that differ strongly (in their most
/// different channel) from both the rows above and below, where those two
/// agree. A row of wrong colour shows up as a run of them.
#[must_use]
pub fn specks(pic: &Picture) -> usize {
    let (w, h) = (pic.width as usize, pic.height as usize);
    let px = |x: usize, y: usize| {
        let i = (y * w + x) * 3;
        [pic.rgb[i], pic.rgb[i + 1], pic.rgb[i + 2]]
    };
    let apart = |a: [u8; 3], b: [u8; 3]| (0..3).map(|c| a[c].abs_diff(b[c])).max().unwrap_or(0);
    (1..h.saturating_sub(1))
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let (p, up, down) = (px(x, y), px(x, y - 1), px(x, y + 1));
            apart(p, up) > 70 && apart(p, down) > 70 && apart(up, down) < 50
        })
        .count()
}
