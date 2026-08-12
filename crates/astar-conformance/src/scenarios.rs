// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Scenario library — each function drives a complete IAX2 conversation
//! against a peer, then returns. The harness binary
//! (`examples/harness.rs`) dispatches by name; the orchestrator script
//! captures the on-wire traffic of each run.

use crate::driver::{DriverError, Session};

pub fn dispatch(name: &str, session: &mut Session) -> Result<(), DriverError> {
    match name {
        "register" => register(session),
        "call_notoken" => call_notoken(session),
        "call_token" => call_token(session),
        "call_ulaw" => call_ulaw(session),
        "peer_hangup" => peer_hangup(session),
        "call_dtmf" => call_dtmf(session),
        _ => Err(DriverError::FsmRejected {
            state: "n/a".into(),
            reason: "unknown scenario",
        }),
    }
}

pub fn register(s: &mut Session) -> Result<(), DriverError> {
    use astar_iax_core::Subclass;
    use astar_iax_core::frame::{Frame, FullFrame};
    use astar_iax_core::ie::Ies;
    use astar_iax_core::session::auth::md5_response;
    use astar_iax_core::subclass::{FrameType, IaxCommand};
    use std::time::Duration;

    let username = s.credentials().username.clone();

    // 1. REGREQ
    let regreq = Frame::Full(Box::new(FullFrame {
        source_call: 1,
        dest_call: 0,
        retransmission: false,
        timestamp: 0,
        oseqno: 0,
        iseqno: 0,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::RegReq),
        ies: Ies {
            username: Some(&username),
            ..Ies::empty()
        },
        payload: &[],
    }));
    s.send_raw_frame(&regreq).map_err(DriverError::Io)?;

    // 2. Wait for REGAUTH.
    let regauth = s.recv_one_frame(Duration::from_secs(3))?;
    if !matches!(regauth.subclass, Subclass::Iax(IaxCommand::RegAuth)) {
        return Err(DriverError::FsmRejected {
            state: "register".into(),
            reason: "expected REGAUTH",
        });
    }
    let auth_ies = Ies::parse(regauth.ie_bytes()).map_err(|_| DriverError::FsmRejected {
        state: "register".into(),
        reason: "REGAUTH IE parse failed",
    })?;
    let challenge = auth_ies
        .challenge
        .ok_or(DriverError::FsmRejected {
            state: "register".into(),
            reason: "REGAUTH missing challenge",
        })?
        .to_string();
    let peer_call = regauth.source_call;

    // 3. REGREQ #2 with MD5_RESULT.
    let md5 = md5_response(&challenge, s.credentials().password.expose());
    let regreq2 = Frame::Full(Box::new(FullFrame {
        source_call: 1,
        dest_call: peer_call,
        retransmission: false,
        timestamp: 0,
        oseqno: 1,
        iseqno: 1,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::RegReq),
        ies: Ies {
            username: Some(&username),
            md5_result: Some(&md5),
            ..Ies::empty()
        },
        payload: &[],
    }));
    s.send_raw_frame(&regreq2).map_err(DriverError::Io)?;

    // 4. Wait for REGACK.
    let regack = s.recv_one_frame(Duration::from_secs(3))?;
    if !matches!(regack.subclass, Subclass::Iax(IaxCommand::RegAck)) {
        return Err(DriverError::FsmRejected {
            state: "register".into(),
            reason: "expected REGACK",
        });
    }

    // 5. REGREL to unregister cleanly so the capture shows a full lifecycle.
    let release = Frame::Full(Box::new(FullFrame {
        source_call: 1,
        dest_call: peer_call,
        retransmission: false,
        timestamp: 0,
        oseqno: 2,
        iseqno: 2,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::RegRel),
        ies: Ies {
            username: Some(&username),
            md5_result: Some(&md5),
            ..Ies::empty()
        },
        payload: &[],
    }));
    s.send_raw_frame(&release).map_err(DriverError::Io)?;
    let _final = s.recv_one_frame(Duration::from_secs(3));

    Ok(())
}
pub fn call_notoken(s: &mut Session) -> Result<(), DriverError> {
    use astar_iax_core::session::fsm::{AppCommand, SessionState};
    use std::time::{Duration, Instant};

    s.send_app_command(AppCommand::StartCall {
        dest: "s".into(),
        now: Instant::now(),
    });
    s.run_event_loop_until(
        |fsm| matches!(fsm.state(), SessionState::Active(_)),
        Duration::from_secs(5),
    )?;

    // Hold the call briefly so the capture shows the steady state.
    std::thread::sleep(Duration::from_millis(500));

    s.send_app_command(AppCommand::Hangup {
        cause: None,
        now: Instant::now(),
    });
    s.run_event_loop_until(
        |fsm| matches!(fsm.state(), SessionState::Closed),
        Duration::from_secs(3),
    )?;
    Ok(())
}
pub fn call_token(s: &mut Session) -> Result<(), DriverError> {
    // The FSM transparently handles CALLTOKEN when the server requires it
    // (with the seqno-reset shortcut in the driver). Behavior identical to
    // call_notoken; only the peer credentials differ.
    call_notoken(s)
}
pub fn call_ulaw(s: &mut Session) -> Result<(), DriverError> {
    use astar_iax_core::session::fsm::{AppCommand, SessionState};
    use astar_iax_core::subclass::VoiceFormat;
    use std::time::{Duration, Instant};

    s.send_app_command(AppCommand::StartCall {
        dest: "s".into(),
        now: Instant::now(),
    });
    s.run_event_loop_until(
        |fsm| matches!(fsm.state(), SessionState::Active(_)),
        Duration::from_secs(5),
    )?;

    // 100 frames of 160-byte ulaw silence (0xff = ulaw 0).
    let payload = vec![0xffu8; 160];
    for i in 0..100u32 {
        s.send_app_command(AppCommand::SendVoice {
            format: VoiceFormat::G711U,
            payload: payload.clone(),
            ts: i * 20,
        });
        // Pump the event loop briefly so retransmits / inbound mini-frames
        // get processed in real time.
        let _ = s.run_event_loop_until(|_| false, Duration::from_millis(20));
    }

    s.send_app_command(AppCommand::Hangup {
        cause: None,
        now: Instant::now(),
    });
    s.run_event_loop_until(
        |fsm| matches!(fsm.state(), SessionState::Closed),
        Duration::from_secs(3),
    )?;
    Ok(())
}
pub fn call_dtmf(s: &mut Session) -> Result<(), DriverError> {
    use astar_iax_core::session::fsm::{AppCommand, SessionState};
    use std::time::{Duration, Instant};

    s.send_app_command(AppCommand::StartCall {
        dest: "s".into(),
        now: Instant::now(),
    });
    s.run_event_loop_until(
        |fsm| matches!(fsm.state(), SessionState::Active(_)),
        Duration::from_secs(5),
    )?;

    // Send *70 — the standard AllStar "local connection status" macro.
    // Three digits; the 100 ms BEGIN→END hold + 120 ms loop pump leaves
    // ample room under the 50 ms rate limit.
    for digit in ['*', '7', '0'] {
        s.send_app_command(AppCommand::SendDtmf {
            digit,
            now: Instant::now(),
        });
        let _ = s.run_event_loop_until(|_| false, Duration::from_millis(120));
    }

    // Hold briefly so any peer reply (e.g. a TEXT frame from app_rpt)
    // lands in the wire capture.
    let _ = s.run_event_loop_until(|_| false, Duration::from_millis(500));

    s.send_app_command(AppCommand::Hangup {
        cause: None,
        now: Instant::now(),
    });
    s.run_event_loop_until(
        |fsm| matches!(fsm.state(), SessionState::Closed),
        Duration::from_secs(3),
    )?;
    Ok(())
}
pub fn peer_hangup(s: &mut Session) -> Result<(), DriverError> {
    use astar_iax_core::session::fsm::{AppCommand, HangupData, HangupOrigin, SessionState};
    use std::time::{Duration, Instant};

    s.send_app_command(AppCommand::StartCall {
        dest: "bye".into(),
        now: Instant::now(),
    });

    // Asterisk routes [bye] → astar-bye → Answer + immediate Hangup.
    // The driver does not currently pump FSM timers, so we treat
    // reaching Hangup{Peer} as the scenario's success state — the
    // capture already shows the full setup + peer-initiated tear-down.
    // Closed-via-timer-timeout is tracked separately.
    s.run_event_loop_until(
        |fsm| {
            matches!(
                fsm.state(),
                SessionState::Hangup(HangupData {
                    initiated_by: HangupOrigin::Peer,
                    ..
                }) | SessionState::Closed
            )
        },
        Duration::from_secs(5),
    )?;
    Ok(())
}
