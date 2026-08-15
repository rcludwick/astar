// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Property tests: arbitrary `RegState × RegEvent` pair never panics, and
//! terminal states (`Failed`, `Closed`) absorb any further event without
//! changing state.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use astar_iax_core::session::auth::{AuthMethods, Credentials, Secret};
use astar_iax_core::session::call_no::CallNo;
use astar_iax_core::session::fsm::{CallToken, TimerKind};
use astar_iax_core::session::reg::{
    RegAppCommand, RegEvent, RegFailReason, RegFsm, RegState, RegisterOptions,
};
use proptest::prelude::*;

fn any_state() -> impl Strategy<Value = RegState> {
    let our = CallNo::new(1).unwrap();
    let peer = CallNo::new(7).unwrap();
    let now = Instant::now();
    let aa: Option<SocketAddr> = "127.0.0.1:4569".parse().ok();
    prop_oneof![
        Just(RegState::Idle),
        Just(RegState::RegReqSent {
            sent_at: now,
            our_call: our,
            attempts: 1,
            refresh_request: Duration::from_secs(60),
        }),
        Just(RegState::RegReqResent {
            sent_at: now,
            our_call: our,
            token: CallToken::new(vec![1, 2, 3]).unwrap(),
            attempts: 1,
            refresh_request: Duration::from_secs(60),
        }),
        Just(RegState::RegAuthRecv {
            challenge: b"c".to_vec(),
            methods: AuthMethods::MD5,
            our_call: our,
            peer_call: peer,
        }),
        Just(RegState::RegPending {
            sent_at: now,
            our_call: our,
            peer_call: peer,
            attempts: 1,
            challenge: b"c".to_vec(),
        }),
        Just(RegState::Registered {
            our_call: our,
            peer_call: peer,
            refresh: Duration::from_secs(60),
            apparent_addr: aa,
            registered_at: now,
        }),
        Just(RegState::RegRelSent {
            our_call: our,
            peer_call: peer,
            sent_at: now,
            attempts: 1,
        }),
        Just(RegState::Closed),
        Just(RegState::Failed(RegFailReason::Aborted)),
    ]
}

fn any_timer_kind() -> impl Strategy<Value = TimerKind> {
    prop_oneof![
        Just(TimerKind::NewRetry),
        Just(TimerKind::HangupRetry),
        Just(TimerKind::AuthRepRetry),
        Just(TimerKind::TokenExpiry),
        Just(TimerKind::RegReqRetry),
        Just(TimerKind::RegAuthRetry),
        Just(TimerKind::RegTokenExpiry),
        Just(TimerKind::RegRefresh),
        Just(TimerKind::RegRelRetry),
    ]
}

fn any_app_command() -> impl Strategy<Value = RegAppCommand> {
    let now = Instant::now();
    prop_oneof![
        Just(RegAppCommand::StartRegister { now }),
        Just(RegAppCommand::Deregister { now }),
    ]
}

fn creds() -> Credentials {
    Credentials {
        username: "u".into(),
        password: Arc::new(Secret::new("p".into())),
        allowed_methods: AuthMethods::MD5,
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn no_panic_on_app_events(
        state in any_state(),
        cmd in any_app_command(),
    ) {
        let mut f = RegFsm::with_state(state, creds(), CallNo::new(1).unwrap(), RegisterOptions::default());
        let _ = f.handle(RegEvent::App(cmd));
    }

    #[test]
    fn no_panic_on_timer_events(
        state in any_state(),
        timer in any_timer_kind(),
        salt in any::<u32>(),
    ) {
        let mut f = RegFsm::with_state(state, creds(), CallNo::new(1).unwrap(), RegisterOptions::default());
        let _ = f.handle(RegEvent::Timer { kind: timer, now: Instant::now(), jitter_salt: salt });
    }
}

#[test]
fn failed_absorbs_any_event() {
    let mut f = RegFsm::with_state(
        RegState::Failed(RegFailReason::Aborted),
        creds(),
        CallNo::new(1).unwrap(),
        RegisterOptions::default(),
    );
    let _ = f.handle(RegEvent::App(RegAppCommand::StartRegister {
        now: Instant::now(),
    }));
    assert!(matches!(f.state(), RegState::Failed(_)));
}

#[test]
fn closed_absorbs_any_event() {
    let mut f = RegFsm::with_state(
        RegState::Closed,
        creds(),
        CallNo::new(1).unwrap(),
        RegisterOptions::default(),
    );
    let _ = f.handle(RegEvent::App(RegAppCommand::StartRegister {
        now: Instant::now(),
    }));
    assert!(matches!(f.state(), RegState::Closed));
}
