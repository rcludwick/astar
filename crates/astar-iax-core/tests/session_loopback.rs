// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Loopback: drive a client FSM through hand-crafted "server" frames that
//! emulate AUTHREQ → ACCEPT (with optional CALLTOKEN preamble).
//!
//! No tokio, no sockets — frames pass through direct `Event::Frame` dispatch.

use std::sync::Arc;
use std::time::{Duration, Instant};

use astar_iax_core::frame::{Frame, FullFrame, OwnedFullFrame, Subclass};
use astar_iax_core::ie::Ies;
use astar_iax_core::session::auth::{AuthMethods, Credentials, Secret};
use astar_iax_core::session::call_no::CallNo;
use astar_iax_core::session::fsm::{Action, AppCommand, AppEvent, Event, Fsm, SessionState};
use astar_iax_core::subclass::{FrameType, IaxCommand};

fn creds() -> Credentials {
    Credentials {
        username: "rob".into(),
        password: Arc::new(Secret::new("hunter2".into())),
        allowed_methods: AuthMethods::MD5,
    }
}

fn reply(
    server_call: u16,
    client_call: u16,
    oseqno: u8,
    iseqno: u8,
    cmd: IaxCommand,
    ies: Ies<'static>,
) -> Frame<'static> {
    Frame::Full(Box::new(FullFrame {
        source_call: server_call,
        dest_call: client_call,
        retransmission: false,
        timestamp: 0,
        oseqno,
        iseqno,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(cmd),
        ies,
        payload: &[],
    }))
}

#[test]
fn client_reaches_active_against_minimal_server() {
    let mut client = Fsm::new(creds(), CallNo::new(1).unwrap());
    let now = Instant::now();

    // 1. App starts the call.
    let acts = client.handle(Event::App(AppCommand::StartCall {
        dest: "1000".into(),
        now,
    }));
    let new_frame: OwnedFullFrame = acts
        .iter()
        .find_map(|a| match a {
            Action::SendReliable(f) => Some(f.clone()),
            _ => None,
        })
        .expect("client emitted NEW");
    assert!(matches!(new_frame.subclass, Subclass::Iax(IaxCommand::New)));

    // 2. Server immediately responds with AUTHREQ (requirecalltoken=no path).
    let authreq = reply(
        7,
        1,
        0,
        1,
        IaxCommand::AuthReq,
        Ies {
            challenge: Some("c0ffee"),
            authmethods: Some(2),
            ..Ies::empty()
        },
    );
    let acts = client.handle(Event::Frame {
        frame: authreq,
        now: now + Duration::from_millis(20),
    });
    #[allow(clippy::similar_names)]
    let authrep_frame: OwnedFullFrame = acts
        .iter()
        .find_map(|a| match a {
            Action::SendReliable(f) if matches!(f.subclass, Subclass::Iax(IaxCommand::AuthRep)) => {
                Some(f.clone())
            }
            _ => None,
        })
        .expect("client emitted AUTHREP");
    let parsed = Ies::parse(authrep_frame.ie_bytes()).expect("parse authrep ies");
    assert!(parsed.md5_result.is_some(), "AUTHREP carries MD5_RESULT");

    // 3. Server sends ACCEPT.
    let accept = reply(7, 1, 1, 2, IaxCommand::Accept, Ies::empty());
    let acts = client.handle(Event::Frame {
        frame: accept,
        now: now + Duration::from_millis(40),
    });
    let mut connected = false;
    for a in &acts {
        if let Action::AppEvent(AppEvent::Connected { .. }) = a {
            connected = true;
        }
    }
    assert!(connected, "client observed Connected");
    assert!(matches!(client.state(), SessionState::Active(_)));
}

#[test]
fn client_reaches_active_via_calltoken_path() {
    let mut client = Fsm::new(creds(), CallNo::new(1).unwrap());
    let now = Instant::now();
    let _ = client.handle(Event::App(AppCommand::StartCall {
        dest: "1000".into(),
        now,
    }));
    // Server demands CALLTOKEN first.
    let token_frame = reply(
        7,
        1,
        0,
        1,
        IaxCommand::CallToken,
        Ies {
            calltoken: Some(b"server-token"),
            ..Ies::empty()
        },
    );
    let _ = client.handle(Event::Frame {
        frame: token_frame,
        now: now + Duration::from_millis(10),
    });
    assert!(matches!(client.state(), SessionState::NewResent(_)));

    // Then AUTHREQ, AUTHREP, ACCEPT.
    let authreq = reply(
        7,
        1,
        1,
        2,
        IaxCommand::AuthReq,
        Ies {
            challenge: Some("x"),
            authmethods: Some(2),
            ..Ies::empty()
        },
    );
    let _ = client.handle(Event::Frame {
        frame: authreq,
        now: now + Duration::from_millis(20),
    });
    let accept = reply(7, 1, 2, 3, IaxCommand::Accept, Ies::empty());
    let acts = client.handle(Event::Frame {
        frame: accept,
        now: now + Duration::from_millis(30),
    });
    assert!(
        acts.iter()
            .any(|a| matches!(a, Action::AppEvent(AppEvent::Connected { .. })))
    );
    assert!(matches!(client.state(), SessionState::Active(_)));
}

