// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! nodebench finding 7: a link's voice counts only while the far node is
//! transmitting, by `app_rpt`'s rule.
//!
//! An `app_rpt` node that has done the `!NEWKEY1!` handshake keys by sending
//! voice. One that has not, within 2 s, marks its transmission with
//! RADIO_KEY, and on a monitor link (`ilink 2`) it streams voice but never
//! keys. astar-server bridged that voice at full level. The far node here is
//! a hand-driven socket on 127.0.0.1.

#![allow(clippy::too_many_lines, clippy::doc_markdown)]

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use astar_iax::{CallEvent, CallMode, RawDial, dial_raw};
use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, FullFrame, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{ControlSubclass, FrameType, IaxCommand, VoiceFormat};

const PEER: u16 = 777;

struct FarNode {
    sock: UdpSocket,
    astar: SocketAddr,
    astar_call: u16,
    oseqno: u8,
    ts: u32,
}

impl FarNode {
    fn send(&mut self, frame_type: FrameType, subclass: Subclass, ies: Ies<'_>, payload: &[u8]) {
        let mut b = Vec::new();
        encode(
            &Frame::Full(Box::new(FullFrame {
                source_call: PEER,
                dest_call: self.astar_call,
                retransmission: false,
                timestamp: self.ts,
                oseqno: self.oseqno,
                iseqno: 1,
                frame_type,
                subclass,
                ies,
                payload,
            })),
            &mut b,
        )
        .unwrap();
        self.oseqno = self.oseqno.wrapping_add(1);
        self.ts += 20;
        self.sock.send_to(&b, self.astar).unwrap();
    }

    fn text(&mut self, body: &str) {
        let mut payload = body.as_bytes().to_vec();
        payload.push(0); // app_rpt's C-string terminator
        self.send(FrameType::Text, Subclass::Raw(0), Ies::empty(), &payload);
    }

    fn voice(&mut self, frames: usize) {
        let tone = [0x10u8; 160]; // loud, steady µ-law
        for _ in 0..frames {
            self.send(
                FrameType::Voice,
                Subclass::Voice(VoiceFormat::G711U),
                Ies::empty(),
                &tone,
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn control(&mut self, c: ControlSubclass) {
        self.send(FrameType::Control, Subclass::Control(c), Ies::empty(), &[]);
    }
}

/// astar dials the far node, which accepts and answers.
fn link_up() -> (RawDial, FarNode) {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let to = sock.local_addr().unwrap();
    let raw = dial_raw(
        to,
        "1998",
        "1999",
        "",
        CallMode::NodeLink {
            node: "1998".into(),
        },
    )
    .expect("dial");
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(2);
    let (astar_call, astar) = loop {
        assert!(Instant::now() < deadline, "no NEW");
        if let Ok((n, from)) = sock.recv_from(&mut buf)
            && let Ok(Frame::Full(f)) = parse_lenient(&buf[..n])
            && f.subclass == Subclass::Iax(IaxCommand::New)
        {
            break (f.source_call, from);
        }
    };
    let mut far = FarNode {
        sock,
        astar,
        astar_call,
        oseqno: 0,
        ts: 3,
    };
    far.send(
        FrameType::Iax,
        Subclass::Iax(IaxCommand::Accept),
        Ies {
            format: Some(VoiceFormat::G711U.as_u32()),
            ..Ies::empty()
        },
        &[],
    );
    far.control(ControlSubclass::Answer);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match raw
            .events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(CallEvent::Answered { .. }) => break,
            Ok(_) => {}
            Err(e) => panic!("never answered: {e}"),
        }
    }
    (raw, far)
}

fn drain(raw: &RawDial, wait: Duration) -> usize {
    let deadline = Instant::now() + wait;
    let mut n = 0;
    while let Ok(_f) = raw
        .rx_frames
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
        n += 1;
    }
    n
}

#[test]
fn an_app_rpt_link_without_newkey_is_heard_only_while_radio_keyed() {
    let (raw, mut far) = link_up();
    far.text("L "); // an app_rpt node, but no !NEWKEY1!
    std::thread::sleep(Duration::from_millis(2200)); // past NEWKEYTIME
    drain(&raw, Duration::from_millis(50));

    far.voice(10);
    let unkeyed = drain(&raw, Duration::from_millis(300));
    assert_eq!(
        unkeyed, 0,
        "voice from an unkeyed old-mode link (a monitor link) must not be heard"
    );

    far.control(ControlSubclass::RadioKey);
    far.voice(10);
    let keyed = drain(&raw, Duration::from_millis(300));
    assert!(keyed >= 5, "RADIO_KEY opens it: {keyed} frames");

    far.control(ControlSubclass::RadioUnkey);
    std::thread::sleep(Duration::from_millis(50));
    drain(&raw, Duration::from_millis(50));
    far.voice(10);
    assert_eq!(
        drain(&raw, Duration::from_millis(300)),
        0,
        "RADIO_UNKEY closes it again"
    );
}

#[test]
fn a_newkey_link_is_heard_whenever_it_sends_voice() {
    let (raw, mut far) = link_up();
    far.text("!NEWKEY1!");
    far.text("L ");
    std::thread::sleep(Duration::from_millis(2200));
    drain(&raw, Duration::from_millis(50));
    far.voice(10);
    let n = drain(&raw, Duration::from_millis(300));
    assert!(n >= 5, "a NEWKEY peer keys by voice: {n} frames");
}

#[test]
fn a_peer_that_is_not_app_rpt_is_always_heard() {
    let (raw, mut far) = link_up();
    std::thread::sleep(Duration::from_millis(2200));
    drain(&raw, Duration::from_millis(50));
    far.voice(10);
    let n = drain(&raw, Duration::from_millis(300));
    assert!(n >= 5, "no link TEXT, no keying rule: {n} frames");
}
