// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The server's `EchoLink` leg: stations the admin dials become peer members
//! of the node's conference.
//!
//! The session is built the way the node builds it, as in
//! `m17_monitor_session.rs`: `set_bridge_config(Bridge)` and then
//! `start_inbound` on 127.0.0.1:0 over a `NullBackend`. Every station is a
//! `FakeStation` on 127.0.0.1 and the directory is a `FakeDirectory`, and
//! every test build of this crate is loopback-only, so nothing here can
//! reach a real `EchoLink` server or station.
//!
//! Both called fakes run with `set_echo(false)`. Two echoing fakes would
//! bounce A's audio through the node to B, back through the node to A, and
//! round again for ever; with echo off, A talks (scripted with
//! `FakeStation::send_audio`) and B only listens.
#![cfg(feature = "echolink")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use astar_audio::{AudioBackend, NullBackend};
use astar_console::echolink::{
    CallLink, EchoLinkLogin, EchoLinkNetwork, EchoLinkNodeEdge, LoginState, PeerAddr, StationEntry,
    Timings,
};
use astar_console::{AnswerPolicy, ConsoleError, ConsoleSession};
use astar_echolink::fake::{FakeDirectory, FakeLogin, FakeStation};
use astar_iax::{BridgeConfig, BridgeMode, CodecPolicy, IncomingCallPolicy};
use zeroize::Zeroizing;

const CALL: &str = "KX7ABC";
const NODE_A: u32 = 1001;
const NODE_B: u32 = 1002;
const NODE_C: u32 = 1003;
const CALL_A: &str = "W1AAA";
const CALL_B: &str = "W1BBB";
const CALL_C: &str = "W1CCC";
const WAIT: Duration = Duration::from_secs(3);

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

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

/// Fast clocks, except "far end talking": all of a burst from
/// `FakeStation::send_audio` lands at once, so the talker has to count as
/// talking for as long as the conference takes to play the burst out.
fn timings() -> Timings {
    Timings {
        keepalive: ms(100),
        connection_timeout: ms(600),
        give_up_after: 20,
        talk_hold: ms(1_500),
        ..Timings::default()
    }
}

/// The three fakes and their nodes, a directory listing them, and a node
/// session in Bridge mode that is logged in with the list in hand.
struct Rig {
    a: FakeStation,
    b: FakeStation,
    c: FakeStation,
    _dir: FakeDirectory,
    s: ConsoleSession,
}

fn network(stations: &[(u32, &FakeStation)], match_by_ip: bool) -> EchoLinkNetwork {
    let map: Vec<(u32, PeerAddr)> = stations.iter().map(|(n, s)| (*n, s.addr())).collect();
    EchoLinkNetwork {
        audio_bind: "127.0.0.1:0".parse().unwrap(),
        control_bind: "127.0.0.1:0".parse().unwrap(),
        peer_addr: Arc::new(move |e: &StationEntry| {
            map.iter()
                .find(|(n, _)| *n == e.node)
                .map_or_else(|| PeerAddr::fixed(e.ip), |(_, a)| *a)
        }),
        // Full-address matching unless a test asks for production's by-IP
        // matching, so a third fake on 127.0.0.1 is a stranger.
        match_by_ip,
        timings: timings(),
    }
}

fn loopback() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

fn rig() -> Rig {
    rig_with(CodecPolicy::default(), false)
}

/// [`rig`] with the engine's codec policy, which pins the conference rate:
/// `PreferSlin16` runs it at 16 kHz, so each peer's rate bridges are in the
/// path. `match_by_ip` is production's matching, under which every fake
/// (all on 127.0.0.1) has the same address.
fn rig_with(codec_policy: CodecPolicy, match_by_ip: bool) -> Rig {
    let a = FakeStation::start(CALL_A);
    let b = FakeStation::start(CALL_B);
    let c = FakeStation::start(CALL_C);
    a.set_echo(false);
    b.set_echo(false);
    c.set_echo(false);
    let dir = FakeDirectory::start(
        FakeLogin::Ok,
        vec![a.entry(NODE_A), b.entry(NODE_B), c.entry(NODE_C)],
    );
    let mut s = ConsoleSession::new();
    s.set_bridge_config(BridgeConfig {
        mode: BridgeMode::Bridge,
        ..BridgeConfig::default()
    })
    .expect("bridge config stored");
    s.start_inbound(
        loopback(),
        IncomingCallPolicy {
            codec_policy,
            ..IncomingCallPolicy::default()
        },
        AnswerPolicy::Auto,
        4,
        || -> Box<dyn AudioBackend> { Box::new(NullBackend::new()) },
        (None, None),
    )
    .expect("inbound listener on 127.0.0.1:0");
    s.set_echolink_network(network(
        &[(NODE_A, &a), (NODE_B, &b), (NODE_C, &c)],
        match_by_ip,
    ));
    s.echolink_login(
        EchoLinkLogin {
            callsign: CALL.to_owned(),
            servers: vec![dir.addr().to_string()],
            location: "Denver".to_owned(),
        },
        Zeroizing::new("s3cret-pw".to_owned()),
    )
    .expect("login starts");
    assert!(
        wait_for(WAIT, || s
            .echolink_state()
            .is_some_and(|st| st.login.state == LoginState::LoggedIn)
            && s.echolink_stations().len() == 3),
        "never logged in: {:?}",
        s.echolink_state()
    );
    Rig {
        a,
        b,
        c,
        _dir: dir,
        s,
    }
}

