// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! `Station`'s `EchoLink` surface against loopback fakes: logging in, calling
//! a station, keying and chatting, hanging up and logging off, all through
//! the facade.
//!
//! Every server here is a `FakeDirectory` or a `FakeStation` on 127.0.0.1,
//! and every test build of this crate is loopback-only, so nothing in this
//! file can reach a real `EchoLink` server or station.
#![cfg(feature = "echolink")]

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use astar_console::echolink::{
    CallLink, EchoLinkNetwork, LoginState, PeerAddr, StationEntry, Timings,
};
use astar_echolink::directory::logoff_line;
use astar_echolink::fake::{FakeDirectory, FakeLogin, FakeStation};
use astar_station::{Station, StationConfig, StationError};

const CALL: &str = "KX7ABC";
const PASSWORD: &str = "s3cret-pw";
const NODE: u32 = 9999;
const ECHO_CALL: &str = "*ECHOTEST*";
const WAIT: Duration = Duration::from_secs(3);

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn wait_for(mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + WAIT;
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

/// Loopback binds on ephemeral ports, and the fake's node mapped to the
/// fake's own two ports.
fn network(station: &FakeStation) -> EchoLinkNetwork {
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
        match_by_ip: false,
        timings: Timings {
            keepalive: ms(100),
            connection_timeout: ms(2_000),
            give_up_after: 20,
            talk_hold: ms(100),
            ..Timings::default()
        },
    }
}

struct Rig {
    fake: FakeStation,
    dir: FakeDirectory,
    station: Station,
}

fn rig(reply: FakeLogin) -> Rig {
    let fake = FakeStation::start(ECHO_CALL);
    let dir = FakeDirectory::start(reply, vec![fake.entry(NODE)]);
    let station = Station::with_backend_factory(
        StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    station.set_echolink_network(network(&fake));
    Rig { fake, dir, station }
}

fn servers(dir: &FakeDirectory) -> Vec<String> {
    vec![dir.addr().to_string()]
}

fn login_state(station: &Station) -> Option<LoginState> {
    station.echolink_state().map(|s| s.login.state)
}

fn call_link(station: &Station) -> Option<CallLink> {
    station
        .echolink_state()
        .and_then(|s| s.call)
        .map(|c| c.link)
}

#[test]
fn login_connect_talk_and_disconnect_through_the_facade() {
    let r = rig(FakeLogin::Ok);
    assert!(r.station.echolink_available());
    assert!(r.station.echolink_state().is_none());

    r.station.echolink_set_location("Denver");
    r.station
        .echolink_login(CALL, PASSWORD.to_owned(), &servers(&r.dir))
        .expect("login starts");
    assert!(
        wait_for(|| login_state(&r.station) == Some(LoginState::LoggedIn)
            && !r.station.echolink_stations().is_empty()),
        "never logged in: {:?}",
        r.station.echolink_state()
    );
    assert!(r.station.snapshot().echolink_logged_in);
    let listed = r.station.echolink_stations();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].call, ECHO_CALL);
    assert_eq!(listed[0].node, NODE);
    assert_eq!(listed[0].kind, "conference");
    assert_eq!(listed[0].status, "online");

    r.station.echolink_connect("9999").expect("connect");
    assert!(
        wait_for(|| call_link(&r.station) == Some(CallLink::Connected)),
        "never connected: {:?}",
        call_link(&r.station)
    );
    assert!(r.station.snapshot().echolink_active);

    // Talk: the key reaches the link, and a chat line reaches the station.
    r.station.set_ptt(true).expect("key");
    assert!(wait_for(|| r
        .station
        .echolink_state()
        .and_then(|s| s.call)
        .is_some_and(|c| c.ptt)));
    r.station.set_ptt(false).expect("unkey");
    r.station.echolink_send_chat("hello").expect("chat");
    assert!(
        wait_for(|| r.fake.chats().iter().any(|c| c.contains("hello"))),
        "{:?}",
        r.fake.chats()
    );

    r.station.echolink_disconnect();
    assert_eq!(call_link(&r.station), None);
    assert!(!r.station.snapshot().echolink_active);
    assert_eq!(login_state(&r.station), Some(LoginState::LoggedIn));
    assert!(matches!(
        r.station.echolink_send_chat("again"),
        Err(StationError::NotConnected)
    ));

    r.station.echolink_logout();
    assert!(r.station.echolink_state().is_none());
    assert!(!r.station.snapshot().echolink_logged_in);
    assert_eq!(
        r.station.echolink_stations(),
        [] as [astar_station::EchoLinkStationInfo; 0]
    );
    assert_eq!(r.station.echolink_last_error(), None);
}

