// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `Station::send_dtmf` exposes a single in-band DTMF tone (iax-0e9b), the
//! binding-exposure slice of the iax-3746 DTMF router. The tone is synthesized
//! with the iax-68eb generator and injected to-air on the active call; the
//! round-trip is verified by decoding the synthesized PCM with the iax-68eb
//! Goertzel decoder.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use astar_codec::dtmf::{DEFAULT_TONE_DBFS, DTMF_SAMPLE_RATE, DtmfDecoder, synthesize};
use astar_iax::{IncomingAuthPolicy, IncomingCallPolicy};
use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, FullFrame, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{FrameType, IaxCommand, VoiceFormat};
use astar_station::{
    AnswerPolicy, CallStatus, DEFAULT_DTMF_MS, DtmfMode, NodeConfig, OperatingMode, Station,
    StationConfig, StationError,
};

const PEER_CALL: u16 = 14101;

fn enc(frame: &Frame<'_>) -> Vec<u8> {
    let mut b = Vec::new();
    encode(frame, &mut b).expect("encode");
    b
}

fn new_datagram(ies: Ies<'_>, source_call: u16) -> Vec<u8> {
    enc(&Frame::Full(Box::new(FullFrame {
        source_call,
        dest_call: 0,
        retransmission: false,
        timestamp: 0,
        oseqno: 0,
        iseqno: 0,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::New),
        ies,
        payload: &[],
    })))
}

fn valid_new_ies() -> Ies<'static> {
    Ies {
        called_number: Some("s"),
        calling_number: Some("1001"),
        calling_name: Some("Rob"),
        capability: Some(VoiceFormat::G711U.as_u32() | VoiceFormat::G711A.as_u32()),
        format: Some(VoiceFormat::G711U.as_u32()),
        version: Some(2),
        calltoken: Some(b""),
        ..Ies::empty()
    }
}

fn peer_socket() -> (UdpSocket, std::net::SocketAddr) {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let a = s.local_addr().unwrap();
    (s, a)
}

/// Drain incoming datagrams and ACK every reliable full frame.
fn pump_acks(sock: &UdpSocket, listener_addr: std::net::SocketAddr, my_call: u16) {
    let mut buf = [0u8; 4096];
    while let Ok((n, _src)) = sock.recv_from(&mut buf) {
        let bytes = buf[..n].to_vec();
        let Ok(Frame::Full(f)) = parse_lenient(&bytes) else {
            continue;
        };
        if !matches!(f.subclass, Subclass::Iax(IaxCommand::Ack)) {
            let ack = enc(&Frame::Full(Box::new(FullFrame {
                source_call: my_call,
                dest_call: f.source_call,
                retransmission: false,
                timestamp: f.timestamp,
                oseqno: f.iseqno,
                iseqno: f.oseqno.wrapping_add(1),
                frame_type: FrameType::Iax,
                subclass: Subclass::Iax(IaxCommand::Ack),
                ies: Ies::empty(),
                payload: &[],
            })));
            let _ = sock.send_to(&ack, listener_addr);
        }
    }
}

fn node_station() -> Station {
    let policy = IncomingCallPolicy {
        auth: IncomingAuthPolicy::Off,
        ..IncomingCallPolicy::default()
    };
    let cfg = StationConfig {
        mode: OperatingMode::Node,
        node: Some(NodeConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            policy,
            answer: AnswerPolicy::Auto,
            register: None,
            max_calls: 20,
            allowlist: None,
        }),
        ..StationConfig::default()
    };
    Station::with_backend_factory(cfg, Box::new(|| Box::new(astar_audio::NullBackend::new())))
}

// --- Round-trip: the exact tone Station synthesizes decodes back to the digit ---

#[test]
fn each_dialer_digit_round_trips_through_the_goertzel_decoder() {
    // This mirrors what Station::send_dtmf builds internally (iax-68eb generator)
    // and confirms each of the 16 DTMF keys decodes back via the Goertzel decoder.
    for digit in [
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', '*', '#', 'A', 'B', 'C', 'D',
    ] {
        let tone =
            synthesize(digit, DTMF_SAMPLE_RATE, DEFAULT_DTMF_MS, DEFAULT_TONE_DBFS).expect("valid");
        let mut decoder = DtmfDecoder::new(DTMF_SAMPLE_RATE);
        assert_eq!(
            decoder.process(&tone),
            vec![digit],
            "in-band tone for {digit} must round-trip through the Goertzel decoder",
        );
    }
}

