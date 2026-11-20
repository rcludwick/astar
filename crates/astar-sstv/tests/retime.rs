// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! Scan-timing repair: pictures from the test-only encoder, damaged the ways
//! linked audio was measured to be damaged (whole 20 ms frames dropped,
//! bursts of 1 ms zero gaps, a sender clock off by 300 ppm), decoded with
//! re-timing on and off.
//!
//! The `#[ignore]`d test at the bottom decodes real recordings named by an
//! environment variable; none is committed.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

mod common;

use astar_sstv::{Decoder, Event, Mode, Picture, Retime};
use common::{
    best_shift, clock_error, decode_with, drop_frames, drop_frames_every, encode, header_len,
    mean_abs_error, run, specks, zero_bursts,
};

const RATE: u32 = 16_000;
const MODES: [Mode; 4] = [Mode::Scottie2, Mode::Martin1, Mode::Robot36, Mode::Pd120];

#[derive(Clone, Copy, Debug)]
enum Damage {
    FrameDrops,
    ZeroBursts,
    Clock,
    All,
}

/// `mode`'s test picture, damaged by `damage` from 3 s into the picture.
fn damaged(mode: Mode, damage: Damage) -> Vec<i16> {
    let clean = encode(mode, RATE);
    let start = header_len(mode, RATE) + 3 * RATE as usize;
    match damage {
        Damage::FrameDrops => drop_frames(&clean, RATE, start, 6),
        Damage::ZeroBursts => zero_bursts(&clean, RATE, start),
        Damage::Clock => clock_error(&clean, 300.0),
        Damage::All => {
            let slow = clock_error(&clean, 300.0);
            let dropped = drop_frames(&slow, RATE, start, 6);
            zero_bursts(&dropped, RATE, start + 2 * RATE as usize)
        }
    }
}

struct Scores {
    clean: f64,
    off: f64,
    on: f64,
    repaired: u32,
}

fn score(mode: Mode, damage: Damage) -> Scores {
    let clean = decode_with(&encode(mode, RATE), RATE, Retime::On);
    let audio = damaged(mode, damage);
    let off = decode_with(&audio, RATE, Retime::Off);
    let on = decode_with(&audio, RATE, Retime::On);
    let s = Scores {
        clean: mean_abs_error(&clean, mode),
        off: mean_abs_error(&off, mode),
        on: mean_abs_error(&on, mode),
        repaired: on.repaired_rows,
    };
    eprintln!(
        "{mode:?} {damage:?}: clean {:.2}, off {:.2} (q {:.2}), on {:.2} (q {:.2}), {} rows repaired",
        s.clean, s.off, off.quality, s.on, on.quality, s.repaired
    );
    assert!(
        on.quality >= 0.43,
        "{mode:?} {damage:?} quality {}",
        on.quality
    );
    s
}

/// Re-timed, the damaged picture is within `margin` of the clean decode and
/// at least `gain` better than without re-timing.
fn assert_repaired(damage: Damage, margin: f64, gain: f64) {
    for mode in MODES {
        let s = score(mode, damage);
        assert!(
            s.on <= s.clean + margin,
            "{mode:?} {damage:?}: re-timed error {:.2} vs clean {:.2}",
            s.on,
            s.clean
        );
        assert!(
            s.off - s.on >= gain,
            "{mode:?} {damage:?}: re-timed {:.2} vs not {:.2}",
            s.on,
            s.off
        );
    }
}

#[test]
fn an_undamaged_picture_decodes_the_same_re_timed() {
    for mode in MODES {
        let audio = encode(mode, RATE);
        let off = decode_with(&audio, RATE, Retime::Off);
        let on = decode_with(&audio, RATE, Retime::On);
        let (e_off, e_on) = (mean_abs_error(&off, mode), mean_abs_error(&on, mode));
        eprintln!("{mode:?} clean: off {e_off:.2}, on {e_on:.2}");
        assert!((e_on - e_off).abs() <= 1.0, "{mode:?}: {e_on} vs {e_off}");
        assert_eq!(on.repaired_rows, 0, "{mode:?}");
        assert_eq!(off.repaired_rows, 0, "{mode:?}");
        assert!(on.quality >= off.quality - 0.02, "{mode:?}");
    }
}

#[test]
fn dropped_frames_are_re_timed_and_their_rows_filled() {
    assert_repaired(Damage::FrameDrops, 3.0, 5.0);
}

