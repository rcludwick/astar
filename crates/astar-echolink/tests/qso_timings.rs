// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The connection state machine against `EchoLib`'s timings, with fake clocks.

use std::time::{Duration, Instant};

use astar_echolink::Timings;
use astar_echolink::qso::{CloseReason, Qso, QsoAction, QsoEvent, QsoState};
use astar_echolink::rtcp;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}
fn s(n: u64) -> Duration {
    Duration::from_secs(n)
}
fn is_sdes(a: &QsoAction) -> bool {
    matches!(a, QsoAction::SendControl(b) if matches!(rtcp::parse(b), Ok(rtcp::Rtcp::Sdes{..})))
}
fn is_bye(a: &QsoAction) -> bool {
    matches!(a, QsoAction::SendControl(b) if *b == rtcp::bye().to_vec())
}
fn far_sdes() -> Vec<u8> {
    rtcp::sdes("W1AW", "Hiram")
}

fn connected(t0: Instant) -> Qso {
    let (mut q, _) = Qso::start("N0CALL", Timings::default(), t0);
    q.on_control(&far_sdes(), t0);
    assert_eq!(q.state(), &QsoState::Connected);
    q
}

#[test]
fn connect_sends_sdes_at_once_and_every_10_s() {
    let t0 = Instant::now();
    let (mut q, first) = Qso::start("N0CALL", Timings::default(), t0);
    assert_eq!(first.len(), 1);
    assert!(is_sdes(&first[0]));
    assert_eq!(
        q.tick(t0 + ms(9_999)),
        [] as [astar_echolink::qso::QsoAction; 0]
    );
    let a = q.tick(t0 + s(10));
    assert_eq!(a.len(), 1);
    assert!(is_sdes(&a[0]));
    assert_eq!(
        q.tick(t0 + s(19)),
        [] as [astar_echolink::qso::QsoAction; 0]
    );
    assert!(is_sdes(&q.tick(t0 + s(20))[0]));
}

#[test]
fn connect_gives_up_after_five_unanswered_sdes() {
    let t0 = Instant::now();
    let (mut q, first) = Qso::start("N0CALL", Timings::default(), t0);
    let mut sdes = usize::from(is_sdes(&first[0]));
    for t in [10, 20, 30, 40] {
        let a = q.tick(t0 + s(t));
        sdes += a.iter().filter(|x| is_sdes(x)).count();
    }
    assert_eq!(sdes, 5);
    let a = q.tick(t0 + s(50));
    assert_eq!(a.len(), 2);
    assert!(is_bye(&a[0]));
    assert_eq!(
        a[1],
        QsoAction::Event(QsoEvent::Closed(CloseReason::NoAnswer))
    );
    assert_eq!(q.state(), &QsoState::Closed(CloseReason::NoAnswer));
}

#[test]
fn an_sdes_reply_connects_and_names_the_far_callsign() {
    let t0 = Instant::now();
    let (mut q, _) = Qso::start("N0CALL", Timings::default(), t0);
    let a = q.on_control(&far_sdes(), t0 + s(1));
    assert_eq!(
        a,
        vec![QsoAction::Event(QsoEvent::Connected {
            callsign: "W1AW".into()
        })]
    );
    assert_eq!(q.state(), &QsoState::Connected);
}

#[test]
fn connected_keepalive_every_10_s() {
    let t0 = Instant::now();
    let mut q = connected(t0);
    assert_eq!(q.tick(t0 + s(9)), [] as [astar_echolink::qso::QsoAction; 0]);
    assert!(is_sdes(&q.tick(t0 + s(10))[0]));
    q.on_control(&far_sdes(), t0 + s(12));
    assert_eq!(
        q.tick(t0 + s(19)),
        [] as [astar_echolink::qso::QsoAction; 0]
    );
    assert!(is_sdes(&q.tick(t0 + s(20))[0]));
}

