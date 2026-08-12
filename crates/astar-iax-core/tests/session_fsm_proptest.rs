// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Property tests: no `(state, event)` combination panics, and the catch-all
//! arm emits exactly one `LogInvalid` action while leaving state unchanged.

#![allow(clippy::too_many_lines)]

use std::sync::Arc;
use std::time::Instant;

use astar_iax_core::session::auth::{AuthMethods, Credentials, Secret};
use astar_iax_core::session::call_no::CallNo;
use astar_iax_core::session::fsm::{
    AcceptSentData, Action, ActiveData, AnswerSentData, AppCommand, AuthRepSentData,
    AuthReqReceivedData, AuthReqSentData, CallToken, CallTokenIssuedData, CallTokenReceivedData,
    CodecMask, Event, FailReason, Fsm, HangupData, HangupOrigin, IncomingOffer, NewReceivedData,
    NewResentData, NewSentData, SessionState, TimerKind,
};
use astar_iax_core::session::keepalive::{KeepaliveConfig, KeepaliveState};
use astar_iax_core::subclass::VoiceFormat;
use proptest::prelude::*;

fn any_state() -> impl Strategy<Value = SessionState> {
    let our = CallNo::new(1).unwrap();
    let peer = CallNo::new(7).unwrap();
    let cap = CodecMask::from_u32(1 << 2);
    let now = Instant::now();
    prop_oneof![
        Just(SessionState::Init),
        Just(SessionState::NewSent(NewSentData {
            sent_at: now,
            our_call: our,
            attempts: 1,
            capabilities: cap,
            ping_seq: 0,
            dest: "s".into(),
        })),
        Just(SessionState::NewResent(NewResentData {
            sent_at: now,
            our_call: our,
            token: CallToken::new(vec![1, 2, 3]).unwrap(),
            attempts: 1,
            capabilities: cap,
            ping_seq: 0,
            dest: "s".into(),
        })),
        Just(SessionState::AuthRepSent(AuthRepSentData {
            sent_at: now,
            our_call: our,
            peer_call: peer,
            attempts: 1,
            challenge: b"c0ffee".to_vec(),
        })),
        Just(SessionState::Active(ActiveData {
            our_call: our,
            peer_call: peer,
            established_at: now,
            pending_dtmf: None,
            last_dtmf_at: None,
            keepalive: KeepaliveState::new(KeepaliveConfig::default(), now),
            last_full_voice: None,
            last_ptt: None,
            last_rx_voice_format: None,
            negotiated_format: VoiceFormat::G711U,
        })),
        Just(SessionState::Hangup(HangupData {
            our_call: our,
            peer_call: peer,
            initiated_by: HangupOrigin::Local,
            sent_at: now,
            attempts: 1,
        })),
        Just(SessionState::Closed),
        Just(SessionState::CallTokenReceived(CallTokenReceivedData {
            token: CallToken::new(vec![9, 9, 9]).unwrap(),
            received_at: now,
            our_call: our,
            capabilities: cap,
            ping_seq: 0,
            dest: "s".into(),
        })),
        Just(SessionState::AuthReqReceived(AuthReqReceivedData {
            challenge: b"c0ffee".to_vec(),
            methods: AuthMethods::MD5,
            our_call: our,
            peer_call: peer,
        })),
        Just(SessionState::Failed(FailReason::Aborted)),
        Just(SessionState::Failed(FailReason::RemoteHangup {
            cause: None,
        })),
        Just(SessionState::Failed(FailReason::RemoteHangup {
            cause: Some("Normal Clearing".into()),
        })),
        // Inbound (callee) handshake states (iax-8baf).
        Just(SessionState::NewReceived(NewReceivedData {
            peer_call: peer,
            our_call: our,
            offered: inbound_offer(),
            received_at: now,
        })),
        Just(SessionState::CallTokenIssued(CallTokenIssuedData {
            peer_call: peer,
            our_call: our,
            offered: inbound_offer(),
            token: CallToken::new(vec![0xAB; 16]).unwrap(),
            issued_at: now,
        })),
        Just(SessionState::AuthReqSent(AuthReqSentData {
            peer_call: peer,
            our_call: our,
            challenge: b"c0ffee".to_vec(),
            attempts: 1,
            sent_at: now,
            offered: inbound_offer(),
        })),
        Just(SessionState::AcceptSent(AcceptSentData {
            peer_call: peer,
            our_call: our,
            chosen_format: VoiceFormat::G711U,
            attempts: 1,
            sent_at: now,
            awaiting_app_decision: true,
        })),
        Just(SessionState::AnswerSent(AnswerSentData {
            peer_call: peer,
            our_call: our,
            chosen_format: VoiceFormat::G711U,
            attempts: 1,
            sent_at: now,
        })),
    ]
}

