// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! THROWAWAY diagnostic (iax-31f7 Task 10): live byte-order check for the
//! `slin` codec against a real IAX2 peer. Not for long-term commit.
//!
//! Background: `astar_codec::slin` encodes/decodes 16-bit signed linear PCM
//! little-endian (Asterisk blits native-endian buffers; RFC 5456 leaves it
//! undefined). This probe verifies that choice end-to-end:
//!
//!   1. Dial the peer offering FORMAT=slin (and, unless `nocaps`,
//!      CAPABILITY=slin|ulaw). Two shapes:
//!      - `wt` mode: DroidStar-style web-transceiver handshake vs an ASL node
//!        (CALLTOKEN pre-flight, !NEWKEY! after ANSWER), as in `probe.rs`.
//!      - `plain` mode: standard Asterisk peer (e.g. the repo's parity
//!        container with `allow=slin` and an Echo() dialplan) — NEW sent
//!        directly; a CALLTOKEN response is still honoured if one arrives.
//!   2. Read the ACCEPT's FORMAT IE — does the peer accept slin or fall back?
//!   3. If slin accepted: stream a 1 kHz / amp 8000 sine as slin voice frames
//!      for the TX window (long enough in plain mode to outlast the
//!      demo-echotest greeting so Echo() reflects the tone), collecting all RX
//!      voice throughout and briefly after.
//!   4. Decode the RX slin stream BOTH little- and big-endian and analyze per
//!      1 s window, two-part:
//!      (a) echo integrity — windows byte-exactly matching our TX tone
//!          waveform (Echo() reflects frames verbatim, so these carry NO byte
//!          order information; a byte-swapped 1 kHz tone is still 1 kHz
//!          periodic, so even Goertzel reads ~0.92 in the wrong order);
//!      (b) byte order — windows of PEER-GENERATED slin (the demo-echotest
//!          greeting, which Asterisk itself encodes in its native order):
//!          the correct decode is smooth lowpass speech, the wrong one is
//!          high-frequency garbage; vote per window on the first-difference
//!          smoothness ratio LE vs BE. Print an explicit BYTE ORDER verdict.
//!   5. If the peer declines slin, print `SLIN DECLINED` — graceful fallback
//!      is itself a valid live result. If the return leg was transcoded to
//!      ulaw, say so and tone-check it (encode-side evidence only).
//!
//! NOTE: plain Asterisk prefers ulaw when CAPABILITY offers both, so run the
//! parity container check with `nocaps` (FORMAT-only offer) to get the slin
//! leg; run once with caps too to document the graceful ulaw fallback.
//!
//! Trailing args (order-independent): `nocaps`, and the format selector
//! `slin` (default, 8 kHz, bit 1<<6) | `slin16` (16 kHz wideband, bit 1<<15,
//! iax-4348). For slin16 the oracle is Asterisk's endless server-GENERATED
//! 1004 Hz `Milliwatt()` tone (dialplan Answer→Wait→Milliwatt→Echo): decoded
//! BE it is broadband garbage, decoded LE it is a pure 1004 Hz sine
//! (smoothness ≈ (2·sin(π·1004/16000))² ≈ 0.155). Live verdict (Asterisk 20):
//! slin16 is LITTLE-endian on the wire — the OPPOSITE of slin (8 kHz, BE).
//!
//! Run: cargo run -p astar-iax --example slin_probe -- \
//!        104.232.32.242:4569 55553 allstar-public allstar astar [wt|plain] [nocaps]
//!      cargo run -p astar-iax --example slin_probe -- \
//!        127.0.0.1:4569 55553 astartest_notok supersecret astar plain nocaps [slin|slin16]