#[test]
fn last_error_is_the_plain_sentence() {
    let r = rig(FakeLogin::BadPassword);
    r.station
        .echolink_login(CALL, PASSWORD.to_owned(), &servers(&r.dir))
        .expect("login starts");
    assert!(
        wait_for(|| login_state(&r.station) == Some(LoginState::Failed)),
        "{:?}",
        r.station.echolink_state()
    );
    // A failed login is still a state, carrying its error, so a front-end
    // can show it.
    let state = r.station.echolink_state().expect("a failed login is Some");
    assert_eq!(
        state.login.error.as_deref(),
        Some("EchoLink said the password is wrong")
    );
    assert_eq!(
        r.station.echolink_last_error().as_deref(),
        Some("EchoLink said the password is wrong")
    );
    match r.station.echolink_connect("9999") {
        Err(StationError::EchoLink(m)) => assert_eq!(m, "Log in to EchoLink first"),
        other => panic!("{other:?}"),
    }
    r.station.echolink_logout();
    assert!(r.station.echolink_state().is_none());
}

#[test]
fn stations_carry_no_ip_address() {
    let r = rig(FakeLogin::Ok);
    r.station
        .echolink_login(CALL, PASSWORD.to_owned(), &servers(&r.dir))
        .expect("login starts");
    assert!(wait_for(|| !r.station.echolink_stations().is_empty()));
    let stations = r.station.echolink_stations();
    let printed = format!("{stations:?}");
    assert!(printed.contains(ECHO_CALL), "{printed}");
    assert!(!printed.contains("127.0.0.1"), "{printed}");
    r.station.echolink_logout();
}

#[test]
fn empty_arguments_are_refused_before_anything_starts() {
    let r = rig(FakeLogin::Ok);
    let good = servers(&r.dir);
    let refusals = [
        r.station.echolink_login("", PASSWORD.to_owned(), &good),
        r.station.echolink_login("  ", PASSWORD.to_owned(), &good),
        r.station.echolink_login(CALL, String::new(), &good),
        r.station.echolink_login(CALL, PASSWORD.to_owned(), &[]),
        r.station
            .echolink_login(CALL, PASSWORD.to_owned(), &[" ".to_owned()]),
        r.station.echolink_connect(""),
        r.station.echolink_node_connect(" "),
    ];
    for refused in refusals {
        assert!(
            matches!(refused, Err(StationError::EchoLink(_))),
            "{refused:?}"
        );
    }
    // Nothing started: no login thread, so no directory exchange.
    assert!(r.station.echolink_state().is_none());
    thread::sleep(ms(100));
    assert_eq!(r.dir.exchanges(), 0);
}

