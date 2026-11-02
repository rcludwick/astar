// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! `EchoLink` against loopback fakes. The first half is the directory
//! presence thread logging in, refreshing its status, fetching the station
//! list and backing off. The second half is a call: the UDP link connecting
//! to a station, keeping the contact alive, carrying audio and chat both
//! ways, hanging up, and ignoring anyone it did not call.
//!
//! Every server here is a `FakeDirectory` or a `FakeStation` on 127.0.0.1,
//! and every test build of this crate is loopback-only, so nothing in this
//! file can reach a real `EchoLink` server or station.
#![cfg(feature = "echolink")]

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use astar_audio::{
    AudioBackend, AudioRouter, CallAudio, MicId, NullBackend, OutputId, RxFrame, StreamConfig,
};
use astar_console::echolink::{
    CallLink, ChatLine, EchoLinkLogin, EchoLinkNetwork, EchoLinkPresence, EchoLinkSession,
    LoginState, TX_CAP_NOTICE,
};
use astar_echolink::directory::{
    StationEntry, StationKind, StationStatus, Status, login_line, logoff_line,
};
use astar_echolink::fake::{FakeDirectory, FakeLogin, FakeStation};
use astar_echolink::rtcp;
use astar_echolink::voice::{AudioPacket, MAX_TEXT_BYTES};
use astar_echolink::{PeerAddr, Timings};
use zeroize::Zeroizing;

const CALL: &str = "KX7ABC";
const PASSWORD: &str = "s3cret-pw";
const LOCATION: &str = "Denver";
/// The fake directory stamps an accepted connection on a 10 ms poll, so two
/// stamps can sit up to that much closer together than the connects were.
const ACCEPT_POLL_SLACK: Duration = Duration::from_millis(10);

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn timings() -> Timings {
    Timings {
        status_refresh: ms(200),
        list_refresh: ms(400),
        list_min_interval: ms(50),
        backoff_start: ms(50),
        backoff_cap: ms(200),
        ..Timings::default()
    }
}

fn login(servers: &[&FakeDirectory]) -> EchoLinkLogin {
    EchoLinkLogin {
        callsign: CALL.to_owned(),
        servers: servers.iter().map(|d| d.addr().to_string()).collect(),
        location: LOCATION.to_owned(),
    }
}

fn start(servers: &[&FakeDirectory], timings: Timings) -> EchoLinkPresence {
    EchoLinkPresence::start(login(servers), Zeroizing::new(PASSWORD.to_owned()), timings)
        .expect("presence starts")
}

fn two_stations() -> Vec<StationEntry> {
    vec![
        StationEntry {
            call: "*ECHOTEST*".to_owned(),
            description: "Test server".to_owned(),
            node: 9999,
            ip: Ipv4Addr::LOCALHOST,
            kind: StationKind::Conference,
            status: StationStatus::Online,
        },
        StationEntry {
            call: "KX7ABC-L".to_owned(),
            description: "Denver link".to_owned(),
            node: 123_456,
            ip: Ipv4Addr::LOCALHOST,
            kind: StationKind::Link,
            status: StationStatus::Busy,
        },
    ]
}

/// Poll `f` every 10 ms until it holds or `within` passes.
fn wait_for(within: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(ms(10));
    }
}

/// Replace the `(hh:mm)` group of a login line with `(##:##)`, returning the
/// masked line and the group's text.
fn mask_time(line: &[u8]) -> (Vec<u8>, String) {
    let open = line
        .iter()
        .position(|&b| b == b'(')
        .expect("a ( in the line");
    let close = open + 6;
    assert_eq!(line[close], b')', "a 5-character group: {line:?}");
    let group = String::from_utf8(line[open + 1..close].to_vec()).unwrap();
    let mut masked = line.to_vec();
    masked[open + 1..close].copy_from_slice(b"##:##");
    (masked, group)
}

fn is_login_line(line: &[u8]) -> bool {
    line.first() == Some(&b'l') && line.windows(8).any(|w| w == b"BUSY3.40")
}