fn peer_link(s: &ConsoleSession, call: &str) -> Option<CallLink> {
    s.echolink_node_peers()
        .into_iter()
        .find(|p| p.call == call)
        .map(|p| p.link)
}

/// Poll until `call` reports connected, collecting the edges seen.
fn wait_peer_connected(s: &mut ConsoleSession, call: &str) -> Vec<EchoLinkNodeEdge> {
    let mut edges = Vec::new();
    let ok = wait_for(WAIT, || {
        edges.extend(s.echolink_node_poll());
        peer_link(s, call) == Some(CallLink::Connected)
    });
    assert!(ok, "{call} never connected: {:?}", s.echolink_node_peers());
    edges
}

/// Both A and B dialled and connected.
fn both_connected(r: &mut Rig) {
    r.s.echolink_node_connect(&NODE_A.to_string())
        .expect("dial A");
    r.s.echolink_node_connect(CALL_B).expect("dial B");
    wait_peer_connected(&mut r.s, CALL_A);
    wait_peer_connected(&mut r.s, CALL_B);
}

/// Run the node's pump the way astar-server does: the leg's poll, then link
/// keying (which opens and closes each peer's receive side).
fn pump(s: &mut ConsoleSession, for_how_long: Duration) -> Vec<EchoLinkNodeEdge> {
    let mut edges = Vec::new();
    let deadline = Instant::now() + for_how_long;
    while Instant::now() < deadline {
        edges.extend(s.echolink_node_poll());
        s.sync_link_keying();
        thread::sleep(ms(10));
    }
    edges
}

#[test]
fn two_echolink_stations_join_the_conference() {
    let mut r = rig();
    r.s.echolink_node_connect(&NODE_A.to_string())
        .expect("dial A");
    r.s.echolink_node_connect(CALL_B).expect("dial B");
    let mut edges = wait_peer_connected(&mut r.s, CALL_A);
    edges.extend(wait_peer_connected(&mut r.s, CALL_B));
    assert!(edges.contains(&EchoLinkNodeEdge::Connected(CALL_A.to_owned())));
    assert!(edges.contains(&EchoLinkNodeEdge::Connected(CALL_B.to_owned())));

    let peers = r.s.echolink_node_peers();
    assert_eq!(peers.len(), 2);
    let a = peers.iter().find(|p| p.call == CALL_A).expect("A listed");
    assert_eq!(a.node, NODE_A);
    assert!(!a.talking);
    assert!(a.up_secs.is_some());
    assert!(r.a.sdes_count() >= 1 && r.b.sdes_count() >= 1);
    // Each station got our information text once it answered.
    assert!(wait_for(WAIT, || !r.a.infos().is_empty() && !r.b.infos().is_empty()));

    // A second dial to a station already in the conference is refused.
    assert!(matches!(
        r.s.echolink_node_connect(CALL_A),
        Err(ConsoleError::AlreadyConnected)
    ));
}

#[test]
fn audio_from_one_station_reaches_the_other() {
    audio_reaches_the_other(rig());
}

#[test]
fn audio_from_one_station_reaches_the_other_at_16_khz() {
    let r = rig_with(CodecPolicy::PreferSlin16, false);
    assert_eq!(r.s.pipeline_sample_rate(), 16_000);
    audio_reaches_the_other(r);
}

fn audio_reaches_the_other(mut r: Rig) {
    let rate = r.s.pipeline_sample_rate();
    both_connected(&mut r);
    let ours = r.s.echolink_node_local_addrs().expect("ports open");
    r.a.send_audio(ours, 40);
    let mut talking_seen = false;
    let ok = wait_for(WAIT, || {
        r.s.echolink_node_poll();
        r.s.sync_link_keying();
        talking_seen |=
            r.s.echolink_node_peers()
                .iter()
                .any(|p| p.call == CALL_A && p.talking);
        r.b.audio_packets() >= 8
    });
    assert!(ok, "B heard {} packets at {rate} Hz", r.b.audio_packets());
    assert!(talking_seen, "A never showed as talking");
    // Mix-minus: the talker hears none of its own audio.
    pump(&mut r.s, ms(300));
    assert_eq!(r.a.audio_packets(), 0);
}