#[test]
fn zero_gap_bursts_are_re_timed_and_their_rows_filled() {
    assert_repaired(Damage::ZeroBursts, 3.0, 3.0);
}

#[test]
fn a_slow_sender_clock_is_followed() {
    // slowrx fits a slant itself, but only roughly; re-timed, the picture
    // is straight: its top and bottom line up the same way. The level of
    // error must stay near the clean decode (a whole-picture shift of one
    // pixel moves it by about 1.5 on the test picture's sharp bars).
    for mode in MODES {
        let audio = damaged(mode, Damage::Clock);
        let on = decode_with(&audio, RATE, Retime::On);
        let clean = mean_abs_error(&decode_with(&encode(mode, RATE), RATE, Retime::On), mode);
        let err = mean_abs_error(&on, mode);
        let h = on.height;
        let (top, bottom) = (best_shift(&on, 0..h / 6), best_shift(&on, h - h / 6..h));
        eprintln!("{mode:?} Clock: clean {clean:.2}, on {err:.2}, shift top {top} bottom {bottom}");
        assert_eq!(top, bottom, "{mode:?} is slanted");
        assert!(err <= clean + 2.0, "{mode:?}: {err:.2} vs {clean:.2}");
        assert_eq!(on.repaired_rows, 0, "{mode:?}");
    }
}

#[test]
fn all_damage_at_once_is_repaired() {
    assert_repaired(Damage::All, 4.0, 5.0);
}

#[test]
fn repaired_rows_are_counted() {
    let s = score(Mode::Scottie2, Damage::FrameDrops);
    // Six drops, each costing at most a few rows.
    assert!((1..=6 * 4).contains(&s.repaired), "{}", s.repaired);
}

#[test]
fn rows_reported_as_lines_match_the_finished_picture() {
    let audio = damaged(Mode::Scottie2, Damage::FrameDrops);
    let mut d = Decoder::new(RATE).unwrap();
    let mut padded = audio;
    padded.extend(std::iter::repeat_n(0, RATE as usize));
    let events = run(&mut d, &padded);
    let pic = events
        .iter()
        .find_map(|e| match e {
            Event::Finished(p) => Some(p.clone()),
            _ => None,
        })
        .expect("finished");
    let stride = (pic.width * 3) as usize;
    for e in &events {
        if let Event::Line { row, rgb } = e {
            let at = *row as usize * stride;
            assert_eq!(rgb[..], pic.rgb[at..at + stride], "row {row}");
        }
    }
}

/// Decode real recordings, re-timed and not, into PNGs. Never committed with
/// a path: `ASTAR_SSTV_REAL` holds one or more FLAC paths separated by `|`
/// (the received audio is the left channel), `ASTAR_SSTV_OUT` the folder
/// for the PNGs (default `/tmp/sstv-retime`).
#[test]
#[ignore = "needs ASTAR_SSTV_REAL naming local recordings"]
fn real_recordings_from_the_environment() {
    let Ok(paths) = std::env::var("ASTAR_SSTV_REAL") else {
        eprintln!("ASTAR_SSTV_REAL is not set");
        return;
    };
    let out = std::path::PathBuf::from(
        std::env::var("ASTAR_SSTV_OUT").unwrap_or_else(|_| "/tmp/sstv-retime".into()),
    );
    std::fs::create_dir_all(&out).unwrap();
    for (n, path) in paths.split('|').filter(|p| !p.is_empty()).enumerate() {
        let mut reader = claxon::FlacReader::open(path).expect("open flac");
        let info = reader.streaminfo();
        let channels = info.channels as usize;
        let rate = info.sample_rate;
        let shift = info.bits_per_sample.saturating_sub(16);
        let left: Vec<i16> = reader
            .samples()
            .step_by(channels)
            .map(|s| (s.expect("sample") >> shift) as i16)
            .collect();
        for retime in [Retime::Off, Retime::On] {
            let mut d = Decoder::new(rate).unwrap();
            d.set_retime(retime);
            let mut events = run(&mut d, &left);
            events.extend(d.finish());
            let pictures: Vec<Picture> = events
                .into_iter()
                .filter_map(|e| match e {
                    Event::Finished(p) | Event::Abandoned(p) => Some(p),
                    _ => None,
                })
                .collect();
            for (i, p) in pictures.iter().enumerate() {
                let name = out.join(format!("rec{}-pic{}-{retime:?}.png", n + 1, i + 1));
                let _ = std::fs::remove_file(&name);
                p.write_png(&name).unwrap();
                eprintln!(
                    "recording {} picture {} {retime:?}: {:?} quality {:.3}, {} rows repaired, partial {}, {}",
                    n + 1,
                    i + 1,
                    p.mode,
                    p.quality,
                    p.repaired_rows,
                    p.partial,
                    name.display()
                );
            }
            if pictures.is_empty() {
                eprintln!("recording {} {retime:?}: no picture", n + 1);
            }
        }
    }
}

