// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The packet log's tap on `EchoLink`, end to end over loopback: a client
//! call and the members of a node's leg, against `FakeStation`s.
//!
//! The tap sits on the audio port, ahead of everything that decides what to
//! do with a datagram, so it sees a repeat the sequence gate throws away. It
//! keeps only the 144-byte voice packets: the port's `oNDATA` text and chat
//! stay out of the log, and the control port and the directory's TCP login
//! have no tap at all.
//!
//! Every server here is a fake on 127.0.0.1, and every test build of this
//! crate is loopback-only, so nothing in this file can reach a real `EchoLink`
//! server or station.
#![cfg(feature = "echolink")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use astar_audio::{AudioBackend, CallAudio, NullBackend, RxFrame};
use astar_console::echolink::{
    CallLink, EchoLinkLogin, EchoLinkNetwork, EchoLinkPresence, EchoLinkSession, LoginState,
    PeerAddr, StationEntry, Timings,
};
use astar_console::{AnswerPolicy, ConsoleSession};
use astar_echolink::fake::{FakeDirectory, FakeLogin, FakeStation};
use astar_echolink::rtcp;
use astar_echolink::voice::{AudioPacket, FRAMES_PER_PACKET, GSM_FRAME_LEN};
use astar_iax::{BridgeConfig, BridgeMode, IncomingCallPolicy};
use astar_record::packet_tap::{
    CallMeta, PACKET_TAP_CAPACITY, PacketCallId, PacketEvent, PacketNetwork, PacketSlot,
    packet_tap_channel,
};
use zeroize::Zeroizing;

const CALL: &str = "KX7ABC";
const PASSWORD: &str = "canary-password";
const STATION: &str = "W1ECH";
const NODE: u32 = 9999;
const NODE_A: u32 = 1001;
const NODE_B: u32 = 1002;
const CALL_A: &str = "W1AAA";
const CALL_B: &str = "W1BBB";
const WAIT: Duration = Duration::from_secs(5);
/// Long enough for a datagram already on the socket to have been handled.
const SETTLE: Duration = Duration::from_millis(150);
const AUDIO_PACKET_LEN: usize = 144;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn wait_until(what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if pred() {
            return;
        }
        thread::sleep(ms(5));
    }
    panic!("timed out waiting for {what}");
}

fn timings() -> Timings {
    Timings {
        keepalive: ms(100),
        connection_timeout: ms(3_000),
        give_up_after: 20,
        talk_hold: ms(1_500),
        ..Timings::default()
    }
}

/// Loopback binds on ephemeral ports, each node mapped to its fake's ports.
/// Datagrams match by full address, so another fake on 127.0.0.1 is a
/// stranger.
fn network(stations: &[(u32, &FakeStation)]) -> EchoLinkNetwork {
    let map: Vec<(u32, PeerAddr)> = stations.iter().map(|(n, s)| (*n, s.addr())).collect();
    EchoLinkNetwork {
        audio_bind: "127.0.0.1:0".parse().unwrap(),
        control_bind: "127.0.0.1:0".parse().unwrap(),
        peer_addr: Arc::new(move |e: &StationEntry| {
            map.iter()
                .find(|(n, _)| *n == e.node)
                .map_or_else(|| PeerAddr::fixed(e.ip), |(_, a)| *a)
        }),
        match_by_ip: false,
        timings: timings(),
    }
}

/// Channel ends the test holds itself: the mic it never plays, and what the
/// link decodes.
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

fn call(station: &FakeStation, audio: CallAudio) -> EchoLinkSession {
    EchoLinkSession::connect(
        station.entry(NODE),
        CALL,
        "Denver",
        &network(&[(NODE, station)]),
        audio,
    )
    .expect("connect")
}

fn wait_connected(s: &EchoLinkSession) {
    wait_until("the call to connect", || {
        s.state().link == CallLink::Connected
    });
}

/// A voice packet whose frames are all `fill`, so a test can tell it apart.
fn voice_packet(seq: u16, fill: u8) -> Vec<u8> {
    AudioPacket {
        seq,
        frames: [[fill; GSM_FRAME_LEN]; FRAMES_PER_PACKET],
    }
    .encode()
    .to_vec()
}

