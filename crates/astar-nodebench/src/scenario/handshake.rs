// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The CALLTOKEN round when astar-server's CALLTOKEN is lost.
//!
//! A caller that never got the CALLTOKEN retransmits its opening NEW, still
//! without a token. `chan_iax2` answers every token-less NEW with CALLTOKEN;
//! astar-server answered the retransmit with REJECT "Call Token Invalid",
//! and the call never came up. ASL3 cannot be made to lose a datagram here,
//! so the bench plays the caller itself: raw IAX2 from the host to
//! astar-server's published port, 127.0.0.1 only.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, FullFrame, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{FrameType, IaxCommand, VoiceFormat};

use super::common::clean_start;
use super::{Check, Ctx, Outcome, Scenario};

/// Our source call number on the raw socket.
const CALLER: u16 = 3141;

pub struct CalltokenRetransmit;

/// What came back for one request.
struct Reply {
    cmd: Option<IaxCommand>,
    source_call: u16,
    oseqno: u8,
    token: Option<Vec<u8>>,
    cause: Option<String>,
}

fn new_frame(token: Option<&[u8]>, retransmission: bool) -> Vec<u8> {
    let ies = Ies {
        called_number: Some("1998"),
        calling_number: Some("nodebench"),
        capability: Some(VoiceFormat::G711U.as_u32()),
        format: Some(VoiceFormat::G711U.as_u32()),
        version: Some(2),
        calltoken: Some(token.unwrap_or(b"")),
        ..Ies::empty()
    };
    frame(0, 0, 0, retransmission, IaxCommand::New, ies)
}

fn frame(
    dest_call: u16,
    oseqno: u8,
    iseqno: u8,
    retransmission: bool,
    cmd: IaxCommand,
    ies: Ies<'_>,
) -> Vec<u8> {
    let mut b = Vec::new();
    // A fixed, well-formed frame: encoding cannot fail.
    let _ = encode(
        &Frame::Full(Box::new(FullFrame {
            source_call: CALLER,
            dest_call,
            retransmission,
            timestamp: 0,
            oseqno,
            iseqno,
            frame_type: FrameType::Iax,
            subclass: Subclass::Iax(cmd),
            ies,
            payload: &[],
        })),
        &mut b,
    );
    b
}

/// The next IAX command frame from astar-server (ACKs skipped), within 1 s.
fn reply(sock: &UdpSocket) -> Option<Reply> {
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        let Ok((n, _)) = sock.recv_from(&mut buf) else {
            continue;
        };
        let Ok(Frame::Full(f)) = parse_lenient(&buf[..n]) else {
            continue;
        };
        let cmd = match f.subclass {
            Subclass::Iax(IaxCommand::Ack) => continue,
            Subclass::Iax(c) => Some(c),
            _ => None,
        };
        return Some(Reply {
            cmd,
            source_call: f.source_call,
            oseqno: f.oseqno,
            token: f.ies.calltoken.map(<[u8]>::to_vec),
            cause: f.ies.cause.map(str::to_string),
        });
    }
    None
}

fn describe(r: Option<&Reply>) -> String {
    match r {
        None => "nothing within 1 s".into(),
        Some(r) => format!("{:?} (oseqno {}, cause {:?})", r.cmd, r.oseqno, r.cause),
    }
}

impl Scenario for CalltokenRetransmit {
    fn id(&self) -> &'static str {
        "calltoken_retransmit"
    }

    fn describe(&self) -> &'static str {
        "a NEW retransmitted without its token (CALLTOKEN lost) gets CALLTOKEN again, not REJECT, and the call completes"
    }

    fn run(&self, ctx: &mut Ctx) -> Outcome {
        let mut o = Outcome::new();
        if let Err(e) = clean_start(ctx) {
            o.check(Check::fail("starts unlinked", e));
            return o;
        }
        let to: SocketAddr = ctx.cfg().server_iax_host;
        let sock = match UdpSocket::bind("127.0.0.1:0") {
            Ok(s) => s,
            Err(e) => {
                o.check(Check::fail("raw caller socket", e.to_string()));
                return o;
            }
        };
        let _ = sock.set_read_timeout(Some(Duration::from_millis(50)));
        let send = |b: &[u8]| sock.send_to(b, to).map(|_| ()).map_err(|e| e.to_string());

        if let Err(e) = send(&new_frame(None, false)) {
            o.check(Check::fail("NEW sent", e));
            return o;
        }
        let first = reply(&sock);
        let Some(token) = first
            .as_ref()
            .filter(|r| r.cmd == Some(IaxCommand::CallToken))
            .and_then(|r| r.token.clone())
        else {
            o.check(Check::fail(
                "CALLTOKEN for the NEW",
                describe(first.as_ref()),
            ));
            return o;
        };
        o.check(Check::pass(
            "CALLTOKEN for the NEW",
            format!("{}-byte token", token.len()),
        ));

        // The CALLTOKEN "was lost": the caller retransmits its NEW.
        let _ = send(&new_frame(None, true));
        let again = reply(&sock);
        let reissued = again.as_ref().is_some_and(|r| {
            r.cmd == Some(IaxCommand::CallToken) && r.token.as_deref() == Some(&token[..])
        });
        o.check(Check::expect(
            "token-less NEW retransmit gets CALLTOKEN again",
            reissued,
            format!("astar-server answered {}", describe(again.as_ref())),
        ));

        let _ = send(&new_frame(Some(&token), false));
        let accept = reply(&sock);
        let accepted = accept
            .as_ref()
            .is_some_and(|r| r.cmd == Some(IaxCommand::Accept));
        o.check(Check::expect(
            "the token NEW is accepted",
            accepted,
            format!("astar-server answered {}", describe(accept.as_ref())),
        ));
        if let Some(a) = accept.filter(|_| accepted) {
            // Leave nothing behind: hang the call up.
            let _ = send(&frame(
                a.source_call,
                1,
                a.oseqno.wrapping_add(1),
                false,
                IaxCommand::Hangup,
                Ies::empty(),
            ));
            let _ = reply(&sock);
        }
        o
    }
}
