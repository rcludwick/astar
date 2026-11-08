// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The packet log through the facade, to real files. `EchoLink` calls run
//! against the loopback fakes, and `AllStar` calls are inbound on a loopback
//! node, as in `tests/record_facade.rs`. Every test build of this crate is
//! loopback-only for `EchoLink`, and the node binds 127.0.0.1, so nothing here
//! leaves this machine.
#![cfg(feature = "echolink")]

use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use astar_console::echolink::{CallLink, EchoLinkNetwork, LoginState, PeerAddr, Timings};
use astar_echolink::fake::{FakeDirectory, FakeLogin, FakeStation};
use astar_iax::{CodecPolicy, IncomingAuthPolicy, IncomingCallPolicy};
use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, FullFrame, encode, is_media, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{FrameType, IaxCommand, VoiceFormat};
use astar_record::pcap::{PcapRecord, read_pcap};
use astar_station::{
    AnswerPolicy, CallStatus, ConsoleSession, DEFAULT_PACKET_LOG_BYTES, DeleteRefused, Layout,
    NodeConfig, OperatingMode, PacketLogConfig, RecordRequest, Station, StationConfig,
    StationError,
};
use serde_json::Value;

const NODE: u32 = 9999;
const PEER_CALL: u16 = 13885;
const WAIT: Duration = Duration::from_secs(5);

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

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("astar-pktlog-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn config(dir: &Path) -> PacketLogConfig {
    PacketLogConfig {
        dir: dir.to_path_buf(),
        max_bytes: DEFAULT_PACKET_LOG_BYTES,
        utc_offset_secs: 0,
        version: "test".to_owned(),
    }
}

/// The folder's pcaps, oldest name first.
fn pcaps(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "pcap"))
        .collect();
    out.sort();
    out
}

fn records(pcap: &Path) -> Vec<PcapRecord> {
    read_pcap(&std::fs::read(pcap).unwrap()).expect("a whole pcap")
}

fn companion_lines(pcap: &Path) -> Vec<Value> {
    let name = pcap.file_name().unwrap().to_str().unwrap();
    let companion = pcap.with_file_name(name.replace(".pcap", ".jitter.jsonl"));
    std::fs::read_to_string(companion)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).expect("one JSON value per line"))
        .collect()
}

/// What the companion's end line says, or `None` for a file with no end.
fn end_of(pcap: &Path) -> Option<String> {
    let lines = companion_lines(pcap);
    lines.last()?.get("end")?.as_str().map(str::to_owned)
}

fn open_calls_reach(station: &Station, n: u32) {
    assert!(
        wait_for(|| station.packet_log_state().open_calls == n),
        "open calls never reached {n}: {:?}",
        station.packet_log_state()
    );
}

// ---- EchoLink over the loopback fakes ---------------------------------------

struct EchoRig {
    fake: FakeStation,
    _dir: FakeDirectory,
    station: Station,
    /// Where the station hears audio.
    ours: PeerAddr,
}