#[test]
fn login_sends_the_exact_login_line_and_reaches_logged_in() {
    let dir = FakeDirectory::start(FakeLogin::Ok, Vec::new());
    let presence = start(&[&dir], timings());
    assert!(
        wait_for(Duration::from_secs(2), || presence.snapshot().state
            == LoginState::LoggedIn),
        "state {:?}",
        presence.snapshot()
    );
    let first = dir.lines().into_iter().next().expect("a login line");
    let (got, group) = mask_time(&first);
    let want = login_line(CALL, PASSWORD, Status::Busy, (0, 0), LOCATION);
    let (want, _) = mask_time(&want);
    assert_eq!(got, want);

    let (h, m) = group.split_once(':').expect("hh:mm");
    assert_eq!((h.len(), m.len()), (2, 2), "{group}");
    let sent = h.parse::<i32>().unwrap() * 60 + m.parse::<i32>().unwrap();
    assert!(sent < 24 * 60, "{group}");
    let now = jiff::Zoned::now();
    let local = i32::from(now.hour()) * 60 + i32::from(now.minute());
    let diff = (sent - local).rem_euclid(24 * 60);
    assert!(
        diff <= 1 || diff >= 24 * 60 - 1,
        "sent {group}, local {:02}:{:02}",
        now.hour(),
        now.minute()
    );
    presence.logout();
}

#[test]
fn station_list_arrives_after_login() {
    let dir = FakeDirectory::start(FakeLogin::Ok, two_stations());
    let presence = start(&[&dir], timings());
    assert!(wait_for(Duration::from_secs(2), || presence
        .stations()
        .len()
        == 2));
    assert_eq!(*presence.stations(), two_stations());
    assert_eq!(presence.find("9999").unwrap().call, "*ECHOTEST*");
    assert_eq!(presence.find("*echotest*").unwrap().node, 9999);
    assert_eq!(presence.find("kx7abc-l").unwrap().node, 123_456);
    assert!(presence.find("123").is_none());
    assert!(presence.find("W1AW").is_none());
    assert!(presence.snapshot().stations_age.is_some());
    presence.logout();
}

#[test]
fn status_is_refreshed_on_schedule() {
    let dir = FakeDirectory::start(FakeLogin::Ok, Vec::new());
    let presence = start(&[&dir], timings());
    thread::sleep(ms(500));
    let logins = dir.lines().iter().filter(|l| is_login_line(l)).count();
    assert!(logins >= 2, "{logins} login lines in 500 ms");
    presence.logout();
}

#[test]
fn wrong_password_says_so_in_plain_words() {
    let dir = FakeDirectory::start(FakeLogin::BadPassword, Vec::new());
    let presence = start(&[&dir], timings());
    assert!(wait_for(Duration::from_secs(2), || presence
        .snapshot()
        .state
        == LoginState::Failed));
    assert_eq!(
        presence.snapshot().error.as_deref(),
        Some("EchoLink said the password is wrong")
    );
    presence.logout();
}

#[test]
fn a_bad_password_is_not_retried() {
    let dir = FakeDirectory::start(FakeLogin::BadPassword, Vec::new());
    let presence = start(&[&dir], timings());
    thread::sleep(Duration::from_secs(1));
    assert_eq!(dir.exchanges(), 1);
    assert_eq!(presence.snapshot().state, LoginState::Failed);
    assert!(!presence.snapshot().retrying, "a refusal is final");
    assert!(
        !presence.refresh_list(),
        "a halted presence fetches nothing"
    );
    thread::sleep(ms(100));
    assert_eq!(dir.exchanges(), 1);
    presence.logout();
    assert_eq!(dir.exchanges(), 1, "no logoff after a failed login");
}

#[test]
fn a_refused_login_is_not_retried() {
    let dir = FakeDirectory::start(FakeLogin::Refuse("NOPE".to_owned()), Vec::new());
    let presence = start(&[&dir], timings());
    thread::sleep(Duration::from_secs(1));
    assert_eq!(dir.exchanges(), 1);
    assert_eq!(presence.snapshot().state, LoginState::Failed);
    assert!(!presence.snapshot().retrying, "a refusal is final");
    presence.logout();
}

#[test]
fn refused_login_carries_the_reply_and_the_validation_hint() {
    let dir = FakeDirectory::start(FakeLogin::Refuse("NOPE".to_owned()), Vec::new());
    let presence = start(&[&dir], timings());
    assert!(wait_for(Duration::from_secs(2), || presence
        .snapshot()
        .state
        == LoginState::Failed));
    let error = presence.snapshot().error.expect("an error");
    assert_eq!(
        error,
        "EchoLink refused the login (\"NOPE\"). If you registered recently, \
         your callsign may not be validated yet."
    );
    assert!(!error.contains(PASSWORD));
    presence.logout();
}

