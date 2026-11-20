// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! Known holes: audio rebuilt from captured packets says exactly where each
//! lost 20 ms frame was. Fed through `push_hole`, the lost time is kept and
//! its rows are filled, instead of the re-timer having to find the step.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

mod common;

use astar_sstv::{Decoder, Event, Mode, Picture, Retime, decode_rebuilt};
use common::{decode_with, drop_frames, encode, header_len, mean_abs_error, run};

const RATE: u32 = 8_000;
const FRAME: usize = (RATE / 50) as usize;
const DROPS: usize = 6;

/// Where `drop_frames` drops its frames: one every 535 ms from 3 s into the
/// picture.
fn drop_points(mode: Mode) -> Vec<usize> {
    let start = header_len(mode, RATE) + 3 * RATE as usize;
    let spacing = (0.535 * f64::from(RATE)) as usize;
    (0..DROPS).map(|i| start + i * spacing).collect()
}

/// The picture as rebuilt from packets: every sample at its own time, the
/// lost frames zeros, and where they are.
fn rebuilt(mode: Mode) -> (Vec<i16>, Vec<(u64, u32)>) {
    let mut audio = encode(mode, RATE);
    let mut holes = Vec::new();
    for at in drop_points(mode) {
        audio[at..at + FRAME].fill(0);
        holes.push((at as u64, FRAME as u32));
    }
    (audio, holes)
}

fn finished(events: &[Event]) -> Picture {
    events
        .iter()
        .find_map(|e| match e {
            Event::Finished(p) => Some(p.clone()),
            _ => None,
        })
        .expect("a finished picture")
}

#[test]
fn known_holes_decode_like_the_clean_picture() {
    let mode = Mode::Scottie2;
    let clean = mean_abs_error(&decode_with(&encode(mode, RATE), RATE, Retime::On), mode);
    // The same frames lost on the audio path: dropped, so later lines come
    // early and the re-timer has to find and fix the steps.
    let start = header_len(mode, RATE) + 3 * RATE as usize;
    let dropped = decode_with(
        &drop_frames(&encode(mode, RATE), RATE, start, DROPS),
        RATE,
        Retime::On,
    );
    let audio_path = mean_abs_error(&dropped, mode);

    let (audio, holes) = rebuilt(mode);
    let events = decode_rebuilt(RATE, &audio, &holes).unwrap();
    let pic = finished(&events);
    let known = mean_abs_error(&pic, mode);
    eprintln!(
        "{mode:?}: clean {clean:.2}, audio path {audio_path:.2} ({} rows), known holes {known:.2} ({} rows, q {:.2})",
        dropped.repaired_rows, pic.repaired_rows, pic.quality
    );
    assert!(pic.quality >= 0.43, "quality {}", pic.quality);
    assert!(
        known <= clean + 1.0,
        "known holes {known:.2} vs clean {clean:.2}"
    );
    assert!(
        known < audio_path,
        "known holes {known:.2} vs audio path {audio_path:.2}"
    );
    // Each 20 ms hole sits inside one or two rows of a 277 ms line.
    assert!(
        (DROPS as u32..=2 * DROPS as u32).contains(&pic.repaired_rows),
        "{} rows repaired",
        pic.repaired_rows
    );
}

#[test]
fn known_holes_keep_their_time() {
    // The same holes fed as plain zeros would look like stuffed audio to
    // the re-timer and be cut, pulling every later line early. As known
    // holes they keep their length: the picture's rows line up.
    let mode = Mode::Scottie2;
    let (audio, holes) = rebuilt(mode);
    let pic = finished(&decode_rebuilt(RATE, &audio, &holes).unwrap());
    let rows = pic.height;
    let top = common::best_shift(&pic, 8..40);
    let bottom = common::best_shift(&pic, rows - 40..rows - 8);
    assert_eq!(
        bottom, top,
        "the bottom of the picture moved against the top"
    );
    // Fed as plain audio, the zeros are cut and the bottom moves.
    let plain = finished(&decode_rebuilt(RATE, &audio, &[]).unwrap());
    assert_ne!(
        common::best_shift(&plain, rows - 40..rows - 8),
        common::best_shift(&plain, 8..40),
        "plain zeros were expected to be cut"
    );
}

#[test]
fn a_hole_before_any_header_does_nothing() {
    let mut d = Decoder::new(RATE).unwrap();
    assert_eq!(d.push_hole(2 * FRAME), Vec::new());
    assert!(!d.in_flight());
    assert_eq!(d.samples_in(), 2 * FRAME as u64);
    // A picture after it still decodes, placed after the hole's time.
    let mut audio = encode(Mode::Robot36, RATE);
    audio.extend(std::iter::repeat_n(0, RATE as usize));
    let events = run(&mut d, &audio);
    let pic = finished(&events);
    assert_eq!(pic.repaired_rows, 0);
    assert!(pic.at_sample > 2 * FRAME as u64);
}

#[test]
fn holes_are_fed_in_order_with_the_samples() {
    // decode_rebuilt and push/push_hole by hand agree.
    let mode = Mode::Scottie2;
    let (audio, holes) = rebuilt(mode);
    let by_fn = finished(&decode_rebuilt(RATE, &audio, &holes).unwrap());
    let mut d = Decoder::new(RATE).unwrap();
    let mut events = Vec::new();
    let mut at = 0;
    for &(start, len) in &holes {
        events.extend(run(&mut d, &audio[at..start as usize]));
        events.extend(d.push_hole(len as usize));
        at = start as usize + len as usize;
    }
    events.extend(run(&mut d, &audio[at..]));
    events.extend(run(&mut d, &vec![0; 3 * RATE as usize]));
    let by_hand = finished(&events);
    assert_eq!(by_fn.rgb, by_hand.rgb);
    assert_eq!(by_fn.repaired_rows, by_hand.repaired_rows);
}