#[test]
fn the_node_leg_needs_a_conference() {
    let r = rig(FakeLogin::Ok);
    r.station
        .echolink_login(CALL, PASSWORD.to_owned(), &servers(&r.dir))
        .expect("login starts");
    assert!(wait_for(|| !r.station.echolink_stations().is_empty()));
    match r.station.echolink_node_connect("9999") {
        Err(StationError::EchoLink(m)) => assert!(m.starts_with("no conference"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        r.station.echolink_node_peers(),
        [] as [astar_station::EchoLinkPeerStatus; 0]
    );
    assert_eq!(
        r.station.echolink_node_poll(),
        [] as [astar_station::EchoLinkNodeEdge; 0]
    );
    assert!(matches!(
        r.station.echolink_node_disconnect(ECHO_CALL),
        Err(StationError::NotConnected)
    ));
    r.station.echolink_logout();
}

#[test]
fn dropping_a_logged_in_station_logs_off_the_directory() {
    let r = rig(FakeLogin::Ok);
    r.station.echolink_set_location("Denver");
    r.station
        .echolink_login(CALL, PASSWORD.to_owned(), &servers(&r.dir))
        .expect("login starts");
    assert!(wait_for(
        || login_state(&r.station) == Some(LoginState::LoggedIn)
    ));
    let Rig { fake, dir, station } = r;
    let started = Instant::now();
    drop(station);
    assert!(
        started.elapsed() < WAIT,
        "the drop took {:?}",
        started.elapsed()
    );
    let logoff = logoff_line(CALL, PASSWORD, "Denver");
    assert!(
        dir.lines().iter().any(|l| l[..] == logoff[..]),
        "the directory never heard the logoff"
    );
    drop(fake);
}

#[test]
fn node_disconnect_refuses_an_empty_call_and_chat_needs_a_call_first() {
    let r = rig(FakeLogin::Ok);
    assert!(matches!(
        r.station.echolink_node_disconnect("  "),
        Err(StationError::EchoLink(_))
    ));
    // No call up: NotConnected comes before the empty-line check.
    assert!(matches!(
        r.station.echolink_send_chat(""),
        Err(StationError::NotConnected)
    ));
}

#[test]
#[allow(
    clippy::assertions_on_constants,
    reason = "a test that fails loudly if a test build ever drops the feature"
)]
fn test_builds_are_loopback_only() {
    assert!(astar_console::echolink::LOOPBACK_ONLY);
}

#[test]
fn refresh_list_needs_a_login_and_honours_the_floor() {
    let fake = FakeStation::start(ECHO_CALL);
    let dir = FakeDirectory::start(FakeLogin::Ok, vec![fake.entry(NODE)]);
    let station = Station::with_backend_factory(
        StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    let mut net = network(&fake);
    net.timings.list_min_interval = ms(200);
    station.set_echolink_network(net);
    assert!(matches!(
        station.echolink_refresh_list(),
        Err(StationError::EchoLink(_))
    ));

    station
        .echolink_login(CALL, PASSWORD.to_owned(), &servers(&dir))
        .expect("login starts");
    assert!(wait_for(|| login_state(&station)
        == Some(LoginState::LoggedIn)
        && !station.echolink_stations().is_empty()));
    // The list has only just arrived: inside the floor, nothing is asked.
    let before = dir.exchanges();
    assert_eq!(station.echolink_refresh_list().ok(), Some(false));
    thread::sleep(ms(250));
    assert_eq!(station.echolink_refresh_list().ok(), Some(true));
    assert!(wait_for(|| dir.exchanges() > before), "no list fetched");
    station.echolink_logout();
}

#[test]
fn a_dial_that_misses_asks_for_a_fresh_list() {
    let r = rig(FakeLogin::Ok);
    let mut net = network(&r.fake);
    net.timings.list_min_interval = ms(50);
    r.station.set_echolink_network(net);
    r.station
        .echolink_login(CALL, PASSWORD.to_owned(), &servers(&r.dir))
        .expect("login starts");
    assert!(wait_for(|| login_state(&r.station)
        == Some(LoginState::LoggedIn)
        && !r.station.echolink_stations().is_empty()));
    thread::sleep(ms(100));
    let before = r.dir.exchanges();
    assert!(matches!(
        r.station.echolink_connect("1234"),
        Err(StationError::EchoLink(_))
    ));
    assert!(wait_for(|| r.dir.exchanges() > before), "no list fetched");
    r.station.echolink_logout();
}

#[test]
fn a_password_the_directory_cannot_take_is_refused_without_echoing_it() {
    let r = rig(FakeLogin::Ok);
    for bad in ["pass\rword", "pass\nword", "pass¬word", "Ьword"] {
        let refused = r
            .station
            .echolink_login(CALL, bad.to_owned(), &servers(&r.dir));
        match refused {
            Err(StationError::EchoLink(why)) => {
                assert_eq!(why, astar_station::ECHOLINK_PASSWORD_UNSENDABLE);
                assert!(!why.contains(bad), "{why}");
            }
            other => panic!("{bad:?} was not refused: {other:?}"),
        }
    }
    assert!(r.station.echolink_state().is_none(), "nothing started");
    thread::sleep(ms(100));
    assert_eq!(r.dir.exchanges(), 0);
    // An ordinary non-ASCII password is fine.
    assert!(astar_station::echolink_password_is_sendable("pässwörd"));
}
