// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! Rebuild a call's received audio from its captured voice packets.
//!
//! By the time received audio reaches a listener it has been through the
//! jitter buffer: a packet that arrived late was concealed or dropped, and a
//! lost one left a gap nobody can measure. The packets themselves say more.
//! Every IAX voice frame carries the sender's millisecond timestamp, so the
//! captured packets show exactly where each frame of audio belongs and which
//! frames never came.
//!
//! [`rebuild_audio`] takes the packets of a packet-log capture (as
//! [`astar_record::pcap::read_pcap`] returns them) and:
//!
//! 1. keeps only voice frames ([`astar_iax_core::frame::is_media`]) from one
//!    sender, the address and call number with the most of them (the first
//!    heard, between equals), never one sent from our own address when
//!    [`rebuild_received`] is told it;
//! 2. drops any full frame whose stamp disagrees with its arrival time by
//!    more than a few seconds, as the rest of the call places it;
//! 3. widens each mini frame's 16-bit timestamp to the full clock, from the
//!    previous frame's stamp (or, for the first, the nearest full voice
//!    frame) moved on by the arrival time between them, so a wrap every
//!    65.536 s is followed;
//! 4. sorts the frames by timestamp and drops duplicates;
//! 5. decodes each one (GSM through one decoder run in timestamp order) and
//!    puts it at `timestamp × rate / 1000` samples from the first frame.
//!
//! A gap of half a frame or more between frames becomes a known hole: zeros
//! in [`Rebuilt::samples`], and its place and length in [`Rebuilt::holes`].
//! Less than that is timing slop, and the frame follows on directly. A gap
//! longer than [`PAUSE_MS`] is the sender pausing, not loss: it is silence
//! without a hole, and no longer than the time the capture shows passing
//! between the two frames.
//!
//! Stamps come from the peer and are not trusted to size anything: the
//! audio is never longer than the capture's arrival span plus a margin of
//! two seconds, nor than [`MAX_MS`]. Frames that would land past that are
//! dropped and counted in [`Rebuilt::rejected`].

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use astar_codec::gsm::{FRAME_BYTES as GSM_BYTES, GsmDecoder};
use astar_iax_core::frame::{self, Frame, Subclass};
use astar_iax_core::{FrameType, VoiceFormat};
use astar_record::pcap::PcapRecord;

use crate::codec_edge::decode_voice;

/// A gap between frames longer than this many milliseconds is the sender
/// pausing, not lost packets.
pub const PAUSE_MS: u32 = 1000;

/// A full frame whose stamp is further than this many milliseconds from
/// where its arrival time and the rest of the call put it is dropped.
const WILD_MS: i64 = 3000;

/// How far past the capture's arrival span the audio may run, in
/// milliseconds: stamps jitter, and the last frame has a length.
const MARGIN_MS: u64 = 2000;

/// The longest audio a rebuild ever makes: 15 minutes, in milliseconds.
/// Frames past it are dropped.
pub const MAX_MS: u64 = 15 * 60 * 1000;

/// How many clusters of full-frame clocks are scored against the minis.
const MAX_CLUSTERS: usize = 16;

/// One 16-bit timestamp wrap, and half of one.
const WRAP: i64 = 0x1_0000;
const HALF_WRAP: i64 = 0x8000;

/// A call's received audio, rebuilt from its voice packets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rebuilt {
    /// Sample rate of `samples`, from the codec.
    pub rate: u32,
    /// The audio, sample 0 being the first frame by timestamp. Holes are
    /// zeros.
    pub samples: Vec<i16>,
    /// Known-lost audio as (first sample, length in samples), in order.
    pub holes: Vec<(u64, u32)>,
    /// Frames placed.
    pub frames: u32,
    /// Frames that arrived after a frame with a later timestamp.
    pub reordered: u32,
    /// Frames dropped as copies of one already placed.
    pub duplicates: u32,
    /// Frames missing, counted in the holes: a gap of half a frame or more,
    /// rounded to whole frames. A gap over [`PAUSE_MS`] is silence and is
    /// not counted here, even if the sender's packets were lost in it.
    pub lost: u32,
    /// Frames dropped as implausible: a full frame stamped far from where
    /// its arrival puts it, or a frame that would land past the length cap.
    pub rejected: u32,
}

/// Why no audio could be rebuilt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RebuildError {
    /// The packets hold no voice frames.
    #[error("the capture holds no voice frames")]
    NoVoice,
    /// No codec was given and no full voice frame names one.
    #[error("no codec given and no full voice frame names one")]
    NoCodec,
    /// The codec is one astar cannot decode.
    #[error("cannot decode {} audio", .0.wire_name())]
    Unsupported(VoiceFormat),
}

