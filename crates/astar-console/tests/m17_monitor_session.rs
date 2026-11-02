// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `ConsoleSession`'s receive-only M17 monitor slot, end to end.
//!
//! Every socket here is on 127.0.0.1. The far side is either the in-process
//! `astar_m17::Reflector`, or a raw `UdpSocket` that records every datagram
//! the monitor sends and answers CONN with ACKN or NACK. Waits are deadline
//! loops, never one fixed-timeout read.
//!
//! The session is built the way the node builds it: `set_bridge_config(Bridge)`
//! and then `start_inbound` on 127.0.0.1:0 over a `NullBackend`. That is what
//! gives the monitor a conference to contribute into. The Codec 2 decoder
//! comes from the crate's `codec2-static` dev-dependency, as in
//! `m17_session.rs`.
#![cfg(feature = "m17")]

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use astar_audio::{AudioBackend, NullBackend};
use astar_console::{
    AnswerPolicy, ConsoleError, ConsoleSession, M17Monitor, M17MonitorConfig, M17MonitorEdge,
    RecordTaps,
};
use astar_iax::{BridgeConfig, BridgeMode, IncomingCallPolicy};
use astar_m17::{BROADCAST, ControlPacket, Lsf, Reflector, StreamPacket, encode_callsign};
use astar_record::frames::{CaptureMode, Dir, FrameEvent, Seq};
use astar_record::tap::{FRAME_TAP_CAPACITY, tap_channel};

const CALLSIGN: &str = "AJ7HR";

fn loopback() -> SocketAddr {
    "127.0.0.1:0".parse().expect("loopback addr")
}

fn wait_until(mut pred: impl FnMut() -> bool, timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    pred()
}

fn monitor_cfg(addr: SocketAddr) -> M17MonitorConfig {
    M17MonitorConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        module: b'A',
        callsign: CALLSIGN.to_string(),
        codec_dirs: Vec::new(),
        keepalive_timeout: Duration::from_secs(30),
    }
}