// --- Validation ---

#[test]
fn send_dtmf_rejects_non_dtmf_characters() {
    let station = node_station();
    // Non-keypad characters are rejected with InvalidDigit — *before* any
    // NotConnected check. The 16 DTMF keys (0-9 * # A-D) are accepted; everything
    // else (letters past D, whitespace, punctuation) is not.
    for bad in ['x', 'E', 'd', ' ', '+', '/'] {
        assert!(
            matches!(station.send_dtmf(bad), Err(StationError::InvalidDigit)),
            "{bad:?} must be rejected as an invalid digit",
        );
    }
}

#[test]
fn send_dtmf_while_idle_is_not_connected() {
    let station = node_station();
    assert!(
        matches!(station.send_dtmf('5'), Err(StationError::NotConnected)),
        "a valid digit with no active call must fail NotConnected",
    );
}

// --- Active call: the tone is accepted on the live leg ---

#[test]
fn send_dtmf_on_an_active_call_is_ok() {
    let station = node_station();
    station.set_mode(OperatingMode::Node).unwrap();
    let addr = station.node_bind_addr().expect("node listener bound");

    let (peer, _pa) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), addr)
        .unwrap();

    // Drive snapshot (answers + adopts + routes) until Answered.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut answered = false;
    while Instant::now() < deadline && !answered {
        let snap = station.snapshot();
        pump_acks(&peer, addr, PEER_CALL);
        if snap.status == CallStatus::Answered {
            answered = true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        answered,
        "inbound call must reach Answered before send_dtmf"
    );

    assert!(
        station.send_dtmf('7').is_ok(),
        "send_dtmf on an active call must succeed",
    );
    // A custom duration override is also accepted.
    assert!(
        station.send_dtmf_for('#', 120).is_ok(),
        "send_dtmf_for on an active call must succeed",
    );
    // A-D (the full 16-key set) is accepted on a live call, not just the 12 keys.
    assert!(
        station.send_dtmf('A').is_ok(),
        "send_dtmf('A') on an active call must succeed",
    );
}

// --- iax-7fff: per-call DTMF emission mode (in-band tone vs protocol frames) ---

/// Drive the snapshot pump until the inbound call reaches Answered.
fn drive_to_answered(station: &Station, peer: &UdpSocket, addr: std::net::SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let snap = station.snapshot();
        pump_acks(peer, addr, PEER_CALL);
        if snap.status == CallStatus::Answered {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("inbound call must reach Answered");
}

/// Drain incoming datagrams for `window`, acking every reliable full frame and
/// collecting each observed out-of-band DTMF frame as
/// `(is_begin, digit)`. Keeps the station pumped (snapshot + announcements) so
/// both emission paths stay driven while we watch the wire.
fn collect_dtmf_frames(
    station: &Station,
    sock: &UdpSocket,
    listener_addr: std::net::SocketAddr,
    window: Duration,
) -> Vec<(bool, char)> {
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        let _ = station.snapshot();
        station.poll_announcements();
        while let Ok((n, _src)) = sock.recv_from(&mut buf) {
            let bytes = buf[..n].to_vec();
            let Ok(Frame::Full(f)) = parse_lenient(&bytes) else {
                continue;
            };
            match (f.frame_type, &f.subclass) {
                (FrameType::DtmfBegin, Subclass::Dtmf(d)) => seen.push((true, *d)),
                (FrameType::DtmfEnd, Subclass::Dtmf(d)) => seen.push((false, *d)),
                _ => {}
            }
            if !matches!(f.subclass, Subclass::Iax(IaxCommand::Ack)) {
                let ack = enc(&Frame::Full(Box::new(FullFrame {
                    source_call: PEER_CALL,
                    dest_call: f.source_call,
                    retransmission: false,
                    timestamp: f.timestamp,
                    oseqno: f.iseqno,
                    iseqno: f.oseqno.wrapping_add(1),
                    frame_type: FrameType::Iax,
                    subclass: Subclass::Iax(IaxCommand::Ack),
                    ies: Ies::empty(),
                    payload: &[],
                })));
                let _ = sock.send_to(&ack, listener_addr);
            }
        }
    }
    seen
}

#[test]
fn dtmf_mode_defaults_to_in_band() {
    // The hard default: a fresh station emits DTMF exactly as before iax-7fff —
    // an in-band tone via the announce path, never protocol frames.
    let station = node_station();
    assert_eq!(station.dtmf_mode(), DtmfMode::InBand);
}