/// One voice frame, as captured.
struct Captured<'a> {
    arrival_ns: u64,
    /// The full 32-bit stamp of a full frame, or `None` for a mini frame.
    full: Option<u32>,
    /// The stamp as sent: 32 bits for a full frame, 16 for a mini frame.
    raw: u32,
    format: Option<VoiceFormat>,
    payload: &'a [u8],
}

/// Rebuild the received audio of the busiest sender in `packets`, decoded
/// as `codec`, or as the first full voice frame says when `codec` is `None`.
///
/// This cannot tell received packets from sent ones, so a capture holding
/// both directions may rebuild our own transmitted audio if we sent more.
/// A packet-log capture holds received packets only; for any other, use
/// [`rebuild_received`] with the local address.
///
/// # Errors
///
/// [`RebuildError::NoVoice`] when there is no voice frame,
/// [`RebuildError::NoCodec`] when the codec is neither given nor named, and
/// [`RebuildError::Unsupported`] for a codec with no decoder.
pub fn rebuild_audio(
    packets: &[PcapRecord],
    codec: Option<VoiceFormat>,
) -> Result<Rebuilt, RebuildError> {
    rebuild_received(packets, codec, None)
}

/// [`rebuild_audio`], leaving out every packet sent from `local` (our own
/// address as the capture shows it), so our transmitted stream is never
/// taken for the call's received audio.
///
/// # Errors
///
/// As [`rebuild_audio`].
pub fn rebuild_received(
    packets: &[PcapRecord],
    codec: Option<VoiceFormat>,
    local: Option<SocketAddr>,
) -> Result<Rebuilt, RebuildError> {
    let frames = busiest_sender(packets, local);
    if frames.is_empty() {
        return Err(RebuildError::NoVoice);
    }
    let format = codec
        .or_else(|| frames.iter().find_map(|f| f.format))
        .ok_or(RebuildError::NoCodec)?;
    let rate = match format {
        VoiceFormat::G711U
        | VoiceFormat::G711A
        | VoiceFormat::Slin
        | VoiceFormat::Slin16
        | VoiceFormat::Gsm => format
            .sample_rate()
            .ok_or(RebuildError::Unsupported(format))?,
        other => return Err(RebuildError::Unsupported(other)),
    };

    let (frames, wild) = drop_wild_fulls(frames);
    let stamps = widen(&frames);
    let mut out = Rebuilt {
        rate,
        samples: Vec::new(),
        holes: Vec::new(),
        frames: 0,
        reordered: 0,
        duplicates: 0,
        lost: 0,
        rejected: wild,
    };

    // In arrival order: count duplicates and late arrivals.
    let mut seen = HashSet::new();
    let mut latest = i64::MIN;
    let mut order = Vec::with_capacity(frames.len());
    for (i, &ts) in stamps.iter().enumerate() {
        if !seen.insert(ts) {
            out.duplicates += 1;
            continue;
        }
        if ts < latest {
            out.reordered += 1;
        }
        latest = latest.max(ts);
        order.push(i);
    }
    order.sort_by_key(|&i| stamps[i]);

    // The audio can be no longer than the capture took to arrive, plus a
    // margin, and never longer than MAX_MS.
    let first = frames.iter().map(|f| f.arrival_ns).min().unwrap_or(0);
    let last = frames.iter().map(|f| f.arrival_ns).max().unwrap_or(0);
    let cap_ms = ((last - first) / 1_000_000)
        .saturating_add(MARGIN_MS)
        .min(MAX_MS);
    let cap = usize::try_from(cap_ms * u64::from(rate) / 1000).unwrap_or(usize::MAX);

    let base = stamps[order[0]];
    // Milliseconds of stamp skipped by shortening pauses to their arrival
    // gap, and the previous frame placed, as (stamp, arrival).
    let mut skipped: i64 = 0;
    let mut prev: Option<(i64, u64)> = None;
    let mut gsm = None;
    for i in order {
        let Some(pcm) = decode(&mut gsm, format, frames[i].payload) else {
            continue;
        };
        if pcm.is_empty() {
            continue;
        }
        let mut pause = false;
        let mut cut = 0;
        if let Some((at, arrived)) = prev {
            let gap = stamps[i] - at;
            if gap > i64::from(PAUSE_MS) {
                pause = true;
                let since = frames[i].arrival_ns.saturating_sub(arrived) / 1_000_000;
                let since = i64::try_from(since).unwrap_or(i64::MAX);
                cut = gap - gap.min(since);
            }
        }
        let ms = u64::try_from(stamps[i] - base - skipped - cut).unwrap_or(0);
        let pos = usize::try_from(ms.saturating_mul(u64::from(rate)) / 1000).unwrap_or(usize::MAX);
        if pos.saturating_add(pcm.len()) > cap {
            out.rejected += 1;
            continue;
        }
        skipped += cut;
        place(&mut out, pos, &pcm, pause);
        out.frames += 1;
        prev = Some((stamps[i], frames[i].arrival_ns));
    }
    Ok(out)
}