/// One noisy undamaged picture: sample rate, mode, noise seed, SNR in dB,
/// and how much worse re-timing On measured than Off for it.
type NoisyCase = (u32, Mode, u32, f64, f64);

/// Every configuration measured. Noise moves each pulse a little as found,
/// and pulses are placed to the nearest (decimated) sample, so On and Off,
/// each fitting the line rate from noisy pulses, can land a pixel apart at
/// the bottom of the picture; on the test picture's sharp bars that costs
/// up to about 2.6 (PD120 at 8 kHz, 6 dB). Each case is held to its own
/// measured gap, not to the worst one. Finding pulse edges to a fraction of
/// a sample is the follow-up that would close these gaps.
const NOISY: [NoisyCase; 8] = [
    (16_000, Mode::Scottie2, 5, 6.0, 0.2),
    (16_000, Mode::Scottie2, 9, 0.0, 0.4),
    (16_000, Mode::Pd120, 5, 6.0, 0.4),
    (16_000, Mode::Pd120, 9, 0.0, 0.3),
    (8_000, Mode::Scottie2, 5, 6.0, 0.1),
    (8_000, Mode::Scottie2, 9, 0.0, 0.0),
    (8_000, Mode::Pd120, 5, 6.0, 2.6),
    (8_000, Mode::Pd120, 9, 0.0, 0.0),
];

/// Above each case's measured gap, this much is allowed.
const NOISY_MARGIN: f64 = 1.0;

/// None of the noise is a timing step, so nothing may be repaired, and the
/// picture decodes within its measured gap of re-timing Off.
fn assert_noisy_not_repaired(cases: impl Iterator<Item = NoisyCase>) {
    for (rate, mode, seed, snr, gap) in cases {
        let audio = common::with_noise(&encode(mode, rate), rate, seed, snr);
        let off = decode_with(&audio, rate, Retime::Off);
        let on = decode_with(&audio, rate, Retime::On);
        let (e_off, e_on) = (mean_abs_error(&off, mode), mean_abs_error(&on, mode));
        eprintln!(
            "{mode:?} {rate} Hz {snr} dB: off {e_off:.2}, on {e_on:.2}, {} rows repaired",
            on.repaired_rows
        );
        assert_eq!(on.repaired_rows, 0, "{mode:?} {rate} Hz {snr} dB");
        assert!(
            e_on <= e_off + gap + NOISY_MARGIN,
            "{mode:?} {rate} Hz {snr} dB: {e_on:.2} vs {e_off:.2}"
        );
    }
}

#[test]
fn noisy_undamaged_pictures_are_not_repaired() {
    // One noise level per rate: 0 dB at 16 kHz, and at 8 kHz the 6 dB
    // level with the largest measured gap.
    assert_noisy_not_repaired(
        NOISY
            .into_iter()
            .filter(|&(rate, _, _, snr, _)| (rate == 16_000) == (snr == 0.0)),
    );
}

#[test]
#[ignore = "slow in debug builds; the full sweep, run with --ignored (best with --release)"]
fn noisy_undamaged_pictures_are_not_repaired_full_sweep() {
    assert_noisy_not_repaired(NOISY.into_iter());
}

