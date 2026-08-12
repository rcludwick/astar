// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! THROWAWAY diagnostic (iax-fd34 dry run): manually drive a DroidStar-style
//! web-transceiver IAX2 handshake against an ASL node and dump every frame.
//! Not for commit.
//!
//! Recipe (from DroidStar iax.cpp, the reference client that works vs ASL3):
//!   1. NEW carrying only an empty CALLTOKEN IE (token request)
//!   2. on CALLTOKEN: NEW again (seq reset) with VERSION=2, CALLED_NUMBER="s",
//!      CALLING_NUMBER=<node>, CALLING_NAME=<name>, USERNAME=allstar-public,
//!      FORMAT=ulaw, CALLTOKEN=<echo>   (note: no CAPABILITY IE)
//!   3. on AUTHREQ: AUTHREP with md5(challenge + "allstar")
//!   4. expect ACCEPT/ANSWER, then voice from the node.
//!
//! Run: cargo run -p astar-iax --example probe -- [host:port] [node] [user] [pass] [callingname]

// Throwaway diagnostic: style lints are not worth chasing here.
#![allow(
    clippy::doc_markdown,
    clippy::unnested_or_patterns,
    clippy::items_after_statements,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::nonminimal_bool,
    clippy::if_not_else
)]

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use astar_codec::g711::ulaw_encode;
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
            if bytes.len() > 12 {
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

#[allow(clippy::too_many_lines)]
fn main() {
    let mut args = std::env::args().skip(1);
    let host = args.next().unwrap_or_else(|| "104.232.32.242:4569".into());
    let node = args.next().unwrap_or_else(|| "55553".into());
    let user = args.next().unwrap_or_else(|| "allstar-public".into());
    let pass = args.next().unwrap_or_else(|| "allstar".into());
    let name = args.next().unwrap_or_else(|| "astar".into());
    eprintln!("probe(wt): {host} node={node} user={user} name={name}");

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

    // 1. Token request: NEW with a single empty CALLTOKEN IE.
    let ies = Ies {
        calltoken: Some(&[]),
        ..Ies::empty()
    };
    w.send_full(0, IaxCommand::New, &ies, false);

    let mut buf = [0u8; 4096];
    let mut accepted = false;
    let mut voice_frames = 0u32;
    let start = Instant::now();
    let deadline = start + Duration::from_secs(25);

    // EXPERIMENT (iax-a116 follow-up): after ANSWER, behave like a CORRECT
    // persistent peer — send a FULL Voice frame first (establishes codec +
    // high-16 ts context), then a continuous 1 kHz tone as 20 ms ulaw mini
    // frames for TX_SECS, then go silent (simulated unkey). Tests whether a
    // valid full-then-mini stream keeps the parrot alive past the ~1 s clear
    // and gets an echo — i.e. is 55553 a normal node or a fixed-timer parrot.
    const TX_SECS: u64 = 5;
    let mut answered_at: Option<Instant> = None;
    let mut voice_started = false;
    let mut next_tx = Instant::now();
    let mut tx_ts: u32 = 0;
    let mut sample_idx: u64 = 0;

    while Instant::now() < deadline {
        // Outbound media pump (runs even on recv timeouts).
        if let Some(at) = answered_at
            && at.elapsed() < Duration::from_secs(TX_SECS)
        {
            let now = Instant::now();
            while now >= next_tx {
                // Build 20 ms of 1 kHz tone as ulaw.
                let mut payload = Vec::with_capacity(160);
                for i in 0..160u64 {
                    let t = (sample_idx + i) as f32;
                    let v = (std::f32::consts::TAU * 1000.0 * t / 8000.0).sin() * 0.5;
                    payload.push(ulaw_encode((v * 32767.0) as i16));
                }
                if !voice_started {
                    // First media frame MUST be a full Voice frame (reliable).
                    let vf = Frame::Full(Box::new(FullFrame {
                        source_call: w.our_call,
                        dest_call: w.peer_call,
                        retransmission: false,
                        timestamp: tx_ts,
                        oseqno: w.oseq,
                        iseqno: w.iseq,
                        frame_type: FrameType::Voice,
                        subclass: Subclass::Voice(VoiceFormat::G711U),
                        ies: Ies::empty(),
                        payload: &payload,
                    }));
                    let mut out = Vec::with_capacity(172);
                    frame::encode(&vf, &mut out).expect("voice full frame encodes");
                    eprintln!(
                        ">> FULL Voice oseq={} ts={} ({} bytes)",
                        w.oseq,
                        tx_ts,
                        out.len()
                    );
                    let _ = w.sock.send(&out);
                    w.oseq = w.oseq.wrapping_add(1);
                    voice_started = true;
                } else {
                    let mut mini = Vec::with_capacity(164);
                    mini.extend_from_slice(&(w.our_call & 0x7FFF).to_be_bytes());
                    mini.extend_from_slice(&((tx_ts & 0xFFFF) as u16).to_be_bytes());
                    mini.extend_from_slice(&payload);
                    let _ = w.sock.send(&mini);
                }
                sample_idx += 160;
                tx_ts = tx_ts.wrapping_add(20);
                next_tx += Duration::from_millis(20);
            }
        }
        let Ok(n) = w.sock.recv(&mut buf) else {
            continue;
        };
        let bytes = &buf[..n];
        describe(bytes);
        let Ok(frame) = parse_lenient(bytes) else {
            continue;
        };
        match frame {
            Frame::Mini(_) => {
                voice_frames += 1;
            }
            Frame::Full(f) => {
                // Track the peer's call number + next-expected iseq.
                if w.peer_call == 0 && f.source_call != 0 {
                    w.peer_call = f.source_call;
                }
                let is_ack = matches!(f.subclass, Subclass::Iax(IaxCommand::Ack));
                if !is_ack {
                    w.iseq = f.oseqno.wrapping_add(1);
                }
                match f.subclass {
                    Subclass::Iax(IaxCommand::CallToken) => {
                        let token = f.ies.calltoken.unwrap_or(&[]).to_vec();
                        // 2. Real NEW, sequence reset, WT shape — RAW bytes
                        // in DroidStar's exact IE order (VERSION first, as
                        // RFC 5456 requires for NEW; Ies::encode would emit
                        // ascending IE-type order instead) and dest_call=0.
                        w.oseq = 0;
                        w.iseq = 0;
                        let mut b: Vec<u8> = Vec::with_capacity(128);
                        b.extend_from_slice(&(0x8000u16 | w.our_call).to_be_bytes());
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
                        #[allow(clippy::cast_possible_truncation)]
                        {
                            b.push(2);
                            b.push(node.len() as u8);
                            b.extend_from_slice(node.as_bytes()); // CALLING_NUMBER
                            b.push(4);
                            b.push(name.len() as u8);
                            b.extend_from_slice(name.as_bytes()); // CALLING_NAME
                            b.push(6);
                            b.push(user.len() as u8);
                            b.extend_from_slice(user.as_bytes()); // USERNAME
                            b.push(9);
                            b.push(4);
                            b.extend_from_slice(&4u32.to_be_bytes()); // FORMAT ulaw
                            b.push(54);
                            b.push(token.len() as u8);
                            b.extend_from_slice(&token); // CALLTOKEN
                        }
                        eprintln!(
                            ">> New (raw WT shape) oseq=0 iseq=0 dst=0 ({} bytes)",
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
                        #[allow(clippy::cast_possible_truncation)]
                        let ts = start.elapsed().as_millis() as u32;
                        w.send_full(ts, IaxCommand::AuthRep, &ies, true);
                    }
                    Subclass::Iax(IaxCommand::Accept) => {
                        accepted = true;
                        eprintln!("   *** ACCEPTED ***");
                        // ACK it (oseq not bumped for ACKs).
                        let ies = Ies::empty();
                        w.send_full(f.timestamp, IaxCommand::Ack, &ies, false);
                    }
                    Subclass::Control(ControlSubclass::Answer) => {
                        // ACK the Answer, then start the media pump from here
                        // (the proper moment to send voice, per the working flow).
                        let ies = Ies::empty();
                        w.send_full(f.timestamp, IaxCommand::Ack, &ies, false);
                        if answered_at.is_none() {
                            // EXPERIMENT: app_rpt expects a TEXT-frame newkey
                            // handshake. The node never sent us !NEWKEY1!, so try
                            // announcing ourselves proactively with !NEWKEY! and
                            // see if that stops the ~1s cause-16 hangup.
                            let text = b"!NEWKEY!";
                            let mut t: Vec<u8> = Vec::with_capacity(12 + text.len());
                            t.extend_from_slice(&(0x8000u16 | w.our_call).to_be_bytes());
                            t.extend_from_slice(&w.peer_call.to_be_bytes());
                            #[allow(clippy::cast_possible_truncation)]
                            t.extend_from_slice(
                                &(start.elapsed().as_millis() as u32).to_be_bytes(),
                            );
                            t.push(w.oseq); // oseq (reliable)
                            t.push(w.iseq); // iseq
                            t.push(7); // AST_FRAME_TEXT
                            t.push(0); // subclass unused
                            t.extend_from_slice(text);
                            eprintln!(
                                ">> TEXT {:?} oseq={}",
                                String::from_utf8_lossy(text),
                                w.oseq
                            );
                            let _ = w.sock.send(&t);
                            w.oseq = w.oseq.wrapping_add(1);

                            answered_at = Some(Instant::now());
                            next_tx = Instant::now();
                            eprintln!(
                                "   *** ANSWERED *** (!NEWKEY! sent; full Voice + {TX_SECS}s tone)"
                            );
                        }
                    }
                    Subclass::Iax(IaxCommand::Reject) => {
                        eprintln!("   *** REJECTED ***");
                        break;
                    }
                    Subclass::Iax(IaxCommand::Hangup) => {
                        eprintln!("   *** PEER HANGUP ***");
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
                        // ACK any other reliable full frame so the peer
                        // stops retransmitting (control ANSWER, TEXT, etc.).
                        let ies = Ies::empty();
                        w.send_full(f.timestamp, IaxCommand::Ack, &ies, false);
                    }
                    _ => {}
                }
            }
        }
        // Once accepted, listen ~10s for the parrot announcement then leave.
        if accepted && start.elapsed() > Duration::from_secs(15) {
            break;
        }
    }
    if accepted || voice_frames > 0 {
        eprintln!("RESULT: accepted={accepted} voice_frames={voice_frames}");
    } else {
        eprintln!("RESULT: no acceptance, no voice");
    }
    // Best-effort hangup.
    let ies = Ies::empty();
    w.send_full(0, IaxCommand::Hangup, &ies, true);
}
