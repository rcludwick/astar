// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! Rebuilding a call's received audio from its captured voice packets:
//! synthetic IAX frames only, built with astar-iax-core's encoder, never a
//! real capture.

#![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

use std::net::SocketAddr;

use astar_codec::g711::ulaw_encode;
use astar_codec::slin;
use astar_iax::rebuild::{RebuildError, rebuild_audio, rebuild_received};
use astar_iax_core::frame::{FullFrame, MiniFrame, Subclass};
use astar_iax_core::{Frame, FrameType, Ies, VoiceFormat, encode};
use astar_record::pcap::PcapRecord;

const PEER: &str = "192.0.2.10:4569";
const US: &str = "192.0.2.20:4569";
const CALL: u16 = 7;

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn full_voice(ts: u32, format: VoiceFormat, payload: &[u8]) -> Vec<u8> {
    let f = FullFrame {
        source_call: CALL,
        dest_call: 3,
        retransmission: false,
        timestamp: ts,
        oseqno: 0,
        iseqno: 0,
        frame_type: FrameType::Voice,
        subclass: Subclass::Voice(format),
        ies: Ies::empty(),
        payload,
    };
    let mut out = Vec::new();
    encode(&Frame::Full(Box::new(f)), &mut out).unwrap();
    out
}

fn mini_voice(ts: u32, payload: &[u8]) -> Vec<u8> {
    let m = MiniFrame {
        source_call: CALL,
        timestamp: (ts & 0xFFFF) as u16,
        payload,
    };
    let mut out = Vec::new();
    encode(&Frame::Mini(m), &mut out).unwrap();
    out
}

fn record(arrival_ms: u64, from: &str, payload: Vec<u8>) -> PcapRecord {
    let (src, dst) = if from == PEER {
        (addr(PEER), addr(US))
    } else {
        (addr(US), addr(PEER))
    };
    PcapRecord {
        ts_ns: 1_000_000_000 + arrival_ms * 1_000_000,
        src,
        dst,
        payload,
    }
}

/// A ramp so every frame's audio is different: frame `k` sample `i`.
fn pcm(k: u32, n: usize) -> Vec<i16> {
    (0..n)
        .map(|i| (((k as usize * 37 + i * 13) % 2000) as i16 - 1000) * 8)
        .collect()
}

fn ulaw(pcm: &[i16]) -> Vec<u8> {
    pcm.iter().map(|&s| ulaw_encode(s)).collect()
}

/// `n` µ-law frames, 20 ms apart from `start_ts`, as they would arrive in
/// order: one full frame first, then minis.
fn ulaw_call(start_ts: u32, n: u32) -> Vec<(u32, Vec<u8>)> {
    (0..n)
        .map(|k| {
            let ts = start_ts + 20 * k;
            let payload = ulaw(&pcm(k, 160));
            let bytes = if k == 0 {
                full_voice(ts, VoiceFormat::G711U, &payload)
            } else {
                mini_voice(ts, &payload)
            };
            (ts, bytes)
        })
        .collect()
}

fn arrive(frames: &[(u32, Vec<u8>)], order: &[usize]) -> Vec<PcapRecord> {
    order
        .iter()
        .enumerate()
        .map(|(n, &k)| {
            // Arrival time follows arrival order, 20 ms apart.
            record(n as u64 * 20, PEER, frames[k].1.clone())
        })
        .collect()
}

#[test]
fn in_order_frames_rebuild_contiguously() {
    let frames = ulaw_call(1000, 50);
    let order: Vec<usize> = (0..50).collect();
    let r = rebuild_audio(&arrive(&frames, &order), None).unwrap();
    assert_eq!(r.rate, 8000);
    assert_eq!(r.samples.len(), 50 * 160);
    assert_eq!(r.frames, 50);
    assert_eq!((r.reordered, r.duplicates, r.lost), (0, 0, 0));
    assert_eq!(r.holes, Vec::new());
    // Frame 3's audio sits where frame 3 belongs.
    let want: Vec<i16> = ulaw(&pcm(3, 160))
        .iter()
        .map(|&b| astar_codec::g711::ulaw_decode(b))
        .collect();
    assert_eq!(&r.samples[3 * 160..4 * 160], &want[..]);
}