#[test]
fn one_displaced_pulse_is_not_a_timing_step() {
    // 3 ms of audio moved from before one line's sync pulse to after it:
    // that pulse is early, the next one on time, and no audio was lost.
    for rate in [16_000, 8_000] {
        let mode = Mode::Scottie2;
        let clean = encode(mode, rate);
        let line = (0.277_692 * f64::from(rate)) as usize;
        let around = header_len(mode, rate) + 120 * line;
        let p = common::sync_pulse_near(&clean, rate, around, line / 2 + 1);
        let shift = (0.003 * f64::from(rate)) as usize;
        let (cut_at, add_at) = (p - line / 4, p + line / 4);
        let mut audio = Vec::with_capacity(clean.len());
        audio.extend_from_slice(&clean[..cut_at]);
        audio.extend_from_slice(&clean[cut_at + shift..add_at]);
        audio.extend_from_slice(&clean[add_at - shift..add_at]);
        audio.extend_from_slice(&clean[add_at..]);
        assert_eq!(audio.len(), clean.len());
        let on = decode_with(&audio, rate, Retime::On);
        let off = decode_with(&audio, rate, Retime::Off);
        let (e_off, e_on) = (mean_abs_error(&off, mode), mean_abs_error(&on, mode));
        eprintln!(
            "{rate} Hz: off {e_off:.2}, on {e_on:.2}, {} rows repaired",
            on.repaired_rows
        );
        assert_eq!(on.repaired_rows, 0, "{rate} Hz");
        assert!(e_on <= e_off + 1.0, "{rate} Hz: {e_on:.2} vs {e_off:.2}");
    }
}

#[test]
fn a_picture_right_after_a_re_timed_one_starts_on_time() {
    // The first picture lost six frames, so re-timing changed its length;
    // the second follows with no gap. Its header must still be placed
    // where it really is.
    let first_mode = Mode::Scottie2;
    let second_mode = Mode::Martin1;
    let first = damaged(first_mode, Damage::FrameDrops);
    let second = encode(second_mode, RATE);

    // How far after the header slowrx reports a lone picture's start.
    let alone = {
        let mut d = Decoder::new(RATE).unwrap();
        let mut padded = second.clone();
        padded.extend(std::iter::repeat_n(0, RATE as usize));
        run(&mut d, &padded)
            .into_iter()
            .find_map(|e| match e {
                Event::Started { at_sample, .. } => Some(at_sample),
                _ => None,
            })
            .expect("started")
    };

    let mut audio = first.clone();
    audio.extend_from_slice(&second);
    audio.extend(std::iter::repeat_n(0, RATE as usize));
    let mut d = Decoder::new(RATE).unwrap();
    let pictures: Vec<Picture> = run(&mut d, &audio)
        .into_iter()
        .filter_map(|e| match e {
            Event::Finished(p) => Some(p),
            _ => None,
        })
        .collect();
    assert_eq!(pictures.len(), 2, "both pictures finish");
    assert_eq!(pictures[0].mode, first_mode);
    assert_eq!(pictures[1].mode, second_mode);
    assert!(pictures[0].repaired_rows > 0, "the first was re-timed");
    let want = first.len() as u64 + alone;
    let got = pictures[1].at_sample;
    eprintln!("second picture at {got}, expected {want}");
    assert!(
        got.abs_diff(want) <= u64::from(RATE / 200),
        "second picture at {got}, expected {want}"
    );
    assert!(mean_abs_error(&pictures[1], second_mode) <= 25.0);
    assert!(
        mean_abs_error(&pictures[0], first_mode)
            <= mean_abs_error(&decode_with(&first, RATE, Retime::On), first_mode) + 1.0
    );
}

/// Mean absolute error over the first `rows` display rows of `pic` only.
fn error_in_rows(pic: &Picture, rows: u32) -> f64 {
    let want = common::test_image(pic.width, pic.height);
    let n = (rows * pic.width * 3) as usize;
    let total: u64 = want[..n]
        .iter()
        .zip(&pic.rgb[..n])
        .map(|(a, b)| u64::from(a.abs_diff(*b)))
        .sum();
    total as f64 / n as f64
}

/// Push `audio`, then either silence long enough to abandon the picture or
/// the end of the stream, and return the abandoned picture.
fn abandoned(audio: &[i16], retime: Retime, by_silence: bool) -> Picture {
    let mut d = Decoder::new(RATE).unwrap();
    d.set_retime(retime);
    let mut events = run(&mut d, audio);
    if by_silence {
        let silence = vec![0; (astar_sstv::SILENCE_SECS + 1) as usize * RATE as usize];
        events.extend(run(&mut d, &silence));
    } else {
        events.extend(d.finish());
    }
    events
        .into_iter()
        .find_map(|e| match e {
            Event::Abandoned(p) => Some(p),
            _ => None,
        })
        .expect("an abandoned picture")
}