/// Logged in to a fake directory, with the call not yet made. The audio port
/// is picked here, because the facade doesn't say which one it bound.
fn echo_rig() -> EchoRig {
    let fake = FakeStation::start("*ECHOTEST*");
    let dir = FakeDirectory::start(FakeLogin::Ok, vec![fake.entry(NODE)]);
    let station = Station::with_backend_factory(
        StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    let audio = UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let peer = fake.addr();
    station.set_echolink_network(EchoLinkNetwork {
        audio_bind: audio,
        control_bind: "127.0.0.1:0".parse().unwrap(),
        peer_addr: Arc::new(move |_| peer),
        match_by_ip: false,
        timings: Timings {
            keepalive: ms(100),
            connection_timeout: ms(10_000),
            give_up_after: 100,
            talk_hold: ms(100),
            ..Timings::default()
        },
    });
    station
        .echolink_login("KX7ABC", "s3cret-pw".to_owned(), &[dir.addr().to_string()])
        .expect("login starts");
    assert!(
        wait_for(|| {
            station.echolink_state().map(|s| s.login.state) == Some(LoginState::LoggedIn)
                && !station.echolink_stations().is_empty()
        }),
        "never logged in"
    );
    EchoRig {
        fake,
        _dir: dir,
        station,
        ours: PeerAddr {
            audio,
            control: audio,
        },
    }
}

impl EchoRig {
    fn call(&self) {
        self.station
            .echolink_connect(&NODE.to_string())
            .expect("connect");
        assert!(
            wait_for(|| {
                let call = self.station.echolink_state().and_then(|s| s.call);
                call.map(|c| c.link) == Some(CallLink::Connected)
            }),
            "never connected"
        );
    }

    /// One 144-byte packet from the far station.
    fn talk(&self) {
        self.fake.send_audio(self.ours, 4);
    }

    /// Talks until the log has a file open for the call.
    fn talk_until_open(&self) {
        assert!(
            wait_for(|| {
                self.talk();
                thread::sleep(ms(20));
                self.station.packet_log_state().open_calls == 1
            }),
            "no file opened: {:?}",
            self.station.packet_log_state()
        );
    }

    fn hang_up(&self) {
        self.station.echolink_disconnect();
        open_calls_reach(&self.station, 0);
    }
}

#[test]
fn an_echolink_call_writes_a_pcap_and_jitter_rows() {
    let dir = scratch("echolink");
    let r = echo_rig();
    r.station.packet_log_configure(Some(config(&dir))).unwrap();
    assert!(r.station.packet_log_state().capturing);
    r.call();

    // Past three of the sampler's seconds, at the pace of real audio.
    let started = Instant::now();
    let mut sent = 0;
    while started.elapsed() < ms(3_300) {
        r.talk();
        sent += 1;
        thread::sleep(ms(80));
    }
    r.hang_up();
    r.station.packet_log_configure(None).unwrap();

    let logs = pcaps(&dir);
    assert_eq!(logs.len(), 1, "{logs:?}");
    let packets = records(&logs[0]);
    assert_eq!(packets.len(), sent);
    assert!(packets.iter().all(|p| p.payload.len() == 144));
    assert!(packets.iter().all(|p| p.src == r.fake.addr().audio));
    assert!(packets.iter().all(|p| p.dst == r.ours.audio));

    let lines = companion_lines(&logs[0]);
    assert_eq!(lines[0]["network"], "echolink");
    let rows: Vec<&Value> = lines
        .iter()
        .filter(|l| l.get("depth_ms").is_some())
        .collect();
    assert!(rows.len() >= 2, "{lines:?}");
    assert!(
        rows.iter().all(|row| row["depth_ms"].is_u64()),
        "a call with a jitter buffer has a depth: {rows:?}"
    );
    for pair in rows.windows(2) {
        let gap = pair[1]["ts_ns"].as_u64().unwrap() - pair[0]["ts_ns"].as_u64().unwrap();
        assert!(
            (900_000_000..2_500_000_000).contains(&gap),
            "rows about a second apart, got {gap} ns"
        );
    }
    assert_eq!(end_of(&logs[0]).as_deref(), Some("hangup"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn turning_on_mid_call_opens_a_file_and_off_closes_it() {
    let dir = scratch("midcall");
    let r = echo_rig();
    r.call();
    r.talk();
    thread::sleep(ms(100));
    assert!(pcaps(&dir).is_empty(), "off means no files");

    r.station.packet_log_configure(Some(config(&dir))).unwrap();
    r.talk_until_open();
    r.station.packet_log_configure(None).unwrap();
    let state = r.station.packet_log_state();
    assert!(!state.capturing);
    assert_eq!(state.open_calls, 0);
    let logs = pcaps(&dir);
    assert_eq!(logs.len(), 1);
    assert_ne!(records(&logs[0]), []);
    assert_eq!(end_of(&logs[0]).as_deref(), Some("stopped"));

    // Off leaves the call alone, and on again gives it a file of its own.
    assert!(r.station.echolink_state().unwrap().call.is_some());
    r.station.packet_log_configure(Some(config(&dir))).unwrap();
    r.talk_until_open();
    r.hang_up();
    r.station.packet_log_configure(None).unwrap();
    let logs = pcaps(&dir);
    assert_eq!(logs.len(), 2, "{logs:?}");
    let mut ends: Vec<_> = logs.iter().filter_map(|p| end_of(p)).collect();
    ends.sort();
    assert_eq!(ends, ["hangup", "stopped"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_capture_holds_only_what_arrived_while_pressed() {
    let dir = scratch("pressed");
    let r = echo_rig();
    r.call();
    r.talk();
    r.station.packet_log_configure(Some(config(&dir))).unwrap();
    r.talk_until_open();
    r.station.packet_log_configure(None).unwrap();
    r.talk();
    thread::sleep(ms(100));

    let logs = pcaps(&dir);
    assert_eq!(logs.len(), 1, "{logs:?}");
    let lines = companion_lines(&logs[0]);
    let end = lines.last().unwrap();
    assert_ne!(records(&logs[0]), []);
    assert_eq!(end["packets"], records(&logs[0]).len());
    assert_eq!(end_of(&logs[0]).as_deref(), Some("stopped"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn two_presses_in_one_call_make_two_logs() {
    let dir = scratch("two-presses");
    let r = echo_rig();
    r.call();
    for _ in 0..2 {
        r.station.packet_log_configure(Some(config(&dir))).unwrap();
        r.talk_until_open();
        r.station.packet_log_configure(None).unwrap();
    }
    let logs = pcaps(&dir);
    assert_eq!(logs.len(), 2, "{logs:?}");
    for log in &logs {
        assert_eq!(end_of(log).as_deref(), Some("stopped"));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ending_for_the_call_writes_hangup_with_the_call_still_up() {
    let dir = scratch("call-ended");
    let r = echo_rig();
    r.call();
    r.station.packet_log_configure(Some(config(&dir))).unwrap();
    r.talk_until_open();

    r.station.packet_log_call_ended();

    assert!(r.station.echolink_state().unwrap().call.is_some());
    assert!(!r.station.packet_log_state().capturing);
    let logs = pcaps(&dir);
    assert_eq!(logs.len(), 1, "{logs:?}");
    assert_eq!(end_of(&logs[0]).as_deref(), Some("hangup"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn call_ended_with_no_capture_does_nothing() {
    let dir = scratch("call-ended-idle");
    let r = echo_rig();
    r.call();
    let before = r.station.packet_log_state();
    r.station.packet_log_call_ended();
    assert_eq!(r.station.packet_log_state(), before);

    // A finished capture stays as it ended, and the next one still starts.
    r.station.packet_log_configure(Some(config(&dir))).unwrap();
    r.talk_until_open();
    r.station.packet_log_configure(None).unwrap();
    let before = r.station.packet_log_state();
    r.station.packet_log_call_ended();
    assert_eq!(r.station.packet_log_state(), before);
    assert_eq!(end_of(&pcaps(&dir)[0]).as_deref(), Some("stopped"));
    r.station.packet_log_configure(Some(config(&dir))).unwrap();
    assert!(r.station.packet_log_state().capturing);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_call_after_call_ended_is_not_captured() {
    let dir = scratch("next-call");
    let r = echo_rig();
    r.call();
    r.station.packet_log_configure(Some(config(&dir))).unwrap();
    r.talk_until_open();
    r.station.packet_log_call_ended();
    r.hang_up();

    r.call();
    r.talk();
    thread::sleep(ms(100));
    assert_eq!(pcaps(&dir).len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_capture_with_no_packets_leaves_no_file() {
    let dir = scratch("silent");
    let r = echo_rig();
    r.call();
    r.station.packet_log_configure(Some(config(&dir))).unwrap();
    r.station.packet_log_configure(None).unwrap();
    assert_eq!(pcaps(&dir), Vec::<PathBuf>::new());
    assert_eq!(r.station.packet_log_state().error, None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn configuring_the_same_settings_leaves_a_live_file_alone() {
    let dir = scratch("same");
    let r = echo_rig();
    r.station.packet_log_configure(Some(config(&dir))).unwrap();
    r.call();
    r.talk_until_open();
    r.station.packet_log_configure(Some(config(&dir))).unwrap();
    r.talk();
    thread::sleep(ms(100));
    r.hang_up();
    r.station.packet_log_configure(None).unwrap();
    let logs = pcaps(&dir);
    assert_eq!(logs.len(), 1, "the file was cut: {logs:?}");
    assert_eq!(end_of(&logs[0]).as_deref(), Some("hangup"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn configure_twice_moves_to_the_new_folder() {
    let (first, second) = (scratch("move-a"), scratch("move-b"));
    let r = echo_rig();
    r.station
        .packet_log_configure(Some(config(&first)))
        .unwrap();
    r.call();
    r.talk_until_open();

    r.station
        .packet_log_configure(Some(config(&second)))
        .unwrap();
    let old = pcaps(&first);
    assert_eq!(old.len(), 1);
    assert_eq!(
        end_of(&old[0]).as_deref(),
        Some("stopped"),
        "the old logger has finished before the new one starts"
    );
    r.talk_until_open();
    let rows = r.station.packet_log_list();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].open);
    assert_eq!(
        rows[0].name,
        pcaps(&second)[0].file_name().unwrap().to_str().unwrap()
    );

    r.hang_up();
    r.station.packet_log_configure(None).unwrap();
    assert_eq!(end_of(&pcaps(&second)[0]).as_deref(), Some("hangup"));
    let _ = std::fs::remove_dir_all(&first);
    let _ = std::fs::remove_dir_all(&second);
}

#[test]
fn a_new_limit_in_the_same_folder_keeps_the_old_file() {
    let dir = scratch("limit");
    let r = echo_rig();
    r.station.packet_log_configure(Some(config(&dir))).unwrap();
    r.call();
    r.talk_until_open();

    let smaller = PacketLogConfig {
        max_bytes: DEFAULT_PACKET_LOG_BYTES / 2,
        ..config(&dir)
    };
    r.station.packet_log_configure(Some(smaller)).unwrap();
    r.talk_until_open();
    r.hang_up();
    r.station.packet_log_configure(None).unwrap();
    let logs = pcaps(&dir);
    assert_eq!(logs.len(), 2, "{logs:?}");
    let mut ends: Vec<_> = logs.iter().filter_map(|p| end_of(p)).collect();
    ends.sort();
    assert_eq!(ends, ["hangup", "stopped"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_open_log_cant_be_deleted_and_a_closed_one_can() {
    let dir = scratch("delete");
    let r = echo_rig();
    assert!(r.station.packet_log_list().is_empty(), "no folder yet");
    assert_eq!(
        r.station.packet_log_delete("nothing.pcap"),
        Err(DeleteRefused::NoSuchLog)
    );

    r.station.packet_log_configure(Some(config(&dir))).unwrap();
    r.call();
    r.talk_until_open();
    let name = r.station.packet_log_list()[0].name.clone();
    assert_eq!(r.station.packet_log_delete(&name), Err(DeleteRefused::Open));

    // The folder stays known once the capture is over: its logs still list.
    r.hang_up();
    r.station.packet_log_configure(None).unwrap();
    let rows = r.station.packet_log_list();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].open);
    assert_eq!(r.station.packet_log_delete(&name), Ok(()));
    assert_eq!(r.station.packet_log_list(), []);
    assert_eq!(pcaps(&dir), Vec::<PathBuf>::new());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stop with nothing running leaves the refusal in place, so only the two
/// ways out below can clear it.
#[test]
fn a_refusal_lasts_until_the_next_start_or_a_clear() {
    let good = scratch("clear-good");
    std::fs::create_dir_all(&good).unwrap();
    let file = good.join("a-file");
    std::fs::write(&file, b"not a folder").unwrap();
    let bad = config(&file.join("logs"));

    let r = echo_rig();
    assert!(r.station.packet_log_configure(Some(bad.clone())).is_err());
    r.station.packet_log_configure(None).unwrap();
    assert!(r.station.packet_log_state().error.is_some());

    r.station.packet_log_clear_error();
    assert_eq!(r.station.packet_log_state().error, None);

    assert!(r.station.packet_log_configure(Some(bad)).is_err());
    r.station.packet_log_configure(Some(config(&good))).unwrap();
    r.station.packet_log_configure(None).unwrap();
    assert_eq!(
        r.station.packet_log_state().error,
        None,
        "the capture that followed ran clean"
    );
    let _ = std::fs::remove_dir_all(&good);
}

#[test]
fn a_bad_folder_is_refused_and_leaves_no_capture() {
    let good = scratch("bad-good");
    std::fs::create_dir_all(&good).unwrap();
    let file = good.join("a-file");
    std::fs::write(&file, b"not a folder").unwrap();
    let bad = config(&file.join("logs"));

    let r = echo_rig();
    let refused = r.station.packet_log_configure(Some(bad.clone()));
    assert!(
        matches!(refused, Err(StationError::Record(_))),
        "{refused:?}"
    );
    let state = r.station.packet_log_state();
    assert!(!state.capturing);
    assert!(state.error.is_some(), "the state says why");

    // Running first changes nothing about that: the old logger has to stop
    // before a new one can be tried, so a refusal leaves no capture either.
    r.station.packet_log_configure(Some(config(&good))).unwrap();
    r.call();
    r.talk_until_open();
    assert!(r.station.packet_log_configure(Some(bad)).is_err());
    assert!(!r.station.packet_log_state().capturing);
    assert_eq!(end_of(&pcaps(&good)[0]).as_deref(), Some("stopped"));
    assert_eq!(r.station.packet_log_list().len(), 1, "the last good folder");

    // And the switch still works afterwards.
    r.station.packet_log_configure(Some(config(&good))).unwrap();
    assert!(r.station.packet_log_state().capturing);
    assert_eq!(r.station.packet_log_state().error, None);
    r.talk_until_open();
    let _ = std::fs::remove_dir_all(&good);
}

#[test]
fn configures_from_two_threads_take_turns() {
    let (first, second) = (scratch("turns-a"), scratch("turns-b"));
    let station = Station::with_backend_factory(
        StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    thread::scope(|scope| {
        for dir in [&first, &second] {
            let station = &station;
            scope.spawn(move || {
                for _ in 0..5 {
                    station.packet_log_configure(Some(config(dir))).unwrap();
                    let _ = station.packet_log_list();
                    station.packet_log_configure(None).unwrap();
                }
            });
        }
    });
    assert!(!station.packet_log_state().capturing);
    let _ = std::fs::remove_dir_all(&first);
    let _ = std::fs::remove_dir_all(&second);
}

// ---- AllStar, inbound on a loopback node ------------------------------------

fn enc(frame: &Frame<'_>) -> Vec<u8> {
    let mut b = Vec::new();
    encode(frame, &mut b).expect("encode");
    b
}

fn new_datagram() -> Vec<u8> {
    enc(&Frame::Full(Box::new(FullFrame {
        source_call: PEER_CALL,
        dest_call: 0,
        retransmission: false,
        timestamp: 0,
        oseqno: 0,
        iseqno: 0,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::New),
        ies: Ies {
            called_number: Some("s"),
            calling_number: Some("1001"),
            calling_name: Some("Rob"),
            capability: Some(VoiceFormat::G711U.as_u32() | VoiceFormat::G711A.as_u32()),
            format: Some(VoiceFormat::G711U.as_u32()),
            version: Some(2),
            calltoken: Some(b""),
            ..Ies::empty()
        },
        payload: &[],
    })))
}

/// Acknowledges everything the node sends: control traffic the tap must keep
/// out of the log.
fn pump_acks(sock: &UdpSocket, node: SocketAddr) {
    let mut buf = [0u8; 4096];
    while let Ok((n, _src)) = sock.recv_from(&mut buf) {
        let Ok(Frame::Full(f)) = parse_lenient(&buf[..n]) else {
            continue;
        };
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
            let _ = sock.send_to(&ack, node);
        }
    }
}

/// A node on a session the test shares.
fn node_station() -> (Station, Arc<Mutex<ConsoleSession>>) {
    let cfg = StationConfig {
        mode: OperatingMode::Node,
        codec_policy: CodecPolicy::PreferSlin16,
        node: Some(NodeConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            policy: IncomingCallPolicy {
                auth: IncomingAuthPolicy::Off,
                ..IncomingCallPolicy::default()
            },
            answer: AnswerPolicy::Auto,
            register: None,
            max_calls: 20,
            allowlist: None,
        }),
        ..StationConfig::default()
    };
    let session = Arc::new(Mutex::new(ConsoleSession::new()));
    let station = Station::with_shared_session(
        cfg,
        Arc::clone(&session),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    (station, session)
}

/// A caller the node has answered.
struct Caller {
    sock: UdpSocket,
    node: SocketAddr,
}

fn answered_caller(station: &Station) -> Caller {
    let node = station.node_bind_addr().expect("node listener bound");
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(ms(20))).unwrap();
    sock.send_to(&new_datagram(), node).unwrap();
    assert!(
        wait_for(|| {
            pump_acks(&sock, node);
            station.snapshot().status == CallStatus::Answered
        }),
        "the loopback call must be answered"
    );
    Caller { sock, node }
}

impl Caller {
    /// `n` mini voice frames, paced, with the node's traffic acknowledged.
    fn talk(&self, n: u16) {
        for i in 0..n {
            let mut frame = PEER_CALL.to_be_bytes().to_vec();
            frame.extend_from_slice(&(i * 20).to_be_bytes());
            frame.extend_from_slice(&[0xff; 160]);
            self.sock.send_to(&frame, self.node).unwrap();
            pump_acks(&self.sock, self.node);
        }
    }
}

#[test]
fn an_allstar_call_writes_only_voice() {
    let dir = scratch("allstar");
    let (station, _session) = node_station();
    station.packet_log_configure(Some(config(&dir))).unwrap();
    let caller = answered_caller(&station);
    caller.talk(10);
    open_calls_reach(&station, 1);
    station.disconnect();
    open_calls_reach(&station, 0);
    station.packet_log_configure(None).unwrap();

    let logs = pcaps(&dir);
    assert_eq!(logs.len(), 1, "{logs:?}");
    let packets = records(&logs[0]);
    assert_eq!(packets.len(), 10);
    assert!(packets.iter().all(|p| is_media(&p.payload)));
    assert_eq!(end_of(&logs[0]).as_deref(), Some("hangup"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_log_runs_without_record_and_beside_it() {
    for recording in [false, true] {
        let dir = scratch(&format!("beside-{recording}"));
        let (station, _session) = node_station();
        station.packet_log_configure(Some(config(&dir))).unwrap();
        let caller = answered_caller(&station);
        caller.talk(3);
        if recording {
            station
                .record_start(RecordRequest {
                    dir: dir.join("recordings"),
                    layout: Layout::Stereo,
                    frames: false,
                    label: "AllStar 1001".to_owned(),
                    utc_offset_secs: 0,
                })
                .expect("recording starts");
        }
        caller.talk(4);
        if recording {
            assert_eq!(station.record_stop().files.len(), 1);
        }
        caller.talk(3);
        open_calls_reach(&station, 1);
        station.disconnect();
        open_calls_reach(&station, 0);
        station.packet_log_configure(None).unwrap();

        let logs = pcaps(&dir);
        assert_eq!(logs.len(), 1, "recording={recording}: {logs:?}");
        assert_eq!(records(&logs[0]).len(), 10, "recording={recording}");
        assert_eq!(end_of(&logs[0]).as_deref(), Some("hangup"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn dropping_the_station_ends_the_files() {
    let dir = scratch("drop");
    let (station, session) = node_station();
    let weak_before = Arc::weak_count(&session);
    station.packet_log_configure(Some(config(&dir))).unwrap();
    assert_eq!(
        Arc::weak_count(&session),
        weak_before + 1,
        "the sampler watches the session"
    );
    let caller = answered_caller(&station);
    caller.talk(5);
    open_calls_reach(&station, 1);

    drop(station);

    assert_eq!(
        Arc::weak_count(&session),
        weak_before,
        "the sampler has gone"
    );
    let logs = pcaps(&dir);
    assert_eq!(logs.len(), 1);
    assert_eq!(records(&logs[0]).len(), 5);
    assert_eq!(end_of(&logs[0]).as_deref(), Some("hangup"));
    let _ = std::fs::remove_dir_all(&dir);
}