#[test]
fn set_dtmf_mode_round_trips() {
    let station = node_station();
    station.set_dtmf_mode(DtmfMode::Protocol);
    assert_eq!(station.dtmf_mode(), DtmfMode::Protocol);
    station.set_dtmf_mode(DtmfMode::InBand);
    assert_eq!(station.dtmf_mode(), DtmfMode::InBand);
}

#[test]
fn default_in_band_mode_emits_no_protocol_dtmf_frames_on_the_wire() {
    // Default (in-band) mode: the digit is injected as tone audio via the
    // announce path; NO DtmfBegin/DtmfEnd protocol frame may reach the wire.
    let station = node_station();
    station.set_mode(OperatingMode::Node).unwrap();
    let addr = station.node_bind_addr().expect("node listener bound");

    let (peer, _pa) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), addr)
        .unwrap();
    drive_to_answered(&station, &peer, addr);

    assert!(station.send_dtmf('7').is_ok());
    let frames = collect_dtmf_frames(&station, &peer, addr, Duration::from_millis(600));
    assert!(
        frames.is_empty(),
        "in-band (default) mode must send no out-of-band DTMF frames, saw {frames:?}",
    );
}

#[test]
fn protocol_mode_emits_dtmf_begin_end_frames_on_the_wire() {
    // Protocol mode: one digit = one DtmfBegin + one DtmfEnd reliable frame
    // carrying the digit, and nothing is handed to the in-band announce path.
    let station = node_station();
    station.set_mode(OperatingMode::Node).unwrap();
    let addr = station.node_bind_addr().expect("node listener bound");

    let (peer, _pa) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), addr)
        .unwrap();
    drive_to_answered(&station, &peer, addr);

    station.set_dtmf_mode(DtmfMode::Protocol);
    assert!(station.send_dtmf('5').is_ok());
    let frames = collect_dtmf_frames(&station, &peer, addr, Duration::from_millis(1500));
    assert!(
        frames.contains(&(true, '5')),
        "protocol mode must send a DtmfBegin for the digit, saw {frames:?}",
    );
    assert!(
        frames.contains(&(false, '5')),
        "protocol mode must send a DtmfEnd for the digit, saw {frames:?}",
    );
}

// --- iax-4b7a: send_dtmf_string sequencer (compose-then-send) ---

/// Boot a node station, ring it from a fake peer, and drive it to Answered.
/// Returns the station plus the peer socket/addr for wire observation.
fn answered_station() -> (Station, UdpSocket, std::net::SocketAddr) {
    let station = node_station();
    station.set_mode(OperatingMode::Node).unwrap();
    let addr = station.node_bind_addr().expect("node listener bound");
    let (peer, _pa) = peer_socket();
    peer.send_to(&new_datagram(valid_new_ies(), PEER_CALL), addr)
        .unwrap();
    drive_to_answered(&station, &peer, addr);
    (station, peer, addr)
}

#[test]
fn send_dtmf_string_rejects_any_invalid_char_sending_nothing() {
    let (station, peer, addr) = answered_station();
    station.set_dtmf_mode(DtmfMode::Protocol);
    assert!(
        matches!(
            station.send_dtmf_string("*3x5"),
            Err(StationError::InvalidDigit)
        ),
        "one bad character must reject the whole command",
    );
    let frames = collect_dtmf_frames(&station, &peer, addr, Duration::from_millis(600));
    assert!(
        frames.is_empty(),
        "nothing may be sent on validation failure, saw {frames:?}",
    );
}

#[test]
fn send_dtmf_string_rejects_empty() {
    let station = node_station();
    assert!(
        matches!(
            station.send_dtmf_string(""),
            Err(StationError::InvalidDigit)
        ),
        "an empty command has nothing to send",
    );
}

#[test]
fn send_dtmf_string_while_idle_is_not_connected() {
    let station = node_station();
    assert!(
        matches!(
            station.send_dtmf_string("*3"),
            Err(StationError::NotConnected)
        ),
        "a valid command with no active call must fail NotConnected, sending nothing",
    );
    // Nothing queued either: snapshot reports no active sequence.
    let snap = station.snapshot();
    assert_eq!((snap.dtmf_played, snap.dtmf_total), (0, 0));
}