#[test]
fn a_picture_cut_off_early_is_still_re_timed() {
    // Frames were dropped, then the picture stopped at about 40%: the held
    // audio covers well under half the picture, but every pulse in it is
    // there to be found.
    let mode = Mode::Scottie2;
    let audio = common::cut(mode, &damaged(mode, Damage::FrameDrops), RATE, 0.4);
    // The same cut of the undamaged picture: slowrx's best for these rows.
    let clean = abandoned(
        &common::cut(mode, &encode(mode, RATE), RATE, 0.4),
        Retime::Off,
        true,
    );
    for by_silence in [true, false] {
        let on = abandoned(&audio, Retime::On, by_silence);
        let off = abandoned(&audio, Retime::Off, by_silence);
        assert!(on.partial && off.partial);
        let rows = on.lines.min(off.lines);
        assert!(rows > 0);
        let (e_on, e_off, e_clean) = (
            error_in_rows(&on, rows),
            error_in_rows(&off, rows),
            error_in_rows(&clean, rows),
        );
        eprintln!(
            "cut at 40%, by silence {by_silence}: {rows} rows, clean {e_clean:.2}, off {e_off:.2}, on {e_on:.2}, {} rows repaired",
            on.repaired_rows
        );
        assert!(on.repaired_rows > 0, "by silence {by_silence}");
        assert!(
            e_off - e_on >= 5.0,
            "by silence {by_silence}: {e_on:.2} vs {e_off:.2}"
        );
        assert!(
            e_on <= e_clean + 2.0,
            "by silence {by_silence}: {e_on:.2} vs clean {e_clean:.2}"
        );
    }
}

/// One radio line of `mode`, in seconds (a PD line carries two rows).
fn line_secs(mode: Mode) -> f64 {
    match mode {
        Mode::Scottie2 => 0.277_692,
        Mode::Martin1 => 0.446_446,
        Mode::Pd120 => 0.508_48,
        other => panic!("no line length for {other:?}"),
    }
}

/// `mode`'s test picture at `rate` with 400 ms lost to silence from the
/// start of radio line `at`, and `jump` lines of audio dropped after the
/// silence, so the sync comes back at a new phase: what a far node's
/// dropout did to two pictures on a net.
fn dropout(mode: Mode, rate: u32, at: f64, jump: f64) -> Vec<i16> {
    let clean = encode(mode, rate);
    let line = line_secs(mode) * f64::from(rate);
    let start = header_len(mode, rate) + (at * line) as usize;
    let quiet = (0.4 * f64::from(rate)) as usize;
    let lost = (jump * line) as usize;
    let mut out = clean[..start].to_vec();
    out.extend(std::iter::repeat_n(0, quiet));
    out.extend_from_slice(&clean[start + quiet + lost..]);
    out
}

/// Mean absolute error of each display row of `pic`.
fn row_errors(pic: &Picture) -> Vec<f64> {
    let want = common::test_image(pic.width, pic.height);
    let stride = pic.width as usize * 3;
    want.chunks(stride)
        .zip(pic.rgb.chunks(stride))
        .map(|(a, b)| {
            a.iter()
                .zip(b)
                .map(|(x, y)| f64::from(x.abs_diff(*y)))
                .sum::<f64>()
                / stride as f64
        })
        .collect()
}

/// Rows per radio line.
fn rows_per_line(mode: Mode) -> usize {
    if mode == Mode::Pd120 { 2 } else { 1 }
}

#[test]
fn a_dropout_that_jumps_the_phase_is_re_acquired() {
    // The tracker looks for each pulse only near where the last one says it
    // should be. After the silence the pulses are a third of a line away,
    // so the tracker must find the new phase again rather than lose the
    // rest of the picture.
    for rate in [8_000, 16_000] {
        for mode in [Mode::Scottie2, Mode::Martin1, Mode::Pd120] {
            let audio = dropout(mode, rate, 80.0, 0.3);
            let clean = mean_abs_error(&decode_with(&encode(mode, rate), rate, Retime::On), mode);
            let on = decode_with(&audio, rate, Retime::On);
            let err = mean_abs_error(&on, mode);
            let rows = row_errors(&on);
            let after = 90 * rows_per_line(mode);
            let bad: Vec<usize> = (after..rows.len()).filter(|&r| rows[r] > 30.0).collect();
            eprintln!(
                "{mode:?} {rate} Hz: clean {clean:.2}, on {err:.2} (q {:.2}), {} rows repaired, bad rows from {after}: {bad:?}",
                on.quality, on.repaired_rows
            );
            assert!(
                err <= clean + 1.0,
                "{mode:?} {rate} Hz: {err:.2} vs clean {clean:.2}"
            );
            assert!(
                on.repaired_rows <= 8,
                "{mode:?} {rate} Hz: {} rows repaired",
                on.repaired_rows
            );
            assert!(bad.is_empty(), "{mode:?} {rate} Hz: bad rows {bad:?}");
        }
    }
}

