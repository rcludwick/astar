// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! Decodes pictures from an independent encoder (the `sstv` crate, a
//! dev-dependency) at the rates astar hears, and checks the wrapper's own
//! behaviour: event order, rejection, silence and PNG output.

#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

mod common;

use astar_sstv::{Decoder, Event, Mode, Picture};
use common::{
    cut, decode_one, encode, expected_size, header_len, header_only, mean_abs_error, noise, run,
    sweep, tone, ulaw_round_trip,
};

fn assert_good(pic: &Picture, m: Mode, max_error: f64) {
    assert_eq!(pic.mode, m);
    assert_eq!((pic.width, pic.height), expected_size(m));
    assert_eq!(pic.rgb.len(), (pic.width * pic.height * 3) as usize);
    assert_eq!(pic.lines, pic.height);
    assert!(!pic.partial);
    assert!(pic.quality >= 0.43, "{m:?} quality {}", pic.quality);
    let err = mean_abs_error(pic, m);
    assert!(err <= max_error, "{m:?} mean error {err}");
}

#[test]
fn every_common_mode_decodes_at_8k() {
    for m in [
        Mode::Pd120,
        Mode::Pd180,
        Mode::Robot36,
        Mode::Martin1,
        Mode::Scottie1,
        Mode::ScottieDx,
    ] {
        let pic = decode_one(encode(m, 8_000), 8_000);
        assert_good(&pic, m, 25.0);
    }
}

#[test]
fn robot36_decodes_at_16k() {
    let pic = decode_one(encode(Mode::Robot36, 16_000), 16_000);
    assert_good(&pic, Mode::Robot36, 20.0);
}

#[test]
fn ulaw_round_trip_still_decodes() {
    for m in [Mode::Robot36, Mode::Scottie1] {
        let pic = decode_one(ulaw_round_trip(&encode(m, 8_000)), 8_000);
        assert_good(&pic, m, 25.0);
    }
}

#[test]
fn events_arrive_in_order() {
    let rate = 8_000;
    let m = Mode::Robot36;
    let mut samples = encode(m, rate);
    samples.extend(std::iter::repeat_n(0, rate as usize));
    let mut d = Decoder::new(rate).unwrap();
    let events = run(&mut d, &samples);

    let Some(Event::Started {
        mode,
        width,
        height,
        at_sample,
    }) = events.first()
    else {
        panic!("first event is not Started: {:?}", events.first().map(kind));
    };
    assert_eq!(*mode, m);
    assert_eq!((*width, *height), expected_size(m));
    let header_end = header_len(m, rate) as u64;
    assert!(
        at_sample.abs_diff(header_end) <= u64::from(rate / 2),
        "at_sample {at_sample}, header ends at {header_end}"
    );

    let rows: Vec<u32> = events[1..events.len() - 1]
        .iter()
        .map(|e| match e {
            Event::Line { row, rgb } => {
                assert_eq!(rgb.len(), (*width * 3) as usize);
                *row
            }
            other => panic!("unexpected {} mid-picture", kind(other)),
        })
        .collect();
    assert_eq!(rows, (0..*height).collect::<Vec<_>>());
    assert!(matches!(events.last(), Some(Event::Finished(_))));
    assert!(!d.in_flight());
}

#[test]
fn whistles_and_noise_start_nothing() {
    let rate = 8_000;
    let mut samples = tone(1900.0, 10.0, rate);
    samples.extend(sweep(1100.0, 2300.0, 10.0, rate));
    samples.extend(noise(30.0, rate, 7));
    let mut d = Decoder::new(rate).unwrap();
    let events = run(&mut d, &samples);
    assert!(
        events.is_empty(),
        "{:?}",
        events.iter().map(kind).collect::<Vec<_>>()
    );
    assert!(d.finish().is_none());
    assert_eq!(d.samples_in(), samples.len() as u64);
}