#[test]
fn unreachable_directory_backs_off_and_rotates() {
    let a = FakeDirectory::start(FakeLogin::Ok, Vec::new());
    let b = FakeDirectory::start(FakeLogin::Ok, Vec::new());
    a.set_refuse_connections(true);
    b.set_refuse_connections(true);
    let presence = start(&[&a, &b], timings());
    thread::sleep(Duration::from_millis(900));
    let snap = presence.snapshot();
    assert_eq!(snap.state, LoginState::Failed);
    assert_eq!(
        snap.error.as_deref(),
        Some("Couldn't reach any EchoLink server")
    );
    assert!(snap.retrying, "no server answered: the login keeps trying");
    presence.logout();

    // Merge both servers' accept times, tagged by server, in order.
    let mut all: Vec<(Instant, char)> = a
        .exchange_times()
        .into_iter()
        .map(|t| (t, 'a'))
        .chain(b.exchange_times().into_iter().map(|t| (t, 'b')))
        .collect();
    all.sort();
    assert!(all.len() >= 4, "{} exchanges", all.len());
    let order: String = all.iter().map(|(_, s)| *s).collect();
    assert!(
        order
            .chars()
            .zip(order.chars().skip(1))
            .all(|(x, y)| x != y),
        "servers alternate: {order}"
    );
    assert!(order.starts_with('a'), "{order}");
    // 50, 100, 200, 200 ms: the back-off in force before each retry.
    let backoffs = [50, 100, 200, 200, 200, 200];
    assert!(
        all.len() <= backoffs.len() + 1,
        "{} exchanges in 900 ms; the back-off allows at most {}: {order}",
        all.len(),
        backoffs.len() + 1
    );
    for (i, pair) in all.windows(2).enumerate() {
        let gap = pair[1].0 - pair[0].0;
        assert!(
            gap + ACCEPT_POLL_SLACK >= ms(backoffs[i]),
            "gap {i} is {gap:?}, back-off {} ms",
            backoffs[i]
        );
    }
}

#[test]
fn a_silent_directory_times_out_and_backs_off() {
    let dir = FakeDirectory::start(FakeLogin::Silent, Vec::new());
    let started = Instant::now();
    let presence = start(
        &[&dir],
        Timings {
            directory_read_timeout: ms(100),
            ..timings()
        },
    );
    assert!(wait_for(Duration::from_secs(1), || presence
        .snapshot()
        .state
        == LoginState::Failed));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(
        presence.snapshot().error.as_deref(),
        Some("Couldn't reach any EchoLink server")
    );
    let deadline = started + Duration::from_secs(1);
    thread::sleep(deadline.saturating_duration_since(Instant::now()));
    let times: Vec<Instant> = dir
        .exchange_times()
        .into_iter()
        .filter(|t| *t <= deadline)
        .collect();
    assert!(times.len() <= 5, "{} exchanges within 1 s", times.len());
    assert!(times.len() >= 2, "{} exchanges within 1 s", times.len());
    let backoffs = [50, 100, 200, 200];
    assert!(
        times.len() <= backoffs.len() + 1,
        "{} exchanges within 1 s; the back-off allows at most {}",
        times.len(),
        backoffs.len() + 1
    );
    for (i, pair) in times.windows(2).enumerate() {
        let gap = pair[1] - pair[0];
        let want = ms(100) + ms(backoffs[i]);
        assert!(
            gap + ACCEPT_POLL_SLACK >= want,
            "gap {i} is {gap:?}, want at least {want:?}"
        );
    }
    presence.logout();
}

#[test]
fn an_unreadable_reply_backs_off_instead_of_halting() {
    // Nothing printable in the reply, so the cleaned refusal text is empty:
    // that is a transport failure, not a final answer.
    let dir = FakeDirectory::start(FakeLogin::Refuse("\0\u{1}\u{7f}".to_owned()), Vec::new());
    let presence = start(&[&dir], timings());
    assert!(wait_for(Duration::from_secs(1), || dir.exchanges() >= 2));
    let snap = presence.snapshot();
    assert_eq!(snap.state, LoginState::Failed);
    assert_eq!(
        snap.error.as_deref(),
        Some("Couldn't reach any EchoLink server")
    );
    presence.logout();
}