#[test]
fn a_dropout_that_jumps_the_phase_by_half_a_line_is_re_acquired() {
    // A jump near half a line is ambiguous: the pulse half a line late and
    // the one half a line early are equally near the prediction. Whichever
    // the tracker takes, the rest of the picture is either on time or one
    // row off, and on the test picture's smooth content one row off is
    // close to invisible. What must not happen is losing lock for good.
    for rate in [8_000, 16_000] {
        for jump in [0.45, 0.5, 0.55] {
            let mode = Mode::Scottie2;
            let audio = dropout(mode, rate, 80.0, jump);
            let clean = mean_abs_error(&decode_with(&encode(mode, rate), rate, Retime::On), mode);
            let on = decode_with(&audio, rate, Retime::On);
            let err = mean_abs_error(&on, mode);
            let rows = row_errors(&on);
            let bad: Vec<usize> = (90..rows.len()).filter(|&r| rows[r] > 30.0).collect();
            eprintln!(
                "{rate} Hz jump {jump}: clean {clean:.2}, on {err:.2} (q {:.2}), {} rows repaired, bad rows {bad:?}",
                on.quality, on.repaired_rows
            );
            assert!(
                err <= clean + 2.0,
                "{rate} Hz jump {jump}: {err:.2} vs clean {clean:.2}"
            );
            assert!(
                on.repaired_rows <= 8,
                "{rate} Hz jump {jump}: {} rows repaired",
                on.repaired_rows
            );
            assert!(bad.len() <= 2, "{rate} Hz jump {jump}: bad rows {bad:?}");
        }
    }
}

/// The picture in `audio`, re-timed, finished or abandoned.
fn decode_any(audio: &[i16], rate: u32) -> Picture {
    let mut d = Decoder::new(rate).unwrap();
    d.set_retime(Retime::On);
    let mut events = run(&mut d, audio);
    events.extend(d.finish());
    events
        .into_iter()
        .find_map(|e| match e {
            Event::Finished(p) | Event::Abandoned(p) => Some(p),
            _ => None,
        })
        .expect("a picture")
}

#[test]
fn noise_after_a_dropout_is_not_re_acquired() {
    // The picture goes quiet late, and what comes back is noise, not the
    // picture. Searching a whole line for the phase must not lock onto
    // the noise: the rows before the dropout decode as well as with
    // nothing after them to re-acquire (silence), and nothing is repaired
    // beyond the dropout's own lines.
    let mode = Mode::Scottie2;
    for rate in [8_000, 16_000] {
        for seed in [3, 7, 11] {
            let clean = encode(mode, rate);
            let line = line_secs(mode) * f64::from(rate);
            let start = header_len(mode, rate) + (180.0 * line) as usize;
            let quiet = (0.4 * f64::from(rate)) as usize;
            let mut base = clean[..start].to_vec();
            base.extend(std::iter::repeat_n(0, quiet));
            let tail = clean.len() - base.len();
            let mut noisy = base.clone();
            noisy.extend(&common::noise(tail as f64 / f64::from(rate), rate, seed)[..tail]);
            let mut silent = base;
            silent.extend(std::iter::repeat_n(0, tail));
            let on = decode_any(&noisy, rate);
            let still = decode_any(&silent, rate);
            let top = 178;
            let (e_noisy, e_still) = (error_in_rows(&on, top), error_in_rows(&still, top));
            eprintln!(
                "{rate} Hz seed {seed}: top {top} rows {e_noisy:.2} (silence after: {e_still:.2}), {} rows repaired (silence after: {})",
                on.repaired_rows, still.repaired_rows
            );
            assert!(
                e_noisy <= e_still + 0.5,
                "{rate} Hz seed {seed}: {e_noisy:.2} vs {e_still:.2}"
            );
            // 400 ms of silence covers parts of at most three lines.
            assert!(
                on.repaired_rows <= 3,
                "{rate} Hz seed {seed}: {} rows repaired",
                on.repaired_rows
            );
        }
    }
}