/// A session with an engine in `mode`, built the node's way.
fn session_in(mode: BridgeMode) -> ConsoleSession {
    let mut s = ConsoleSession::new();
    s.set_bridge_config(BridgeConfig {
        mode,
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
    s
}

/// Poll until an edge appears or `timeout_ms` passes; returns that poll's result.
fn poll_for_edge(
    s: &mut ConsoleSession,
    timeout_ms: u64,
) -> (Option<M17MonitorEdge>, Option<M17Monitor>) {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let (edge, reaped) = s.m17_monitor_poll();
        if edge.is_some() || Instant::now() >= deadline {
            return (edge, reaped);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn is_linked(s: &ConsoleSession) -> bool {
    s.m17_monitor_status().is_some_and(|st| st.link == "linked")
}

fn disc() -> ControlPacket {
    ControlPacket::Disc {
        callsign: Some(encode_callsign(CALLSIGN).expect("callsign encodes")),
    }
}

/// Every datagram the monitor sent is a CONN, PONG or DISC, and none is a
/// stream packet.
fn assert_only_control(datagrams: &[Vec<u8>]) {
    assert!(!datagrams.is_empty(), "the monitor sent nothing at all");
    for d in datagrams {
        assert!(
            StreamPacket::parse(d).is_none(),
            "a {}-byte stream packet left the monitor",
            d.len()
        );
        assert!(
            matches!(
                ControlPacket::parse(d),
                Some(
                    ControlPacket::Conn { .. }
                        | ControlPacket::Pong { .. }
                        | ControlPacket::Disc { .. }
                )
            ),
            "a {}-byte datagram that is not CONN/PONG/DISC left the monitor",
            d.len()
        );
    }
}

#[derive(Clone, Copy)]
enum Reply {
    Ackn,
    Nack,
}

/// A raw fake reflector: records every datagram, answers CONN with `reply`.
struct Recorder {
    addr: SocketAddr,
    log: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    fn start(reply: Reply) -> Recorder {
        let socket = UdpSocket::bind(loopback()).expect("bind recorder");
        socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .expect("recorder read timeout");
        let addr = socket.local_addr().expect("recorder addr");
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (thread_log, thread_stop) = (Arc::clone(&log), Arc::clone(&stop));
        let thread = thread::spawn(move || {
            let mut buf = [0u8; 128];
            while !thread_stop.load(Ordering::Relaxed) {
                let Ok((n, src)) = socket.recv_from(&mut buf) else {
                    continue;
                };
                thread_log.lock().expect("log").push(buf[..n].to_vec());
                if matches!(
                    ControlPacket::parse(&buf[..n]),
                    Some(ControlPacket::Conn { .. })
                ) {
                    let answer = match reply {
                        Reply::Ackn => ControlPacket::Ackn,
                        Reply::Nack => ControlPacket::Nack,
                    };
                    let _ = socket.send_to(&answer.to_bytes(), src);
                }
            }
        });
        Recorder {
            addr,
            log,
            stop,
            thread: Some(thread),
        }
    }

    fn datagrams(&self) -> Vec<Vec<u8>> {
        self.log.lock().expect("log").clone()
    }

    fn last_is_disc(&self) -> bool {
        self.datagrams()
            .last()
            .and_then(|d| ControlPacket::parse(d.as_slice()))
            == Some(disc())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[test]
fn connect_without_an_engine_is_refused_as_engine_not_built() {
    let mut s = ConsoleSession::new();
    s.set_bridge_config(BridgeConfig {
        mode: BridgeMode::Bridge,
        ..BridgeConfig::default()
    })
    .expect("stored for the engine that does not exist yet");
    let target = UdpSocket::bind(loopback()).expect("bind unused target");
    let err = s
        .m17_monitor_connect(monitor_cfg(target.local_addr().expect("addr")))
        .unwrap_err();
    assert_eq!(err.to_string(), "m17: no conference: engine not built");
    assert!(s.m17_monitor_status().is_none());
    assert!(!s.has_engine(), "a refusal must not build an engine");
}

#[test]
fn validation_runs_before_the_engine_check() {
    let mut s = ConsoleSession::new();
    let target = UdpSocket::bind(loopback()).expect("bind unused target");
    let mut cfg = monitor_cfg(target.local_addr().expect("addr"));
    cfg.callsign = "TOOLONGCALL".to_string();
    let err = s.m17_monitor_connect(cfg).unwrap_err();
    assert_eq!(err.to_string(), "m17: invalid M17 callsign \"TOOLONGCALL\"");
}

#[test]
fn a_bad_callsign_is_refused_before_any_member_is_enrolled() {
    let mut s = session_in(BridgeMode::Bridge);
    let target = UdpSocket::bind(loopback()).expect("bind unused target");
    let mut cfg = monitor_cfg(target.local_addr().expect("addr"));
    cfg.callsign = "TOOLONGCALL".to_string();
    let err = s.m17_monitor_connect(cfg).unwrap_err();
    assert_eq!(err.to_string(), "m17: invalid M17 callsign \"TOOLONGCALL\"");
    assert_eq!(s.conference_member_count(), 0);
    assert!(s.m17_monitor_status().is_none());
}

#[test]
fn handset_and_parrot_refuse_with_the_topology_error_and_enrol_nobody() {
    for mode in [BridgeMode::Handset, BridgeMode::Parrot] {
        let mut s = session_in(mode);
        let before = s.conference_member_count();
        let target = UdpSocket::bind(loopback()).expect("bind unused target");
        let err = s
            .m17_monitor_connect(monitor_cfg(target.local_addr().expect("addr")))
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "m17: no conference: bridge topology is handset/parrot",
            "{mode:?}"
        );
        assert_eq!(s.conference_member_count(), before, "{mode:?}");
        assert!(s.m17_monitor_status().is_none(), "{mode:?}");
    }
}

#[test]
fn a_second_connect_is_already_connected_before_validation() {
    let reflector = Reflector::bind(loopback()).expect("bind reflector");
    let addr = reflector.local_addr();
    let _refl = reflector.run();
    let mut s = session_in(BridgeMode::Bridge);
    s.m17_monitor_connect(monitor_cfg(addr))
        .expect("first monitor");
    let mut bad = monitor_cfg(addr);
    bad.callsign = "TOOLONGCALL".to_string();
    assert!(matches!(
        s.m17_monitor_connect(bad),
        Err(ConsoleError::AlreadyConnected)
    ));
    assert_eq!(s.conference_member_count(), 1, "no second member enrolled");
    s.m17_monitor_take().expect("leg").disconnect();
}

#[test]
fn poll_reports_linked_once_then_nothing_and_take_withdraws_and_discs() {
    let reflector = Reflector::bind(loopback()).expect("bind reflector");
    let addr = reflector.local_addr();
    let refl = reflector.run();
    let mut s = session_in(BridgeMode::Bridge);

    s.m17_monitor_connect(monitor_cfg(addr))
        .expect("monitor up");
    assert_eq!(s.conference_member_count(), 1, "one source member enrolled");

    let (edge, reaped) = poll_for_edge(&mut s, 3000);
    assert_eq!(edge, Some(M17MonitorEdge::Linked));
    assert!(reaped.is_none());
    let status = s.m17_monitor_status().expect("status while live");
    assert_eq!(status.link, "linked");
    assert_eq!(status.fail_reason, None);
    assert_eq!(status.refused_sends, 0);
    assert!(wait_until(|| refl.client_count() == 1, 2000));

    for _ in 0..5 {
        let (edge, reaped) = s.m17_monitor_poll();
        assert_eq!(edge, None, "Linked is reported once");
        assert!(reaped.is_none());
        thread::sleep(Duration::from_millis(10));
    }

    let monitor = s.m17_monitor_take().expect("the leg is handed out");
    assert_eq!(s.conference_member_count(), 0, "take withdraws the member");
    assert!(s.m17_monitor_status().is_none());
    monitor.disconnect();
    assert!(
        wait_until(|| refl.client_count() == 0, 2000),
        "the monitor's DISC unlinks it from the reflector"
    );
    assert!(s.m17_monitor_take().is_none());
    let (edge, reaped) = s.m17_monitor_poll();
    assert_eq!(edge, None);
    assert!(reaped.is_none());
}

#[test]
fn switching_to_handset_reaps_as_conference_gone_ahead_of_a_pending_linked() {
    let reflector = Reflector::bind(loopback()).expect("bind reflector");
    let addr = reflector.local_addr();
    let refl = reflector.run();
    let mut s = session_in(BridgeMode::Bridge);
    s.m17_monitor_connect(monitor_cfg(addr))
        .expect("monitor up");
    // Wait on status, not poll, so the Linked edge is still pending.
    assert!(wait_until(|| is_linked(&s), 3000));
    assert!(wait_until(|| refl.client_count() == 1, 2000));

    s.set_bridge_config(BridgeConfig {
        mode: BridgeMode::Handset,
        ..BridgeConfig::default()
    })
    .expect("switch to handset");

    let (edge, reaped) = s.m17_monitor_poll();
    assert_eq!(
        edge,
        Some(M17MonitorEdge::Ended("conference gone".to_string())),
        "a reap wins over a same-poll Linked"
    );
    let monitor = reaped.expect("the reaped monitor is handed out for an off-lock join");
    assert!(s.m17_monitor_status().is_none());
    monitor.disconnect();
    assert!(
        wait_until(|| refl.client_count() == 0, 2000),
        "the reaped monitor sent DISC"
    );
    let (edge, reaped) = s.m17_monitor_poll();
    assert_eq!(edge, None);
    assert!(reaped.is_none());
}

#[test]
fn switching_to_parrot_reaps_and_withdraws_the_member() {
    let reflector = Reflector::bind(loopback()).expect("bind reflector");
    let addr = reflector.local_addr();
    let refl = reflector.run();
    let mut s = session_in(BridgeMode::Bridge);
    s.m17_monitor_connect(monitor_cfg(addr))
        .expect("monitor up");
    assert_eq!(poll_for_edge(&mut s, 3000).0, Some(M17MonitorEdge::Linked));

    s.set_bridge_config(BridgeConfig {
        mode: BridgeMode::Parrot,
        ..BridgeConfig::default()
    })
    .expect("switch to parrot");
    assert_eq!(
        s.conference_member_count(),
        1,
        "Parrot keeps the conference and the member: only the mode check can reap"
    );

    let (edge, reaped) = s.m17_monitor_poll();
    assert_eq!(
        edge,
        Some(M17MonitorEdge::Ended("conference gone".to_string()))
    );
    assert_eq!(
        s.conference_member_count(),
        0,
        "the reap withdraws the member"
    );
    reaped.expect("reaped monitor").disconnect();
    assert!(wait_until(|| refl.client_count() == 0, 2000));
}

#[test]
fn a_nack_is_reaped_with_the_monitors_own_reason() {
    let rec = Recorder::start(Reply::Nack);
    let mut s = session_in(BridgeMode::Bridge);
    s.m17_monitor_connect(monitor_cfg(rec.addr))
        .expect("monitor spawned");

    let (edge, reaped) = poll_for_edge(&mut s, 3000);
    assert_eq!(
        edge,
        Some(M17MonitorEdge::Ended(
            "reflector refused the link".to_string()
        ))
    );
    reaped.expect("reaped monitor").disconnect();
    assert_eq!(s.conference_member_count(), 0);
    assert!(s.m17_monitor_status().is_none());
    assert_only_control(&rec.datagrams());
}

#[test]
fn set_ptt_with_only_a_monitor_up_is_not_connected_and_only_control_leaves() {
    let rec = Recorder::start(Reply::Ackn);
    let mut s = session_in(BridgeMode::Bridge);
    s.m17_monitor_connect(monitor_cfg(rec.addr))
        .expect("monitor up");
    assert_eq!(poll_for_edge(&mut s, 3000).0, Some(M17MonitorEdge::Linked));

    assert!(matches!(s.set_ptt(true), Err(ConsoleError::NotConnected)));
    assert!(matches!(s.set_ptt(false), Err(ConsoleError::NotConnected)));
    assert!(!s.snapshot().ptt);

    s.m17_monitor_take().expect("leg").disconnect();
    assert!(
        wait_until(|| rec.last_is_disc(), 2000),
        "DISC is the last datagram"
    );
    assert_only_control(&rec.datagrams());
}

#[test]
fn the_snapshot_never_reports_the_monitor_as_the_m17_client() {
    let reflector = Reflector::bind(loopback()).expect("bind reflector");
    let addr = reflector.local_addr();
    let _refl = reflector.run();
    let mut s = session_in(BridgeMode::Bridge);
    s.m17_monitor_connect(monitor_cfg(addr))
        .expect("monitor up");
    assert_eq!(poll_for_edge(&mut s, 3000).0, Some(M17MonitorEdge::Linked));

    let snap = s.snapshot();
    assert!(
        !snap.m17_active,
        "m17_active means the console's own M17 client, which is not up"
    );
    assert!(!snap.ptt);
    s.m17_monitor_take().expect("leg").disconnect();
}

#[test]
fn a_conference_rebuilt_between_polls_reaps_as_conference_gone() {
    let reflector = Reflector::bind(loopback()).expect("bind reflector");
    let addr = reflector.local_addr();
    let refl = reflector.run();
    let mut s = session_in(BridgeMode::Bridge);
    s.m17_monitor_connect(monitor_cfg(addr))
        .expect("monitor up");
    assert_eq!(poll_for_edge(&mut s, 3000).0, Some(M17MonitorEdge::Linked));
    assert!(wait_until(|| refl.client_count() == 1, 2000));

    // Two `POST /bridge` calls with no poll between them: the old conference
    // (and the member in it) is dropped and a fresh one is built. The mode is
    // Bridge and the conference is active again, and a quiet reflector never
    // trips `sink_lost`, so only the `enrolled` term can see the leg is dead.
    s.set_bridge_config(BridgeConfig {
        mode: BridgeMode::Handset,
        ..BridgeConfig::default()
    })
    .expect("switch to handset");
    s.set_bridge_config(BridgeConfig {
        mode: BridgeMode::Bridge,
        ..BridgeConfig::default()
    })
    .expect("switch back to bridge");

    let (edge, reaped) = s.m17_monitor_poll();
    assert_eq!(
        edge,
        Some(M17MonitorEdge::Ended("conference gone".to_string()))
    );
    assert_eq!(s.conference_member_count(), 0);
    assert!(s.m17_monitor_status().is_none());
    reaped.expect("reaped monitor").disconnect();
    assert!(
        wait_until(|| refl.client_count() == 0, 2000),
        "the reaped monitor sent DISC"
    );
}

#[test]
fn heard_names_a_far_talker_on_the_module() {
    let reflector = Reflector::bind(loopback()).expect("bind reflector");
    let addr = reflector.local_addr();
    let refl = reflector.run();

    let far = UdpSocket::bind(loopback()).expect("bind far talker");
    let conn = ControlPacket::Conn {
        callsign: encode_callsign("KC1ABC").expect("far callsign"),
        module: b'A',
    };
    far.send_to(&conn.to_bytes(), addr).expect("far CONN");
    assert!(wait_until(|| refl.client_count() == 1, 2000));

    let mut s = session_in(BridgeMode::Bridge);
    assert!(s.m17_monitor_heard().is_empty(), "no leg, no history");
    s.m17_monitor_connect(monitor_cfg(addr))
        .expect("monitor up");
    assert_eq!(poll_for_edge(&mut s, 3000).0, Some(M17MonitorEdge::Linked));
    assert!(wait_until(|| refl.client_count() == 2, 2000));
    assert!(s.m17_monitor_heard().is_empty(), "nobody has talked yet");

    for n in [0u16, 1, 2, 3 | 0x8000] {
        let pkt = StreamPacket {
            stream_id: 0x4242,
            lsf: Lsf {
                dst: BROADCAST,
                src: encode_callsign("KC1ABC").expect("far callsign"),
                type_field: Lsf::TYPE_VOICE_3200_STREAM,
                meta: [0; 14],
            },
            frame_number: n,
            payload: [0u8; 16],
        };
        far.send_to(&pkt.to_bytes(), addr)
            .expect("far stream packet");
    }

    assert!(wait_until(|| !s.m17_monitor_heard().is_empty(), 2000));
    let heard = s.m17_monitor_heard();
    assert_eq!(heard.len(), 1);
    assert_eq!(heard[0].callsign, "KC1ABC");
    assert_eq!(heard[0].network, "m17");
    // `note_talker` records the heard entry before it sets `talker`, so a
    // status read right after `heard` turns non-empty can still see `None`.
    assert!(
        wait_until(
            || s.m17_monitor_status()
                .is_some_and(|st| st.talker.as_deref() == Some("KC1ABC")),
            2000
        ),
        "talker names the far station"
    );
    s.m17_monitor_take().expect("leg").disconnect();
}

#[test]
fn dropping_the_session_sends_disc() {
    let rec = Recorder::start(Reply::Ackn);
    let mut s = session_in(BridgeMode::Bridge);
    s.m17_monitor_connect(monitor_cfg(rec.addr))
        .expect("monitor up");
    assert!(wait_until(|| is_linked(&s), 3000));

    drop(s);

    assert!(
        wait_until(|| rec.last_is_disc(), 2000),
        "the monitor's Drop sends DISC with the callsign"
    );
    assert_only_control(&rec.datagrams());
}

// ---- recording taps (Task 10) ----------------------------------------------

/// A real reflector with a far talker (KC1ABC) and this session's monitor
/// both linked on module A, the monitor fed to `tap`. Returns what to keep
/// alive plus the far talker's socket and the reflector's address.
fn monitored_reflector(
    tap: astar_record::tap::Tap<FrameEvent>,
) -> (
    ConsoleSession,
    astar_m17::ReflectorHandle,
    UdpSocket,
    SocketAddr,
) {
    let reflector = Reflector::bind(loopback()).expect("bind reflector");
    let addr = reflector.local_addr();
    let refl = reflector.run();

    let far = UdpSocket::bind(loopback()).expect("bind far talker");
    let conn = ControlPacket::Conn {
        callsign: encode_callsign("KC1ABC").expect("far callsign"),
        module: b'A',
    };
    far.send_to(&conn.to_bytes(), addr).expect("far CONN");
    assert!(wait_until(|| refl.client_count() == 1, 2000));

    let mut s = session_in(BridgeMode::Bridge);
    s.set_record_taps(RecordTaps {
        frames: Some(tap),
        ..RecordTaps::default()
    });
    s.m17_monitor_connect(monitor_cfg(addr))
        .expect("monitor up");
    assert_eq!(poll_for_edge(&mut s, 3000).0, Some(M17MonitorEdge::Linked));
    assert!(wait_until(|| refl.client_count() == 2, 2000));
    (s, refl, far, addr)
}

/// Packet `n`'s payload: the first half all `2n`, the second all `2n + 1`.
fn far_packet(frame_number: u16) -> StreamPacket {
    let n = u8::try_from(frame_number & 0x7f).expect("small");
    let mut payload = [n * 2; 16];
    payload[8..].fill(n * 2 + 1);
    StreamPacket {
        stream_id: 0x4242,
        lsf: Lsf {
            dst: BROADCAST,
            src: encode_callsign("KC1ABC").expect("far callsign"),
            type_field: Lsf::TYPE_VOICE_3200_STREAM,
            meta: [0; 14],
        },
        frame_number,
        payload,
    }
}

/// Everything the tap delivers until the received recording's `End`.
fn events_until_end(events: &Receiver<FrameEvent>) -> Vec<FrameEvent> {
    let mut got = Vec::new();
    assert!(
        wait_until(
            || {
                got.extend(std::iter::from_fn(|| events.try_recv().ok()));
                got.contains(&FrameEvent::End { dir: Dir::Rx })
            },
            3000
        ),
        "the EOS packet must close the recording, got {got:#?}"
    );
    got
}

#[test]
fn the_monitor_feeds_its_codec2_frames_to_the_frame_tap() {
    let (tap, events) = tap_channel(FRAME_TAP_CAPACITY);
    let (mut s, _refl, far, addr) = monitored_reflector(tap);

    for n in [0u16, 1, 2 | StreamPacket::EOS_BIT] {
        far.send_to(&far_packet(n).to_bytes(), addr)
            .expect("far stream packet");
    }
    let got = events_until_end(&events);

    let FrameEvent::Start {
        dir,
        network,
        mode,
        talker,
        vocoder,
        ..
    } = &got[0]
    else {
        panic!("the recording opens with Start, got {got:#?}");
    };
    assert_eq!(
        (*dir, *network, *mode, talker.as_deref(), *vocoder),
        (
            Dir::Rx,
            "m17",
            CaptureMode::Codec2_3200,
            Some("KC1ABC"),
            "codec2"
        )
    );
    let body = &got[1..got.len() - 1];
    assert_eq!(
        body.len(),
        12,
        "three packets, two halves, Frame + Pcm each"
    );
    for (i, pair) in body.chunks(2).enumerate() {
        let n = u16::try_from(i / 2).expect("small");
        let fill = u8::try_from(i).expect("small");
        assert_eq!(
            pair[0],
            FrameEvent::Frame {
                dir: Dir::Rx,
                bytes: vec![fill; 8],
                seq: Some(Seq {
                    value: n,
                    modulus: 0x8000
                }),
            },
            "half {i}"
        );
        assert!(
            matches!(pair[1], FrameEvent::Pcm { dir: Dir::Rx, .. }),
            "half {i}'s Pcm follows its Frame, got {:?}",
            pair[1]
        );
    }
    assert_eq!(got.last(), Some(&FrameEvent::End { dir: Dir::Rx }));
    s.m17_monitor_take().expect("leg").disconnect();
}

#[test]
fn a_skipped_m17_packet_shows_as_a_gap_in_seq() {
    let (tap, events) = tap_channel(FRAME_TAP_CAPACITY);
    let (mut s, _refl, far, addr) = monitored_reflector(tap);

    // Frame 5 never arrives.
    for n in [4u16, 6 | StreamPacket::EOS_BIT] {
        far.send_to(&far_packet(n).to_bytes(), addr)
            .expect("far stream packet");
    }
    let seqs: Vec<u16> = events_until_end(&events)
        .iter()
        .filter_map(|e| match e {
            FrameEvent::Frame { seq: Some(seq), .. } => Some(seq.value),
            _ => None,
        })
        .collect();
    assert_eq!(seqs, vec![4, 4, 6, 6]);
    s.m17_monitor_take().expect("leg").disconnect();
}