#[test]
fn nothing_is_sent_to_stations_while_the_net_is_quiet() {
    let mut r = rig();
    both_connected(&mut r);
    pump(&mut r.s, Duration::from_secs(1));
    assert_eq!(r.a.audio_packets(), 0);
    assert_eq!(r.b.audio_packets(), 0);
}

#[test]
fn disconnecting_one_station_leaves_the_other() {
    let mut r = rig();
    both_connected(&mut r);
    r.s.echolink_node_disconnect(CALL_A).expect("hang up A");
    assert!(
        wait_for(WAIT, || !r.a.bye_times().is_empty()),
        "A got no BYE"
    );
    let peers = r.s.echolink_node_peers();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].call, CALL_B);
    pump(&mut r.s, ms(300));
    assert_eq!(peer_link(&r.s, CALL_B), Some(CallLink::Connected));
    assert!(r.b.bye_times().is_empty(), "B was hung up on too");
    assert!(matches!(
        r.s.echolink_node_disconnect(CALL_A),
        Err(ConsoleError::NotConnected)
    ));
}

#[test]
fn a_dropped_station_is_reaped_and_its_member_removed() {
    let mut r = rig();
    both_connected(&mut r);
    r.a.set_answer(false);
    let mut edges = Vec::new();
    let ended = EchoLinkNodeEdge::Ended {
        call: CALL_A.to_owned(),
        reason: "timed out".to_owned(),
    };
    assert!(
        wait_for(WAIT, || {
            edges.extend(r.s.echolink_node_poll());
            edges.contains(&ended)
        }),
        "no timed-out edge: {edges:?}"
    );
    let calls: Vec<String> =
        r.s.echolink_node_peers()
            .into_iter()
            .map(|p| p.call)
            .collect();
    assert_eq!(calls, vec![CALL_B.to_owned()]);

    // The station can be called again.
    r.a.set_answer(true);
    r.s.echolink_node_connect(CALL_A).expect("redial A");
    wait_peer_connected(&mut r.s, CALL_A);
}

#[test]
fn node_leg_peers_set_echolink_active() {
    let mut r = rig();
    assert!(!r.s.snapshot().echolink_active);
    r.a.set_answer(false);
    r.s.echolink_node_connect(CALL_A).expect("dial A");
    assert_eq!(peer_link(&r.s, CALL_A), Some(CallLink::Connecting));
    assert!(r.s.snapshot().echolink_active, "a connecting peer counts");
    r.a.set_answer(true);
    wait_peer_connected(&mut r.s, CALL_A);
    assert!(r.s.snapshot().echolink_active);
    r.s.echolink_node_disconnect(CALL_A).expect("hang up A");
    assert!(!r.s.snapshot().echolink_active);
}

#[test]
fn node_leg_and_client_session_exclude_each_other() {
    let mut r = rig();
    r.s.echolink_connect(Box::new(NullBackend::new()), CALL_C, None, None)
        .expect("client call");
    assert!(matches!(
        r.s.echolink_node_connect(CALL_A),
        Err(ConsoleError::AlreadyConnected)
    ));
    assert_eq!(
        r.s.echolink_node_peers(),
        [] as [astar_console::echolink::EchoLinkPeerStatus; 0]
    );
    r.s.echolink_disconnect();

    r.s.echolink_node_connect(CALL_A).expect("node dial");
    assert!(matches!(
        r.s.echolink_connect(Box::new(NullBackend::new()), CALL_C, None, None),
        Err(ConsoleError::AlreadyConnected)
    ));
    assert!(!r.s.echolink_can_connect());
    r.s.echolink_node_disconnect(CALL_A).expect("hang up A");
}

#[test]
fn uninvited_station_gets_nothing_from_the_node() {
    let mut r = rig();
    both_connected(&mut r);
    let ours = r.s.echolink_node_local_addrs().expect("ports open");
    let stranger = FakeStation::start("W9XYZ");
    stranger.set_echo(false);
    stranger.send_sdes_to(ours.control);
    stranger.send_audio(ours, 40);
    pump(&mut r.s, ms(600));
    assert_eq!(stranger.controls_from(ours.control), 0);
    assert_eq!(stranger.audio_packets(), 0);
    assert_eq!(stranger.infos(), [] as [std::string::String; 0]);
    assert_eq!(r.a.audio_packets(), 0, "A heard the stranger");
    assert_eq!(r.b.audio_packets(), 0, "B heard the stranger");
    assert!(r.s.echolink_node_peers().iter().all(|p| !p.talking));
    // C, listed but never called, was never contacted either.
    assert_eq!(r.c.sdes_count(), 0);
}

