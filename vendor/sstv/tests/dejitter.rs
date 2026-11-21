//! The de-jitter in front of the decoder, on a transmission that lost a frame
//! of audio on its way, as happens on network links that carry audio in
//! 20 ms frames.

// Test helpers outside #[test] functions are not covered by the clippy.toml
// test allowances.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use sstv::dejitter::{Plan, radio_lines};
use sstv::modes::{PD_120, ROBOT_36, SCOTTIE_2};
use sstv::{Decoder, Encoder, Mode, RgbPixel, Synthesizer};

const SAMPLE_RATE: u32 = 8_000;
/// One 20 ms frame of audio.
const FRAME: usize = SAMPLE_RATE as usize / 50;

/// An image at the mode's resolution: seven colour bars over the top two
/// thirds, a grey ramp across the bottom third.
fn test_image(mode: Mode) -> Vec<RgbPixel> {
    const BARS: [(u8, u8, u8); 7] = [
        (255, 255, 255),
        (255, 255, 0),
        (0, 255, 255),
        (0, 255, 0),
        (255, 0, 255),
        (255, 0, 0),
        (0, 0, 255),
    ];
    let (width, height) = mode.resolution();
    (0..height)
        .flat_map(|row| {
            (0..width).map(move |column| {
                if row < height * 2 / 3 {
                    let (r, g, b) = BARS[(column * 7 / width) as usize];
                    RgbPixel::new(r, g, b)
                } else {
                    let grey = (column * 255 / (width - 1)) as u8;
                    RgbPixel::new(grey, grey, grey)
                }
            })
        })
        .collect()
}

fn transmit(mode: Mode, image: &[RgbPixel]) -> Vec<i16> {
    let encoder = Encoder::new(mode, image.iter().copied()).unwrap();
    Synthesizer::new(encoder, SAMPLE_RATE).collect()
}

/// The sample at which radio line `line` starts: the image ends where the
/// transmission does.
fn line_start(mode: Mode, samples: usize, line: u32) -> usize {
    let timing = mode.scan_timing();
    let after = f64::from(radio_lines(&timing) - line) * timing.line_seconds;
    samples - (after * f64::from(SAMPLE_RATE)).round() as usize
}

/// Mean absolute per-channel difference between two images of equal size.
fn mean_abs_error(expected: &[RgbPixel], actual: &[RgbPixel]) -> f64 {
    assert_eq!(expected.len(), actual.len());
    let difference = |a: u8, b: u8| f64::from(a.abs_diff(b));
    let total: f64 = expected
        .iter()
        .zip(actual)
        .map(|(e, a)| {
            difference(e.red(), a.red())
                + difference(e.green(), a.green())
                + difference(e.blue(), a.blue())
        })
        .sum();
    total / (expected.len() as f64 * 3.0)
}

/// The mean error of the plain decode and of the de-jittered decode of
/// `samples` (with a second of silence after them) against `image`.
fn errors(mode: Mode, image: &[RgbPixel], samples: &[i16]) -> (f64, f64) {
    let tail = || std::iter::repeat_n(0, SAMPLE_RATE as usize);
    let plain = Decoder::new(samples.iter().copied().chain(tail()), SAMPLE_RATE)
        .decode()
        .unwrap();
    let dejittered = Decoder::new(samples.iter().copied().chain(tail()), SAMPLE_RATE)
        .decode_dejittered()
        .unwrap();
    assert_eq!(dejittered.mode(), mode);
    let errors = (
        mean_abs_error(image, plain.pixels()),
        mean_abs_error(image, dejittered.pixels()),
    );
    eprintln!(
        "{mode:?}: plain {:.2}, de-jittered {:.2}",
        errors.0, errors.1
    );
    errors
}