fn inbound_offer() -> IncomingOffer {
    IncomingOffer {
        called_number: Some("s".into()),
        calling_number: Some("1001".into()),
        calling_name: Some("Rob".into()),
        username: Some("u".into()),
        offered_codecs: CodecMask::from_u32(VoiceFormat::G711U.as_u32()),
        preferred_codec: Some(VoiceFormat::G711U),
        language: None,
        peer_calltoken: Some(Vec::new()),
    }
}

fn any_timer_kind() -> impl Strategy<Value = TimerKind> {
    prop_oneof![
        Just(TimerKind::NewRetry),
        Just(TimerKind::HangupRetry),
        Just(TimerKind::AuthRepRetry),
        Just(TimerKind::TokenExpiry),
        Just(TimerKind::Keepalive),
        Just(TimerKind::RegReqRetry),
        Just(TimerKind::RegAuthRetry),
        Just(TimerKind::RegTokenExpiry),
        Just(TimerKind::RegRefresh),
        Just(TimerKind::RegRelRetry),
        // Inbound (callee) timers (iax-8baf).
        Just(TimerKind::InboundTokenExpiry),
        Just(TimerKind::AuthReqRetry),
        Just(TimerKind::AcceptRetry),
        Just(TimerKind::AnswerRetry),
        Just(TimerKind::AcceptDecisionTimeout),
    ]
}

fn any_app_command() -> impl Strategy<Value = AppCommand> {
    let now = Instant::now();
    prop_oneof![
        Just(AppCommand::StartCall {
            dest: "x".into(),
            now
        }),
        Just(AppCommand::Hangup { cause: None, now }),
        Just(AppCommand::SendDtmf { digit: '1', now }),
        Just(AppCommand::SendPtt(true)),
        Just(AppCommand::SendText("hi".into())),
        Just(AppCommand::SendVoice {
            format: astar_iax_core::subclass::VoiceFormat::G711U,
            payload: vec![0xff; 16],
            ts: 0,
        }),
        // Inbound (callee) seams (iax-8baf).
        Just(AppCommand::DriveInbound { now }),
        Just(AppCommand::AcceptIncoming { now }),
        Just(AppCommand::AnswerIncoming { now }),
        Just(AppCommand::RejectIncoming { cause: None, now }),
        Just(AppCommand::AnswerAcked { now }),
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
    fn no_panic_on_app_or_timer_events(
        state in any_state(),
        cmd in any_app_command(),
    ) {
        let mut f = Fsm::with_state(state, creds(), CallNo::new(1).unwrap());
        let _ = f.handle(Event::App(cmd));
    }

    #[test]
    fn no_panic_on_timer_events(
        state in any_state(),
        timer in any_timer_kind(),
    ) {
        let mut f = Fsm::with_state(state, creds(), CallNo::new(1).unwrap());
        let _ = f.handle(Event::Timer { kind: timer, now: Instant::now() });
    }
}

fn dtmf_digit() -> impl Strategy<Value = char> {
    proptest::sample::select(vec![
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '*', '#', 'A', 'B', 'C', 'D',
    ])
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn active_send_dtmf_always_emits_two_reliable_frames(digit in dtmf_digit()) {
        let active = SessionState::Active(ActiveData {
            our_call: CallNo::new(1).unwrap(),
            peer_call: CallNo::new(7).unwrap(),
            established_at: Instant::now(),
            pending_dtmf: None,
            last_dtmf_at: None,
            keepalive: KeepaliveState::new(KeepaliveConfig::default(), Instant::now()),
            last_full_voice: None,
            last_ptt: None,
            last_rx_voice_format: None,
            negotiated_format: VoiceFormat::G711U,
        });
        let mut f = Fsm::with_state(active, creds(), CallNo::new(1).unwrap());
        let actions = f.handle(Event::App(AppCommand::SendDtmf {
            digit,
            now: Instant::now(),
        }));
        let reliables = actions
            .iter()
            .filter(|a| matches!(a, Action::SendReliable(_)))
            .count();
        prop_assert_eq!(reliables, 2, "BEGIN + END for digit {}", digit);
    }
}

#[test]
fn catch_all_emits_log_invalid() {
    let mut f = Fsm::with_state(SessionState::Closed, creds(), CallNo::new(1).unwrap());
    let actions = f.handle(Event::App(AppCommand::SendDtmf {
        digit: '5',
        now: Instant::now(),
    }));
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::LogInvalid { .. }))
    );
    assert!(matches!(f.state(), SessionState::Closed));
}