fn tapped(slot: &PacketSlot) -> Receiver<PacketEvent> {
    let (tap, events) = packet_tap_channel(PACKET_TAP_CAPACITY, PACKET_TAP_CAPACITY);
    slot.set(Some(tap));
    events
}

fn packets_in(events: &[PacketEvent]) -> Vec<Vec<u8>> {
    events
        .iter()
        .filter_map(|e| match e {
            PacketEvent::Packet { bytes, .. } => Some(bytes.to_vec()),
            _ => None,
        })
        .collect()
}

/// Read events until `count` packets have come through.
fn gather(events: &Receiver<PacketEvent>, count: usize) -> Vec<PacketEvent> {
    let mut seen = Vec::new();
    wait_until(&format!("{count} logged packets"), || {
        seen.extend(events.try_iter());
        packets_in(&seen).len() >= count
    });
    seen
}

fn meta_of(events: &[PacketEvent], want: PacketCallId) -> &CallMeta {
    events
        .iter()
        .find_map(|e| match e {
            PacketEvent::Start { call, meta, .. } if *call == want => Some(&**meta),
            _ => None,
        })
        .expect("a Start for the call")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn a_client_call_logs_audio_and_nothing_else() {
    let station = FakeStation::start(STATION);
    station.set_echo(false);
    let (audio, _mic, _speaker) = manual_audio();
    let s = call(&station, audio);
    let events = tapped(&s.packet_slot());
    wait_connected(&s);
    // Our information text has reached the fake, so it knows where to send
    // its own.
    wait_until("our info at the station", || !station.infos().is_empty());
    let ours = s.local_addrs().expect("ports open");

    station.send_audio(ours, 8);
    station.send_info("Fake station, Denver");
    station.send_chat("hello from the far end");
    // Control datagrams aimed at the audio port: the filter has to refuse
    // them, because the port's tap does see them.
    station.send_raw_audio(ours.audio, &rtcp::sdes(STATION, ""));
    station.send_raw_audio(ours.audio, &rtcp::bye());
    station.send_audio(ours, 4);

    wait_until("the text to be handled", || {
        let state = s.state();
        state.info.is_some() && !state.chat.is_empty()
    });
    station.send_bye();
    wait_until("the far end's BYE", || s.state().link == CallLink::Closed);

    let id = s.packet_call();
    let mut seen = gather(&events, 3);
    wait_until("the call's End", || {
        seen.extend(events.try_iter());
        matches!(seen.last(), Some(PacketEvent::End { .. }))
    });

    let packets = packets_in(&seen);
    assert_eq!(packets.len(), 3, "only the voice packets are logged");
    for packet in &packets {
        assert_eq!(packet.len(), AUDIO_PACKET_LEN);
        assert_eq!(packet[..2], [0xC0, 0x03]);
    }
    assert!(matches!(seen.first(), Some(PacketEvent::Start { .. })));
    let meta = meta_of(&seen, id);
    assert_eq!(meta.network, PacketNetwork::EchoLink);
    assert_eq!(meta.label, format!("EchoLink {STATION}"));
    assert_eq!(meta.local, ours.audio);
    assert_eq!(meta.peer, station.addr().audio);
    for event in &seen {
        if let PacketEvent::Packet { call, from, .. } = event {
            assert_eq!(*call, id);
            assert_eq!(*from, station.addr().audio);
        }
    }
    s.disconnect();
}

#[test]
fn the_directory_login_is_never_tapped() {
    let station = FakeStation::start(STATION);
    station.set_echo(false);
    let (audio, _mic, _speaker) = manual_audio();
    let s = call(&station, audio);
    let events = tapped(&s.packet_slot());
    wait_connected(&s);

    let dir = FakeDirectory::start(FakeLogin::Ok, Vec::new());
    let presence = EchoLinkPresence::start(
        EchoLinkLogin {
            callsign: CALL.to_owned(),
            servers: vec![dir.addr().to_string()],
            location: "Denver".to_owned(),
        },
        Zeroizing::new(PASSWORD.to_owned()),
        timings(),
    )
    .expect("presence starts");
    wait_until("the login", || {
        presence.snapshot().state == LoginState::LoggedIn
    });
    // The canary really did leave this process, while the tap was live.
    assert!(
        dir.lines()
            .iter()
            .any(|line| contains(line, PASSWORD.as_bytes()))
    );

    station.send_audio(s.local_addrs().expect("ports open"), 8);
    let seen = gather(&events, 2);
    for packet in packets_in(&seen) {
        assert!(!contains(&packet, PASSWORD.as_bytes()));
    }
    assert!(!meta_of(&seen, s.packet_call()).label.contains(PASSWORD));
    s.disconnect();
}

#[test]
fn a_duplicate_sequence_number_is_logged_though_the_gate_drops_it() {
    let station = FakeStation::start(STATION);
    station.set_echo(false);
    let (audio, _mic, speaker) = manual_audio();
    let s = call(&station, audio);
    let events = tapped(&s.packet_slot());
    wait_connected(&s);
    let ours = s.local_addrs().expect("ports open");

    let packet = voice_packet(7, 0x11);
    let next = voice_packet(8, 0x22);
    station.send_raw_audio(ours.audio, &packet);
    station.send_raw_audio(ours.audio, &packet);
    station.send_raw_audio(ours.audio, &next);

    let logged = packets_in(&gather(&events, 3));
    assert_eq!(logged, vec![packet.clone(), packet, next]);

    // The later packet is there to show the repeat has been dealt with.
    let mut decoded = 0;
    wait_until("the decoded frames", || {
        decoded += speaker.try_iter().count();
        decoded >= 2 * FRAMES_PER_PACKET
    });
    thread::sleep(SETTLE);
    decoded += speaker.try_iter().count();
    assert_eq!(decoded, 2 * FRAMES_PER_PACKET, "the repeat was not decoded");
    s.disconnect();
}

#[test]
fn a_datagram_from_a_stranger_is_not_logged() {
    let station = FakeStation::start(STATION);
    station.set_echo(false);
    let stranger = FakeStation::start("N0BODY");
    let (audio, _mic, _speaker) = manual_audio();
    let s = call(&station, audio);
    let events = tapped(&s.packet_slot());
    wait_connected(&s);
    let ours = s.local_addrs().expect("ports open");

    let theirs = voice_packet(1, 0x33);
    let called = voice_packet(2, 0x44);
    stranger.send_raw_audio(ours.audio, &theirs);
    station.send_raw_audio(ours.audio, &called);

    let mut seen = gather(&events, 1);
    thread::sleep(SETTLE);
    seen.extend(events.try_iter());
    assert_eq!(packets_in(&seen), vec![called]);
    s.disconnect();
}

/// Time one burst of `FRAMES` frames from the fake to the last decoded frame.
/// A wedged run has a tap of capacity 1 that nobody reads; the other has no
/// tap at all.
fn time_a_burst(wedged: bool) -> Duration {
    const PACKETS: usize = 500;
    const FRAMES: usize = PACKETS * FRAMES_PER_PACKET;

    let station = FakeStation::start(STATION);
    station.set_echo(false);
    let (audio, _mic, speaker) = manual_audio();
    let s = call(&station, audio);
    let events = wedged.then(|| {
        let (tap, events) = packet_tap_channel(1, 1);
        s.packet_slot().set(Some(tap));
        events
    });
    wait_connected(&s);
    let ours = s.local_addrs().expect("ports open");

    let started = Instant::now();
    thread::scope(|scope| {
        scope.spawn(|| station.send_audio(ours, FRAMES));
        for n in 0..FRAMES {
            if let Err(e) = speaker.recv_timeout(WAIT) {
                panic!("frame {n} of {FRAMES} never arrived: {e}");
            }
        }
    });
    let elapsed = started.elapsed();
    if let Some(events) = events {
        // The tap really was in the way: its one place holds the Start.
        assert!(matches!(
            events.try_iter().collect::<Vec<_>>().as_slice(),
            [PacketEvent::Start { .. }]
        ));
    }
    s.disconnect();
    elapsed
}

#[test]
fn a_wedged_log_does_not_slow_receive() {
    // The two kinds of run take turns and the best of each is compared, so
    // a stall from the rest of the machine doesn't count against either.
    let mut clear = Duration::MAX;
    let mut wedged = Duration::MAX;
    for _ in 0..4 {
        clear = clear.min(time_a_burst(false));
        wedged = wedged.min(time_a_burst(true));
    }
    assert!(
        wedged <= clear.mul_f64(1.2),
        "a wedged log took {wedged:?} against {clear:?} with no tap"
    );
}

// ---- the node's leg ---------------------------------------------------------

/// A node session in Bridge mode, logged in with `stations` listed, as
/// `tests/echolink_node.rs` builds it.
fn node(stations: &[(u32, &FakeStation)], dir: &FakeDirectory) -> ConsoleSession {
    let mut s = ConsoleSession::new();
    s.set_bridge_config(BridgeConfig {
        mode: BridgeMode::Bridge,
        ..BridgeConfig::default()
    })
    .expect("bridge config stored");
    s.start_inbound(
        SocketAddr::from(([127, 0, 0, 1], 0)),
        IncomingCallPolicy::default(),
        AnswerPolicy::Auto,
        4,
        || -> Box<dyn AudioBackend> { Box::new(NullBackend::new()) },
        (None, None),
    )
    .expect("inbound listener on 127.0.0.1:0");
    s.set_echolink_network(network(stations));
    s.echolink_login(
        EchoLinkLogin {
            callsign: CALL.to_owned(),
            servers: vec![dir.addr().to_string()],
            location: "Denver".to_owned(),
        },
        Zeroizing::new(PASSWORD.to_owned()),
    )
    .expect("login starts");
    wait_until("the node's login and list", || {
        s.echolink_state()
            .is_some_and(|st| st.login.state == LoginState::LoggedIn)
            && s.echolink_stations().len() == stations.len()
    });
    s
}

fn peer_connected(s: &ConsoleSession, call: &str) -> bool {
    s.echolink_node_peers()
        .iter()
        .any(|p| p.call == call && p.link == CallLink::Connected)
}

#[test]
fn each_member_of_a_node_leg_gets_its_own_call() {
    let a = FakeStation::start(CALL_A);
    let b = FakeStation::start(CALL_B);
    a.set_echo(false);
    b.set_echo(false);
    let dir = FakeDirectory::start(FakeLogin::Ok, vec![a.entry(NODE_A), b.entry(NODE_B)]);
    let mut s = node(&[(NODE_A, &a), (NODE_B, &b)], &dir);

    s.echolink_node_connect(CALL_A).expect("dial A");
    s.echolink_node_connect(CALL_B).expect("dial B");
    wait_until("both stations to connect", || {
        s.echolink_node_poll();
        peer_connected(&s, CALL_A) && peer_connected(&s, CALL_B)
    });
    let leg = s.echolink_node_leg().expect("the leg is up");
    let events = tapped(&leg.packet_slot());
    let calls = leg.packet_calls();
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0], calls[1]);
    let ours = s.echolink_node_local_addrs().expect("ports open");

    let from_a = voice_packet(1, 0x55);
    let from_b = voice_packet(1, 0x66);
    a.send_raw_audio(ours.audio, &from_a);
    b.send_raw_audio(ours.audio, &from_b);
    let mut seen = gather(&events, 2);

    let starts = seen
        .iter()
        .filter(|e| matches!(e, PacketEvent::Start { .. }))
        .count();
    assert_eq!(starts, 2);
    for (id, call, station, bytes) in [
        (calls[0], CALL_A, &a, &from_a),
        (calls[1], CALL_B, &b, &from_b),
    ] {
        let meta = meta_of(&seen, id);
        assert_eq!(meta.label, format!("EchoLink {call}"));
        assert_eq!(meta.peer, station.addr().audio);
        assert_eq!(meta.local, ours.audio);
        assert!(seen.iter().any(|e| matches!(
            e,
            PacketEvent::Packet { call, from, bytes: got, .. }
                if *call == id && *from == station.addr().audio && **got == **bytes
        )));
    }

    // One station hanging up ends its call and leaves the other's running.
    s.echolink_node_disconnect(CALL_A).expect("hang up on A");
    wait_until("A's End", || {
        seen.extend(events.try_iter());
        seen.iter()
            .any(|e| matches!(e, PacketEvent::End { call, .. } if *call == calls[0]))
    });
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, PacketEvent::End { call, .. } if *call == calls[1]))
    );
    let leg = s.echolink_node_leg().expect("the leg is still up");
    assert_eq!(leg.packet_calls(), vec![calls[1]]);
}