/// Bursts like a far node bridging two clocks: from radio line `line`, every
/// 8.3 s, fifteen 1 ms runs of exact zeros are inserted 21 ms apart, then
/// the next 20 ms of audio is lost.
fn zero_bursts(mode: Mode, samples: &[i16], line: u32) -> Vec<i16> {
    let ms = (SAMPLE_RATE / 1000) as usize;
    let every = (8.3 * f64::from(SAMPLE_RATE)) as usize;
    let mut out = Vec::with_capacity(samples.len() + samples.len() / 100);
    let mut at = 0;
    let mut burst = line_start(mode, samples.len(), line);
    while burst + 400 * ms < samples.len() {
        for i in 0..15 {
            let gap = burst + i * 21 * ms;
            out.extend_from_slice(&samples[at..gap]);
            out.extend(std::iter::repeat_n(0, ms));
            at = gap;
        }
        let lost = burst + 15 * 21 * ms;
        out.extend_from_slice(&samples[at..lost]);
        at = lost + FRAME;
        burst += every;
    }
    out.extend_from_slice(&samples[at..]);
    out
}

/// From radio line `line` on, one 1 ms run of exact zeros after every
/// `every_ms` of audio: a far node steadily bridging a faster clock.
fn steady_stuffing(mode: Mode, samples: &[i16], line: u32, every_ms: usize) -> Vec<i16> {
    let ms = (SAMPLE_RATE / 1000) as usize;
    let start = line_start(mode, samples.len(), line);
    let mut out = samples[..start].to_vec();
    for chunk in samples[start..].chunks(every_ms * ms) {
        out.extend_from_slice(chunk);
        out.extend(std::iter::repeat_n(0, ms));
    }
    out
}

/// `samples` as a sender whose clock is off by `ppm` parts per million
/// would make them: stretched (slow, positive) or squeezed by
/// `1 + ppm / 1e6`, linearly interpolated.
fn clock_error(samples: &[i16], ppm: f64) -> Vec<i16> {
    let factor = 1.0 + ppm / 1e6;
    let length = (samples.len() as f64 * factor) as usize;
    let at = |index: usize| f64::from(samples[index.min(samples.len() - 1)]);
    (0..length)
        .map(|index| {
            let time = index as f64 / factor;
            let before = time.floor() as usize;
            let frac = time - before as f64;
            let (first, second) = (at(before), at(before + 1));
            (first + (second - first) * frac).round() as i16
        })
        .collect()
}

/// `samples` with white noise added at a signal-to-noise ratio of `snr_db`.
fn with_noise(samples: &[i16], seed: u32, snr_db: f64) -> Vec<i16> {
    let mut x = seed.max(1);
    let noise: Vec<f64> = (0..samples.len())
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            f64::from((x >> 16) as u16 as i16)
        })
        .collect();
    let rms = |v: &mut dyn Iterator<Item = f64>| {
        let (sum, n) = v.fold((0.0, 0.0), |(s, n), x| (s + x * x, n + 1.0));
        (sum / f64::max(n, 1.0)).sqrt()
    };
    let gain = rms(&mut samples.iter().map(|&s| f64::from(s)))
        / rms(&mut noise.iter().copied())
        / 10_f64.powf(snr_db / 20.0);
    samples
        .iter()
        .zip(&noise)
        .map(|(&s, &v)| (f64::from(s) + v * gain).round().clamp(-32768.0, 32767.0) as i16)
        .collect()
}

/// `samples` with `frames` consecutive frames cut out a third of the way
/// into radio line `line`, and a second of silence after the transmission.
fn dropped_frames(mode: Mode, mut samples: Vec<i16>, line: u32, frames: usize) -> Vec<i16> {
    let timing = mode.scan_timing();
    let at = line_start(mode, samples.len(), line)
        + (timing.line_seconds * f64::from(SAMPLE_RATE) / 3.0) as usize;
    samples.drain(at..at + frames * FRAME);
    samples.extend(std::iter::repeat_n(0, SAMPLE_RATE as usize));
    samples
}