/// Put one decoded frame at sample `pos`. Within half a frame of where the
/// audio so far ends counts as right on it: a stamp rounded to the
/// millisecond, or a sender's clock a few milliseconds out, is not a loss.
/// A gap of half a frame or more is a hole of whole frames lost, unless it
/// follows a `pause` or is longer than [`PAUSE_MS`]: that is silence.
fn place(out: &mut Rebuilt, pos: usize, pcm: &[i16], pause: bool) {
    let len = out.samples.len();
    let tol = pcm.len() / 2;
    if pos >= len + tol.max(1) {
        let gap = pos - len;
        out.samples.resize(pos, 0);
        let limit = u64::from(PAUSE_MS) * u64::from(out.rate) / 1000;
        if !pause && (gap as u64) <= limit {
            out.holes
                .push((len as u64, u32::try_from(gap).unwrap_or(u32::MAX)));
            let lost = (gap + pcm.len() / 2) / pcm.len();
            out.lost += u32::try_from(lost).unwrap_or(u32::MAX);
        }
        out.samples.extend_from_slice(pcm);
    } else if pos + tol >= len {
        out.samples.extend_from_slice(pcm);
    } else {
        // The sender's clock stepped back: the later frame wins the overlap.
        let end = pos + pcm.len();
        if end > len {
            out.samples.resize(end, 0);
        }
        out.samples[pos..end].copy_from_slice(pcm);
    }
}

fn decode(gsm: &mut Option<GsmDecoder>, format: VoiceFormat, payload: &[u8]) -> Option<Vec<i16>> {
    if format == VoiceFormat::Gsm {
        let frame: &[u8; GSM_BYTES] = payload.try_into().ok()?;
        return Some(
            gsm.get_or_insert_with(GsmDecoder::new)
                .decode(frame)
                .to_vec(),
        );
    }
    decode_voice(format, payload)
}

/// The voice frames of the sender (address and call number) that sent the
/// most of them, in arrival order, leaving out anything sent from `local`.
/// Between equally busy senders, the one heard first wins.
fn busiest_sender(packets: &[PcapRecord], local: Option<SocketAddr>) -> Vec<Captured<'_>> {
    let mut index: HashMap<(SocketAddr, u16), usize> = HashMap::new();
    // Each sender's frames and earliest arrival, in the order first seen.
    let mut senders: Vec<(Vec<Captured<'_>>, u64)> = Vec::new();
    for p in packets {
        if Some(p.src) == local || !frame::is_media(&p.payload) {
            continue;
        }
        let (call, captured) = match frame::parse(&p.payload) {
            Ok(Frame::Full(f)) if f.frame_type == FrameType::Voice => {
                let format = match f.subclass {
                    Subclass::Voice(v) => Some(v),
                    _ => None,
                };
                (
                    f.source_call,
                    Captured {
                        arrival_ns: p.ts_ns,
                        full: Some(f.timestamp),
                        raw: f.timestamp,
                        format,
                        payload: &p.payload[frame::FULL_HEADER_LEN..],
                    },
                )
            }
            Ok(Frame::Mini(m)) => (
                m.source_call,
                Captured {
                    arrival_ns: p.ts_ns,
                    full: None,
                    raw: u32::from(m.timestamp),
                    format: None,
                    payload: &p.payload[frame::MINI_HEADER_LEN..],
                },
            ),
            _ => continue,
        };
        let n = *index.entry((p.src, call)).or_insert_with(|| {
            senders.push((Vec::new(), p.ts_ns));
            senders.len() - 1
        });
        let sender = &mut senders[n];
        sender.1 = sender.1.min(p.ts_ns);
        sender.0.push(captured);
    }
    // Reversed, so that between exact equals the first seen wins.
    senders
        .into_iter()
        .rev()
        .max_by(|a, b| a.0.len().cmp(&b.0.len()).then(b.1.cmp(&a.1)))
        .map(|(frames, _)| frames)
        .unwrap_or_default()
}