#[test]
fn shuffled_frames_rebuild_byte_identical() {
    let frames = ulaw_call(1000, 60);
    let order: Vec<usize> = (0..60).collect();
    let clean = rebuild_audio(&arrive(&frames, &order), None).unwrap();
    // Every frame displaced by up to three places, the full frame included.
    let mut shuffled = order.clone();
    for k in (0..57).step_by(4) {
        shuffled.swap(k, k + 3);
        shuffled.swap(k + 1, k + 2);
    }
    assert_ne!(shuffled, order);
    let r = rebuild_audio(&arrive(&frames, &shuffled), None).unwrap();
    assert_eq!(r.samples, clean.samples);
    assert_eq!(r.holes, Vec::new());
    assert!(r.reordered > 0, "{}", r.reordered);
    assert_eq!(r.lost, 0);
}

#[test]
fn duplicates_are_dropped() {
    let frames = ulaw_call(1000, 20);
    let mut order: Vec<usize> = (0..20).collect();
    order.insert(5, 4);
    order.insert(12, 10);
    order.push(0); // a retransmitted full frame, late
    let r = rebuild_audio(&arrive(&frames, &order), None).unwrap();
    assert_eq!(r.samples.len(), 20 * 160);
    assert_eq!(r.duplicates, 3);
    assert_eq!(r.frames, 20);
}

#[test]
fn mini_timestamps_wrap_across_65536_ms() {
    // Starts 400 ms before the 16-bit stamp wraps.
    let start = 0x0003_0000 - 400;
    let frames = ulaw_call(start, 50);
    let order: Vec<usize> = (0..50).collect();
    let r = rebuild_audio(&arrive(&frames, &order), None).unwrap();
    assert_eq!(r.samples.len(), 50 * 160);
    assert_eq!(r.holes, Vec::new());
    assert_eq!(r.lost, 0);
}

#[test]
fn a_lost_frame_becomes_a_hole_of_its_length() {
    let frames = ulaw_call(1000, 30);
    let order: Vec<usize> = (0..30).filter(|&k| k != 10 && k != 20 && k != 21).collect();
    let r = rebuild_audio(&arrive(&frames, &order), None).unwrap();
    assert_eq!(r.samples.len(), 30 * 160);
    assert_eq!(r.holes, vec![(10 * 160, 160), (20 * 160, 320)]);
    assert_eq!(r.lost, 3);
    assert_eq!(r.frames, 27);
    assert!(r.samples[10 * 160..11 * 160].iter().all(|&s| s == 0));
}

#[test]
fn the_codec_comes_from_the_full_frame_unless_given() {
    // Slin (big-endian, 8 kHz) named only by the full frame.
    let make = |format: VoiceFormat| -> Vec<PcapRecord> {
        (0..10u32)
            .map(|k| {
                let payload = slin::encode(&pcm(k, 160));
                let bytes = if k == 0 {
                    full_voice(500 + 20 * k, format, &payload)
                } else {
                    mini_voice(500 + 20 * k, &payload)
                };
                record(u64::from(k) * 20, PEER, bytes)
            })
            .collect()
    };
    let r = rebuild_audio(&make(VoiceFormat::Slin), None).unwrap();
    assert_eq!(r.rate, 8000);
    assert_eq!(&r.samples[160..320], &pcm(1, 160)[..]);
    // Told otherwise, the argument wins: read as µ-law, 320 bytes per frame.
    let r = rebuild_audio(&make(VoiceFormat::Slin), Some(VoiceFormat::G711U)).unwrap();
    assert_eq!(r.rate, 8000);
    assert_ne!(&r.samples[320..480], &pcm(1, 160)[..]);
}

#[test]
fn slin16_rebuilds_at_16k() {
    let records: Vec<PcapRecord> = (0..10u32)
        .map(|k| {
            let payload = slin::encode_le(&pcm(k, 320));
            let bytes = if k == 0 {
                full_voice(20 * k, VoiceFormat::Slin16, &payload)
            } else {
                mini_voice(20 * k, &payload)
            };
            record(u64::from(k) * 20, PEER, bytes)
        })
        .collect();
    let r = rebuild_audio(&records, None).unwrap();
    assert_eq!(r.rate, 16_000);
    assert_eq!(r.samples.len(), 10 * 320);
    assert_eq!(&r.samples[320 * 4..320 * 5], &pcm(4, 320)[..]);
}