#[test]
fn dejittered_decode_survives_a_dropped_frame() {
    // Every line after the lost frame arrives 20 ms early. The decoder alone
    // follows a line's sync pulse only near where it expects it, so it loses
    // the timing for the rest of the picture. De-jittered, the lines after
    // the frame go back on time and only the rows around it are rebuilt.
    let mode = SCOTTIE_2;
    let image = test_image(mode);
    let samples = dropped_frames(mode, transmit(mode, &image), 80, 1);

    let plain = Decoder::new(samples.iter().copied(), SAMPLE_RATE)
        .decode()
        .unwrap();
    let dejittered = Decoder::new(samples.into_iter(), SAMPLE_RATE)
        .decode_dejittered()
        .unwrap();
    assert_eq!(dejittered.mode(), mode);

    let plain = mean_abs_error(&image, plain.pixels());
    let dejittered = mean_abs_error(&image, dejittered.pixels());
    eprintln!("mean error: plain {plain:.1}, de-jittered {dejittered:.1}");
    assert!(dejittered < 6.0, "de-jittered {dejittered:.1}");
    assert!(plain > 20.0, "plain {plain:.1}");
}

#[test]
fn an_undamaged_transmission_decodes_as_well_dejittered() {
    for mode in [SCOTTIE_2, ROBOT_36] {
        let image = test_image(mode);
        let mut samples = transmit(mode, &image);
        samples.extend(std::iter::repeat_n(0, SAMPLE_RATE as usize));
        let plain = Decoder::new(samples.iter().copied(), SAMPLE_RATE)
            .decode()
            .unwrap();
        let dejittered = Decoder::new(samples.into_iter(), SAMPLE_RATE)
            .decode_dejittered()
            .unwrap();
        let plain = mean_abs_error(&image, plain.pixels());
        let dejittered = mean_abs_error(&image, dejittered.pixels());
        assert!(
            dejittered <= plain + 1.0,
            "{mode:?}: de-jittered {dejittered:.1}, plain {plain:.1}"
        );
    }
}

#[test]
fn the_plan_marks_the_line_that_lost_a_frame() {
    // Driven by hand, as another decoder would use it: the audio from the
    // first line on, re-timed, with the line that lost the frame marked.
    let mode = SCOTTIE_2;
    let image = test_image(mode);
    let samples = transmit(mode, &image);
    let start = line_start(mode, samples.len(), 0);
    let damaged = dropped_frames(mode, samples, 80, 1);

    let timing = mode.scan_timing();
    let plan = Plan::new(&damaged[start..], 0, SAMPLE_RATE, &timing, &[]);
    let marked: Vec<usize> = (0..radio_lines(&timing) as usize)
        .filter(|&k| plan.damaged()[k] || plan.patched()[k])
        .collect();
    assert!(
        !marked.is_empty() && marked.iter().all(|k| (79..=81).contains(k)),
        "marked lines {marked:?}"
    );
    // The re-timed audio is as long as the picture again.
    let line = timing.line_seconds * f64::from(SAMPLE_RATE);
    let length = plan.covered(0, (f64::from(radio_lines(&timing)) * line) as usize);
    assert_eq!(plan.render(0, length).len(), length);
    assert!(length as f64 >= f64::from(radio_lines(&timing)) * line - 1.0);
}