#[test]
fn send_dtmf_string_protocol_mode_emits_all_digits_in_order_on_the_wire() {
    let (station, peer, addr) = answered_station();
    station.set_dtmf_mode(DtmfMode::Protocol);
    station.send_dtmf_string("*30").unwrap();
    // collect_dtmf_frames pumps snapshot() + poll_announcements() — exactly the
    // embedder's poll loop, which is what drives the queue. 3 digits at
    // ~350 ms cadence need ~1.1 s; give slack.
    let frames = collect_dtmf_frames(&station, &peer, addr, Duration::from_millis(2500));
    let begins: Vec<char> = frames.iter().filter(|f| f.0).map(|f| f.1).collect();
    assert_eq!(
        begins,
        vec!['*', '3', '0'],
        "all digits must go out, in order (full frames: {frames:?})",
    );
}

#[test]
fn send_dtmf_string_reports_progress_and_finishes_clean() {
    let (station, peer, addr) = answered_station();
    station.set_dtmf_mode(DtmfMode::Protocol);
    station.send_dtmf_string("*30").unwrap();
    let snap = station.snapshot();
    assert_eq!(snap.dtmf_total, 3, "total = command length while playing");
    assert!(snap.dtmf_played >= 1, "first digit goes out synchronously");
    // Pump to completion (deadline-polling, house style — no fixed sleeps).
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        pump_acks(&peer, addr, PEER_CALL);
        let s = station.snapshot();
        if s.dtmf_total == 0 {
            assert_eq!(s.dtmf_played, 0, "progress must clear with the queue");
            break;
        }
        assert!(s.dtmf_played <= s.dtmf_total);
        assert!(
            Instant::now() < deadline,
            "sequence never completed (stuck at {}/{})",
            s.dtmf_played,
            s.dtmf_total
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn second_send_while_playing_is_busy() {
    let (station, _peer, _addr) = answered_station();
    station.set_dtmf_mode(DtmfMode::Protocol);
    station.send_dtmf_string("123456").unwrap();
    assert!(
        matches!(station.send_dtmf_string("7"), Err(StationError::DtmfBusy)),
        "a second command while one is playing must be rejected busy",
    );
}

#[test]
fn cancel_dtmf_drops_the_unplayed_remainder() {
    let (station, peer, addr) = answered_station();
    station.set_dtmf_mode(DtmfMode::Protocol);
    station.send_dtmf_string("123456").unwrap();
    let _ = station.snapshot(); // give the queue one pump
    station.cancel_dtmf();
    let snap = station.snapshot();
    assert_eq!(
        (snap.dtmf_played, snap.dtmf_total),
        (0, 0),
        "cancel clears the queue and its progress",
    );
    // Only what already played (the synchronous first digit, possibly a second
    // from the single pump) may appear on the wire — never the remainder.
    let frames = collect_dtmf_frames(&station, &peer, addr, Duration::from_millis(1200));
    let begins = frames.iter().filter(|f| f.0).count();
    assert!(
        begins <= 2,
        "the un-played remainder must not be sent, saw {frames:?}",
    );
}

#[test]
fn disconnect_mid_sequence_cancels_the_queue() {
    let (station, _peer, _addr) = answered_station();
    station.set_dtmf_mode(DtmfMode::Protocol);
    station.send_dtmf_string("123456").unwrap();
    station.disconnect();
    let snap = station.snapshot();
    assert_eq!(
        (snap.dtmf_played, snap.dtmf_total),
        (0, 0),
        "teardown must cancel the active sequence",
    );
}

#[test]
fn snapshot_with_no_sequence_reports_zero_progress() {
    // Byte-identical defaults: a station that never calls send_dtmf_string
    // reports 0/0 forever, and the pump does no work.
    let station = node_station();
    let s = station.snapshot();
    assert_eq!((s.dtmf_played, s.dtmf_total), (0, 0));
}

#[test]
fn protocol_mode_validation_matches_in_band() {
    // Mode changes the emission path, not the digit contract: non-keypad
    // characters are still InvalidDigit and idle is still NotConnected.
    let station = node_station();
    station.set_dtmf_mode(DtmfMode::Protocol);
    assert!(
        matches!(station.send_dtmf('x'), Err(StationError::InvalidDigit)),
        "protocol mode must reject a non-DTMF character",
    );
    assert!(
        matches!(station.send_dtmf('5'), Err(StationError::NotConnected)),
        "protocol mode with no active call must fail NotConnected",
    );
}