/// A session-level tap, as the packet log installs it.
fn session_tap(s: &mut ConsoleSession) -> Receiver<PacketEvent> {
    let (tap, events) = packet_tap_channel(64, 64);
    s.set_packet_tap(Some(tap));
    events
}

fn starts_in(events: &[PacketEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, PacketEvent::Start { .. }))
        .count()
}

/// The leg's slot is shared, and setting a slot makes every call on it offer
/// `Start` again, which the writer answers with a new file. So a second
/// station joining must not touch the first one's log.
#[test]
fn a_station_joining_the_leg_leaves_the_first_ones_log_running() {
    let a = FakeStation::start(CALL_A);
    let b = FakeStation::start(CALL_B);
    a.set_echo(false);
    b.set_echo(false);
    let dir = FakeDirectory::start(FakeLogin::Ok, vec![a.entry(NODE_A), b.entry(NODE_B)]);
    let mut s = node(&[(NODE_A, &a), (NODE_B, &b)], &dir);
    let events = session_tap(&mut s);

    s.echolink_node_connect(CALL_A).expect("dial A");
    wait_until("A to connect", || {
        s.echolink_node_poll();
        peer_connected(&s, CALL_A)
    });
    let ours = s.echolink_node_local_addrs().expect("ports open");
    a.send_raw_audio(ours.audio, &voice_packet(1, 0x55));
    let seen = gather(&events, 1);
    assert_eq!(starts_in(&seen), 1, "the tap reached the first station");

    s.echolink_node_connect(CALL_B).expect("dial B");
    wait_until("B to connect", || {
        s.echolink_node_poll();
        peer_connected(&s, CALL_B)
    });
    a.send_raw_audio(ours.audio, &voice_packet(2, 0x56));
    let seen = gather(&events, 1);
    assert_eq!(starts_in(&seen), 0, "A's log carries on in the same file");
}

