// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Replay `crates/astar-conformance/fixtures/c-iaxclient/incoming.pcap` against
//! an inbound (callee) `Fsm`, mirroring `reg_fixture_replay.rs`.
//!
//! The capture (see `incoming.md`) is the NEW-received side of libiax2:
//!   REGREQ/REGAUTH/REGREQ+MD5/REGACK (registration leg, ignored here)
//!   NEW (inbound, dcallno=0) → ACCEPT + ANSWER (we accept) → PING/PONG →
//!   HANGUP (from asterisk) → ACK chain.
//!
//! The `[astartest_notok]` context uses **no auth, no CALLTOKEN** (per
//! `incoming.md`), so we drive the FSM with `InboundPolicy { auto_answer:
//! true, .. }` to emit ACCEPT + ANSWER back-to-back (matches pcap frames
//! 8-9). We assert **state progression** and the **presence** of the emitted
//! ACCEPT / ANSWER frames — not byte-equality of the whole stream. The
//! libiax2 PING at frame 7 is omitted by our FSM (an intentional divergence,
//! per the design's "Test plan"), so oseqnos differ; that does not affect the
//! state-progression assertions.
//!
//! Progression asserted: `NewReceived → AnswerSent → Active → Hangup{Peer} →
//! Closed`.

#![allow(clippy::too_many_lines)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, parse_lenient};
use astar_iax_core::session::auth::{AuthMethods, Credentials, Secret};
use astar_iax_core::session::call_no::CallNo;
use astar_iax_core::session::fsm::{
    Action, AppCommand, AppEvent, Event, Fsm, HangupData, HangupOrigin, InboundPolicy,
    IncomingOffer, SessionState,
};
use astar_iax_core::subclass::{ControlSubclass, FrameType, IaxCommand};

const FIXTURE_RELPATH: &str = "../astar-conformance/fixtures/c-iaxclient/incoming.pcap";

/// Minimal `PcapNG` iterator (copied from `reg_fixture_replay.rs`). Yields each
/// Enhanced Packet Block's raw packet data in file order. Linux SLL link layer.
fn iterate_packets(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let btype = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        let blen = u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
            as usize;
        if blen == 0 || pos + blen > data.len() {
            break;
        }
        if btype == 0x0000_0006 {
            let body = &data[pos + 8..pos + blen];
            if body.len() >= 20 {
                let cap_len = u32::from_le_bytes([body[12], body[13], body[14], body[15]]) as usize;
                if 20 + cap_len <= body.len() {
                    out.push(body[20..20 + cap_len].to_vec());
                }
            }
        }
        pos += blen;
    }
    out
}

/// Extract IAX2 payload from an SLL-wrapped IPv4/UDP packet (linktype=113).
fn iax_payload(pkt: &[u8]) -> Option<&[u8]> {
    if pkt.len() < 16 + 20 + 8 {
        return None;
    }
    let ether_proto = u16::from_be_bytes([pkt[14], pkt[15]]);
    if ether_proto != 0x0800 {
        return None;
    }
    let ip = &pkt[16..];
    let ihl = (ip[0] & 0x0f) as usize * 4;
    if ip.len() < ihl + 8 {
        return None;
    }
    if ip[9] != 17 {
        return None;
    }
    let udp = &ip[ihl..];
    let dst_port = u16::from_be_bytes([udp[2], udp[3]]);
    let src_port = u16::from_be_bytes([udp[0], udp[1]]);
    if dst_port != 4569 && src_port != 4569 {
        return None;
    }
    let ulen = u16::from_be_bytes([udp[4], udp[5]]) as usize;
    if ulen < 8 || ulen > udp.len() {
        return None;
    }
    Some(&udp[8..ulen])
}

fn creds() -> Credentials {
    Credentials {
        username: "astartest_notok".into(),
        password: Arc::new(Secret::new("supersecret".into())),
        allowed_methods: AuthMethods::MD5,
    }
}

