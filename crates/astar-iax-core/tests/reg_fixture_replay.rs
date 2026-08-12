// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Replay `crates/astar-conformance/fixtures/c-iaxclient/register.pcap` against
//! a synthetic `RegFsm`. Honours R-9.0-03 (state-progression parity with a
//! recorded `REGREQ` → `REGAUTH` → `REGREQ(+MD5)` → `REGACK` → `REGREL` →
//! `ACK` flow).
//!
//! The pcap is `PcapNG` (Linux SLL link layer, IPv4/UDP/4569). We parse the
//! file inline — no `pcap-parser` dependency — extract each Asterisk → client
//! IAX2 frame, and feed it to the FSM. We assert state progression:
//!   `StartRegister` → `RegReqSent` → `RegPending` → `Registered` →
//!   `RegRelSent` → `Closed`.
//!
//! We do not byte-compare MD5 challenge/response, since the FSM computes the
//! response from the captured challenge and our test-creds password; that
//! value will not match `c-iaxclient`'s recorded response (it used a different
//! password). State progression is the load-bearing assertion.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, parse_lenient};
use astar_iax_core::session::auth::{AuthMethods, Credentials, Secret};
use astar_iax_core::session::call_no::CallNo;
use astar_iax_core::session::reg::{RegAppCommand, RegEvent, RegFsm, RegState, RegisterOptions};
use astar_iax_core::subclass::IaxCommand;

const FIXTURE_RELPATH: &str = "../astar-conformance/fixtures/c-iaxclient/register.pcap";

/// Minimal `PcapNG` iterator. Yields each Enhanced Packet Block's raw packet
/// data (capture-length bytes), in file order. Linux SLL link layer.
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
            // Enhanced Packet Block body starts at pos+8:
            //   interface_id(4) + ts_high(4) + ts_low(4) + cap_len(4) + orig_len(4)
            //   + packet_data (padded to 4) + options + trailer length(4)
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
/// SLL header: 16 bytes (cooked-mode). Returns None if not IPv4/UDP/4569.
fn iax_payload(pkt: &[u8]) -> Option<&[u8]> {
    if pkt.len() < 16 + 20 + 8 {
        return None;
    }
    // SLL: bytes 14..16 are protocol; 0x0800 = IPv4.
    let ether_proto = u16::from_be_bytes([pkt[14], pkt[15]]);
    if ether_proto != 0x0800 {
        return None;
    }
    let ip = &pkt[16..];
    let ihl = (ip[0] & 0x0f) as usize * 4;
    if ip.len() < ihl + 8 {
        return None;
    }
    let proto = ip[9];
    if proto != 17 {
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

fn is_peer_to_client(subclass: Subclass) -> bool {
    matches!(
        subclass,
        Subclass::Iax(
            IaxCommand::CallToken
                | IaxCommand::RegAuth
                | IaxCommand::RegAck
                | IaxCommand::RegRej
                | IaxCommand::Ack,
        )
    )
}

#[test]
fn register_pcap_drives_regfsm_to_registered_then_closed() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let path = PathBuf::from(manifest).join(FIXTURE_RELPATH);
    let data = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let packets = iterate_packets(&data);
    assert!(!packets.is_empty(), "fixture should contain packet blocks");

    let creds = Credentials {
        username: "astartest_notok".into(),
        password: Arc::new(Secret::new("supersecret".into())),
        allowed_methods: AuthMethods::MD5,
    };
    let mut f = RegFsm::new(creds, CallNo::new(1).unwrap(), RegisterOptions::default());

    // 1. App: StartRegister.
    let now = Instant::now();
    let _ = f.handle(RegEvent::App(RegAppCommand::StartRegister { now }));
    assert!(matches!(f.state(), RegState::RegReqSent { .. }));

    // 2. Feed each peer → client full-frame from the capture in order.
    let mut reached_registered = false;
    let mut reached_closed = false;
    for (idx, pkt) in packets.iter().enumerate() {
        let Some(payload) = iax_payload(pkt) else {
            continue;
        };
        let Ok(frame) = parse_lenient(payload) else {
            continue;
        };
        let subclass = match &frame {
            Frame::Full(f) => f.subclass,
            Frame::Mini(_) => continue,
        };
        if !is_peer_to_client(subclass) {
            continue;
        }
        let actions = f.handle(RegEvent::Frame {
            frame,
            now: now + Duration::from_millis(20 * (idx as u64 + 1)),
        });
        // Drain LogInvalid actions silently; we care about state progression.
        let _ = actions;
        if matches!(f.state(), RegState::Registered { .. }) && !reached_registered {
            reached_registered = true;
            let _ = f.handle(RegEvent::App(RegAppCommand::Deregister {
                now: now + Duration::from_secs(1),
            }));
            assert!(matches!(f.state(), RegState::RegRelSent { .. }));
        }
        if matches!(f.state(), RegState::Closed) {
            reached_closed = true;
            break;
        }
    }
    assert!(
        reached_registered,
        "FSM should have reached Registered; final state = {:?}",
        f.state()
    );
    assert!(
        reached_closed,
        "FSM should have reached Closed; final state = {:?}",
        f.state()
    );
}