#[test]
fn dejittered_decode_survives_stuffed_zeros() {
    // A far node bridging two clocks stuffs runs of exact zeros into the
    // audio, each pushing every later line late. The de-jitter cuts them
    // out. Steady stuffing, 1 ms every 60 ms: measured plain 95.3,
    // de-jittered 6.4 (23.5 with the zeros left in and only the timing
    // followed).
    let mode = SCOTTIE_2;
    let image = test_image(mode);
    let samples = steady_stuffing(mode, &transmit(mode, &image), 40, 60);
    let (plain, dejittered) = errors(mode, &image, &samples);
    assert!(dejittered < 7.5, "steady: de-jittered {dejittered:.2}");
    assert!(plain > 3.0 * dejittered, "steady: plain {plain:.2}");

    // Bursts of fifteen 1 ms runs 21 ms apart, then a lost frame, every
    // 8.3 s: measured plain 95.5, de-jittered 5.7.
    let samples = zero_bursts(mode, &transmit(mode, &image), 40);
    let (plain, dejittered) = errors(mode, &image, &samples);
    assert!(dejittered < 6.5, "bursts: de-jittered {dejittered:.2}");
    assert!(plain > 3.0 * dejittered, "bursts: plain {plain:.2}");
}

#[test]
fn dejittered_pd120_survives_lost_frames() {
    // A PD120 sync pulse lasts 20 ms, and the decoder looks for each one
    // within a pulse's length of where it expects it, so one lost 20 ms frame
    // is still found (plain 11.2, de-jittered 9.1). Two lost together move
    // every later line, both rows of it, out of that window.
    let mode = PD_120;
    let image = test_image(mode);
    let one = dropped_frames(mode, transmit(mode, &image), 60, 1);
    let (plain, dejittered) = errors(mode, &image, &one);
    assert!(dejittered <= plain, "one frame: {dejittered:.2} vs {plain:.2}");

    let two = dropped_frames(mode, transmit(mode, &image), 60, 2);
    let (plain, dejittered) = errors(mode, &image, &two);
    // Measured: plain 95.1, de-jittered 9.1.
    assert!(dejittered < 10.0, "two frames: de-jittered {dejittered:.2}");
    assert!(plain > 3.0 * dejittered, "two frames: plain {plain:.2}");
}

#[test]
fn dejittered_decode_follows_an_off_frequency_clock() {
    // A sender whose clock is a little off makes every line a little long
    // or short. Over a Scottie 2 picture, 300 ppm adds up to 21 ms, more
    // than a sync pulse; the de-jitter puts each line back on the grid.
    // Measured, plain / de-jittered: 300 ppm 83.6 / 5.7, +2000 ppm
    // 115.0 / 9.4, -2000 ppm 116.1 / 5.9.
    let mode = SCOTTIE_2;
    let image = test_image(mode);
    for ppm in [300.0, 2000.0, -2000.0] {
        let samples = clock_error(&transmit(mode, &image), ppm);
        let (plain, dejittered) = errors(mode, &image, &samples);
        assert!(dejittered < 10.5, "{ppm} ppm: de-jittered {dejittered:.2}");
        assert!(plain > 3.0 * dejittered, "{ppm} ppm: plain {plain:.2}");
    }
}

#[test]
fn a_noisy_undamaged_picture_is_not_repaired() {
    // Noise is not a timing step: no line is marked, and the de-jittered
    // decode is no worse than the plain one. 20 dB is about as noisy as the
    // decoder still finds the header at. Measured, plain / de-jittered:
    // Scottie 2 7.92 / 7.71, PD120 13.06 / 13.03.
    for mode in [SCOTTIE_2, PD_120] {
        let image = test_image(mode);
        let samples = with_noise(&transmit(mode, &image), 5, 20.0);
        let timing = mode.scan_timing();
        let start = line_start(mode, samples.len(), 0);
        let retimed = Plan::new(&samples[start..], 0, SAMPLE_RATE, &timing, &[]);
        let repaired: Vec<usize> = (0..radio_lines(&timing) as usize)
            .filter(|&k| retimed.damaged()[k] || retimed.patched()[k])
            .collect();
        assert!(repaired.is_empty(), "{mode:?}: lines marked {repaired:?}");
        let (plain, dejittered) = errors(mode, &image, &samples);
        assert!(
            dejittered <= plain + 0.5,
            "{mode:?}: de-jittered {dejittered:.2} vs plain {plain:.2}"
        );
    }
}