#[test]
fn a_failed_exchange_after_login_stays_logged_in() {
    let dir = FakeDirectory::start(FakeLogin::Ok, two_stations());
    let presence = start(&[&dir], timings());
    assert!(wait_for(Duration::from_secs(2), || presence
        .stations()
        .len()
        == 2));
    assert_eq!(presence.snapshot().state, LoginState::LoggedIn);

    // The directory goes away just as a list is asked for.
    dir.set_refuse_connections(true);
    let before = dir.exchanges();
    thread::sleep(ms(60)); // past `list_min_interval`
    assert!(presence.refresh_list());
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while dir.exchanges() < before + 3 && Instant::now() < deadline {
        seen.push(presence.snapshot());
        thread::sleep(ms(5));
    }
    seen.push(presence.snapshot());
    assert!(
        dir.exchanges() >= before + 3,
        "three failed exchanges, saw {}",
        dir.exchanges() - before
    );
    assert!(
        seen.iter()
            .all(|s| s.state == LoginState::LoggedIn && s.error.is_none() && !s.retrying),
        "still logged in through the back-off: {:?}",
        seen.iter().map(|s| s.state).collect::<Vec<_>>()
    );
    presence.logout();
}

#[test]
fn logout_sends_logoff_once() {
    let dir = FakeDirectory::start(FakeLogin::Ok, two_stations());
    let presence = start(&[&dir], timings());
    assert!(wait_for(Duration::from_secs(2), || presence
        .stations()
        .len()
        == 2));
    presence.logout();
    let lines = dir.lines();
    let logoff = logoff_line(CALL, PASSWORD, LOCATION);
    let logoffs = lines.iter().filter(|l| l[..] == logoff[..]).count();
    assert_eq!(logoffs, 1);
    assert_eq!(lines.last().map(|l| &l[..]), Some(&logoff[..]));
    // Nothing more once the thread is gone.
    let after = dir.exchanges();
    thread::sleep(ms(300));
    assert_eq!(dir.exchanges(), after);
}

// ---- calls ------------------------------------------------------------------

/// The fake station's node number.
const NODE: u32 = 9999;
const ECHO_CALL: &str = "*ECHOTEST*";
/// Long enough for anything a test waits for; a pass ends far sooner.
const WAIT: Duration = Duration::from_secs(2);

fn call_timings() -> Timings {
    Timings {
        keepalive: ms(100),
        connection_timeout: ms(500),
        give_up_after: 5,
        talk_hold: ms(100),
        ..Timings::default()
    }
}

/// Loopback binds on ephemeral ports, and the fake's node mapped to the
/// fake's own two ports.
fn network(station: &FakeStation, match_by_ip: bool) -> EchoLinkNetwork {
    let addr = station.addr();
    EchoLinkNetwork {
        audio_bind: "127.0.0.1:0".parse().unwrap(),
        control_bind: "127.0.0.1:0".parse().unwrap(),
        peer_addr: Arc::new(move |e: &StationEntry| {
            if e.node == NODE {
                addr
            } else {
                PeerAddr::fixed(e.ip)
            }
        }),
        match_by_ip,
        timings: call_timings(),
    }
}

/// A real voice route over a `NullBackend`, as `tests/m17_session.rs` opens.
fn route(backend: Box<dyn AudioBackend>) -> (AudioRouter, CallAudio) {
    let mic = MicId::new(backend.default_input().expect("an input").id.as_str());
    let out = OutputId::new(backend.default_output().expect("an output").id.as_str());
    let mut router = AudioRouter::new(backend);
    let (audio, mic_tx, _mix) = router
        .open_monitor_call(&out, StreamConfig::default())
        .expect("bus");
    router
        .open_mic_lane(
            &mic,
            mic_tx,
            Arc::clone(&audio.preroll_lead),
            StreamConfig::default(),
        )
        .expect("mic");
    (router, audio)
}

/// Channel ends the test holds itself: it plays the mic (`Sender`) and
/// listens to what the link decodes (`Receiver`).
fn manual_audio() -> (CallAudio, Sender<Vec<i16>>, Receiver<RxFrame>) {
    let (mic, tx_frames) = mpsc::channel();
    let (rx_frames, speaker) = mpsc::channel();
    let audio = CallAudio {
        tx_frames,
        rx_frames,
        preroll_lead: Arc::new(AtomicU32::new(0)),
    };
    (audio, mic, speaker)
}

fn call(station: &FakeStation, match_by_ip: bool, audio: CallAudio) -> EchoLinkSession {
    EchoLinkSession::connect(
        station.entry(NODE),
        CALL,
        LOCATION,
        &network(station, match_by_ip),
        audio,
    )
    .expect("connect")
}

fn link(s: &EchoLinkSession) -> CallLink {
    s.state().link
}

fn wait_connected(s: &EchoLinkSession) {
    assert!(
        wait_for(WAIT, || link(s) == CallLink::Connected),
        "never connected: {:?}",
        s.state().link
    );
}

