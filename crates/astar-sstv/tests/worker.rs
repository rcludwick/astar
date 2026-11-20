// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The decoder thread: outcomes in order, the live picture, stop, a change of
//! sample rate, silence, and a full tap.

mod common;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

use astar_record::tap::{PcmChunk, Tap, tap_channel};
use astar_sstv::{Mode, Outcome, SSTV_TAP_CAPACITY, Worker};
use common::{cut, encode};

type Seen = Arc<Mutex<Vec<Outcome>>>;

fn start() -> (Tap<PcmChunk>, Worker, Seen) {
    let (tap, rx) = tap_channel(SSTV_TAP_CAPACITY);
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let worker = Worker::spawn(rx, Box::new(move |o| sink.lock().unwrap().push(o))).expect("spawn");
    (tap, worker, seen)
}

/// Offer `samples` in 20 ms chunks, waiting when the tap is full.
fn feed(tap: &Tap<PcmChunk>, samples: &[i16], rate: u32) {
    for c in samples.chunks((rate / 50) as usize) {
        let chunk = PcmChunk {
            rate,
            samples: c.to_vec(),
        };
        while !tap.offer(chunk.clone()) {
            sleep(Duration::from_millis(5));
        }
    }
}

fn wait_for(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !ok() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        sleep(Duration::from_millis(20));
    }
}

fn kinds(seen: &Seen) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|o| match o {
            Outcome::Started { seq, .. } => format!("started {seq}"),
            Outcome::Finished { seq, .. } => format!("finished {seq}"),
            Outcome::Abandoned { seq, .. } => format!("abandoned {seq}"),
            Outcome::Rejected { seq } => format!("rejected {seq}"),
        })
        .collect()
}

#[test]
fn worker_reports_started_then_finished() {
    let (tap, worker, seen) = start();
    let mut audio = encode(Mode::Robot36, 8_000);
    audio.extend(std::iter::repeat_n(0, 8_000));
    feed(&tap, &audio, 8_000);
    wait_for("finished", 60, || {
        seen.lock()
            .unwrap()
            .iter()
            .any(|o| matches!(o, Outcome::Finished { .. }))
    });
    worker.stop();
    assert_eq!(kinds(&seen), ["started 1", "finished 1"]);
    let seen = seen.lock().unwrap();
    let Outcome::Started {
        mode,
        width,
        height,
        rate,
        expected_secs,
        ..
    } = seen[0]
    else {
        panic!("first outcome");
    };
    assert_eq!(
        (mode, width, height, rate),
        (Mode::Robot36, 320, 240, 8_000)
    );
    assert!((expected_secs - 36.0).abs() < 2.0, "{expected_secs}");
    let Outcome::Finished { picture, rate, .. } = &seen[1] else {
        panic!("second outcome");
    };
    assert_eq!(*rate, 8_000);
    assert_eq!(picture.lines, 240);
}

#[test]
fn live_fills_while_decoding() {
    let (tap, worker, seen) = start();
    let audio = encode(Mode::Robot36, 8_000);
    feed(&tap, &cut(Mode::Robot36, &audio, 8_000, 0.5), 8_000);
    wait_for("some rows", 20, || {
        worker.live_lines().is_some_and(|(_, l)| l > 0)
    });
    let live = worker.live().expect("live picture");
    assert_eq!(live.seq, 1);
    assert_eq!(live.mode, Mode::Robot36);
    assert!(
        live.lines >= 1 && live.lines < live.height,
        "{}",
        live.lines
    );
    assert_eq!(live.rgb.len(), (live.width * live.height * 3) as usize);
    worker.stop();
    assert_eq!(kinds(&seen), ["started 1", "abandoned 1"]);
}

#[test]
fn stop_abandons_the_picture_in_flight() {
    let (tap, worker, seen) = start();
    let audio = encode(Mode::Robot36, 8_000);
    feed(&tap, &cut(Mode::Robot36, &audio, 8_000, 0.4), 8_000);
    wait_for("start", 20, || worker.live().is_some());
    worker.stop();
    let seen = seen.lock().unwrap();
    assert!(matches!(seen[0], Outcome::Started { seq: 1, .. }));
    let Outcome::Abandoned {
        seq: 1, picture, ..
    } = &seen[1]
    else {
        panic!("{seen:?}");
    };
    assert!(picture.partial && picture.lines > 0);
}

#[test]
fn a_rate_change_rebuilds_the_decoder() {
    let (tap, worker, seen) = start();
    let slow = encode(Mode::Robot36, 8_000);
    feed(&tap, &cut(Mode::Robot36, &slow, 8_000, 0.5), 8_000);
    let mut fast = encode(Mode::Robot36, 16_000);
    fast.extend(std::iter::repeat_n(0, 16_000));
    feed(&tap, &fast, 16_000);
    wait_for("second picture", 90, || {
        seen.lock()
            .unwrap()
            .iter()
            .any(|o| matches!(o, Outcome::Finished { .. }))
    });
    worker.stop();
    assert_eq!(
        kinds(&seen),
        ["started 1", "abandoned 1", "started 2", "finished 2"]
    );
    let seen = seen.lock().unwrap();
    assert!(matches!(seen[3], Outcome::Finished { rate: 16_000, .. }));
}

#[test]
fn no_audio_for_two_seconds_abandons() {
    let (tap, worker, seen) = start();
    let audio = encode(Mode::Robot36, 8_000);
    feed(&tap, &cut(Mode::Robot36, &audio, 8_000, 0.4), 8_000);
    wait_for("abandon", 30, || {
        seen.lock()
            .unwrap()
            .iter()
            .any(|o| matches!(o, Outcome::Abandoned { .. }))
    });
    assert!(worker.live().is_none());
    worker.stop();
    assert_eq!(kinds(&seen), ["started 1", "abandoned 1"]);
}

#[test]
fn a_full_tap_drops_and_counts() {
    let (tap, rx) = tap_channel::<PcmChunk>(SSTV_TAP_CAPACITY);
    for _ in 0..300 {
        tap.offer(PcmChunk {
            rate: 8_000,
            samples: vec![0; 160],
        });
    }
    assert_eq!(tap.dropped_counter().load(Ordering::Relaxed), 44);
    drop(rx);
}