// Throwaway diagnostic: style lints are not worth chasing here.
#![allow(
    clippy::doc_markdown,
    clippy::doc_overindented_list_items,
    clippy::unnested_or_patterns,
    clippy::items_after_statements,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::nonminimal_bool,
    clippy::if_not_else
)]

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use astar_codec::g711::{ulaw_decode, ulaw_encode};
use astar_codec::slin;
use astar_iax_core::frame::{self, Frame, FullFrame, Subclass, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::session::auth::md5_response;
use astar_iax_core::subclass::{ControlSubclass, FrameType, IaxCommand, VoiceFormat};

struct Wire {
    sock: UdpSocket,
    our_call: u16,
    peer_call: u16,
    oseq: u8,
    iseq: u8,
}

impl Wire {
    fn send_full(&mut self, ts: u32, cmd: IaxCommand, ies: &Ies<'_>, bump_oseq: bool) {
        let f = Frame::Full(Box::new(FullFrame {
            source_call: self.our_call,
            dest_call: self.peer_call,
            retransmission: false,
            timestamp: ts,
            oseqno: self.oseq,
            iseqno: self.iseq,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(cmd),
            ies: ies.clone(),
            payload: &[],
        }));
        let mut out = Vec::with_capacity(128);
        frame::encode(&f, &mut out).expect("probe frame encodes");
        eprintln!(
            ">> {:?} oseq={} iseq={} dst={} ({} bytes)",
            cmd,
            self.oseq,
            self.iseq,
            self.peer_call,
            out.len()
        );
        let _ = self.sock.send(&out);
        if bump_oseq {
            self.oseq = self.oseq.wrapping_add(1);
        }
    }
}

fn describe(bytes: &[u8]) {
    match parse_lenient(bytes) {
        Ok(Frame::Full(f)) => {
            eprintln!(
                "<< full {:?}/{:?} src={} dst={} oseq={} iseq={} ts={}",
                f.frame_type,
                f.subclass,
                f.source_call,
                f.dest_call,
                f.oseqno,
                f.iseqno,
                f.timestamp,
            );
            if bytes.len() > 12 && matches!(f.frame_type, FrameType::Iax) {
                let mut rest = &bytes[12..];
                while rest.len() >= 2 {
                    let (t, l) = (rest[0], rest[1] as usize);
                    if rest.len() < 2 + l {
                        break;
                    }
                    let data = &rest[2..2 + l];
                    eprintln!(
                        "     ie type={t} len={l} ascii={:?}",
                        String::from_utf8_lossy(data)
                    );
                    rest = &rest[2 + l..];
                }
            }
        }
        Ok(Frame::Mini(m)) => {
            eprintln!(
                "<< mini src={} ts={} {} voice bytes",
                m.source_call,
                m.timestamp,
                m.payload.len()
            );
        }
        Err(e) => eprintln!("<< unparseable ({e:?}) {} bytes", bytes.len()),
    }
}

/// Raw NEW frame, DroidStar's exact IE order (VERSION first, as RFC 5456
/// requires for NEW; `Ies::encode` would emit ascending IE-type order) and
/// dest_call=0. `token`: `None` = no CALLTOKEN IE (plain first NEW);
/// `Some(t)` = echo the server's token (WT resend / calltoken retry).
fn build_new(
    our_call: u16,
    node: &str,
    name: &str,
    user: &str,
    send_caps: bool,
    slin_fmt: u32,
    ulaw_fmt: u32,
    token: Option<&[u8]>,
) -> Vec<u8> {
    let mut b: Vec<u8> = Vec::with_capacity(160);
    b.extend_from_slice(&(0x8000u16 | our_call).to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes()); // dest 0, like DroidStar
    b.extend_from_slice(&0u32.to_be_bytes()); // ts 0
    b.push(0); // oseq
    b.push(0); // iseq
    b.push(6); // AST_FRAME_IAX
    b.push(1); // IAX_COMMAND_NEW
    b.push(11);
    b.push(2);
    b.extend_from_slice(&2u16.to_be_bytes()); // VERSION=2
    b.push(1);
    b.push(1);
    b.push(b's'); // CALLED_NUMBER "s"
    b.push(2);
    b.push(node.len() as u8);
    b.extend_from_slice(node.as_bytes()); // CALLING_NUMBER
    b.push(4);
    b.push(name.len() as u8);
    b.extend_from_slice(name.as_bytes()); // CALLING_NAME
    b.push(6);
    b.push(user.len() as u8);
    b.extend_from_slice(user.as_bytes()); // USERNAME
    if send_caps {
        // CAPABILITY = slin | ulaw (advertise both).
        b.push(8);
        b.push(4);
        b.extend_from_slice(&(slin_fmt | ulaw_fmt).to_be_bytes());
    }
    b.push(9);
    b.push(4);
    b.extend_from_slice(&slin_fmt.to_be_bytes()); // FORMAT = slin
    if let Some(t) = token {
        b.push(54);
        b.push(t.len() as u8);
        b.extend_from_slice(t); // CALLTOKEN
    }
    b
}

/// `n` samples (20 ms) of a `tone_hz` sine, amplitude ~8000, at `sample_rate`.
/// slin: 160 samples @ 8 kHz; slin16: 320 samples @ 16 kHz.
fn tone_20ms(sample_idx: u64, n: u64, tone_hz: f64, sample_rate: f64) -> Vec<i16> {
    (0..n)
        .map(|i| {
            let t = (sample_idx + i) as f64;
            (8000.0 * (std::f64::consts::TAU * tone_hz * t / sample_rate).sin()).round() as i16
        })
        .collect()
}

/// Generalised Goertzel: magnitude-squared at `target_hz` for `samples`.
fn goertzel_mag_sq(samples: &[f64], target_hz: f64, sample_rate: f64) -> f64 {
    let w = std::f64::consts::TAU * target_hz / sample_rate;
    let (cosw, sinw) = (w.cos(), w.sin());
    let coeff = 2.0 * cosw;
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in samples {
        let s = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    let real = s1 - s2 * cosw;
    let imag = s2 * sinw;
    real * real + imag * imag
}

/// Fraction of total energy concentrated at `target_hz` (~1.0 for a pure tone,
/// ~0 for broadband noise or speech).
fn tone_concentration(pcm: &[i16], target_hz: f64, sample_rate: f64) -> f64 {
    if pcm.is_empty() {
        return 0.0;
    }
    let samples: Vec<f64> = pcm.iter().map(|&s| f64::from(s)).collect();
    let total: f64 = samples.iter().map(|x| x * x).sum();
    if total <= 0.0 {
        return 0.0;
    }
    let mag_sq = goertzel_mag_sq(&samples, target_hz, sample_rate);
    (2.0 / samples.len() as f64) * mag_sq / total
}

/// Max `target_hz` concentration over sliding `win`-sample windows (hop `win/2`).
/// Windowing isolates the tone from any speech without burst bookkeeping.
/// Returns (max_concentration, window_index).
fn max_windowed_concentration(
    pcm: &[i16],
    target_hz: f64,
    sample_rate: f64,
    win: usize,
) -> (f64, usize) {
    let hop = win / 2;
    if pcm.len() < win {
        return (tone_concentration(pcm, target_hz, sample_rate), 0);
    }
    let mut best = (0.0f64, 0usize);
    let mut idx = 0usize;
    let mut off = 0usize;
    while off + win <= pcm.len() {
        let c = tone_concentration(&pcm[off..off + win], target_hz, sample_rate);
        if c > best.0 {
            best = (c, idx);
        }
        off += hop;
        idx += 1;
    }
    best
}

/// Fraction of samples that are (within ±2) one of the exact values our
/// 1 kHz / amp-8000 tone generator emits: 8 samples per cycle at 8 kHz →
/// {0, ±5657, ±8000}. ~1.0 means this window IS our tone, byte-exact.
fn tone_waveform_match(pcm: &[i16]) -> f64 {
    if pcm.is_empty() {
        return 0.0;
    }
    let hits = pcm
        .iter()
        .filter(|&&s| {
            let a = i32::from(s).abs();
            a <= 2 || (a - 5657).abs() <= 2 || (a - 8000).abs() <= 2
        })
        .count();
    hits as f64 / pcm.len() as f64
}

/// First-difference energy over signal energy. Natural 8 kHz speech/audio is
/// lowpass, so adjacent samples are correlated and this is small (< ~1).
/// Byte-swapped PCM destroys that correlation (the fast-varying low byte lands
/// in the high byte), pushing this toward ~2 (white-noise-like).
fn smoothness_ratio(pcm: &[i16]) -> f64 {
    let energy: f64 = pcm.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    if energy <= 0.0 {
        return f64::INFINITY;
    }
    let diff: f64 = pcm
        .windows(2)
        .map(|p| {
            let d = f64::from(p[1]) - f64::from(p[0]);
            d * d
        })
        .sum();
    diff / energy
}

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let host = args.next().unwrap_or_else(|| "104.232.32.242:4569".into());
    let node = args.next().unwrap_or_else(|| "55553".into());
    let user = args.next().unwrap_or_else(|| "allstar-public".into());
    let pass = args.next().unwrap_or_else(|| "allstar".into());
    let name = args.next().unwrap_or_else(|| "astar".into());
    let mode = args.next().unwrap_or_else(|| "wt".into());
    let plain = mode == "plain";
    // Remaining args (order-independent): `nocaps` and the format selector
    // `slin` (default) | `slin16`.
    let rest: Vec<String> = args.collect();
    let send_caps = !rest.iter().any(|a| a == "nocaps");
    let want_slin16 = rest.iter().any(|a| a == "slin16");
    // Format-dependent knobs: negotiated codec, tone TX rate, RX analysis rate,
    // and the Goertzel target. slin16's oracle is the server's endless 1004 Hz
    // Milliwatt tone; slin's is our own 1 kHz tone echoed / the greeting.
    let voice_fmt = if want_slin16 {
        VoiceFormat::Slin16
    } else {
        VoiceFormat::Slin
    };
    let sample_rate: f64 = if want_slin16 { 16000.0 } else { 8000.0 };
    let samples_per_frame: u64 = if want_slin16 { 320 } else { 160 };
    let tx_tone_hz: f64 = 1000.0;
    let analysis_hz: f64 = if want_slin16 { 1004.0 } else { 1000.0 };
    // 1 s analysis window in samples.
    let win: usize = if want_slin16 { 16000 } else { 8000 };
    let slin_fmt = voice_fmt.as_u32(); // slin 1<<6=64, slin16 1<<15=32768
    let ulaw_fmt = VoiceFormat::G711U.as_u32(); // 1<<2 = 4
    eprintln!(
        "slin_probe({mode}): {host} node={node} user={user} name={name} caps={send_caps} \
         offer FORMAT={voice_fmt:?}({slin_fmt}) rate={sample_rate} analysis_hz={analysis_hz}"
    );

    let sock = UdpSocket::bind("0.0.0.0:0").unwrap();
    sock.connect(&host).unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut w = Wire {
        sock,
        our_call: 1,
        peer_call: 0,
        oseq: 0,
        iseq: 0,
    };

    if plain {
        // Standard Asterisk peer: send the NEW directly (requirecalltoken=no).
        // If the server answers with CALLTOKEN anyway, the handler below resends.
        let b = build_new(
            w.our_call, &node, &name, &user, send_caps, slin_fmt, ulaw_fmt, None,
        );
        eprintln!(
            ">> New (raw, plain shape, FORMAT={voice_fmt:?}, caps={send_caps}) ({} bytes)",
            b.len()
        );
        let _ = w.sock.send(&b);
        w.oseq = 1;
    } else {
        // WT shape: token request first — NEW with a single empty CALLTOKEN IE.
        let ies = Ies {
            calltoken: Some(&[]),
            ..Ies::empty()
        };
        w.send_full(0, IaxCommand::New, &ies, false);
    }

    let mut buf = [0u8; 4096];
    let mut accepted = false;
    let mut accepted_fmt: Option<u32> = None;
    let start = Instant::now();

    // Plain mode (parity container): dialplan answers, waits 1 s, plays the
    // ~10 s demo-echotest greeting, then runs Echo() which reflects live — so
    // TX must outlast the greeting, and RX during TX IS the echo.
    let tx_secs: u64 = if plain { 18 } else { 4 };
    let rx_secs: u64 = if plain { 3 } else { 10 };
    let mut answered_at: Option<Instant> = None;
    let mut voice_started = false;
    let mut next_tx = Instant::now();
    let mut tx_ts: u32 = 0;
    let mut sample_idx: u64 = 0;
    let mut frames_sent: u32 = 0;

    // RX voice payloads tagged with the format in effect when they arrived.
    // Full Voice frames carry the format in the subclass; minis inherit the
    // last full frame's format (the IAX2 rule), seeded from the ACCEPT format.
    let mut rx_voice: Vec<(VoiceFormat, Vec<u8>)> = Vec::new();
    let mut rx_format = VoiceFormat::G711U;
    // The codec we transmit, decided at ACCEPT.
    let mut tx_format = VoiceFormat::G711U;
    let mut done_at: Option<Instant> = None; // when TX finished; RX tail starts

    let deadline = start + Duration::from_secs(60);
    while Instant::now() < deadline {
        // Outbound media pump (runs even on recv timeouts).
        if let Some(at) = answered_at {
            let sending = at.elapsed() < Duration::from_secs(tx_secs);
            if sending {
                let now = Instant::now();
                while now >= next_tx {
                    let pcm = tone_20ms(sample_idx, samples_per_frame, tx_tone_hz, sample_rate);
                    // TX through the shipping library codec paths: slin = BE
                    // encode, slin16 = LE encode (the iax-4348 fix) — so the
                    // transmit leg exercises exactly what the library ships.
                    let payload: Vec<u8> = match tx_format {
                        VoiceFormat::Slin => slin::encode(&pcm),
                        VoiceFormat::Slin16 => slin::encode_le(&pcm),
                        _ => pcm.iter().map(|&s| ulaw_encode(s)).collect(),
                    };
                    if !voice_started {
                        // First media frame MUST be a full Voice frame (reliable),
                        // establishing the codec + high-16 timestamp context.
                        let vf = Frame::Full(Box::new(FullFrame {
                            source_call: w.our_call,
                            dest_call: w.peer_call,
                            retransmission: false,
                            timestamp: tx_ts,
                            oseqno: w.oseq,
                            iseqno: w.iseq,
                            frame_type: FrameType::Voice,
                            subclass: Subclass::Voice(tx_format),
                            ies: Ies::empty(),
                            payload: &payload,
                        }));
                        let mut out = Vec::with_capacity(payload.len() + 16);
                        frame::encode(&vf, &mut out).expect("voice full frame encodes");
                        eprintln!(
                            ">> FULL Voice({tx_format:?}) oseq={} ts={} ({} bytes)",
                            w.oseq,
                            tx_ts,
                            out.len()
                        );
                        let _ = w.sock.send(&out);
                        w.oseq = w.oseq.wrapping_add(1);
                        voice_started = true;
                    } else {
                        let mut mini = Vec::with_capacity(payload.len() + 4);
                        mini.extend_from_slice(&(w.our_call & 0x7FFF).to_be_bytes());
                        mini.extend_from_slice(&((tx_ts & 0xFFFF) as u16).to_be_bytes());
                        mini.extend_from_slice(&payload);
                        let _ = w.sock.send(&mini);
                    }
                    frames_sent += 1;
                    sample_idx += samples_per_frame;
                    tx_ts = tx_ts.wrapping_add(20);
                    next_tx += Duration::from_millis(20);
                }
            } else if done_at.is_none() {
                done_at = Some(Instant::now());
                eprintln!("   *** TX complete: {frames_sent} frames sent; collecting tail ***");
            }
        }

        // End condition: we finished TX and the RX tail elapsed.
        if let Some(d) = done_at
            && d.elapsed() > Duration::from_secs(rx_secs)
        {
            break;
        }

        let Ok(n) = w.sock.recv(&mut buf) else {
            continue;
        };
        let bytes = &buf[..n];
        // Suppress per-frame dumps during the media phase; keep them for setup.
        let is_mini_rx = matches!(parse_lenient(bytes), Ok(Frame::Mini(_)));
        if answered_at.is_none() || !is_mini_rx {
            describe(bytes);
        }
        let Ok(frame) = parse_lenient(bytes) else {
            continue;
        };
        match frame {
            Frame::Mini(m) => {
                rx_voice.push((rx_format, m.payload.to_vec()));
            }
            Frame::Full(f) => {
                if w.peer_call == 0 && f.source_call != 0 {
                    w.peer_call = f.source_call;
                }
                let is_ack = matches!(f.subclass, Subclass::Iax(IaxCommand::Ack));
                if !is_ack {
                    w.iseq = f.oseqno.wrapping_add(1);
                }
                // A full Voice frame from the peer is echo material AND sets
                // the format for subsequent minis. Dispatch on its actual
                // subclass — this is how we detect a transcoded return leg.
                if let Subclass::Voice(vfmt) = f.subclass {
                    if vfmt != rx_format {
                        eprintln!("   RX format now {vfmt:?} (was {rx_format:?})");
                    }
                    rx_format = vfmt;
                    if !f.payload.is_empty() {
                        rx_voice.push((rx_format, f.payload.to_vec()));
                    }
                }
                match f.subclass {
                    Subclass::Iax(IaxCommand::CallToken) => {
                        let token = f.ies.calltoken.unwrap_or(&[]).to_vec();
                        // Resend the NEW with the token echoed, seq reset.
                        w.oseq = 0;
                        w.iseq = 0;
                        let b = build_new(
                            w.our_call,
                            &node,
                            &name,
                            &user,
                            send_caps,
                            slin_fmt,
                            ulaw_fmt,
                            Some(&token),
                        );
                        eprintln!(
                            ">> New (raw, token echoed, FORMAT={voice_fmt:?}, caps={send_caps}) \
                             oseq=0 iseq=0 dst=0 ({} bytes)",
                            b.len()
                        );
                        let _ = w.sock.send(&b);
                        w.oseq = 1;
                    }
                    Subclass::Iax(IaxCommand::AuthReq) => {
                        let challenge = f.ies.challenge.unwrap_or("");
                        let digest = md5_response(challenge, &pass);
                        eprintln!("   answering challenge {challenge:?}");
                        let ies = Ies {
                            md5_result: Some(&digest),
                            ..Ies::empty()
                        };
                        let ts = start.elapsed().as_millis() as u32;
                        w.send_full(ts, IaxCommand::AuthRep, &ies, true);
                    }
                    Subclass::Iax(IaxCommand::Accept) => {
                        accepted = true;
                        accepted_fmt = f.ies.format;
                        tx_format = accepted_fmt
                            .and_then(VoiceFormat::from_u32)
                            .unwrap_or(VoiceFormat::G711U);
                        rx_format = tx_format;
                        eprintln!(
                            "   *** ACCEPTED *** FORMAT IE = {accepted_fmt:?} -> {tx_format:?}"
                        );
                        let ies = Ies::empty();
                        w.send_full(f.timestamp, IaxCommand::Ack, &ies, false);
                    }
                    Subclass::Control(ControlSubclass::Answer) => {
                        let ies = Ies::empty();
                        w.send_full(f.timestamp, IaxCommand::Ack, &ies, false);
                        if answered_at.is_none() {
                            if !plain {
                                // WT key signal: announce ourselves with a !NEWKEY!
                                // TEXT frame (as probe.rs does) before voice.
                                let text = b"!NEWKEY!";
                                let mut t: Vec<u8> = Vec::with_capacity(12 + text.len());
                                t.extend_from_slice(&(0x8000u16 | w.our_call).to_be_bytes());
                                t.extend_from_slice(&w.peer_call.to_be_bytes());
                                t.extend_from_slice(
                                    &(start.elapsed().as_millis() as u32).to_be_bytes(),
                                );
                                t.push(w.oseq);
                                t.push(w.iseq);
                                t.push(7); // AST_FRAME_TEXT
                                t.push(0);
                                t.extend_from_slice(text);
                                eprintln!(
                                    ">> TEXT {:?} oseq={}",
                                    String::from_utf8_lossy(text),
                                    w.oseq
                                );
                                let _ = w.sock.send(&t);
                                w.oseq = w.oseq.wrapping_add(1);
                            }
                            answered_at = Some(Instant::now());
                            next_tx = Instant::now();
                            eprintln!(
                                "   *** ANSWERED *** (full Voice({tx_format:?}) + \
                                 {tx_secs}s tone, then {rx_secs}s tail collect)"
                            );
                        }
                    }
                    Subclass::Iax(IaxCommand::Reject) => {
                        eprintln!("   *** REJECTED ***");
                        break;
                    }
                    Subclass::Iax(IaxCommand::Hangup) => {
                        eprintln!("   *** PEER HANGUP ***");
                        // Keep whatever echo we already collected.
                        break;
                    }
                    Subclass::Iax(IaxCommand::Ping) | Subclass::Iax(IaxCommand::Poke) => {
                        let ies = Ies::empty();
                        w.send_full(f.timestamp, IaxCommand::Pong, &ies, true);
                    }
                    Subclass::Iax(IaxCommand::LagRq) => {
                        let ies = Ies::empty();
                        w.send_full(f.timestamp, IaxCommand::LagRp, &ies, true);
                    }
                    _ if !is_ack => {
                        let ies = Ies::empty();
                        w.send_full(f.timestamp, IaxCommand::Ack, &ies, false);
                    }
                    _ => {}
                }
            }
        }
    }

    // Best-effort hangup (also ends Echo() on the plain target).
    let ies = Ies::empty();
    w.send_full(0, IaxCommand::Hangup, &ies, true);

    // ---- Verdict ---------------------------------------------------------
    // RX bytes in the negotiated wideband/narrowband slin format.
    let slin_bytes: Vec<u8> = rx_voice
        .iter()
        .filter(|(f, _)| *f == voice_fmt)
        .flat_map(|(_, p)| p.iter().copied())
        .collect();
    let ulaw_pcm: Vec<i16> = rx_voice
        .iter()
        .filter(|(f, _)| *f == VoiceFormat::G711U)
        .flat_map(|(_, p)| p.iter().map(|&b| ulaw_decode(b)))
        .collect();

    eprintln!("\n======================= VERDICT =======================");
    eprintln!(
        "accepted={accepted} accepted_fmt={accepted_fmt:?} tx_format={tx_format:?} \
         frames_sent={frames_sent} rx_voice_frames={} rx_slin_bytes={} rx_ulaw_samples={}",
        rx_voice.len(),
        slin_bytes.len(),
        ulaw_pcm.len()
    );

    if !accepted {
        println!("VERDICT UNAVAILABLE: call not accepted (no ACCEPT from peer)");
        return;
    }

    // ---- slin16 wideband path (iax-4348) --------------------------------
    // Oracle: the dialplan runs Milliwatt() — an endless server-GENERATED
    // 1004 Hz tone — before the (unreachable) Echo(). So every RX slin16 frame
    // is Asterisk's own native-order 16 kHz PCM: the correct decode is a smooth
    // 1004 Hz sine, the wrong decode is byte-swapped broadband garbage.
    if voice_fmt == VoiceFormat::Slin16 {
        if tx_format != VoiceFormat::Slin16 {
            println!("SLIN16 DECLINED: accepted = {tx_format:?} ({accepted_fmt:?})");
            println!(
                "VERDICT UNAVAILABLE: slin16 not negotiated \
                 (frames_sent={frames_sent} rx_frames={} rx_slin16_bytes={})",
                rx_voice.len(),
                slin_bytes.len()
            );
            return;
        }
        println!("SLIN16 ACCEPTED (0x8000)");

        let even = slin_bytes.len() & !1;
        // Explicit LE and BE readers (the library codec is BE per iax-31f7);
        // this probe stays an independent instrument for either order.
        let pcm_le: Vec<i16> = slin_bytes[..even]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        let pcm_be: Vec<i16> = slin_bytes[..even]
            .chunks_exact(2)
            .map(|c| i16::from_be_bytes([c[0], c[1]]))
            .collect();

        let hop = win / 2;
        let e_floor = (win as f64) * 100.0 * 100.0; // ~100 RMS gate
        let mut votes_le = 0u32;
        let mut votes_be = 0u32;
        let mut r_les: Vec<f64> = Vec::new();
        let mut r_bes: Vec<f64> = Vec::new();
        let mut off = 0usize;
        while off + win <= pcm_le.len() {
            let (wl, wb) = (&pcm_le[off..off + win], &pcm_be[off..off + win]);
            off += hop;
            let e_le: f64 = wl.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
            let e_be: f64 = wb.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
            if e_le.max(e_be) < e_floor {
                continue; // near-silent: no byte-order information
            }
            let (r_le, r_be) = (smoothness_ratio(wl), smoothness_ratio(wb));
            r_les.push(r_le);
            r_bes.push(r_be);
            if r_be < r_le {
                votes_be += 1;
            } else {
                votes_le += 1;
            }
        }
        let med_r_le = median(&mut r_les);
        let med_r_be = median(&mut r_bes);
        let (conc_le, _) = max_windowed_concentration(&pcm_le, analysis_hz, sample_rate, win);
        let (conc_be, _) = max_windowed_concentration(&pcm_be, analysis_hz, sample_rate, win);
        let total = votes_le + votes_be;
        // Expected under the CORRECT reading of a 1004 Hz tone @ 16 kHz —
        // which is LITTLE-endian for slin16 (iax-4348 live verdict; the
        // library ships `slin::encode_le/decode_le` for it):
        //   smoothness ratio ≈ (2·sin(π·1004/16000))² ≈ 0.155, conc ≈ 1.
        let expected = (2.0 * (std::f64::consts::PI * analysis_hz / sample_rate).sin()).powi(2);
        eprintln!(
            "slin16 RX {} bytes; peer-generated windows={total} \
             smoothness votes LE={votes_le} BE={votes_be} \
             (median r_le={med_r_le:.3} r_be={med_r_be:.3}; expected correct≈{expected:.3}); \
             max {analysis_hz}Hz conc LE={conc_le:.4} BE={conc_be:.4}",
            slin_bytes.len()
        );
        let supporting = format!(
            "accepted_fmt={accepted_fmt:?} frames_sent={frames_sent} rx_frames={} \
             rx_slin16_bytes={} windows={total} smoothness_votes LE={votes_le} BE={votes_be} \
             median r_le={med_r_le:.3} r_be={med_r_be:.3} expected={expected:.3} \
             conc_le={conc_le:.4} conc_be={conc_be:.4}",
            rx_voice.len(),
            slin_bytes.len()
        );
        if total < 3 {
            println!(
                "BYTE ORDER (slin16): VERDICT UNAVAILABLE: insufficient peer-generated \
                 slin16 audio ({supporting})"
            );
        } else if votes_le * 4 >= total * 3 && med_r_le < 0.5 && conc_le > conc_be {
            println!(
                "BYTE ORDER (slin16): little-endian CONFIRMED (expected for slin16) \
                 ({supporting})"
            );
        } else if votes_be * 4 >= total * 3 && med_r_be < 0.5 && conc_be > conc_le {
            println!("BYTE ORDER (slin16): big-endian — FLIP?! ({supporting})");
        } else {
            println!("BYTE ORDER (slin16): VERDICT UNAVAILABLE: indecisive ({supporting})");
        }
        return;
    }

    if tx_format != VoiceFormat::Slin {
        println!("SLIN DECLINED: accepted format = {tx_format:?} ({accepted_fmt:?})");
        if rx_voice.is_empty() {
            println!("VERDICT UNAVAILABLE: slin not negotiated and no echo received");
        } else {
            println!(
                "VERDICT UNAVAILABLE: byte-order check needs slin; echo sanity OK \
                 ({} voice frames received in fallback {tx_format:?})",
                rx_voice.len()
            );
        }
        return;
    }

    // slin negotiated. Two independent analyses over the slin RX stream:
    //
    // (a) Echo round-trip: Asterisk's Echo() reflects frames VERBATIM (same
    //     format both legs, no transcode), so the echoed tone is byte-order
    //     agnostic — it only proves media-path integrity. We detect it by
    //     exact waveform match against the known TX sample set. (Live check
    //     of this: a byte-swapped 1 kHz tone is still periodic at 1 kHz, so
    //     Goertzel alone reads 0.92 concentration in the WRONG order too.)
    //
    // (b) Byte order: the demo-echotest greeting is generated BY Asterisk
    //     (sound file decoded/transcoded into the channel's slin, written in
    //     its native byte order). Decoded with the correct order it is smooth
    //     lowpass speech; wrong order is high-frequency garbage. Vote per
    //     window on the first-difference smoothness ratio, LE vs BE.
    if slin_bytes.len() >= 16000 {
        let even = slin_bytes.len() & !1;
        // Explicit LE reader (NOT the library codec, which is big-endian
        // since the iax-31f7 live verdict) so this probe stays a valid
        // independent instrument for either order.
        let pcm_le: Vec<i16> = slin_bytes[..even]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        let pcm_be: Vec<i16> = slin_bytes[..even]
            .chunks_exact(2)
            .map(|c| i16::from_be_bytes([c[0], c[1]]))
            .collect();

        const WIN: usize = 8000; // 1 s
        const HOP: usize = 4000;
        let mut echo_windows = 0u32;
        let mut best_match_le = 0.0f64;
        let mut best_match_be = 0.0f64;
        let mut votes_le = 0u32;
        let mut votes_be = 0u32;
        let mut r_les: Vec<f64> = Vec::new();
        let mut r_bes: Vec<f64> = Vec::new();
        let mut off = 0usize;
        while off + WIN <= pcm_le.len() {
            let (wl, wb) = (&pcm_le[off..off + WIN], &pcm_be[off..off + WIN]);
            off += HOP;
            let (m_le, m_be) = (tone_waveform_match(wl), tone_waveform_match(wb));
            best_match_le = best_match_le.max(m_le);
            best_match_be = best_match_be.max(m_be);
            if m_le > 0.8 || m_be > 0.8 {
                echo_windows += 1; // our own tone reflected verbatim: no order info
                continue;
            }
            // Skip near-silent windows (byte energy is order-independent-ish;
            // require some signal in either reading).
            let e_le: f64 = wl.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
            let e_be: f64 = wb.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
            if e_le.max(e_be) < (WIN as f64) * 100.0 * 100.0 {
                continue;
            }
            let (r_le, r_be) = (smoothness_ratio(wl), smoothness_ratio(wb));
            r_les.push(r_le);
            r_bes.push(r_be);
            if r_le < r_be {
                votes_le += 1;
            } else {
                votes_be += 1;
            }
        }
        let med_r_le = median(&mut r_les);
        let med_r_be = median(&mut r_bes);
        let (conc_le, _) = max_windowed_concentration(&pcm_le, 1000.0, 8000.0, 8000);
        let (conc_be, _) = max_windowed_concentration(&pcm_be, 1000.0, 8000.0, 8000);
        eprintln!(
            "slin RX {} bytes; echo windows={echo_windows} \
             (tone waveform match LE={best_match_le:.3} BE={best_match_be:.3}); \
             peer-generated windows={} smoothness votes LE={votes_le} BE={votes_be} \
             (median r_le={med_r_le:.3} r_be={med_r_be:.3}); \
             max 1kHz conc LE={conc_le:.4} BE={conc_be:.4}",
            slin_bytes.len(),
            votes_le + votes_be
        );
        if echo_windows > 0 && best_match_le > 0.9 {
            eprintln!("echo round-trip intact: reflected tone is byte-exact under the LE reading");
        }
        let supporting = format!(
            "accepted_fmt={accepted_fmt:?} frames_sent={frames_sent} rx_frames={} \
             rx_slin_bytes={} greeting_windows={} smoothness_votes LE={votes_le} BE={votes_be} \
             median r_le={med_r_le:.3} r_be={med_r_be:.3} echo_windows={echo_windows} \
             tone_match_le={best_match_le:.3}",
            rx_voice.len(),
            slin_bytes.len(),
            votes_le + votes_be
        );
        let total = votes_le + votes_be;
        if total >= 3 && votes_le * 4 >= total * 3 {
            println!("BYTE ORDER: little-endian CONFIRMED ({supporting})");
        } else if total >= 3 && votes_be * 4 >= total * 3 {
            println!("BYTE ORDER: big-endian — FLIP REQUIRED ({supporting})");
        } else {
            println!("VERDICT UNAVAILABLE: no decisive peer-generated slin audio ({supporting})");
        }
    } else if !ulaw_pcm.is_empty() {
        // Return leg transcoded to ulaw: no decode-side byte-order verdict, but
        // if our tone comes back intact the peer read our slin TX correctly —
        // encode-side evidence for the current (little-endian) order.
        let (conc, win_idx) = max_windowed_concentration(&ulaw_pcm, 1000.0, 8000.0, 8000);
        eprintln!(
            "return leg transcoded to ulaw ({} samples); max windowed \
             1kHz-concentration={conc:.4} (win {win_idx})",
            ulaw_pcm.len()
        );
        if conc >= 0.5 {
            println!(
                "VERDICT PARTIAL: return leg transcoded to ulaw; tone echoed intact \
                 (conc={conc:.4}) — peer decoded our slin TX, encode-side little-endian OK; \
                 decode-side byte order unverified"
            );
        } else {
            println!(
                "VERDICT UNAVAILABLE: return leg transcoded to ulaw and no clear tone \
                 (conc={conc:.4}) — check TX byte order / echo path"
            );
        }
    } else {
        println!(
            "VERDICT UNAVAILABLE: slin accepted but insufficient echo ({} slin bytes)",
            slin_bytes.len()
        );
    }
}