#[test]
fn only_the_busiest_peer_and_only_voice_count() {
    let frames = ulaw_call(1000, 20);
    let order: Vec<usize> = (0..20).collect();
    let mut records = arrive(&frames, &order);
    // Our own transmitted voice, fewer frames, and a non-voice full frame.
    for k in 0..5u32 {
        let bytes = mini_voice(90_000 + 20 * k, &ulaw(&[1000; 160]));
        records.push(record(400 + u64::from(k), US, bytes));
    }
    let ping = {
        let f = FullFrame {
            source_call: CALL,
            dest_call: 3,
            retransmission: false,
            timestamp: 1200,
            oseqno: 1,
            iseqno: 1,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(astar_iax_core::IaxCommand::Ping),
            ies: Ies::empty(),
            payload: &[],
        };
        let mut out = Vec::new();
        encode(&Frame::Full(Box::new(f)), &mut out).unwrap();
        out
    };
    records.insert(3, record(55, PEER, ping));
    let r = rebuild_audio(&records, None).unwrap();
    assert_eq!(r.samples.len(), 20 * 160);
    assert_eq!(r.frames, 20);
}

#[test]
fn no_codec_anywhere_is_an_error() {
    let records: Vec<PcapRecord> = (0..5u32)
        .map(|k| record(u64::from(k) * 20, PEER, mini_voice(20 * k, &[0xFF; 160])))
        .collect();
    assert_eq!(rebuild_audio(&records, None), Err(RebuildError::NoCodec));
    // Minis alone rebuild fine when the codec is given.
    let r = rebuild_audio(&records, Some(VoiceFormat::G711U)).unwrap();
    assert_eq!(r.samples.len(), 5 * 160);
    assert_eq!(rebuild_audio(&[], None), Err(RebuildError::NoVoice));
}

/// Frame `k` of a µ-law call, stamped `ts`, as a full frame or a mini.
fn ulaw_frame(k: u32, ts: u32, full: bool) -> Vec<u8> {
    let payload = ulaw(&pcm(k, 160));
    if full {
        full_voice(ts, VoiceFormat::G711U, &payload)
    } else {
        mini_voice(ts, &payload)
    }
}

/// Records arriving 20 ms apart, in the order given.
fn in_arrival_order(frames: Vec<Vec<u8>>) -> Vec<PcapRecord> {
    frames
        .into_iter()
        .enumerate()
        .map(|(n, bytes)| record(n as u64 * 20, PEER, bytes))
        .collect()
}

#[test]
fn a_wild_full_frame_is_rejected_not_allocated() {
    let frames = ulaw_call(1000, 50);
    let order: Vec<usize> = (0..50).collect();
    let clean = rebuild_audio(&arrive(&frames, &order), None).unwrap();
    // Frame 25 comes as a full frame stamped near the top of the clock:
    // placed as stamped it would sit 49 days in, about 68 GB of audio.
    let mut wild = frames.clone();
    wild[25].1 = ulaw_frame(25, 0xFFFF_0000, true);
    let r = rebuild_audio(&arrive(&wild, &order), None).unwrap();
    assert_eq!(r.samples.len(), clean.samples.len());
    assert_eq!(r.holes, vec![(25 * 160, 160)]);
    assert_eq!((r.lost, r.rejected, r.frames), (1, 1, 49));
    assert_eq!(&r.samples[26 * 160..], &clean.samples[26 * 160..]);
}

#[test]
fn a_far_stamped_full_frame_is_rejected() {
    // Ten minutes ahead: within reach of the old code, so a plain failure.
    let frames = ulaw_call(1000, 50);
    let order: Vec<usize> = (0..50).collect();
    let mut wild = frames.clone();
    wild[25].1 = ulaw_frame(25, 1000 + 600_000, true);
    let r = rebuild_audio(&arrive(&wild, &order), None).unwrap();
    assert_eq!(r.samples.len(), 50 * 160);
    assert_eq!((r.lost, r.rejected), (1, 1));
}

#[test]
fn a_wild_only_full_frame_still_gives_a_bounded_result() {
    // The one full frame, first, is wild; every mini is sane.
    let frames: Vec<Vec<u8>> = (0..50u32)
        .map(|k| {
            if k == 0 {
                ulaw_frame(0, 0xFFFF_0000, true)
            } else {
                ulaw_frame(k, 1000 + 20 * k, false)
            }
        })
        .collect();
    let r = rebuild_audio(&in_arrival_order(frames), None).unwrap();
    // No longer than the capture's arrival span plus its margin.
    assert!(
        r.samples.len() <= (49 * 20 + 2000 + 20) * 8,
        "{}",
        r.samples.len()
    );
    assert!(r.frames >= 49, "{}", r.frames);
}