/// An empty leg is replaced when the location changes, and its replacement
/// has a slot of its own that must get the tap.
#[test]
fn a_replaced_leg_gets_the_tap() {
    let a = FakeStation::start(CALL_A);
    a.set_echo(false);
    let dir = FakeDirectory::start(FakeLogin::Ok, vec![a.entry(NODE_A)]);
    let mut s = node(&[(NODE_A, &a)], &dir);
    let events = session_tap(&mut s);

    s.echolink_node_connect(CALL_A).expect("dial A");
    let first = s.echolink_node_leg().expect("the leg is up").packet_slot();
    assert!(first.is_set(), "the first leg picks up the tap");
    s.echolink_node_disconnect(CALL_A).expect("hang up");
    s.echolink_set_location("Boulder");

    s.echolink_node_connect(CALL_A).expect("dial A again");
    let second = s.echolink_node_leg().expect("the leg is up").packet_slot();
    wait_until("A to connect on the new leg", || {
        s.echolink_node_poll();
        peer_connected(&s, CALL_A)
    });
    let ours = s.echolink_node_local_addrs().expect("ports open");
    a.send_raw_audio(ours.audio, &voice_packet(1, 0x55));
    assert!(second.is_set(), "the replacement picks up the tap");
    assert_eq!(starts_in(&gather(&events, 1)), 1, "and logs the new call");
}