/// One cycle of a 1 kHz sine at 8 kHz, peak 8000.
const TONE_1KHZ: [i16; 8] = [0, 5657, 8000, 5657, 0, -5657, -8000, -5657];

/// 20 ms of a 1 kHz tone at 8 kHz, continuing from `phase`.
fn tone_frame(phase: &mut usize) -> Vec<i16> {
    (0..160)
        .map(|_| {
            let s = TONE_1KHZ[*phase % TONE_1KHZ.len()];
            *phase += 1;
            s
        })
        .collect()
}

fn loud(frame: &RxFrame) -> bool {
    frame.pcm.iter().any(|s| s.unsigned_abs() > 1000)
}

/// Key, push `frames` frames of tone, unkey, and wait for the unkey to land.
fn talk(s: &mut EchoLinkSession, mic: &Sender<Vec<i16>>, frames: usize) {
    s.set_ptt(true);
    assert!(wait_for(WAIT, || s.state().ptt), "PTT never applied");
    let mut phase = 0;
    for _ in 0..frames {
        mic.send(tone_frame(&mut phase)).unwrap();
    }
    s.set_ptt(false);
    assert!(wait_for(WAIT, || !s.state().ptt), "PTT never released");
}

#[test]
fn connect_completes_when_the_station_answers_sdes() {
    let station = FakeStation::start(ECHO_CALL);
    let (_router, audio) = route(Box::new(NullBackend::new()));
    let s = call(&station, false, audio);
    wait_connected(&s);
    let state = s.state();
    assert_eq!(state.peer, ECHO_CALL);
    assert_eq!(state.node, NODE);
    assert!(!state.ptt && !state.receiving);
    assert!(station.sdes_count() >= 1);
    s.disconnect();
}

#[test]
fn keepalive_sdes_repeats_while_connected() {
    let station = FakeStation::start(ECHO_CALL);
    let (_router, audio) = route(Box::new(NullBackend::new()));
    let started = Instant::now();
    let s = call(&station, false, audio);
    wait_connected(&s);
    thread::sleep((started + ms(350)).saturating_duration_since(Instant::now()));
    let n = station.sdes_count();
    assert!(n >= 3, "{n} SDES in 350 ms");
    assert_eq!(link(&s), CallLink::Connected);
    s.disconnect();
}

#[test]
fn our_info_text_reaches_the_station_once() {
    let station = FakeStation::start(ECHO_CALL);
    let (_router, audio) = route(Box::new(NullBackend::new()));
    let s = call(&station, false, audio);
    wait_connected(&s);
    assert!(wait_for(WAIT, || !station.infos().is_empty()));
    // Several keepalives later, still only the one.
    thread::sleep(ms(400));
    assert_eq!(station.infos(), vec![LOCATION.to_owned()]);
    s.disconnect();
}

#[test]
fn audio_round_trips_through_the_echo_station() {
    let station = FakeStation::start(ECHO_CALL);
    let (audio, mic, speaker) = manual_audio();
    let mut s = call(&station, false, audio);
    wait_connected(&s);
    s.set_ptt(true);
    assert!(wait_for(WAIT, || s.state().ptt));
    let mut phase = 0;
    for _ in 0..40 {
        mic.send(tone_frame(&mut phase)).unwrap();
        thread::sleep(ms(5));
    }
    s.set_ptt(false);
    assert!(wait_for(WAIT, || station.audio_packets() >= 10));

    let mut heard = Vec::new();
    let deadline = Instant::now() + WAIT;
    while heard.iter().filter(|f| loud(f)).count() < 30 && Instant::now() < deadline {
        if let Ok(f) = speaker.recv_timeout(ms(50)) {
            heard.push(f);
        }
    }
    assert!(heard.iter().all(|f| f.pcm.len() == 160));
    let loud_frames = heard.iter().filter(|f| loud(f)).count();
    assert!(
        loud_frames >= 30,
        "{loud_frames} loud frames of {}",
        heard.len()
    );
    assert_eq!(station.audio_packets(), 10, "40 frames are 10 packets");
    // The echo stops, so "far end talking" clears after talk_hold.
    assert!(wait_for(WAIT, || !s.state().receiving));
    s.disconnect();
}

#[test]
fn a_partial_packet_is_padded_on_key_up() {
    let station = FakeStation::start(ECHO_CALL);
    station.set_echo(false);
    let (audio, mic, _speaker) = manual_audio();
    let mut s = call(&station, false, audio);
    wait_connected(&s);
    talk(&mut s, &mic, 6);
    thread::sleep(ms(200));
    assert_eq!(station.audio_packets(), 2);
    s.disconnect();
}