#[test]
fn vis_then_noise_is_rejected() {
    let rate = 8_000;
    let mut samples = header_only(Mode::Pd120, rate);
    samples.extend(noise(130.0, rate, 11));
    let mut d = Decoder::new(rate).unwrap();
    let events = run(&mut d, &samples);
    assert!(matches!(
        events.first(),
        Some(Event::Started {
            mode: Mode::Pd120,
            ..
        })
    ));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Rejected { mode: Mode::Pd120 }))
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::Finished(_) | Event::Abandoned(_)))
    );
    assert!(!d.in_flight());
}

fn cut_then_silence(fraction: f64) -> Vec<i16> {
    let rate = 8_000;
    let mut samples = cut(Mode::Robot36, &encode(Mode::Robot36, rate), rate, fraction);
    samples.extend(std::iter::repeat_n(0, 3 * rate as usize));
    samples
}

fn last_abandoned(events: Vec<Event>) -> Picture {
    match events.into_iter().last() {
        Some(Event::Abandoned(p)) => p,
        other => panic!("last event is {:?}", other.as_ref().map(kind)),
    }
}

#[test]
fn silence_abandons_with_the_lines_heard() {
    let mut d = Decoder::new(8_000).unwrap();
    let events = run(&mut d, &cut_then_silence(0.6));
    let line_rows: Vec<u32> = events
        .iter()
        .filter_map(|e| match e {
            Event::Line { row, .. } => Some(*row),
            _ => None,
        })
        .collect();
    let p = last_abandoned(events);
    assert_eq!(p.mode, Mode::Robot36);
    // The rows heard are reported as lines before the picture is abandoned.
    assert_eq!(line_rows, (0..p.lines).collect::<Vec<_>>());
    assert!((i64::from(p.lines) - 144).abs() <= 4, "lines {}", p.lines);
    assert!(p.worth_keeping());
    assert!(p.partial);
    assert!(!d.in_flight());
    // Rows after the silence began are blank.
    let row = (p.width * 3) as usize;
    assert!(p.rgb[p.lines as usize * row..].iter().all(|&b| b == 0));
    // ...and the rows heard are the picture that was sent.
    let want = common::test_image(p.width, p.height);
    let n = p.lines as usize * row;
    let err: u64 = want[..n]
        .iter()
        .zip(&p.rgb[..n])
        .map(|(a, b)| u64::from(a.abs_diff(*b)))
        .sum();
    let err = err as f64 / n as f64;
    assert!(err <= 25.0, "kept rows mean error {err}");
    assert!(d.finish().is_none());
}

#[test]
fn finish_keeps_only_rows_heard_before_a_short_silence() {
    let rate = 8_000;
    let mut samples = cut(Mode::Robot36, &encode(Mode::Robot36, rate), rate, 0.6);
    // Shorter than the abandon threshold, but many rows long.
    samples.extend(std::iter::repeat_n(0, rate as usize));
    let mut d = Decoder::new(rate).unwrap();
    let _ = run(&mut d, &samples);
    assert!(d.in_flight());
    match d.finish() {
        Some(Event::Abandoned(p)) => {
            assert!((i64::from(p.lines) - 144).abs() <= 4, "lines {}", p.lines);
        }
        other => panic!("finish gave {:?}", other.as_ref().map(kind)),
    }
}

#[test]
fn under_half_is_not_worth_keeping() {
    let mut d = Decoder::new(8_000).unwrap();
    let p = last_abandoned(run(&mut d, &cut_then_silence(0.3)));
    assert!(p.partial);
    assert!(!p.worth_keeping(), "lines {}", p.lines);
}

