// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `Station::m17_monitor_*`: the receive-only M17 leg as a node sees it.
//!
//! Every socket here is on 127.0.0.1. The reflector is either
//! `astar_m17::Reflector` running in-process, or a raw recording socket.
//! Nothing reaches a real reflector or a real name. The validation tests at
//! the top run in every build. `m17_on` needs the default `m17` feature, and
//! `m17_off` runs only under
//! `cargo test -p astar-station --no-default-features --test m17_monitor_station`.

use astar_station::{M17MonitorRequest, Station, StationConfig, StationError};

/// A station with no engine: nothing has built a `Manager` yet.
fn idle_station() -> Station {
    Station::with_backend_factory(
        StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    )
}

/// A request whose target is never contacted. Every test that uses it is
/// refused before a socket exists, by validation, by a missing engine, or by
/// the feature being off. Port 9 on loopback, so even a regression that got
/// past the refusal could not leave the machine.
fn unreachable_request(module: char, callsign: &str) -> M17MonitorRequest {
    M17MonitorRequest {
        host: "127.0.0.1".to_string(),
        port: 9,
        module,
        callsign: callsign.to_string(),
    }
}

/// The message inside a `StationError::M17`, or a panic naming what came back.
fn m17_message(result: Result<(), StationError>) -> String {
    match result {
        Err(StationError::M17(message)) => message,
        other => panic!("expected StationError::M17, got {other:?}"),
    }
}

#[test]
fn a_module_outside_a_to_z_is_refused_in_every_build() {
    let station = idle_station();
    assert_eq!(
        m17_message(station.m17_monitor_connect(unreachable_request('1', "AJ7HR"))),
        "module must be A-Z"
    );
    // 'é' is U+00E9: it fits in a u8, so the check must be "is A-Z", not "fits in a byte".
    assert_eq!(
        m17_message(station.m17_monitor_connect(unreachable_request('é', "AJ7HR"))),
        "module must be A-Z"
    );
}

#[test]
fn an_empty_callsign_is_refused_in_every_build() {
    let station = idle_station();
    assert_eq!(
        m17_message(station.m17_monitor_connect(unreachable_request('A', ""))),
        "callsign must not be empty"
    );
}

#[cfg(feature = "m17")]
mod m17_on {
    use std::net::{SocketAddr, UdpSocket};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use astar_iax::{BridgeConfig, BridgeMode, CallMode, CodecPolicy, LinkMode};
    use astar_m17::{
        BROADCAST, ControlPacket, Lsf, Reflector, ReflectorHandle, StreamPacket, encode_callsign,
    };
    use astar_station::{
        InboundConfig, M17MonitorEdge, M17MonitorRequest, Station, StationConfig, StationError,
    };

    use super::{idle_station, m17_message, unreachable_request};

    const MONITOR_CALL: &str = "AJ7HR";
    const FAR_CALL: &str = "N0FAR";

    fn station_with(policy: CodecPolicy) -> Station {
        Station::with_backend_factory(
            StationConfig {
                codec_policy: policy,
                ..StationConfig::default()
            },
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        )
    }

    fn loopback_inbound() -> InboundConfig {
        InboundConfig {
            bind: "127.0.0.1:0".parse().expect("loopback bind"),
            ..InboundConfig::default()
        }
    }

    /// The shape astar-server runs in: Bridge topology plus a loopback
    /// inbound listener, which is what builds the engine the monitor joins.
    fn bridge_station(policy: CodecPolicy) -> Station {
        let station = station_with(policy);
        station
            .set_bridge_config(BridgeConfig {
                mode: BridgeMode::Bridge,
                ..BridgeConfig::default()
            })
            .expect("bridge topology");
        station
            .enable_inbound(loopback_inbound())
            .expect("inbound listener on 127.0.0.1:0");
        station
    }

    fn request(addr: SocketAddr, module: char) -> M17MonitorRequest {
        M17MonitorRequest {
            host: addr.ip().to_string(),
            port: addr.port(),
            module,
            callsign: MONITOR_CALL.to_string(),
        }
    }