#[test]
fn capture_queued_while_unkeyed_is_never_sent() {
    let station = FakeStation::start(ECHO_CALL);
    station.set_echo(false);
    let (audio, mic, _speaker) = manual_audio();
    let mut s = call(&station, false, audio);
    wait_connected(&s);
    let mut phase = 0;
    for _ in 0..20 {
        mic.send(tone_frame(&mut phase)).unwrap();
    }
    thread::sleep(ms(200));
    talk(&mut s, &mic, 4);
    thread::sleep(ms(200));
    assert_eq!(station.audio_packets(), 1);
    s.disconnect();
}

#[test]
fn chat_goes_out_and_replies_come_back() {
    let station = FakeStation::start(ECHO_CALL);
    let (_router, audio) = route(Box::new(NullBackend::new()));
    let s = call(&station, false, audio);
    wait_connected(&s);
    s.send_chat("hello");
    assert!(wait_for(WAIT, || station.chats() == vec!["hello".to_owned()]));
    station.send_chat("hi back");
    assert!(wait_for(WAIT, || s.state().chat.len() == 2));
    assert_eq!(
        s.state().chat,
        vec![
            ChatLine {
                from: CALL.to_owned(),
                text: "hello".to_owned(),
                mine: true
            },
            ChatLine {
                from: ECHO_CALL.to_owned(),
                text: "hi back".to_owned(),
                mine: false
            },
        ]
    );
    s.disconnect();
}

#[test]
fn hang_up_sends_bye() {
    let station = FakeStation::start(ECHO_CALL);
    let (_router, audio) = route(Box::new(NullBackend::new()));
    let s = call(&station, false, audio);
    wait_connected(&s);
    assert_eq!(station.bye_times(), [] as [std::time::Instant; 0]);
    s.disconnect();
    assert!(wait_for(WAIT, || station.bye_times().len() == 1));
    thread::sleep(ms(200));
    assert_eq!(station.bye_times().len(), 1);
}

#[test]
fn a_far_end_bye_closes_without_a_reply() {
    let station = FakeStation::start(ECHO_CALL);
    let (_router, audio) = route(Box::new(NullBackend::new()));
    let s = call(&station, false, audio);
    wait_connected(&s);
    let ours = s.local_addrs().expect("sockets open");
    station.send_bye();
    assert!(wait_for(WAIT, || link(&s) == CallLink::Closed));
    let after_close = station.controls_from(ours.control);
    // Past several keepalive periods: no BYE back, and no more SDES.
    thread::sleep(ms(300));
    assert_eq!(station.controls_from(ours.control), after_close);
    assert_eq!(station.bye_times(), [] as [std::time::Instant; 0]);
    s.disconnect();
    thread::sleep(ms(100));
    assert!(
        station.bye_times().is_empty(),
        "disconnect after a BYE sends none"
    );
}

#[test]
fn a_silent_station_times_out_with_bye() {
    let station = FakeStation::start(ECHO_CALL);
    let (_router, audio) = route(Box::new(NullBackend::new()));
    let s = call(&station, false, audio);
    wait_connected(&s);
    station.set_answer(false);
    let silent_from = Instant::now();
    assert!(wait_for(WAIT, || link(&s) == CallLink::Closed));
    let took = silent_from.elapsed();
    // The last answer was at most one keepalive before the silence began.
    assert!(took >= ms(350), "closed after {took:?}");
    assert!(wait_for(WAIT, || station.bye_times().len() == 1));
    s.disconnect();
}

#[test]
fn an_unanswered_connect_gives_up() {
    let station = FakeStation::start(ECHO_CALL);
    station.set_answer(false);
    let (_router, audio) = route(Box::new(NullBackend::new()));
    let s = call(&station, false, audio);
    assert_eq!(link(&s), CallLink::Connecting);
    assert!(wait_for(WAIT, || link(&s) == CallLink::Closed));
    assert!(wait_for(WAIT, || station.bye_times().len() == 1));
    assert_eq!(station.sdes_count(), 5);
    s.disconnect();
}