/// `mode`'s test picture at `rate` with `secs` of its audio replaced by
/// silence, keeping time, from `into` of the way through radio line `at`.
fn short_dropout(mode: Mode, rate: u32, at: usize, into: f64, secs: f64) -> Vec<i16> {
    let mut audio = encode(mode, rate);
    let line = line_secs(mode) * f64::from(rate);
    let start = header_len(mode, rate) + ((at as f64 + into) * line) as usize;
    let quiet = (secs * f64::from(rate)) as usize;
    audio[start..start + quiet].fill(0);
    audio
}

#[test]
fn a_short_dropout_inside_one_line_is_repaired() {
    // 90 ms of silence is a third of a Scottie line: too little to make the
    // line mostly silent, but enough to wipe out a long stretch of one
    // colour channel. The line's rows are rebuilt from their neighbours.
    // The silence sits clear of the line's sync pulse, so the timing is
    // untouched and only the marking can repair it.
    for rate in [8_000, 16_000] {
        for (mode, into) in [
            (Mode::Scottie2, 0.15),
            (Mode::Martin1, 0.4),
            (Mode::Pd120, 0.4),
        ] {
            let at = 120;
            let audio = short_dropout(mode, rate, at, into, 0.09);
            let clean = decode_with(&encode(mode, rate), rate, Retime::On);
            let e_clean = mean_abs_error(&clean, mode);
            let on = decode_with(&audio, rate, Retime::On);
            let err = mean_abs_error(&on, mode);
            let rows = row_errors(&on);
            let clean_rows = row_errors(&clean);
            let span = at * rows_per_line(mode)..(at + 1) * rows_per_line(mode);
            let worst = span
                .clone()
                .map(|r| rows[r] - clean_rows[r])
                .fold(0.0, f64::max);
            eprintln!(
                "{mode:?} {rate} Hz: clean {e_clean:.2}, on {err:.2}, {} rows repaired, line {at} rows worse by {worst:.2}",
                on.repaired_rows
            );
            assert!(
                (1..=4).contains(&on.repaired_rows),
                "{mode:?} {rate} Hz: {} rows repaired",
                on.repaired_rows
            );
            assert!(
                worst <= 10.0,
                "{mode:?} {rate} Hz: line {at} rows worse than clean by {worst:.2}"
            );
            assert!(
                err <= e_clean + 0.5,
                "{mode:?} {rate} Hz: {err:.2} vs clean {e_clean:.2}"
            );
        }
    }
}

#[test]
fn rows_patched_for_a_dropped_frame_leave_no_dash() {
    // A lost 20 ms frame is made up from the line before, which leaves a
    // short dash of wrong colour where the splice lands: even placed
    // exactly, the copy is a different line's audio, joined with a jump in
    // phase. The patched rows are rebuilt from their neighbours, so no
    // one-row specks are left. Twelve drops, 1.73 s apart (about one every
    // few lines, as in linked audio), land all over the line.
    for rate in [8_000, 16_000] {
        for mode in [Mode::Scottie2, Mode::Scottie1, Mode::Martin1, Mode::Robot36] {
            let clean_audio = encode(mode, rate);
            let start = header_len(mode, rate) + 3 * rate as usize;
            let audio = drop_frames_every(&clean_audio, rate, start, 12, 1.73);
            let clean = decode_with(&clean_audio, rate, Retime::On);
            let on = decode_with(&audio, rate, Retime::On);
            let (e_clean, e_on) = (mean_abs_error(&clean, mode), mean_abs_error(&on, mode));
            let (s_clean, s_on) = (specks(&clean), specks(&on));
            eprintln!(
                "{mode:?} {rate} Hz: error clean {e_clean:.2}, on {e_on:.2}; specks clean {s_clean}, on {s_on}; {} rows repaired",
                on.repaired_rows
            );
            assert!(
                s_on <= s_clean + 2,
                "{mode:?} {rate} Hz: {s_on} specks vs {s_clean} clean"
            );
            assert!(
                e_on <= e_clean + 1.0,
                "{mode:?} {rate} Hz: error {e_on:.2} vs clean {e_clean:.2}"
            );
            assert!(
                (12..=12 * 4).contains(&on.repaired_rows),
                "{mode:?} {rate} Hz"
            );
        }
    }
}
