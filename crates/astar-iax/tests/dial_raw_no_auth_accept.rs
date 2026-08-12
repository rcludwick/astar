// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Regression (iax-64b6): `dial_raw` must reach Answered against the REAL
//! [`IncomingCallListener`] (the node-as-handset inbound path) with `auth=Off`,
//! not just a hand-rolled UDP peer. The peer accepts the NEW directly (no auth,
//! no CALLTOKEN), so the outbound FSM sees ACCEPT as the first reliable frame in
//! `NewSent` — which previously fell through to `LogInvalid` and timed out.

use std::time::{Duration, Instant};

use astar_iax::{
    CallEvent, CallMode, IncomingAuthPolicy, IncomingCallEvent, IncomingCallListener,
    IncomingCallPolicy, IncomingDecisionPolicy, dial_raw,
};

#[test]
fn dial_raw_into_autoaccept_listener_reaches_answered() {
    let policy = IncomingCallPolicy {
        decision: IncomingDecisionPolicy::AutoAccept,
        auth: IncomingAuthPolicy::Off,
        ..IncomingCallPolicy::default()
    };
    let (listener, _events) = IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(policy)
        .start()
        .expect("listener starts");
    let addr = listener.local_addr();

    let raw = dial_raw(addr, "echo-test", "s", "", CallMode::Standard).expect("dial");

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut answered = false;
    while Instant::now() < deadline && !answered {
        while let Ok(ev) = raw.events.try_recv() {
            match ev {
                CallEvent::Answered { .. } => answered = true,
                CallEvent::Hangup { reason } => panic!("parrot hung up early: {reason:?}"),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        answered,
        "dial_raw must reach Answered against a real auto-accept listener"
    );
}

#[test]
fn dial_raw_into_appdecide_listener_reaches_answered_when_answered() {
    let policy = IncomingCallPolicy {
        decision: IncomingDecisionPolicy::AppDecide,
        auth: IncomingAuthPolicy::Off,
        ..IncomingCallPolicy::default()
    };
    let (listener, levents) = IncomingCallListener::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .policy(policy)
        .start()
        .expect("listener starts");
    let addr = listener.local_addr();

    let raw = dial_raw(addr, "echo-test", "s", "", CallMode::Standard).expect("dial");

    // Answer the offer as soon as it surfaces (mirrors the NodeEngine Auto path).
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut answered = false;
    let mut answered_call = None;
    while Instant::now() < deadline && !answered {
        if let Ok(IncomingCallEvent::Incoming(c)) = levents.try_recv() {
            let (call, _e) = c.answer().expect("answer");
            answered_call = Some(call);
        }
        while let Ok(ev) = raw.events.try_recv() {
            if let CallEvent::Answered { .. } = ev {
                answered = true;
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(
        answered,
        "dial_raw must reach Answered after the listener answers"
    );
    drop(answered_call);
}