#[test]
fn an_uninvited_station_is_dropped_without_reply() {
    let station = FakeStation::start(ECHO_CALL);
    let (audio, _mic, speaker) = manual_audio();
    let s = call(&station, false, audio);
    wait_connected(&s);
    let ours = s.local_addrs().expect("sockets open");

    let stranger = FakeStation::start("W1XYZ");
    stranger.send_sdes_to(ours.control);
    stranger.send_audio(ours, 20);
    stranger.send_raw_audio(ours.audio, b"oNDATAW1XYZ>let me in\r\n");
    // A stranger's BYE does not end our call.
    stranger.send_raw_control(ours.control, &rtcp::bye());
    thread::sleep(ms(300));

    assert_eq!(stranger.controls_from(ours.control), 0);
    assert_eq!(stranger.audio_packets(), 0);
    assert!(stranger.chats().is_empty() && stranger.infos().is_empty());
    let loud_frames = speaker.try_iter().filter(loud).count();
    assert_eq!(loud_frames, 0, "the stranger's audio reached the speaker");
    let state = s.state();
    assert!(!state.receiving);
    assert_eq!(state.chat, [] as [astar_console::echolink::ChatLine; 0]);
    assert_eq!(state.link, CallLink::Connected);
    s.disconnect();
}

#[test]
fn local_addrs_are_loopback_while_connected_and_gone_after() {
    let station = FakeStation::start(ECHO_CALL);
    let (_router, audio) = route(Box::new(NullBackend::new()));
    let s = call(&station, false, audio);
    wait_connected(&s);
    let ours = s.local_addrs().expect("sockets open");
    for a in [ours.audio, ours.control] {
        assert_eq!(a.ip(), std::net::IpAddr::from(Ipv4Addr::LOCALHOST));
        assert_ne!(a.port(), 0);
    }
    assert_ne!(ours.audio, ours.control);
    station.send_bye();
    assert!(wait_for(WAIT, || s.local_addrs().is_none()));
    s.disconnect();
}

#[test]
fn by_ip_matching_accepts_a_reply_from_another_port() {
    let station = FakeStation::start(ECHO_CALL);
    station.set_answer_from_another_port(true);

    // Matched by full address, the reply from the other port is a stranger's.
    let (_router, audio) = route(Box::new(NullBackend::new()));
    let exact = call(&station, false, audio);
    thread::sleep(ms(300));
    assert_eq!(link(&exact), CallLink::Connecting);
    exact.disconnect();

    // Matched by IP, as production does, it is the station answering.
    let (_router, audio) = route(Box::new(NullBackend::new()));
    let by_ip = call(&station, true, audio);
    wait_connected(&by_ip);
    by_ip.disconnect();
}

#[test]
fn a_short_or_hostile_datagram_does_not_drop_the_link() {
    let station = FakeStation::start(ECHO_CALL);
    let (_router, audio) = route(Box::new(NullBackend::new()));
    let s = call(&station, false, audio);
    wait_connected(&s);
    let ours = s.local_addrs().expect("sockets open");

    let mut short_audio = vec![0xC0, 0x03];
    short_audio.resize(100, 0);
    station.send_raw_audio(ours.audio, &short_audio);
    let mut bad_info = b"oNDATA\r".to_vec();
    bad_info.extend_from_slice(&[0xFF; 2000]);
    station.send_raw_audio(ours.audio, &bad_info);
    let mut long_chat = b"oNDATA\xFF\xFE>".to_vec();
    long_chat.extend(std::iter::repeat_n(b'a', 8000));
    station.send_raw_audio(ours.audio, &long_chat);
    station.send_raw_audio(ours.audio, b"");
    // An RTCP header whose length runs far past the datagram.
    station.send_raw_control(ours.control, &[0xC0, 202, 0xFF, 0xFF, 0, 0, 0, 0]);
    station.send_raw_control(ours.control, &[0x00; 3]);
    station.send_raw_control(ours.control, b"");
    thread::sleep(ms(300));

    let state = s.state();
    assert_eq!(state.link, CallLink::Connected);
    let info = state.info.expect("the lossy info text is kept");
    assert!(info.len() <= MAX_TEXT_BYTES && info.contains('\u{FFFD}'));
    assert_eq!(state.chat.len(), 1);
    assert_eq!(state.chat[0].text.len(), MAX_TEXT_BYTES);
    // And the link still carries chat.
    s.send_chat("still here");
    assert!(wait_for(WAIT, || station.chats() == vec!["still here".to_owned()]));
    s.disconnect();
}

#[test]
fn hanging_up_while_keyed_sends_the_last_partial_packet() {
    let station = FakeStation::start(ECHO_CALL);
    station.set_echo(false);
    let (audio, mic, _speaker) = manual_audio();
    let mut s = call(&station, false, audio);
    wait_connected(&s);
    s.set_ptt(true);
    assert!(wait_for(WAIT, || s.state().ptt));
    let mut phase = 0;
    for _ in 0..6 {
        mic.send(tone_frame(&mut phase)).unwrap();
    }
    // Still keyed: one full packet out, two frames waiting.
    s.disconnect();
    assert!(wait_for(WAIT, || station.bye_times().len() == 1));
    thread::sleep(ms(100));
    assert_eq!(station.audio_packets(), 2);
}