#[test]
fn a_long_stamp_jump_is_no_longer_than_its_arrival_gap() {
    // Frames 10.. are stamped five seconds on but arrive without a break.
    let frames: Vec<Vec<u8>> = (0..20u32)
        .map(|k| {
            let ts = 1000 + 20 * k + if k >= 10 { 5000 } else { 0 };
            ulaw_frame(k, ts, k == 0)
        })
        .collect();
    let r = rebuild_audio(&in_arrival_order(frames), None).unwrap();
    assert_eq!(r.samples.len(), 20 * 160);
    assert_eq!((r.lost, r.holes.len()), (0, 0));
}

#[test]
fn a_real_pause_keeps_its_length_as_silence() {
    // Five seconds of nothing, in both stamps and arrival.
    let records: Vec<PcapRecord> = (0..20u32)
        .map(|k| {
            let gap = if k >= 10 { 5000 } else { 0 };
            let ts = 1000 + 20 * k + gap;
            record(u64::from(20 * k + gap), PEER, ulaw_frame(k, ts, k == 0))
        })
        .collect();
    let r = rebuild_audio(&records, None).unwrap();
    assert_eq!(r.samples.len(), 20 * 160 + 5000 * 8);
    assert_eq!((r.lost, r.holes.len()), (0, 0));
}

#[test]
fn the_timeline_never_outgrows_the_capture() {
    // Every stamp 900 ms on from the last (under the pause limit), but the
    // frames arrive 20 ms apart: the audio cannot be longer than the
    // capture plus its margin.
    let frames: Vec<Vec<u8>> = (0..40u32)
        .map(|k| ulaw_frame(k, 1000 + 900 * k, k == 0))
        .collect();
    let r = rebuild_audio(&in_arrival_order(frames), None).unwrap();
    assert!(
        r.samples.len() <= (39 * 20 + 2000) * 8,
        "{}",
        r.samples.len()
    );
    assert!(r.rejected > 0);
}

#[test]
fn timing_slop_under_half_a_frame_is_not_a_loss() {
    // Frame 10 stamped 8 ms late (64 samples, under half a frame).
    let frames: Vec<Vec<u8>> = (0..20u32)
        .map(|k| {
            let ts = 1000 + 20 * k + if k == 10 { 8 } else { 0 };
            ulaw_frame(k, ts, k == 0)
        })
        .collect();
    let r = rebuild_audio(&in_arrival_order(frames), None).unwrap();
    assert_eq!((r.lost, r.holes.len()), (0, 0));
    assert_eq!(r.samples.len(), 20 * 160);
    // Half a frame or more is a loss of one.
    let frames: Vec<Vec<u8>> = (0..20u32)
        .map(|k| {
            let ts = 1000 + 20 * k + if k >= 10 { 12 } else { 0 };
            ulaw_frame(k, ts, k == 0)
        })
        .collect();
    let r = rebuild_audio(&in_arrival_order(frames), None).unwrap();
    assert_eq!(r.lost, 1);
    assert_eq!(r.holes, vec![(10 * 160, 96)]);
}

#[test]
fn equally_busy_senders_pick_the_first_to_arrive() {
    const OTHER: &str = "192.0.2.30:4569";
    for _ in 0..20 {
        let mut records = Vec::new();
        for k in 0..10u32 {
            // OTHER starts first, PEER 10 ms after; both 20 ms apart.
            let other = PcapRecord {
                ts_ns: 1_000_000_000 + u64::from(k) * 20_000_000,
                src: addr(OTHER),
                dst: addr(US),
                payload: if k == 0 {
                    full_voice(20 * k, VoiceFormat::G711U, &ulaw(&[2000; 160]))
                } else {
                    mini_voice(20 * k, &ulaw(&[2000; 160]))
                },
            };
            records.push(other);
            records.push(record(
                u64::from(k) * 20 + 10,
                PEER,
                ulaw_frame(k, 20 * k, k == 0),
            ));
        }
        let r = rebuild_audio(&records, None).unwrap();
        let want = astar_codec::g711::ulaw_decode(ulaw_encode(2000));
        assert!(r.samples.iter().all(|&s| s == want));
    }
}