#[test]
fn the_next_picture_decodes_after_an_abandon() {
    let rate = 8_000;
    let mut samples = cut_then_silence(0.6);
    let second_starts = samples.len() as u64;
    samples.extend(encode(Mode::Robot36, rate));
    samples.extend(std::iter::repeat_n(0, rate as usize));
    let mut d = Decoder::new(rate).unwrap();
    let ends: Vec<Event> = run(&mut d, &samples)
        .into_iter()
        .filter(|e| !matches!(e, Event::Line { .. }))
        .collect();
    assert_eq!(
        ends.len(),
        4,
        "{:?}",
        ends.iter().map(kind).collect::<Vec<_>>()
    );
    assert!(matches!(ends[0], Event::Started { .. }));
    assert!(matches!(ends[1], Event::Abandoned(_)));
    let Event::Started { at_sample, .. } = ends[2] else {
        panic!("third event is {}", kind(&ends[2]));
    };
    // The sample clock runs on across the internal reset.
    let header_end = second_starts + header_len(Mode::Robot36, rate) as u64;
    assert!(
        at_sample.abs_diff(header_end) <= u64::from(rate / 2),
        "{at_sample} vs {header_end}"
    );
    let Event::Finished(p) = &ends[3] else {
        panic!("fourth event is {}", kind(&ends[3]));
    };
    assert_eq!(p.at_sample, at_sample);
    assert!(mean_abs_error(p, Mode::Robot36) <= 25.0);
}

#[test]
fn finish_abandons_and_is_idempotent() {
    let rate = 8_000;
    let half = cut(Mode::Robot36, &encode(Mode::Robot36, rate), rate, 0.5);
    let mut d = Decoder::new(rate).unwrap();
    let _ = run(&mut d, &half);
    assert!(d.in_flight());
    let Some(Event::Abandoned(p)) = d.finish() else {
        panic!("finish did not abandon");
    };
    assert!(p.partial);
    assert!(p.lines > 100 && p.lines <= 120, "lines {}", p.lines);
    assert!(!d.in_flight());
    assert!(d.finish().is_none());
}

#[test]
fn png_round_trips_and_never_overwrites() {
    let (w, h) = (4_u32, 3_u32);
    let rgb: Vec<u8> = (0..w * h * 3).map(|i| (i * 7) as u8).collect();
    let pic = Picture {
        mode: Mode::Robot36,
        width: w,
        height: h,
        rgb: rgb.clone(),
        lines: h,
        quality: 0.9,
        at_sample: 0,
        partial: false,
        repaired_rows: 0,
    };
    let dir = std::env::temp_dir().join(format!("astar-sstv-png-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("N0CALL.png");
    let _ = std::fs::remove_file(&path);
    pic.write_png(&path).unwrap();

    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).unwrap()));
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    assert_eq!((info.width, info.height), (w, h));
    assert_eq!(info.color_type, png::ColorType::Rgb);
    assert_eq!(info.bit_depth, png::BitDepth::Eight);
    assert_eq!(&buf[..info.buffer_size()], &rgb[..]);

    // Another writer's file at the path is left exactly as it was.
    std::fs::write(&path, b"someone else's").unwrap();
    let err = pic.write_png(&path).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&path).unwrap(), b"someone else's");
    // A picture that can't be written leaves no file behind.
    let bad = sstv_picture_with_rgb(&pic, Vec::new());
    let other = dir.join("KX0AAA.png");
    assert!(bad.write_png(&other).is_err());
    assert!(!other.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn bad_rates_are_refused() {
    assert!(Decoder::new(0).is_err());
    assert!(Decoder::new(192_001).is_err());
    assert_eq!(Decoder::new(8_000).unwrap().rate(), 8_000);
}

fn kind(e: &Event) -> &'static str {
    match e {
        Event::Started { .. } => "Started",
        Event::Line { .. } => "Line",
        Event::Finished(_) => "Finished",
        Event::Abandoned(_) => "Abandoned",
        Event::Rejected { .. } => "Rejected",
    }
}

fn sstv_picture_with_rgb(pic: &astar_sstv::Picture, rgb: Vec<u8>) -> astar_sstv::Picture {
    astar_sstv::Picture { rgb, ..pic.clone() }
}