/// Regression: a node that demands a CALLTOKEN but does NOT challenge
/// (`auth=off`, e.g. an `astar-server` listener on the default policy) sends
/// ACCEPT directly after the token'd NEW — there is no AUTHREQ. The dial FSM
/// sits in `NewResent`, which previously handled only AUTHREQ, so the ACCEPT
/// fell through to `LogInvalid` and the client re-sent NEW forever, never
/// connecting. ACCEPT in `NewResent` must complete the call.
#[test]
fn client_reaches_active_via_calltoken_then_direct_accept() {
    let mut client = Fsm::new(creds(), CallNo::new(1).unwrap());
    let now = Instant::now();
    let _ = client.handle(Event::App(AppCommand::StartCall {
        dest: "1000".into(),
        now,
    }));
    // Server demands CALLTOKEN first.
    let token_frame = reply(
        7,
        1,
        0,
        1,
        IaxCommand::CallToken,
        Ies {
            calltoken: Some(b"server-token"),
            ..Ies::empty()
        },
    );
    let _ = client.handle(Event::Frame {
        frame: token_frame,
        now: now + Duration::from_millis(10),
    });
    assert!(matches!(client.state(), SessionState::NewResent(_)));

    // auth=off: the server ACCEPTs the token'd NEW directly — no AUTHREQ.
    let accept = reply(7, 1, 1, 2, IaxCommand::Accept, Ies::empty());
    let acts = client.handle(Event::Frame {
        frame: accept,
        now: now + Duration::from_millis(20),
    });
    assert!(
        acts.iter()
            .any(|a| matches!(a, Action::AppEvent(AppEvent::Connected { .. }))),
        "client connects on a direct ACCEPT after CALLTOKEN"
    );
    assert!(matches!(client.state(), SessionState::Active(_)));
}

#[test]
fn client_emits_dtmf_begin_end_pair_when_active() {
    use astar_iax_core::frame::{OwnedFrame, encode, parse};

    let mut client = Fsm::new(creds(), CallNo::new(1).unwrap());
    let now = Instant::now();
    let _ = client.handle(Event::App(AppCommand::StartCall {
        dest: "1000".into(),
        now,
    }));
    let authreq = reply(
        7,
        1,
        0,
        1,
        IaxCommand::AuthReq,
        Ies {
            challenge: Some("c0ffee"),
            authmethods: Some(2),
            ..Ies::empty()
        },
    );
    let _ = client.handle(Event::Frame {
        frame: authreq,
        now,
    });
    let accept = reply(7, 1, 1, 2, IaxCommand::Accept, Ies::empty());
    let _ = client.handle(Event::Frame { frame: accept, now });
    assert!(matches!(client.state(), SessionState::Active(_)));

    let actions = client.handle(Event::App(AppCommand::SendDtmf {
        digit: '3',
        now: now + Duration::from_millis(200),
    }));
    let frames: Vec<OwnedFullFrame> = actions
        .iter()
        .filter_map(|a| match a {
            Action::SendReliable(f) => Some(f.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(frames.len(), 2, "BEGIN + END");

    let mut wire = Vec::new();
    encode(
        &OwnedFrame::Full(frames[0].clone()).as_frame().unwrap(),
        &mut wire,
    )
    .expect("DTMF frame has empty IE set");
    let Frame::Full(p) = parse(&wire).unwrap() else {
        panic!()
    };
    assert_eq!(p.frame_type, FrameType::DtmfBegin);
    assert_eq!(p.subclass, Subclass::Dtmf('3'));

    let mut wire2 = Vec::new();
    encode(
        &OwnedFrame::Full(frames[1].clone()).as_frame().unwrap(),
        &mut wire2,
    )
    .expect("DTMF frame has empty IE set");
    let Frame::Full(p2) = parse(&wire2).unwrap() else {
        panic!()
    };
    assert_eq!(p2.frame_type, FrameType::DtmfEnd);
    assert_eq!(p2.subclass, Subclass::Dtmf('3'));
}

/// Regression for iax-6813: the FSM previously hardcoded `dest=""` when
/// re-sending NEW after CALLTOKEN, breaking dialplan routing on real
/// Asterisk servers. The resent NEW must carry the original called number.
#[test]
fn resent_new_after_calltoken_preserves_called_number() {
    let mut client = Fsm::new(creds(), CallNo::new(1).unwrap());
    let now = Instant::now();
    let _ = client.handle(Event::App(AppCommand::StartCall {
        dest: "1000".into(),
        now,
    }));
    let token_frame = reply(
        7,
        1,
        0,
        1,
        IaxCommand::CallToken,
        Ies {
            calltoken: Some(b"server-token"),
            ..Ies::empty()
        },
    );
    let acts = client.handle(Event::Frame {
        frame: token_frame,
        now: now + Duration::from_millis(10),
    });
    let resent: OwnedFullFrame = acts
        .iter()
        .find_map(|a| match a {
            Action::SendReliable(f) if matches!(f.subclass, Subclass::Iax(IaxCommand::New)) => {
                Some(f.clone())
            }
            _ => None,
        })
        .expect("client emitted resent NEW");
    let ies = Ies::parse(resent.ie_bytes()).expect("parse resent NEW ies");
    assert_eq!(
        ies.called_number,
        Some("1000"),
        "resent NEW must carry the original called number, not empty"
    );
}