#[test]
fn our_own_stream_never_wins_when_the_local_address_is_known() {
    let frames = ulaw_call(1000, 10);
    let order: Vec<usize> = (0..10).collect();
    let mut records = arrive(&frames, &order);
    // We sent twice as much as we heard.
    for k in 0..20u32 {
        let bytes = mini_voice(90_000 + 20 * k, &ulaw(&[1000; 160]));
        records.push(record(500 + u64::from(k) * 20, US, bytes));
    }
    let r = rebuild_received(&records, Some(VoiceFormat::G711U), Some(addr(US))).unwrap();
    assert_eq!(r.samples.len(), 10 * 160);
    assert_eq!(&r.samples[..160], &clean_frame(0)[..]);
}

fn clean_frame(k: u32) -> Vec<i16> {
    ulaw(&pcm(k, 160))
        .iter()
        .map(|&b| astar_codec::g711::ulaw_decode(b))
        .collect()
}

#[test]
fn reordered_frames_across_a_16_bit_wrap_rebuild_identically() {
    let frames = ulaw_call(0x0003_0000 - 200, 40);
    let order: Vec<usize> = (0..40).collect();
    let clean = rebuild_audio(&arrive(&frames, &order), None).unwrap();
    let mut shuffled = order.clone();
    // Swaps right over the wrap (frames 9..=11 straddle it).
    shuffled.swap(8, 11);
    shuffled.swap(9, 10);
    shuffled.swap(12, 14);
    let r = rebuild_audio(&arrive(&frames, &shuffled), None).unwrap();
    assert_eq!(r.samples, clean.samples);
    assert_eq!((r.lost, r.rejected), (0, 0));
    assert!(r.reordered > 0);
}

#[test]
fn minis_before_the_first_full_frame_find_their_epoch() {
    // Just short of a wrap; the only full frame is frame 10, so the first
    // ten minis take their high bits from a frame that arrives later.
    let start = 0x0005_0000 - 100;
    let frames: Vec<Vec<u8>> = (0..30u32)
        .map(|k| ulaw_frame(k, start + 20 * k, k == 10))
        .collect();
    let r = rebuild_audio(&in_arrival_order(frames), None).unwrap();
    assert_eq!(r.samples.len(), 30 * 160);
    assert_eq!((r.lost, r.holes.len(), r.frames), (0, 0, 30));
    assert_eq!(&r.samples[3 * 160..4 * 160], &clean_frame(3)[..]);
}

#[test]
fn garbage_is_skipped_never_a_panic() {
    // Slin with odd-length payloads among good ones.
    let records: Vec<PcapRecord> = (0..10u32)
        .map(|k| {
            let mut payload = slin::encode(&pcm(k, 160));
            if k % 3 == 1 {
                payload.pop();
            }
            let bytes = if k == 0 {
                full_voice(20 * k, VoiceFormat::Slin, &payload)
            } else {
                mini_voice(20 * k, &payload)
            };
            record(u64::from(k) * 20, PEER, bytes)
        })
        .collect();
    let r = rebuild_audio(&records, None).unwrap();
    assert_eq!(r.frames, 7);
    assert_eq!(r.lost, 3);

    // GSM with short payloads among whole 33-byte frames.
    let records: Vec<PcapRecord> = (0..10u32)
        .map(|k| {
            let payload = vec![0u8; if k % 4 == 2 { 20 } else { 33 }];
            let bytes = if k == 0 {
                full_voice(20 * k, VoiceFormat::Gsm, &payload)
            } else {
                mini_voice(20 * k, &payload)
            };
            record(u64::from(k) * 20, PEER, bytes)
        })
        .collect();
    let r = rebuild_audio(&records, None).unwrap();
    assert_eq!(r.frames, 8);

    // Unparseable and truncated packets, from a fixed pseudo-random stream,
    // mixed into a good call.
    let frames = ulaw_call(1000, 20);
    let order: Vec<usize> = (0..20).collect();
    let mut records = arrive(&frames, &order);
    let mut x: u32 = 0x1234_5678;
    for n in 0..400u64 {
        let len = (n % 48) as usize;
        let bytes: Vec<u8> = (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        records.push(record(n, PEER, bytes));
    }
    // A full voice header with nothing after it, and an empty mini.
    let mut header_only = full_voice(1500, VoiceFormat::G711U, &[]);
    header_only.truncate(12);
    records.push(record(30, PEER, header_only));
    records.push(record(31, PEER, mini_voice(1520, &[])));
    let _ = rebuild_audio(&records, None);
    let _ = rebuild_audio(&records, Some(VoiceFormat::Gsm));
    let _ = rebuild_audio(&records, Some(VoiceFormat::Slin16));
}