#[test]
fn fifty_s_without_sdes_times_out_with_bye() {
    let t0 = Instant::now();
    let mut q = connected(t0);
    let last = t0 + s(5);
    q.on_control(&far_sdes(), last);
    // Our own keepalives continue; the far end is silent.
    let mut now = last;
    while now + s(10) < last + ms(49_900) {
        now += s(10);
        let a = q.tick(now);
        assert!(!a.iter().any(is_bye));
    }
    let a = q.tick(last + ms(49_900));
    assert!(!a.iter().any(is_bye));
    assert_eq!(q.state(), &QsoState::Connected);
    let a = q.tick(last + s(50));
    assert!(a.iter().any(is_bye));
    assert!(a.contains(&QsoAction::Event(QsoEvent::Closed(CloseReason::TimedOut))));
    assert_eq!(q.state(), &QsoState::Closed(CloseReason::TimedOut));
}

#[test]
fn a_received_bye_closes_without_sending_bye() {
    let t0 = Instant::now();
    let mut q = connected(t0);
    let a = q.on_control(&rtcp::bye(), t0 + s(1));
    assert_eq!(
        a,
        vec![QsoAction::Event(QsoEvent::Closed(CloseReason::ByeReceived))]
    );
    assert!(!a.iter().any(|x| matches!(x, QsoAction::SendControl(_))));
}

#[test]
fn hang_up_sends_bye() {
    let t0 = Instant::now();
    let mut q = connected(t0);
    let a = q.hang_up();
    assert_eq!(a.len(), 2);
    assert!(is_bye(&a[0]));
    assert_eq!(
        a[1],
        QsoAction::Event(QsoEvent::Closed(CloseReason::Hangup))
    );
}

#[test]
fn talk_starts_on_audio_and_stops_320_ms_after_the_last_packet() {
    let t0 = Instant::now();
    let mut q = connected(t0);
    assert!(!q.talking());
    assert_eq!(
        q.on_audio(t0 + ms(100)),
        vec![QsoAction::Event(QsoEvent::TalkStart)]
    );
    assert!(q.talking());
    assert_eq!(
        q.on_audio(t0 + ms(120)),
        [] as [astar_echolink::qso::QsoAction; 0]
    );
    assert_eq!(
        q.tick(t0 + ms(439)),
        [] as [astar_echolink::qso::QsoAction; 0]
    );
    assert!(q.talking());
    assert_eq!(
        q.tick(t0 + ms(440)),
        vec![QsoAction::Event(QsoEvent::TalkStop)]
    );
    assert!(!q.talking());
}

#[test]
fn closed_qso_ignores_everything() {
    let t0 = Instant::now();
    let mut q = connected(t0);
    q.hang_up();
    assert_eq!(
        q.on_control(&far_sdes(), t0),
        [] as [astar_echolink::qso::QsoAction; 0]
    );
    assert_eq!(
        q.on_control(&rtcp::bye(), t0),
        [] as [astar_echolink::qso::QsoAction; 0]
    );
    assert_eq!(q.on_audio(t0), [] as [astar_echolink::qso::QsoAction; 0]);
    assert_eq!(
        q.tick(t0 + s(100)),
        [] as [astar_echolink::qso::QsoAction; 0]
    );
    assert_eq!(q.hang_up(), [] as [astar_echolink::qso::QsoAction; 0]);
    assert_eq!(q.state(), &QsoState::Closed(CloseReason::Hangup));
}

#[test]
fn our_sdes_name_is_the_callsign_padded_to_15_and_nothing_else() {
    let t0 = Instant::now();
    let (_q, first) = Qso::start("N0CALL", Timings::default(), t0);
    let QsoAction::SendControl(b) = &first[0] else {
        panic!("first action is not a control datagram");
    };
    // 8 bytes of empty RR, then the 8-byte SDES header, then items.
    let mut i = 16;
    let mut name = None;
    while i + 2 <= b.len() && b[i] != 0 {
        let len = usize::from(b[i + 1]);
        if b[i] == 2 {
            name = Some(b[i + 2..i + 2 + len].to_vec());
        }
        i += 2 + len;
    }
    assert_eq!(name.as_deref(), Some(&b"N0CALL         "[..]));
}
