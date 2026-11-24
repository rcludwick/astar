// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! P25 end to end, on `127.0.0.1` only: a second client speaks a tone into
//! the bench parrot, the parrot relays it, and a `P25Link` with the real
//! software IMBE decoder plays it onto the audio lane.

#![cfg(feature = "p25")]

use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::mpsc::channel;
use std::thread;
use std::time::{Duration, Instant};

use astar_audio::{CallAudio, RxFrame};
use astar_codec::p25::{FRAME_BYTES, FRAME_SAMPLES, ImbeEncoder};
use astar_console::{P25Config, P25Link};
use astar_p25::{RECORDS, Reflector, Superframe, end_record, superframe, wire};

/// `frames` frames of a sine at `hz`, at `dbfs` RMS.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn tone_frames(hz: f32, dbfs: f32, frames: usize) -> Vec<[i16; FRAME_SAMPLES]> {
    let amp = 32768.0 * 10f32.powf(dbfs / 20.0) * std::f32::consts::SQRT_2;
    let step = std::f32::consts::TAU * hz / 8000.0;
    (0..frames)
        .map(|f| {
            std::array::from_fn(|i| {
                let n = (f * FRAME_SAMPLES + i) as f32;
                (amp * (step * n).sin()).round() as i16
            })
        })
        .collect()
}

/// The frequency, in 10 Hz steps from 100 to 3900 Hz, with the most energy
/// in `pcm`: a plain DFT, enough to tell 1 kHz from anything else.
#[allow(clippy::cast_precision_loss)]
fn dominant_hz(pcm: &[i16]) -> f32 {
    let mut best = (0.0f32, 0.0f64);
    for step in 10..=390 {
        let hz = step as f32 * 10.0;
        let w = std::f64::consts::TAU * f64::from(hz) / 8000.0;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (n, s) in pcm.iter().enumerate() {
            let x = f64::from(*s);
            re += x * (w * n as f64).cos();
            im -= x * (w * n as f64).sin();
        }
        let power = re * re + im * im;
        if power > best.1 {
            best = (hz, power);
        }
    }
    best.0
}

/// RMS level in dBFS.
#[allow(clippy::cast_precision_loss)]
fn rms_dbfs(pcm: &[i16]) -> f64 {
    let sum: f64 = pcm.iter().map(|s| f64::from(*s).powi(2)).sum();
    let rms = (sum / pcm.len() as f64).sqrt();
    20.0 * (rms / 32768.0).max(1e-9).log10()
}

/// The level the software decoder gives a -20 dBFS tone through its own
/// encoder with `RX_TRIM_DB` at 0.0: `MEASURED_LOOP_DBFS` in
/// `astar_codec::p25`'s tests, -24.9 dBFS. Plus the trim, so a later trim
/// moves this with it.
const EXPECTED_DBFS: f64 = -24.9 + astar_codec::p25::RX_TRIM_DB as f64;

#[test]
fn a_phrase_fed_into_the_parrot_by_a_second_client_reaches_the_audio_lane() {
    let r = Reflector::bind_parrot("127.0.0.1:0".parse().expect("v4")).expect("bind parrot");
    let addr = r.local_addr();
    let parrot = r.run();

    let (rx_tx, rx_rx) = channel::<RxFrame>();
    let (_mic, tx_rx) = channel::<Vec<i16>>();
    let link = P25Link::connect_with_audio(
        &P25Config {
            host: addr.to_string(),
            callsign: "KC0ABC".into(),
        },
        CallAudio {
            tx_frames: tx_rx,
            rx_frames: rx_tx,
            preroll_lead: Arc::new(AtomicU32::new(0)),
        },
    )
    .expect("connect");
    let deadline = Instant::now() + Duration::from_secs(5);
    while link.snapshot().link_state != "linked" && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(link.snapshot().link_state, "linked");
    assert_eq!(link.snapshot().backend, Some("software"));

    // The injector: a second client on the same reflector.
    let injector = UdpSocket::bind("127.0.0.1:0").expect("bind injector");
    let them = wire::Callsign::new("W1AW").expect("callsign");
    injector
        .send_to(&wire::poll(&them), addr)
        .expect("register");
    thread::sleep(Duration::from_millis(100));

    let mut encoder = ImbeEncoder::new();
    let frames: Vec<[u8; FRAME_BYTES]> = tone_frames(1000.0, -20.0, 6 * RECORDS)
        .iter()
        .map(|pcm| encoder.encode(pcm))
        .collect();
    let info = Superframe {
        source_id: 1_234_567,
        talkgroup: 10_200,
        algid: 0x80,
        kid: 0,
    };
    let start = Instant::now();
    let mut sent = 0u32;
    for chunk in frames.chunks(RECORDS) {
        let chunk: &[[u8; FRAME_BYTES]; RECORDS] = chunk.try_into().expect("18 frames");
        for record in superframe(&info, chunk) {
            injector.send_to(&record, addr).expect("send");
            sent += 1;
            // One record every 20 ms, on a clock rather than by sleeping a
            // fixed 20 ms, so the pace does not drift.
            let due = start + Duration::from_millis(20 * u64::from(sent));
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
        }
    }
    injector.send_to(&end_record(), addr).expect("end");

    let mut pcm: Vec<i16> = Vec::new();
    let mut count = 0usize;
    let mut silent_frames = 0usize;
    let collect_until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < collect_until {
        if let Ok(f) = rx_rx.recv_timeout(Duration::from_millis(50)) {
            count += 1;
            if f.pcm.iter().all(|s| *s == 0) {
                silent_frames += 1;
            }
            pcm.extend_from_slice(&f.pcm);
        }
    }

    assert!(count >= 100, "only {count} frames reached the lane");
    let hz = dominant_hz(&pcm[FRAME_SAMPLES * 20..]);
    assert!((940.0..=1060.0).contains(&hz), "dominant {hz} Hz");
    let level = rms_dbfs(&pcm[FRAME_SAMPLES * 20..]);
    assert!(
        (level - EXPECTED_DBFS).abs() <= 6.0,
        "level {level:.1} dBFS, expected {EXPECTED_DBFS:.1} ± 6"
    );
    assert!(
        silent_frames <= 3,
        "{silent_frames} all-zero frames: lost records were filled with silence"
    );
    println!("frames {count}, level {level:.1} dBFS, silent {silent_frames}");
    let snap = link.snapshot();
    assert_eq!(snap.last_heard_id, Some(1_234_567));
    assert_eq!(snap.talkgroup, Some(10_200));
    assert!(!snap.encrypted);

    link.disconnect();
    parrot.shutdown();
}