#[test]
fn incoming_pcap_drives_no_auth_callee_to_active_then_closed() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let path = PathBuf::from(manifest).join(FIXTURE_RELPATH);
    let data = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let payloads: Vec<Vec<u8>> = iterate_packets(&data)
        .iter()
        .filter_map(|pkt| iax_payload(pkt).map(<[u8]>::to_vec))
        .collect();
    assert!(!payloads.is_empty(), "fixture should contain IAX2 payloads");

    // 1. Locate the inbound NEW (dest_call == 0, IaxCommand::New). It carries the
    //    peer's source_call (our peer_call) and the offer IEs.
    let mut new_payload: Option<usize> = None;
    let mut peer_call = None;
    let mut offer = None;
    for (idx, p) in payloads.iter().enumerate() {
        let Ok(Frame::Full(f)) = parse_lenient(p) else {
            continue;
        };
        if f.dest_call == 0 && f.subclass == Subclass::Iax(IaxCommand::New) {
            peer_call = Some(CallNo::new(f.source_call).expect("non-zero source_call"));
            offer = Some(IncomingOffer::from_new_ies(&f.ies).expect("valid NEW offer"));
            new_payload = Some(idx);
            break;
        }
    }
    let new_idx = new_payload.expect("capture contains an inbound NEW");
    let peer_call = peer_call.unwrap();
    let offer = offer.unwrap();

    // 2. Build the inbound FSM seeded from the NEW. our_call mirrors the capture
    //    (16379) but any non-zero value works — the FSM is address-implicit.
    let our_call = CallNo::new(16379).unwrap();
    let now = Instant::now();
    let mut f = Fsm::for_inbound(creds(), our_call, peer_call, offer, now).with_inbound_policy(
        InboundPolicy {
            auto_answer: true,
            ..InboundPolicy::default()
        },
    );
    f.seed_entropy([0xAB; 16], "c0ffeebabec0ffeebabec0ffeebabe00".into());
    assert!(matches!(f.state(), SessionState::NewReceived(_)));

    // 3. Kick the leg (the NEW was already consumed for demux). auto_answer ⇒
    //    ACCEPT + ANSWER emitted back-to-back, → AnswerSent.
    let actions = f.handle(Event::App(AppCommand::DriveInbound { now }));
    assert!(
        actions.iter().any(|a| matches!(a, Action::SendReliable(fr)
            if matches!(fr.subclass, Subclass::Iax(IaxCommand::Accept)))),
        "ACCEPT emitted (matches pcap frame 8)"
    );
    assert!(
        actions.iter().any(|a| matches!(a, Action::SendReliable(fr)
            if fr.frame_type == FrameType::Control
                && fr.subclass == Subclass::Control(ControlSubclass::Answer))),
        "ANSWER (Control) emitted (matches pcap frame 9)"
    );
    assert!(matches!(f.state(), SessionState::AnswerSent(_)));

    // 4. Reliability releases the in-flight ANSWER once the peer ACKs it
    //    (pcap frames 11/12). The FSM never sees the bare ACK; the runtime
    //    fires AnswerAcked. → Active, emits Connected.
    let actions = f.handle(Event::App(AppCommand::AnswerAcked {
        now: now + Duration::from_millis(40),
    }));
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::AppEvent(AppEvent::Connected { .. }))),
        "Connected event on reaching Active"
    );
    assert!(matches!(f.state(), SessionState::Active(_)));

    // 5. Feed the peer HANGUP (pcap frame 14) → Hangup{Peer} (we ACK it).
    let hangup = payloads[new_idx..]
        .iter()
        .find_map(|p| match parse_lenient(p) {
            Ok(Frame::Full(fr))
                if fr.subclass == Subclass::Iax(IaxCommand::Hangup)
                    && fr.dest_call == our_call.value() =>
            {
                Some(p.clone())
            }
            _ => None,
        })
        .expect("capture contains a peer HANGUP");
    let frame = parse_lenient(&hangup).unwrap();
    let _ = f.handle(Event::Frame {
        frame,
        now: now + Duration::from_millis(80),
    });
    assert!(
        matches!(
            f.state(),
            SessionState::Hangup(HangupData {
                initiated_by: HangupOrigin::Peer,
                ..
            })
        ),
        "peer HANGUP → Hangup{{Peer}}; got {:?}",
        f.state()
    );

    // 6. The closing ACK confirms our HANGUP-ACK is consumed; in Hangup{Peer}
    //    an inbound ACK frame drives → Closed. Synthesize one from any ACK
    //    payload in the capture (on_hangup only matches on subclass == Ack).
    let ack = payloads
        .iter()
        .find_map(|p| match parse_lenient(p) {
            Ok(Frame::Full(fr)) if fr.subclass == Subclass::Iax(IaxCommand::Ack) => Some(p.clone()),
            _ => None,
        })
        .expect("capture contains an ACK frame");
    let frame = parse_lenient(&ack).unwrap();
    let _ = f.handle(Event::Frame {
        frame,
        now: now + Duration::from_millis(100),
    });
    assert!(
        matches!(f.state(), SessionState::Closed),
        "closing ACK → Closed; got {:?}",
        f.state()
    );
}