    /// An in-process `mrefd`-compatible reflector on an ephemeral loopback port.
    fn reflector() -> (SocketAddr, ReflectorHandle) {
        let reflector =
            Reflector::bind("127.0.0.1:0".parse().expect("loopback")).expect("bind reflector");
        let addr = reflector.local_addr();
        (addr, reflector.run())
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

    /// Poll the facade until it reports an edge, or give up at the deadline.
    fn wait_for_edge(station: &Station, timeout_ms: u64) -> Option<M17MonitorEdge> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if let Some(edge) = station.m17_monitor_poll() {
                return Some(edge);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn link_state(station: &Station) -> Option<&'static str> {
        station.m17_monitor_status().map(|s| s.link)
    }

    #[test]
    fn with_no_leg_the_accessors_are_empty_and_disconnect_is_a_no_op() {
        let station = idle_station();
        station.m17_monitor_disconnect();
        assert_eq!(station.m17_monitor_poll(), None);
        assert_eq!(station.m17_monitor_status(), None);
        assert_eq!(
            station.m17_monitor_heard(),
            [] as [astar_station::HeardEntry; 0]
        );
    }

    #[test]
    fn an_unencodable_callsign_is_refused_by_the_console_validation() {
        let station = idle_station();
        assert_eq!(
            m17_message(station.m17_monitor_connect(unreachable_request('A', "AJ7_HR"))),
            "invalid M17 callsign \"AJ7_HR\""
        );
    }

    #[test]
    fn with_no_engine_the_monitor_is_refused() {
        let station = idle_station();
        assert_eq!(
            m17_message(station.m17_monitor_connect(unreachable_request('A', MONITOR_CALL))),
            "no conference: engine not built"
        );
    }

    #[test]
    fn a_handset_topology_is_refused_with_the_topology_reason() {
        let station = station_with(CodecPolicy::UlawOnly);
        station
            .enable_inbound(loopback_inbound())
            .expect("inbound listener on 127.0.0.1:0");
        let err = station
            .m17_monitor_connect(unreachable_request('A', MONITOR_CALL))
            .expect_err("Handset has no conference to join");
        assert_eq!(
            err.to_string(),
            "m17 error: no conference: bridge topology is handset/parrot"
        );
        assert!(
            matches!(&err, StationError::M17(m) if m == "no conference: bridge topology is handset/parrot"),
            "got {err:?}"
        );
        assert_eq!(station.m17_monitor_status(), None);
    }

    /// Parrot runs a conference (a listener enrolled under Bridge survives the
    /// switch), but gives each member a private record/replay, so a source
    /// member would feed nobody. The refusal must leave that conference as it
    /// was. Handset has no conference at all, so only this variant can show a
    /// refused monitor enrolled nobody.
    #[test]
    fn a_parrot_topology_is_refused_and_leaves_the_conference_alone() {
        let station = bridge_station(CodecPolicy::UlawOnly);
        let listener = station
            .add_bridge_member()
            .expect("Bridge enrols a listener");
        station
            .set_bridge_config(BridgeConfig {
                mode: BridgeMode::Parrot,
                ..BridgeConfig::default()
            })
            .expect("switch to parrot topology");
        let before = station.conference_member_count();
        assert!(before >= 1, "the listener survives the switch to Parrot");

        assert_eq!(
            m17_message(station.m17_monitor_connect(unreachable_request('A', MONITOR_CALL))),
            "no conference: bridge topology is handset/parrot"
        );
        assert_eq!(
            station.conference_member_count(),
            before,
            "the refused monitor enrolled nobody"
        );
        assert_eq!(station.m17_monitor_status(), None);
        station.remove_bridge_member(listener.id);
    }

    #[test]
    fn monitor_round_trip_links_reports_and_unlinks() {
        let (addr, refl) = reflector();
        let station = bridge_station(CodecPolicy::UlawOnly);
        let members_before = station.conference_member_count();

        station
            .m17_monitor_connect(request(addr, 'a'))
            .expect("monitor connect");
        assert_eq!(
            station.conference_member_count(),
            members_before + 1,
            "the monitor enrols exactly one source member"
        );
        assert_eq!(wait_for_edge(&station, 3_000), Some(M17MonitorEdge::Linked));
        assert_eq!(station.m17_monitor_poll(), None, "Linked is reported once");
        assert_eq!(link_state(&station), Some("linked"));
        assert!(
            wait_until(|| refl.client_count() == 1, 2_000),
            "the reflector counts the monitor as a client"
        );

        station.m17_monitor_disconnect();
        assert_eq!(station.m17_monitor_status(), None);
        assert_eq!(
            station.m17_monitor_heard(),
            [] as [astar_station::HeardEntry; 0]
        );
        assert_eq!(station.conference_member_count(), members_before);
        assert!(
            wait_until(|| refl.client_count() == 0, 2_000),
            "the monitor's DISC unlinked it from the reflector"
        );
    }

    #[test]
    fn a_second_monitor_while_one_is_live_is_already_connected() {
        let (addr, _refl) = reflector();
        let station = bridge_station(CodecPolicy::UlawOnly);
        station
            .m17_monitor_connect(request(addr, 'A'))
            .expect("first monitor");
        let second = station.m17_monitor_connect(request(addr, 'B'));
        assert!(
            matches!(second, Err(StationError::AlreadyConnected)),
            "got {second:?}"
        );
        station.m17_monitor_disconnect();
    }

    /// The facade's reap path: the session ends the leg on its own (here, a
    /// switch to a topology with no conference), `m17_monitor_poll` reports
    /// the `Ended` edge once, and the reaped monitor is joined, which is what
    /// sends its DISC and unlinks it from the reflector.
    #[test]
    fn a_topology_switch_to_handset_ends_the_monitor_through_the_facade() {
        let (addr, refl) = reflector();
        let station = bridge_station(CodecPolicy::UlawOnly);
        station
            .m17_monitor_connect(request(addr, 'A'))
            .expect("monitor connect");
        assert_eq!(wait_for_edge(&station, 3_000), Some(M17MonitorEdge::Linked));
        assert!(
            wait_until(|| refl.client_count() == 1, 2_000),
            "the reflector counts the monitor as a client"
        );

        station
            .set_bridge_config(BridgeConfig {
                mode: BridgeMode::Handset,
                ..BridgeConfig::default()
            })
            .expect("switch to handset topology");

        assert_eq!(
            wait_for_edge(&station, 3_000),
            Some(M17MonitorEdge::Ended("conference gone".to_string()))
        );
        assert_eq!(station.m17_monitor_status(), None);
        assert_eq!(station.m17_monitor_poll(), None, "Ended is reported once");
        assert!(
            wait_until(|| refl.client_count() == 0, 2_000),
            "the reaped monitor was joined and its DISC unlinked it from the reflector"
        );
    }

    // ---- a recording fake reflector (raw socket; records every datagram) ----

    /// ACKNs every CONN (and remembers the sender as the client to PING),
    /// PINGs that client every 100 ms, and records every
    /// datagram it receives. Being a raw socket, it sees exactly what the
    /// monitor put on the wire. Nothing is filtered.
    struct RecordingReflector {
        addr: SocketAddr,
        seen: Arc<Mutex<Vec<Vec<u8>>>>,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl RecordingReflector {
        fn start() -> Self {
            let sock = UdpSocket::bind("127.0.0.1:0").expect("bind recording reflector");
            sock.set_read_timeout(Some(Duration::from_millis(20)))
                .expect("read timeout");
            let addr = sock.local_addr().expect("recorder addr");
            let seen = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let (t_seen, t_stop) = (Arc::clone(&seen), Arc::clone(&stop));
            let thread = thread::spawn(move || {
                let ping = ControlPacket::Ping {
                    callsign: encode_callsign("M17-REC").expect("reflector callsign encodes"),
                }
                .to_bytes();
                let mut buf = [0u8; 2_048];
                let mut client: Option<SocketAddr> = None;
                let mut last_ping = Instant::now();
                while !t_stop.load(Ordering::Acquire) {
                    if let Ok((n, from)) = sock.recv_from(&mut buf) {
                        let datagram = buf[..n].to_vec();
                        if matches!(
                            ControlPacket::parse(&datagram),
                            Some(ControlPacket::Conn { .. })
                        ) {
                            client = Some(from);
                            let _ = sock.send_to(&ControlPacket::Ackn.to_bytes(), from);
                        }
                        t_seen.lock().expect("recorder lock").push(datagram);
                    }
                    if let Some(to) = client
                        && last_ping.elapsed() >= Duration::from_millis(100)
                    {
                        let _ = sock.send_to(&ping, to);
                        last_ping = Instant::now();
                    }
                }
            });
            Self {
                addr,
                seen,
                stop,
                thread: Some(thread),
            }
        }

        fn datagrams(&self) -> Vec<Vec<u8>> {
            self.seen.lock().expect("recorder lock").clone()
        }
    }

    impl Drop for RecordingReflector {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn count_pongs(datagrams: &[Vec<u8>]) -> usize {
        datagrams
            .iter()
            .filter(|d| {
                matches!(
                    ControlPacket::parse(d.as_slice()),
                    Some(ControlPacket::Pong { .. })
                )
            })
            .count()
    }

    // ---- a far talker on the in-process reflector ----

    /// A raw client linked to `module` of `reflector` as `callsign`.
    fn far_client(reflector: SocketAddr, callsign: &str, module: u8) -> UdpSocket {
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind far client");
        sock.connect(reflector).expect("connect far client");
        sock.set_read_timeout(Some(Duration::from_millis(50)))
            .expect("read timeout");
        let conn = ControlPacket::Conn {
            callsign: encode_callsign(callsign).expect("far callsign encodes"),
            module,
        }
        .to_bytes();
        sock.send(&conn).expect("send CONN");
        let mut buf = [0u8; 64];
        let acked = wait_until(
            || {
                matches!(
                    sock.recv(&mut buf)
                        .ok()
                        .map(|n| ControlPacket::parse(&buf[..n])),
                    Some(Some(ControlPacket::Ackn))
                )
            },
            2_000,
        );
        assert!(acked, "far client linked to the reflector");
        sock
    }

    /// One 20 ms frame of a 1 kHz tone at 8 kHz. `n` carries the phase across frames.
    #[allow(clippy::cast_possible_truncation)] // amplitude 8000 < i16::MAX: the cast never truncates
    fn tone_frame(n: &mut u32) -> [i16; 160] {
        let mut pcm = [0i16; 160];
        for sample in &mut pcm {
            let t = f64::from(*n) / 8_000.0;
            *sample = (8_000.0 * (2.0 * std::f64::consts::PI * 1_000.0 * t).sin()) as i16;
            *n += 1;
        }
        pcm
    }

    /// Stream `packets` Codec 2 packets of the tone as `FAR_CALL`. The last
    /// one carries EOS. The 40 ms sleep between packets is real-time
    /// pacing, which the monitor's flood gate expects. It is not a wait for
    /// a condition.
    fn stream_tone(sock: &UdpSocket, packets: u16) {
        let (mut codec, _) = astar_codec::codec2::open_codec2(&[])
            .expect("Codec 2 via the codec2-static dev-dependency");
        let src = encode_callsign(FAR_CALL).expect("far callsign encodes");
        let mut n = 0u32;
        for k in 0..packets {
            let first = codec.encode(&tone_frame(&mut n));
            let second = codec.encode(&tone_frame(&mut n));
            let mut payload = [0u8; 16];
            payload[..8].copy_from_slice(&first);
            payload[8..].copy_from_slice(&second);
            let frame_number = if k + 1 == packets {
                k | StreamPacket::EOS_BIT
            } else {
                k
            };
            let packet = StreamPacket {
                stream_id: 0x4D17,
                lsf: Lsf {
                    dst: BROADCAST,
                    src,
                    type_field: Lsf::TYPE_VOICE_3200_STREAM,
                    meta: [0; 14],
                },
                frame_number,
                payload,
            };
            sock.send(&packet.to_bytes()).expect("send stream packet");
            thread::sleep(Duration::from_millis(40));
        }
    }

    fn rms(frame: &[i16]) -> f64 {
        if frame.is_empty() {
            return 0.0;
        }
        let sum: f64 = frame.iter().map(|&s| f64::from(s).powi(2)).sum();
        (sum / f64::from(u32::try_from(frame.len()).expect("frame length fits u32"))).sqrt()
    }

    // ---- guards ----

    #[test]
    fn a_pooled_iax_link_does_not_refuse_the_monitor() {
        let (addr, _refl) = reflector();
        let station = bridge_station(CodecPolicy::UlawOnly);
        // A bound loopback socket that never answers: the IAX2 link dials it
        // and stays pooled, and nothing leaves 127.0.0.1.
        let dead = UdpSocket::bind("127.0.0.1:0").expect("bind dead-port peer");
        let dead_addr = dead.local_addr().expect("dead peer addr");
        station
            .link_connect_at(
                "55553",
                &dead_addr.to_string(),
                LinkMode::Monitor,
                "1999",
                "",
                CallMode::Standard,
                false,
            )
            .expect("pool an IAX2 link");
        assert!(station.call_count() > 0, "the IAX2 link is pooled");

        // Control: the transmit-capable M17 client IS refused by this pooled
        // link, so the exclusion really is live on this station.
        let excluded = station.m17_connect(&addr.ip().to_string(), addr.port(), 'A', MONITOR_CALL);
        assert!(
            matches!(excluded, Err(StationError::AlreadyConnected)),
            "got {excluded:?}"
        );

        station
            .m17_monitor_connect(request(addr, 'A'))
            .expect("a monitor is link-shaped and must not be refused by an IAX2 link");
        assert_eq!(wait_for_edge(&station, 3_000), Some(M17MonitorEdge::Linked));
        assert!(station.call_count() > 0, "the IAX2 link is untouched");
        station.m17_monitor_disconnect();
    }

    #[test]
    fn set_ptt_with_only_a_monitor_up_is_not_connected_and_only_control_goes_out() {
        let recorder = RecordingReflector::start();
        let station = bridge_station(CodecPolicy::UlawOnly);
        station
            .m17_monitor_connect(request(recorder.addr, 'A'))
            .expect("monitor connect");
        assert_eq!(wait_for_edge(&station, 3_000), Some(M17MonitorEdge::Linked));

        let keyed = station.set_ptt(true);
        assert!(
            matches!(keyed, Err(StationError::NotConnected)),
            "got {keyed:?}"
        );
        let snap = station.snapshot();
        assert!(!snap.ptt, "no PTT with only a monitor up");
        assert!(
            !snap.m17_active,
            "m17_active means the console's own M17 client"
        );
        // Hold the would-be keyed window open across several keepalives, long
        // enough that a keyed leg would have sent something.
        assert!(
            wait_until(|| count_pongs(&recorder.datagrams()) >= 3, 3_000),
            "the monitor answered at least three PINGs while 'keyed'"
        );
        let _ = station.set_ptt(false);
        assert_eq!(
            station.m17_monitor_status().map(|s| s.refused_sends),
            Some(0)
        );

        station.m17_monitor_disconnect();
        let callsign = encode_callsign(MONITOR_CALL).expect("monitor callsign encodes");
        let disc = ControlPacket::Disc {
            callsign: Some(callsign),
        };
        assert!(
            wait_until(
                || recorder
                    .datagrams()
                    .last()
                    .and_then(|d| ControlPacket::parse(d.as_slice()))
                    == Some(disc),
                2_000
            ),
            "the last datagram is the monitor's DISC"
        );
        let seen = recorder.datagrams();
        assert_eq!(
            ControlPacket::parse(seen[0].as_slice()),
            Some(ControlPacket::Conn {
                callsign,
                module: b'A'
            })
        );
        for (i, d) in seen.iter().enumerate() {
            assert!(
                StreamPacket::parse(d).is_none(),
                "datagram {i} ({} bytes) is an M17 stream packet",
                d.len()
            );
            assert!(
                matches!(
                    ControlPacket::parse(d.as_slice()),
                    Some(
                        ControlPacket::Conn { .. }
                            | ControlPacket::Pong { .. }
                            | ControlPacket::Disc { .. }
                    )
                ),
                "datagram {i} is not CONN, PONG or DISC: {d:?}"
            );
        }
    }

    #[test]
    fn station_disconnect_leaves_the_monitor_linked() {
        let (addr, refl) = reflector();
        let station = bridge_station(CodecPolicy::UlawOnly);
        station
            .m17_monitor_connect(request(addr, 'A'))
            .expect("monitor connect");
        assert_eq!(wait_for_edge(&station, 3_000), Some(M17MonitorEdge::Linked));
        assert!(wait_until(|| refl.client_count() == 1, 2_000));

        // What NodeCommand::Hangup reaches: a handset hang-up.
        station.disconnect();

        // For 300 ms after it, the leg must stay up: no Ended edge, still
        // linked, still a client of the reflector.
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            assert_eq!(
                station.m17_monitor_poll(),
                None,
                "Station::disconnect must not end the monitor"
            );
            assert_eq!(link_state(&station), Some("linked"));
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(refl.client_count(), 1);
        station.m17_monitor_disconnect();
    }

    /// `NodeCommand::Hangup` with a handset caller actually on the node, not
    /// just an idle listener. astar-server keeps its inbound listener up, so
    /// `Station::mode()` reads Node and `disconnect` takes the
    /// `disconnect_inbound` path: it hangs the caller up and keeps the engine,
    /// and with it the monitor's conference.
    mod adopted_call {
        use std::net::{SocketAddr, UdpSocket};
        use std::thread;
        use std::time::{Duration, Instant};

        use astar_iax::{
            BridgeConfig, BridgeMode, CodecPolicy, IncomingAuthPolicy, IncomingCallPolicy,
        };
        use astar_iax_core::Subclass;
        use astar_iax_core::frame::{Frame, FullFrame, encode, parse_lenient};
        use astar_iax_core::ie::Ies;
        use astar_iax_core::subclass::{FrameType, IaxCommand, VoiceFormat};
        use astar_station::{AnswerPolicy, CallStatus, InboundConfig, M17MonitorEdge};

        use super::{link_state, reflector, request, station_with, wait_for_edge, wait_until};

        const PEER_CALL: u16 = 13_885;

        fn enc(frame: &Frame<'_>) -> Vec<u8> {
            let mut b = Vec::new();
            encode(frame, &mut b).expect("encode");
            b
        }

        /// A hand-built NEW from a 127.0.0.1 peer, as `tests/node_loopback.rs`
        /// sends it.
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

        /// ACK every reliable full frame the listener sent the peer, so the
        /// ANSWER is released and the call reaches Answered.
        fn pump_acks(sock: &UdpSocket, listener: SocketAddr) {
            let mut buf = [0u8; 4096];
            while let Ok((n, _src)) = sock.recv_from(&mut buf) {
                let bytes = buf[..n].to_vec();
                let Ok(Frame::Full(f)) = parse_lenient(&bytes) else {
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
                    let _ = sock.send_to(&ack, listener);
                }
            }
        }

        #[test]
        fn a_hangup_with_a_handset_caller_connected_leaves_the_monitor_linked() {
            let (addr, refl) = reflector();
            let station = station_with(CodecPolicy::UlawOnly);
            station
                .set_bridge_config(BridgeConfig {
                    mode: BridgeMode::Bridge,
                    ..BridgeConfig::default()
                })
                .expect("bridge topology");
            station
                .enable_inbound(InboundConfig {
                    bind: "127.0.0.1:0".parse().expect("loopback bind"),
                    policy: IncomingCallPolicy {
                        auth: IncomingAuthPolicy::Off,
                        ..IncomingCallPolicy::default()
                    },
                    answer: AnswerPolicy::Auto,
                    max_calls: 4,
                    allowlist: None,
                })
                .expect("inbound listener on 127.0.0.1:0");
            let listener = station.node_bind_addr().expect("listener bound");

            station
                .m17_monitor_connect(request(addr, 'A'))
                .expect("monitor connect");
            assert_eq!(wait_for_edge(&station, 3_000), Some(M17MonitorEdge::Linked));
            assert!(wait_until(|| refl.client_count() == 1, 2_000));

            // A handset caller on 127.0.0.1 is answered and adopted.
            let peer = UdpSocket::bind("127.0.0.1:0").expect("bind peer");
            peer.set_read_timeout(Some(Duration::from_millis(50)))
                .expect("peer read timeout");
            peer.send_to(&new_datagram(), listener).expect("send NEW");
            assert!(
                wait_until(
                    || {
                        pump_acks(&peer, listener);
                        station.snapshot().status == CallStatus::Answered
                    },
                    5_000
                ),
                "the handset caller must reach Answered"
            );

            // What NodeCommand::Hangup reaches, with that caller connected.
            station.disconnect();
            assert_eq!(
                station.snapshot().status,
                CallStatus::Idle,
                "the handset caller was hung up"
            );

            // For 300 ms after it, the monitor must stay up.
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                assert_eq!(
                    station.m17_monitor_poll(),
                    None,
                    "hanging up a handset caller must not end the monitor"
                );
                assert_eq!(link_state(&station), Some("linked"));
                thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(
                refl.client_count(),
                1,
                "the reflector still has the monitor"
            );
            station.m17_monitor_disconnect();
            assert!(
                wait_until(|| refl.client_count() == 0, 2_000),
                "an explicit monitor disconnect still sends DISC"
            );
        }
    }

    /// `NodeCommand::Hangup` with the inbound listener OFF and a WT call up.
    /// `Station::mode()` then reads Wt, so `disconnect` goes through
    /// `ConsoleSession::detach` instead of `disconnect_inbound`. That path
    /// used to hand the whole engine back to be dropped, taking with it the
    /// conference the monitor feeds.
    mod wt_call {
        use std::net::{SocketAddr, UdpSocket};
        use std::sync::{Arc, Mutex};
        use std::thread;
        use std::time::{Duration, Instant};

        use astar_console::{ConsoleConfig, ConsoleSession};
        use astar_iax::{BridgeConfig, BridgeMode, CodecPolicy};
        use astar_station::{M17MonitorEdge, OperatingMode, Station, StationConfig};

        use super::{
            FAR_CALL, far_client, link_state, reflector, request, rms, stream_tone, wait_for_edge,
            wait_until,
        };

        /// A Bridge-topology station with NO inbound listener. It shares its
        /// session with the test, so the test can place a WT call the way the
        /// `astar-inspect` harness does, without a registrar lookup or a
        /// portal token mint.
        fn wt_station() -> (Station, Arc<Mutex<ConsoleSession>>) {
            let session = Arc::new(Mutex::new(ConsoleSession::new()));
            let station = Station::with_shared_session(
                StationConfig {
                    codec_policy: CodecPolicy::UlawOnly,
                    ..StationConfig::default()
                },
                Arc::clone(&session),
                Box::new(|| Box::new(astar_audio::NullBackend::new())),
            );
            station
                .set_bridge_config(BridgeConfig {
                    mode: BridgeMode::Bridge,
                    ..BridgeConfig::default()
                })
                .expect("bridge topology");
            (station, session)
        }

        /// Dial a WT call at `peer`, a bound and silent 127.0.0.1 socket. The
        /// call stays Dialing, which is enough: `detach()` acts on the
        /// session's active call whatever its status. The dial is also what
        /// builds the engine, so no listener is ever started.
        fn dial_wt(session: &Mutex<ConsoleSession>, peer: SocketAddr) {
            session
                .lock()
                .expect("session lock")
                .connect(
                    Box::new(astar_audio::NullBackend::new()),
                    peer,
                    ConsoleConfig {
                        node: "55553".into(),
                        calling_node: "55553".into(),
                        secret: String::new(),
                        name: "wt-hangup-test".into(),
                        input_device: None,
                        output_device: None,
                        codec_policy: CodecPolicy::UlawOnly,
                    },
                )
                .expect("the WT dial starts");
        }

        #[test]
        fn a_wt_hangup_with_no_listener_leaves_the_monitor_linked_and_contributing() {
            const PACKETS: u16 = 25;
            const VOICED_RMS: f64 = 300.0;
            let (addr, refl) = reflector();
            let (station, session) = wt_station();
            let wt_peer = UdpSocket::bind("127.0.0.1:0").expect("bind silent WT peer");
            dial_wt(&session, wt_peer.local_addr().expect("WT peer addr"));
            assert_eq!(
                station.mode(),
                OperatingMode::Wt,
                "no listener, so disconnect takes detach()"
            );
            assert_eq!(station.call_count(), 1, "the WT call is pooled");

            let listener = station
                .add_bridge_member()
                .expect("a bridge member in Bridge mode");
            assert_eq!(listener.sample_rate, 8_000);
            station
                .m17_monitor_connect(request(addr, 'A'))
                .expect("monitor connect");
            assert_eq!(wait_for_edge(&station, 3_000), Some(M17MonitorEdge::Linked));
            assert!(
                wait_until(|| refl.client_count() == 1, 2_000),
                "the reflector counts the monitor as a client"
            );

            // What NodeCommand::Hangup reaches, with the WT call up.
            station.disconnect();
            assert_eq!(station.call_count(), 0, "the WT call was hung up");

            // For 300 ms after it, the monitor must stay up.
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                assert_eq!(
                    station.m17_monitor_poll(),
                    None,
                    "hanging up a WT call must not end the monitor"
                );
                assert_eq!(link_state(&station), Some("linked"));
                thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(
                refl.client_count(),
                1,
                "the reflector still has the monitor"
            );

            // And it still feeds the conference: a far talker on the module
            // reaches the bridge member through the engine that survived.
            let far = far_client(addr, FAR_CALL, b'A');
            assert!(
                wait_until(|| refl.client_count() == 2, 2_000),
                "the reflector counts the monitor and the far talker"
            );
            let talker = thread::spawn(move || stream_tone(&far, PACKETS));
            let mut voiced = 0usize;
            let mut wrong_lengths = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline && voiced < usize::from(PACKETS) {
                if let Ok(frame) = listener
                    .from_conference
                    .recv_timeout(Duration::from_millis(50))
                {
                    if frame.len() != 160 {
                        wrong_lengths.push(frame.len());
                    }
                    if rms(&frame) > VOICED_RMS {
                        voiced += 1;
                    }
                }
            }
            talker.join().expect("talker thread");
            assert!(
                wrong_lengths.is_empty(),
                "every mix frame at 8 kHz is 160 samples, got {wrong_lengths:?}"
            );
            assert!(
                voiced >= usize::from(PACKETS),
                "after the hang-up the bridge member heard {voiced} voiced frames of the far talker, wanted at least {PACKETS}"
            );
            assert_eq!(
                station.m17_monitor_poll(),
                None,
                "the monitor is still up after the far talker's stream"
            );
            assert_eq!(link_state(&station), Some("linked"));

            station.remove_bridge_member(listener.id);
            station.m17_monitor_disconnect();
            assert!(
                wait_until(|| refl.client_count() == 1, 2_000),
                "the monitor's DISC unlinked it, and only the far talker is left"
            );
        }

        /// The carve-out is narrow. With no monitor up, a WT hang-up still
        /// hands the engine back to be dropped, so its router closes the
        /// audio device streams between calls exactly as before.
        #[test]
        fn a_wt_hangup_with_no_monitor_still_drops_the_engine() {
            let (station, session) = wt_station();
            let wt_peer = UdpSocket::bind("127.0.0.1:0").expect("bind silent WT peer");
            dial_wt(&session, wt_peer.local_addr().expect("WT peer addr"));
            assert!(
                session.lock().expect("session lock").has_engine(),
                "the dial built the engine"
            );
            assert_eq!(station.call_count(), 1, "the WT call is pooled");

            station.disconnect();

            assert_eq!(station.call_count(), 0, "the WT call was hung up");
            assert!(
                !session.lock().expect("session lock").has_engine(),
                "with no monitor up, the WT hang-up drops the engine as before"
            );
        }
    }

    /// What one disconnect looked like from a second thread that called
    /// `snapshot()` repeatedly throughout it, pausing briefly between calls.
    struct JoinObservation {
        /// How long `m17_monitor_disconnect` took.
        join_time: Duration,
        /// Snapshots that finished inside the disconnect's window. With the
        /// join under the session lock this is 0-2 (one already in flight
        /// when it began, one squeezed in between the unlock and the return).
        /// That bound holds only because the observer pauses after every
        /// snapshot: std's `Mutex` is not fair on macOS, and an observer
        /// re-locking in a tight loop could keep winning the lock while the
        /// disconnect thread waits for it, finishing several fast snapshots
        /// inside the window before the join even starts. With the pause the
        /// disconnect thread takes the lock at once. With the join off-lock
        /// it is about ten or more for any join of 15 ms or longer.
        completed_during_join: usize,
        /// The median latency of those snapshots, `Duration::MAX` if none.
        median_latency: Duration,
        /// Whether the observer was running before the disconnect began.
        observing: bool,
    }

    /// One disconnect with a second thread calling `snapshot()` throughout,
    /// sleeping 500 us after each call so it cannot starve the disconnect
    /// thread of the session lock (see [`JoinObservation`]). Every
    /// snapshot's finish instant and latency is recorded, and those that
    /// finished inside the disconnect's window are counted.
    fn disconnect_while_snapshotting(station: &Station) -> JoinObservation {
        let stop = AtomicBool::new(false);
        let started = AtomicBool::new(false);
        thread::scope(|s| {
            let observer = s.spawn(|| {
                // (finished at, latency) for every snapshot taken.
                let mut samples: Vec<(Instant, Duration)> = Vec::new();
                while !stop.load(Ordering::Acquire) {
                    let t = Instant::now();
                    let _ = station.snapshot();
                    let done = Instant::now();
                    samples.push((done, done - t));
                    started.store(true, Ordering::Release);
                    // Yield the lock so an on-lock join cannot hide behind
                    // an unfair mutex hand-off.
                    thread::sleep(Duration::from_micros(500));
                }
                samples
            });
            let observing = wait_until(|| started.load(Ordering::Acquire), 2_000);
            let begin = Instant::now();
            station.m17_monitor_disconnect();
            let end = Instant::now();
            stop.store(true, Ordering::Release);
            let samples = observer.join().expect("observer thread");
            let mut during: Vec<Duration> = samples
                .iter()
                .filter(|(done, _)| *done > begin && *done <= end)
                .map(|(_, latency)| *latency)
                .collect();
            during.sort_unstable();
            let median_latency = during
                .get(during.len() / 2)
                .copied()
                .unwrap_or(Duration::MAX);
            JoinObservation {
                join_time: end - begin,
                completed_during_join: during.len(),
                median_latency,
                observing,
            }
        })
    }

    /// The discriminator is how many snapshots finish while the disconnect
    /// runs, not the single worst latency: one scheduler preemption in a
    /// parallel debug test run can push any one snapshot past 10 ms, but it
    /// cannot make several snapshots finish while the session lock is held.
    #[test]
    fn snapshot_is_not_blocked_while_disconnect_joins_the_monitor_thread() {
        const SNAPSHOT_BOUND: Duration = Duration::from_millis(10);
        // A join shorter than this cannot tell an on-lock join from an off-lock one.
        const MEANINGFUL_JOIN: Duration = Duration::from_millis(15);
        // An on-lock join lets at most 2 snapshots finish inside its window
        // (the observer's 500 us pause keeps the hand-off prompt); an off-lock
        // join of MEANINGFUL_JOIN or longer lets about 10 or more finish.
        const MIN_SNAPSHOTS_DURING_JOIN: usize = 3;
        let (addr, _refl) = reflector();
        let station = bridge_station(CodecPolicy::UlawOnly);
        // Warm the one-off costs (codec probe cache) out of the measured window.
        let _ = station.snapshot();

        let mut meaningful = 0;
        for cycle in 0..20 {
            if meaningful >= 3 {
                break;
            }
            station
                .m17_monitor_connect(request(addr, 'A'))
                .expect("monitor connect");
            assert_eq!(
                wait_for_edge(&station, 3_000),
                Some(M17MonitorEdge::Linked),
                "cycle {cycle}"
            );
            let obs = disconnect_while_snapshotting(&station);
            assert!(
                obs.observing,
                "cycle {cycle}: the snapshot thread never started"
            );
            if obs.join_time < MEANINGFUL_JOIN {
                continue;
            }
            meaningful += 1;
            assert!(
                obs.completed_during_join >= MIN_SNAPSHOTS_DURING_JOIN,
                "cycle {cycle}: only {} snapshots completed while m17_monitor_disconnect took \
                 {:?}; the join ran under the session lock",
                obs.completed_during_join,
                obs.join_time
            );
            assert!(
                obs.median_latency < SNAPSHOT_BOUND,
                "cycle {cycle}: the median snapshot() took {:?} while m17_monitor_disconnect \
                 took {:?}",
                obs.median_latency,
                obs.join_time
            );
        }
        assert!(
            meaningful >= 1,
            "no disconnect took {MEANINGFUL_JOIN:?}, so the test never saw a join long enough \
             to tell an on-lock join from an off-lock one"
        );
    }

    #[test]
    fn at_16_khz_a_bridge_member_hears_the_far_talker_in_320_sample_frames() {
        const PACKETS: u16 = 25;
        const VOICED_RMS: f64 = 300.0;
        let (addr, _refl) = reflector();
        let station = bridge_station(CodecPolicy::PreferSlin16);
        let listener = station
            .add_bridge_member()
            .expect("a bridge member in Bridge mode");
        assert_eq!(listener.sample_rate, 16_000);
        station
            .m17_monitor_connect(request(addr, 'A'))
            .expect("monitor connect");
        assert_eq!(wait_for_edge(&station, 3_000), Some(M17MonitorEdge::Linked));

        let far = far_client(addr, FAR_CALL, b'A');
        let talker = thread::spawn(move || stream_tone(&far, PACKETS));

        let mut voiced = 0usize;
        let mut wrong_lengths = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && voiced < usize::from(PACKETS) {
            if let Ok(frame) = listener
                .from_conference
                .recv_timeout(Duration::from_millis(50))
            {
                if frame.len() != 320 {
                    wrong_lengths.push(frame.len());
                }
                if rms(&frame) > VOICED_RMS {
                    voiced += 1;
                }
            }
        }
        talker.join().expect("talker thread");

        assert!(
            wrong_lengths.is_empty(),
            "every mix frame at 16 kHz is 320 samples, got {wrong_lengths:?}"
        );
        assert!(
            voiced >= usize::from(PACKETS),
            "the bridge member heard {voiced} voiced frames of the far talker, wanted at least {PACKETS}"
        );
        assert!(
            wait_until(
                || station
                    .m17_monitor_heard()
                    .first()
                    .map(|h| h.callsign.clone())
                    == Some(FAR_CALL.to_string())
                    && station.m17_monitor_status().and_then(|s| s.talker)
                        == Some(FAR_CALL.to_string()),
                2_000
            ),
            "the monitor names the far talker in heard() and status().talker"
        );
        station.remove_bridge_member(listener.id);
        station.m17_monitor_disconnect();
    }
}

#[cfg(not(feature = "m17"))]
mod m17_off {
    use super::{idle_station, m17_message, unreachable_request};

    #[test]
    fn without_the_feature_connect_says_not_compiled_and_the_rest_are_inert() {
        let station = idle_station();
        assert_eq!(
            m17_message(station.m17_monitor_connect(unreachable_request('A', "AJ7HR"))),
            "m17 support not compiled"
        );
        station.m17_monitor_disconnect();
        assert_eq!(station.m17_monitor_poll(), None);
        assert_eq!(station.m17_monitor_status(), None);
        assert_eq!(
            station.m17_monitor_heard(),
            [] as [astar_station::HeardEntry; 0]
        );
    }
}