#[test]
fn a_second_station_at_the_same_ip_is_refused_under_by_ip_matching() {
    let mut r = rig_with(CodecPolicy::default(), true);
    r.s.echolink_node_connect(CALL_A).expect("dial A");
    wait_peer_connected(&mut r.s, CALL_A);
    let err = r.s.echolink_node_connect(CALL_B);
    assert!(matches!(err, Err(ConsoleError::EchoLink(_))), "{err:?}");
    let calls: Vec<String> =
        r.s.echolink_node_peers()
            .into_iter()
            .map(|p| p.call)
            .collect();
    assert_eq!(calls, vec![CALL_A.to_owned()]);
    pump(&mut r.s, ms(300));
    assert_eq!(peer_link(&r.s, CALL_A), Some(CallLink::Connected));
    assert_eq!(r.b.sdes_count(), 0, "B was called anyway");
}

#[test]
fn a_switch_to_handset_reaps_every_station_as_conference_closed() {
    let mut r = rig();
    both_connected(&mut r);
    r.s.set_bridge_config(BridgeConfig {
        mode: BridgeMode::Handset,
        ..BridgeConfig::default()
    })
    .expect("switch to handset");
    let edges = r.s.echolink_node_poll();
    for call in [CALL_A, CALL_B] {
        let ended = EchoLinkNodeEdge::Ended {
            call: call.to_owned(),
            reason: "conference closed".to_owned(),
        };
        assert!(edges.contains(&ended), "no {ended:?} in {edges:?}");
    }
    assert_eq!(
        r.s.echolink_node_peers(),
        [] as [astar_console::echolink::EchoLinkPeerStatus; 0]
    );
    assert!(
        wait_for(WAIT, || !r.a.bye_times().is_empty()
            && !r.b.bye_times().is_empty()),
        "a station got no BYE"
    );
    assert!(!r.s.snapshot().echolink_active);
}

#[test]
fn the_transmit_cap_stops_a_net_that_never_pauses_and_resumes_after_one() {
    let a = FakeStation::start(CALL_A);
    let b = FakeStation::start(CALL_B);
    a.set_echo(false);
    b.set_echo(false);
    let dir = FakeDirectory::start(FakeLogin::Ok, vec![a.entry(NODE_A), b.entry(NODE_B)]);
    let mut s = ConsoleSession::new();
    s.set_bridge_config(BridgeConfig {
        mode: BridgeMode::Bridge,
        ..BridgeConfig::default()
    })
    .expect("bridge config stored");
    s.start_inbound(
        loopback(),
        IncomingCallPolicy::default(),
        AnswerPolicy::Auto,
        4,
        || -> Box<dyn AudioBackend> { Box::new(NullBackend::new()) },
        (None, None),
    )
    .expect("inbound listener on 127.0.0.1:0");
    let mut net = network(&[(NODE_A, &a), (NODE_B, &b)], false);
    net.timings.max_transmit = ms(300);
    s.set_echolink_network(net);
    s.echolink_login(
        EchoLinkLogin {
            callsign: CALL.to_owned(),
            servers: vec![dir.addr().to_string()],
            location: "Denver".to_owned(),
        },
        Zeroizing::new("s3cret-pw".to_owned()),
    )
    .expect("login starts");
    assert!(wait_for(WAIT, || s.echolink_stations().len() == 2));
    s.echolink_node_connect(CALL_A).expect("dial A");
    s.echolink_node_connect(CALL_B).expect("dial B");
    wait_peer_connected(&mut s, CALL_A);
    wait_peer_connected(&mut s, CALL_B);

    // Two seconds of A without a break: 25 packets to B, were it not for the
    // cap, which stops them after 300 ms.
    let ours = s.echolink_node_local_addrs().expect("ports open");
    a.send_audio(ours, 100);
    let edges = pump(&mut s, Duration::from_millis(3_500));
    let heard = b.audio_packets();
    assert!(heard >= 1, "B heard nothing at all");
    assert!(
        heard < 15,
        "B heard {heard} packets: the cap never stopped the net"
    );
    assert!(
        edges.iter().any(|e| matches!(
            e,
            EchoLinkNodeEdge::TransmitCapped { call, notice }
                if call == CALL_B && notice.contains("10 minutes of continuous talk")
        )),
        "no TransmitCapped edge for B: {edges:?}"
    );

    // The net paused, so the next over reaches B again.
    a.send_audio(ours, 12);
    let ok = wait_for(WAIT, || {
        s.echolink_node_poll();
        s.sync_link_keying();
        b.audio_packets() > heard
    });
    assert!(ok, "B never heard the next over");
}