#[test]
fn old_packets_are_dropped_until_the_far_end_stops_talking() {
    let station = FakeStation::start(ECHO_CALL);
    let (audio, _mic, speaker) = manual_audio();
    let s = call(&station, false, audio);
    wait_connected(&s);
    let ours = s.local_addrs().expect("sockets open");
    let packet = |seq: u16| {
        AudioPacket {
            seq,
            frames: [[0xD0; 33]; 4],
        }
        .encode()
    };
    // Every frame that reaches the speaker within `within`.
    let played = |within: Duration| {
        let deadline = Instant::now() + within;
        let mut n = 0;
        while let Ok(_frame) =
            speaker.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            n += 1;
        }
        n
    };

    // One transmission: every check here sits well inside talk_hold (100 ms),
    // and each arriving packet, dropped or not, holds the talk open.
    station.send_raw_audio(ours.audio, &packet(100));
    assert!(
        wait_for(WAIT, || s.state().receiving),
        "packet 100 never arrived"
    );
    station.send_raw_audio(ours.audio, &packet(100));
    station.send_raw_audio(ours.audio, &packet(99));
    assert_eq!(
        played(ms(40)),
        4,
        "packet 100 plays; its repeat and the older 99 are dropped"
    );

    assert!(
        wait_for(WAIT, || !s.state().receiving),
        "talk never stopped"
    );
    station.send_raw_audio(ours.audio, &packet(5));
    assert_eq!(played(ms(150)), 4, "a new transmission may start lower");
    s.disconnect();
}

// ---- the transmit cap -------------------------------------------------------

/// Keep the key down and the mic running until PTT drops or `within` runs
/// out; returns how long the key stayed applied.
fn hold_key(s: &mut EchoLinkSession, mic: &Sender<Vec<i16>>, within: Duration) -> Duration {
    s.set_ptt(true);
    assert!(wait_for(WAIT, || s.state().ptt), "PTT never applied");
    let start = Instant::now();
    let mut phase = 0;
    while s.state().ptt && start.elapsed() < within {
        mic.send(tone_frame(&mut phase)).unwrap();
        thread::sleep(ms(20));
    }
    start.elapsed()
}

#[test]
fn a_held_key_stops_transmitting_at_the_cap() {
    let station = FakeStation::start(ECHO_CALL);
    station.set_echo(false);
    let (audio, mic, _speaker) = manual_audio();
    let mut net = network(&station, false);
    net.timings.max_transmit = ms(300);
    let mut s = EchoLinkSession::connect(station.entry(NODE), CALL, LOCATION, &net, audio)
        .expect("connect");
    wait_connected(&s);

    let held = hold_key(&mut s, &mic, WAIT);
    assert!(!s.state().ptt, "the cap never unkeyed after {held:?}");
    assert!(held >= ms(250), "unkeyed early, after {held:?}");
    assert_eq!(s.state().notice.as_deref(), Some(TX_CAP_NOTICE));

    // The key is still down, but nothing more goes out.
    thread::sleep(ms(150));
    let sent = station.audio_packets();
    let mut phase = 0;
    for _ in 0..20 {
        mic.send(tone_frame(&mut phase)).unwrap();
        thread::sleep(ms(10));
    }
    thread::sleep(ms(150));
    assert_eq!(station.audio_packets(), sent, "audio went out past the cap");
    assert!(!s.state().ptt);

    // Releasing re-arms: the next over goes out, and the sentence clears.
    s.set_ptt(false);
    thread::sleep(ms(150));
    talk(&mut s, &mic, 8);
    assert!(wait_for(WAIT, || station.audio_packets() >= sent + 2));
    assert_eq!(s.state().notice, None);
    s.disconnect();
}

#[test]
fn the_default_cap_is_ten_minutes_and_a_short_over_carries_no_notice() {
    assert_eq!(Timings::default().max_transmit, Duration::from_secs(600));
    let station = FakeStation::start(ECHO_CALL);
    station.set_echo(false);
    let (audio, mic, _speaker) = manual_audio();
    let mut s = call(&station, false, audio);
    wait_connected(&s);
    talk(&mut s, &mic, 8);
    assert!(wait_for(WAIT, || station.audio_packets() >= 2));
    assert_eq!(s.state().notice, None);
    s.disconnect();
}