/// Drop every full frame whose stamp disagrees with its arrival time, as
/// the call's other frames place it, by more than [`WILD_MS`].
///
/// Each full frame gives an offset, stamp minus arrival in milliseconds,
/// and a sane call keeps that offset steady. The offsets are grouped
/// into clusters, and the cluster the most frames agree with is the call's
/// clock: its full frames count once each, and a mini frame agrees when its
/// 16 bits match that clock at its arrival time. The full frames outside it
/// are dropped before any mini takes its epoch from them. Returns the
/// frames kept and how many were dropped.
fn drop_wild_fulls(frames: Vec<Captured<'_>>) -> (Vec<Captured<'_>>, u32) {
    let Some(first) = frames.iter().map(|f| f.arrival_ns).min() else {
        return (frames, 0);
    };
    let arrival_ms =
        |f: &Captured<'_>| i64::try_from((f.arrival_ns - first) / 1_000_000).unwrap_or(i64::MAX);
    let offset = |f: &Captured<'_>| f.full.map(|s| i64::from(s).saturating_sub(arrival_ms(f)));
    let mut offsets: Vec<(i64, usize)> = frames
        .iter()
        .enumerate()
        .filter_map(|(i, f)| offset(f).map(|o| (o, i)))
        .collect();
    if offsets.is_empty() {
        return (frames, 0);
    }
    offsets.sort_unstable();

    // Clusters: runs of sorted offsets each within WILD_MS of the last.
    let mut clusters: Vec<&[(i64, usize)]> = Vec::new();
    let mut from = 0;
    for k in 1..=offsets.len() {
        if k == offsets.len() || offsets[k].0 - offsets[k - 1].0 > WILD_MS {
            clusters.push(&offsets[from..k]);
            from = k;
        }
    }
    // Score the biggest few by fulls plus agreeing minis; ties go to the
    // cluster holding the earliest full frame.
    clusters.sort_by_key(|c| (std::cmp::Reverse(c.len()), c.iter().map(|o| o.1).min()));
    clusters.truncate(MAX_CLUSTERS);
    let agree = |clock: i64| {
        frames
            .iter()
            .filter(|f| f.full.is_none())
            .filter(|f| {
                let expected = clock.saturating_add(arrival_ms(f)) & 0xFFFF;
                let d = (i64::from(f.raw) - expected).rem_euclid(WRAP);
                d.min(WRAP - d) <= WILD_MS
            })
            .count()
    };
    let Some(clock) = clusters
        .iter()
        .map(|c| {
            let clock = c[c.len() / 2].0;
            (c.len() + agree(clock), c.iter().map(|o| o.1).min(), clock)
        })
        .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
        .map(|(_, _, clock)| clock)
    else {
        return (frames, 0);
    };

    let before = frames.len();
    let kept: Vec<Captured<'_>> = frames
        .into_iter()
        .filter(|f| offset(f).is_none_or(|o| (o - clock).abs() <= WILD_MS))
        .collect();
    let dropped = u32::try_from(before - kept.len()).unwrap_or(u32::MAX);
    (kept, dropped)
}

/// Every frame's timestamp on the full millisecond clock, in arrival order.
///
/// A mini frame carries only the low 16 bits. The high bits come from where
/// the sender's clock should be: the previous frame's stamp (the nearest
/// full frame's, for minis before any) plus the arrival time since it. The
/// candidate nearest that estimate wins, so a wrap is followed and a frame
/// reordered by a few tens of milliseconds still lands in the right epoch.
#[allow(clippy::cast_possible_wrap)]
fn widen(frames: &[Captured<'_>]) -> Vec<i64> {
    let ms_between = |from: u64, to: u64| -> i64 {
        let d = i128::from(to) - i128::from(from);
        i64::try_from(d / 1_000_000).unwrap_or(0)
    };
    let fulls: Vec<usize> = (0..frames.len())
        .filter(|&i| frames[i].full.is_some())
        .collect();
    let mut anchor: Option<(i64, u64)> = None;
    let mut out = Vec::with_capacity(frames.len());
    for (i, f) in frames.iter().enumerate() {
        let ts = if let Some(full) = f.full {
            i64::from(full)
        } else {
            let (at, arrived) = anchor.unwrap_or_else(|| {
                fulls
                    .iter()
                    .min_by_key(|&&j| j.abs_diff(i))
                    .map_or((i64::from(f.raw), f.arrival_ns), |&j| {
                        (i64::from(frames[j].raw), frames[j].arrival_ns)
                    })
            });
            let expected = at + ms_between(arrived, f.arrival_ns);
            let mut candidate = (expected & !0xFFFF) | i64::from(f.raw);
            if candidate + HALF_WRAP < expected {
                candidate += WRAP;
            } else if candidate > expected + HALF_WRAP {
                candidate -= WRAP;
            }
            candidate
        };
        anchor = Some((ts, f.arrival_ns));
        out.push(ts);
    }
    out
}
