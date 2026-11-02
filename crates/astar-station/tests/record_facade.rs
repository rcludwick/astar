// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The recording facade: `Station::record_start` / `record_stop` /
//! `record_state` / `set_frame_tap`, end to end over 127.0.0.1 only.
//!
//! The IAX2 tests answer a loopback inbound call on a `prefer_slin16` node
//! (the `tests/node_loopback.rs` pattern), so the call is one
//! `ConsoleSession::has_connection` sees and the pipeline runs at 16 kHz.
//! `NullBackend` never produces heard or sent audio, so those files are
//! silence, kept at full length. The M17 tests run against a parrot
//! reflector on 127.0.0.1 with a push-capable backend.
#![allow(clippy::too_many_lines)]

use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use astar_console::ConsoleSession;
use astar_iax::{CodecPolicy, IncomingAuthPolicy, IncomingCallPolicy};
use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, FullFrame, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{FrameType, IaxCommand, VoiceFormat};
use astar_station::{
    AnswerPolicy, CallStatus, Layout, NodeConfig, OperatingMode, RecordRequest, RecordState,
    Station, StationConfig, StationError,
};

// ---- helpers ----------------------------------------------------------------

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

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("astar-recfacade-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn request(dir: &Path, frames: bool) -> RecordRequest {
    RecordRequest {
        dir: dir.to_path_buf(),
        layout: Layout::Stereo,
        frames,
        label: "AllStar 1001".to_owned(),
        utc_offset_secs: 0,
    }
}

// ---- IAX2 loopback (tests/node_loopback.rs) ---------------------------------

const PEER_CALL: u16 = 13885;

fn enc(frame: &Frame<'_>) -> Vec<u8> {
    let mut b = Vec::new();
    encode(frame, &mut b).expect("encode");
    b
}

fn new_datagram(source_call: u16) -> Vec<u8> {
    enc(&Frame::Full(Box::new(FullFrame {
        source_call,
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

fn pump_acks(sock: &UdpSocket, listener_addr: SocketAddr, my_call: u16) {
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

/// A `prefer_slin16` node with an answered loopback call, the session it
/// shares with the test, and the peer socket (kept alive by the caller).
fn answered_call() -> (Station, Arc<Mutex<ConsoleSession>>, UdpSocket) {
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
    let addr = station.node_bind_addr().expect("node listener bound");
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    peer.send_to(&new_datagram(PEER_CALL), addr).unwrap();
    let answered = wait_until(
        || {
            let snap = station.snapshot();
            pump_acks(&peer, addr, PEER_CALL);
            snap.status == CallStatus::Answered
        },
        5_000,
    );
    assert!(answered, "the loopback call must be answered");
    assert!(
        session.lock().unwrap().has_connection(),
        "an answered inbound call is a connection has_connection() sees"
    );
    assert_eq!(session.lock().unwrap().pipeline_sample_rate(), 16_000);
    (station, session, peer)
}

fn flac_info(path: &Path) -> (u32, u32, u64) {
    let r = claxon::FlacReader::open(path).expect("a readable FLAC");
    let i = r.streaminfo();
    (i.channels, i.sample_rate, i.samples.unwrap_or(0))
}

// ---- IAX2 tests ---------------------------------------------------------------

#[test]
fn record_start_while_idle_is_not_connected() {
    let station = Station::with_backend_factory(
        StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    let dir = scratch("idle");
    assert!(matches!(
        station.record_start(request(&dir, false)),
        Err(StationError::NotConnected)
    ));
    assert_eq!(station.record_state(), RecordState::idle());
    assert!(!dir.exists(), "a refused start creates nothing");
}

#[test]
fn a_loopback_call_records_one_stereo_file() {
    let (station, _session, _peer) = answered_call();
    let dir = scratch("stereo");
    station.record_start(request(&dir, false)).expect("start");
    assert!(station.record_state().recording);
    thread::sleep(Duration::from_secs(2));
    let state = station.record_stop();
    assert!(!state.recording);
    assert_eq!(state.layout, Layout::Stereo);
    assert_eq!(state.files.len(), 1, "{state:?}");
    let (channels, rate, samples) = flac_info(&state.files[0]);
    assert_eq!(channels, 2, "stereo: receive left, transmit right");
    assert_eq!(rate, 16_000, "the prefer_slin16 pipeline's rate");
    // About 2 s of silence: NullBackend drops the source, and the clock
    // still writes a frame per tick.
    assert!(
        (28_000..=36_000).contains(&samples),
        "about 2 s at 16 kHz, got {samples} samples"
    );
    station.disconnect();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn record_start_twice_is_refused() {
    let (station, _session, _peer) = answered_call();
    let dir = scratch("twice");
    station.record_start(request(&dir, false)).expect("start");
    match station.record_start(request(&dir, false)) {
        Err(StationError::Record(m)) => assert_eq!(m, "already recording"),
        other => panic!("expected Record(already recording), got {other:?}"),
    }
    // Past the first tick, so the file exists (a stop before it leaves none).
    thread::sleep(Duration::from_millis(200));
    let state = station.record_stop();
    assert_eq!(state.files.len(), 1, "the refused start opened no file");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn record_stop_twice_is_harmless() {
    let (station, _session, _peer) = answered_call();
    let dir = scratch("stop-twice");
    station.record_start(request(&dir, false)).expect("start");
    thread::sleep(Duration::from_millis(200));
    let first = station.record_stop();
    let second = station.record_stop();
    assert!(!second.recording);
    assert_eq!(second.files, first.files);
    assert_eq!(station.record_state().files, first.files);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn disconnect_finalizes_an_open_recording() {
    let (station, _session, _peer) = answered_call();
    let dir = scratch("disconnect");
    let began = Instant::now();
    station.record_start(request(&dir, false)).expect("start");
    thread::sleep(Duration::from_millis(500));
    station.disconnect();
    let elapsed = began.elapsed();
    let state = station.record_state();
    assert!(!state.recording, "disconnect stops the recording");
    assert_eq!(state.files.len(), 1);
    let (_, rate, samples) = flac_info(&state.files[0]);
    // One frame per 20 ms tick from the first tick to the stop: about the
    // wall time at the file's rate, give or take a few ticks and a slow
    // scheduler.
    let expected = elapsed.as_secs_f64() * f64::from(rate);
    #[allow(clippy::cast_precision_loss)]
    let got = samples as f64;
    assert!(
        got > expected * 0.6 && got < expected + 0.3 * f64::from(rate),
        "STREAMINFO total {samples} should be about {expected:.0} ({elapsed:?} at {rate} Hz)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stop issued while another is joining the recorder must already see
/// `recording: false`, and a start then must not overlap the finishing
/// recorder. The window is one join (up to a tick), so this cannot force
/// the overlap every run; every answer it gets must still hold.
#[test]
fn a_second_stop_during_the_join_reports_not_recording() {
    let (station, _session, _peer) = answered_call();
    let dir = scratch("concurrent-stop");
    station.record_start(request(&dir, false)).expect("start");
    thread::sleep(Duration::from_millis(200));
    let done = AtomicBool::new(false);
    thread::scope(|sc| {
        sc.spawn(|| {
            let st = station.record_stop();
            done.store(true, Ordering::SeqCst);
            assert!(!st.recording);
        });
        while !done.load(Ordering::SeqCst) {
            let st = station.record_stop();
            assert!(!st.recording, "a concurrent stop never reports recording");
            assert!(!station.record_state().recording);
            if let Err(e) = station.record_start(request(&dir, false)) {
                assert!(
                    matches!(&e, StationError::Record(m) if m == "already recording"),
                    "a start during the join is refused as busy, got {e:?}"
                );
            } else {
                // The join had already finished: this is a fresh recording.
                let _ = station.record_stop();
            }
        }
    });
    let _ = station.record_stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Items lost before the recorder (a full tap here) are folded into
/// `dropped_frames`: the station wires each tap's drop counter in.
#[test]
fn tap_drops_reach_dropped_frames() {
    use astar_record::frames::{Dir, FrameEvent};
    let (station, session, _peer) = answered_call();
    let dir = scratch("drops");
    station.record_start(request(&dir, true)).expect("start");
    let tap = session
        .lock()
        .unwrap()
        .record_taps()
        .frames
        .clone()
        .expect("the app's frame tap is installed");
    // `End` with no open stream is a no-op for the frame writer, so the only
    // loss is the full channel's (capacity 1024), offered far faster than a
    // 20 ms clock drains it.
    let failed = (0..5_000)
        .filter(|_| !tap.offer(FrameEvent::End { dir: Dir::Rx }))
        .count() as u64;
    assert!(failed > 0, "the burst must overflow the tap");
    drop(tap);
    thread::sleep(Duration::from_millis(100));
    let state = station.record_stop();
    assert!(
        state.dropped_frames >= failed,
        "dropped_frames {} must count the {failed} tap drops",
        state.dropped_frames
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn record_state_json_has_the_spec_keys() {
    let (station, _session, _peer) = answered_call();
    let dir = scratch("json");
    station.record_start(request(&dir, false)).expect("start");
    let v = serde_json::to_value(station.record_state()).expect("json");
    let mut keys: Vec<&str> = v
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "dropped_frames",
            "elapsed_ms",
            "error",
            "files",
            "layout",
            "recording"
        ]
    );
    let _ = station.record_stop();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- M17 (tests/m17_station.rs helpers) ----------------------------------------

#[cfg(feature = "m17")]
mod m17 {
    use super::*;
    use astar_audio::{AudioBackend, AudioError, DeviceInfo, Direction, InputSink, OutputSource};
    use astar_m17::StreamPacket;
    use astar_record::capture::read_rows;
    use astar_record::frames::FrameEvent;
    use astar_record::tap::{FRAME_TAP_CAPACITY, tap_channel};

    type Recorded = Arc<Mutex<Vec<Vec<u8>>>>;

    /// CONN -> ACKN; records and echoes every 54-byte stream packet.
    fn spawn_recording_parrot() -> (SocketAddr, Recorded, thread::JoinHandle<()>) {
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind fake reflector");
        sock.set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set read timeout");
        let addr = sock.local_addr().expect("local addr");
        let recorded: Recorded = Arc::new(Mutex::new(Vec::new()));
        let rec = Arc::clone(&recorded);
        let t = thread::spawn(move || {
            let mut buf = [0u8; 4_096];
            let mut acked = false;
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                let Ok((n, src)) = sock.recv_from(&mut buf) else {
                    continue;
                };
                if !acked && n == 11 && &buf[0..4] == b"CONN" {
                    acked = true;
                    let _ = sock.send_to(b"ACKN", src);
                } else if n == 54 && &buf[0..4] == b"M17 " {
                    rec.lock().unwrap().push(buf[..n].to_vec());
                    let _ = sock.send_to(&buf[..n], src);
                }
            }
        });
        (addr, recorded, t)
    }

    type MicSink = Arc<Mutex<Option<Box<dyn InputSink>>>>;

    fn dev(dir: Direction, tag: &str) -> DeviceInfo {
        DeviceInfo {
            id: astar_audio::DeviceId::new(tag.to_string()),
            name: tag.to_string(),
            direction: dir,
            channels: 1,
            native_sample_rates: vec![8_000],
        }
    }

    struct NullHandle;
    impl astar_audio::StreamHandle for NullHandle {
        fn stop(self: Box<Self>) {}
        fn pause(&self) -> Result<(), AudioError> {
            Ok(())
        }
        fn resume(&self) -> Result<(), AudioError> {
            Ok(())
        }
    }

    struct PullHandle {
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }
    impl astar_audio::StreamHandle for PullHandle {
        fn stop(mut self: Box<Self>) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
        fn pause(&self) -> Result<(), AudioError> {
            Ok(())
        }
        fn resume(&self) -> Result<(), AudioError> {
            Ok(())
        }
    }

    /// A pushable mic and an output pulled every 10 ms, as a device would.
    struct PushPullBackend {
        mic_sink: MicSink,
    }
    impl AudioBackend for PushPullBackend {
        fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
            Ok(vec![
                dev(Direction::Input, "m17-in"),
                dev(Direction::Output, "m17-out"),
            ])
        }
        fn default_input(&self) -> Option<DeviceInfo> {
            Some(dev(Direction::Input, "m17-in"))
        }
        fn default_output(&self) -> Option<DeviceInfo> {
            Some(dev(Direction::Output, "m17-out"))
        }
        fn open_input(
            &self,
            _device: &DeviceInfo,
            _config: astar_audio::StreamConfig,
            sink: Box<dyn InputSink>,
            _overruns: Arc<std::sync::atomic::AtomicU64>,
        ) -> Result<Box<dyn astar_audio::StreamHandle>, AudioError> {
            *self.mic_sink.lock().unwrap() = Some(sink);
            Ok(Box::new(NullHandle))
        }
        fn open_output(
            &self,
            _device: &DeviceInfo,
            _config: astar_audio::StreamConfig,
            mut source: Box<dyn OutputSource>,
        ) -> Result<Box<dyn astar_audio::StreamHandle>, AudioError> {
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = Arc::clone(&stop);
            let thread = thread::spawn(move || {
                let mut buf = [0.0f32; 160];
                while !stop_thread.load(Ordering::Relaxed) {
                    let _ = source.read(&mut buf);
                    thread::sleep(Duration::from_millis(10));
                }
            });
            Ok(Box::new(PullHandle {
                stop,
                thread: Some(thread),
            }))
        }
    }

    fn m17_station() -> (Station, Arc<Mutex<ConsoleSession>>, MicSink) {
        let mic_sink: MicSink = Arc::new(Mutex::new(None));
        let for_backend = Arc::clone(&mic_sink);
        let session = Arc::new(Mutex::new(ConsoleSession::new()));
        let station = Station::with_shared_session(
            StationConfig::default(),
            Arc::clone(&session),
            Box::new(move || {
                Box::new(PushPullBackend {
                    mic_sink: Arc::clone(&for_backend),
                }) as Box<dyn AudioBackend>
            }),
        );
        (station, session, mic_sink)
    }

    #[allow(clippy::cast_precision_loss)]
    fn push_mic_tone(mic_sink: &MicSink, ms: u32) {
        let n = (8_000 * ms / 1_000) as usize;
        let tone: Vec<f32> = (0..n)
            .map(|i| 0.6 * (std::f32::consts::TAU * 400.0 * i as f32 / 8_000.0).sin())
            .collect();
        if let Some(sink) = mic_sink.lock().unwrap().as_mut() {
            sink.write(&tone, 0.0);
        }
    }

    fn link(station: &Station, addr: SocketAddr) {
        station
            .m17_connect(&addr.ip().to_string(), addr.port(), 'A', "N0CALL")
            .expect("m17 connect");
        assert!(
            wait_until(|| station.snapshot().status == CallStatus::Answered, 2_000),
            "must link"
        );
    }

    /// Key, push 400 ms of tone, unkey.
    fn talk(station: &Station, mic_sink: &MicSink) {
        station.set_ptt(true).expect("key");
        assert!(wait_until(|| station.snapshot().ptt, 1_000), "must key");
        push_mic_tone(mic_sink, 400);
        station.set_ptt(false).expect("unkey");
        assert!(wait_until(|| !station.snapshot().ptt, 1_000), "must unkey");
    }

    #[test]
    fn app_and_server_frame_taps_do_not_trample() {
        let (addr, _rec, _reflector) = spawn_recording_parrot();
        let (station, session, mic_sink) = m17_station();
        link(&station, addr);
        let dir = scratch("trample");

        // The server's logger owns the frame slot: the app must not start.
        let (t, server_rx) = tap_channel::<FrameEvent>(FRAME_TAP_CAPACITY);
        station.set_frame_tap(Some(t));
        match station.record_start(request(&dir, true)) {
            Err(StationError::Record(m)) => assert_eq!(m, "frame tap in use"),
            other => panic!("expected Record(frame tap in use), got {other:?}"),
        }

        // With no app recorder, neither record_stop nor disconnect touches it.
        let _ = station.record_stop();
        station.disconnect();
        assert!(
            session.lock().unwrap().record_taps().frames.is_some(),
            "the server's frame tap survives record_stop and disconnect"
        );
        let (addr2, _rec2, _reflector2) = spawn_recording_parrot();
        link(&station, addr2);
        talk(&station, &mic_sink);
        assert!(
            wait_until(
                || matches!(server_rx.try_recv(), Ok(FrameEvent::Frame { .. })),
                3_000
            ),
            "a frame offered after the disconnect still reaches the server's tap"
        );

        // Server tap off: the app records, and its stop clears all three.
        station.set_frame_tap(None);
        station.record_start(request(&dir, true)).expect("start");
        {
            let s = session.lock().unwrap();
            let taps = s.record_taps();
            assert!(taps.heard.is_some() && taps.sent.is_some() && taps.frames.is_some());
        }
        let _ = station.record_stop();
        {
            let s = session.lock().unwrap();
            let taps = s.record_taps();
            assert!(
                taps.heard.is_none() && taps.sent.is_none() && taps.frames.is_none(),
                "record_stop clears what record_start installed"
            );
        }
        station.disconnect();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The 8-byte halves of `pkt`.
    fn halves(pkt: &StreamPacket) -> [&[u8]; 2] {
        [&pkt.payload[0..8], &pkt.payload[8..16]]
    }

    #[test]
    fn an_m17_loopback_writes_the_ambe_the_reflector_sent() {
        let (addr, recorded, _reflector) = spawn_recording_parrot();
        let (station, _session, mic_sink) = m17_station();
        link(&station, addr);
        let dir = scratch("m17");
        station.record_start(request(&dir, true)).expect("start");
        talk(&station, &mic_sink);

        let manifest = dir.join("captured/codec2-3200/manifest.jsonl");
        // Wait for the echo to play out: the receive stream closes on its
        // EOS, and the recorder writes the row on its next tick.
        assert!(
            wait_until(
                || read_rows(&manifest).is_ok_and(|r| r.iter().any(|r| r.astar.direction == "rx")),
                5_000
            ),
            "the echoed stream is captured"
        );
        let state = station.record_stop();
        assert!(state.error.is_none(), "{state:?}");
        station.disconnect();

        let sent: Vec<StreamPacket> = recorded
            .lock()
            .unwrap()
            .iter()
            .map(|b| StreamPacket::parse(b).expect("a stream packet"))
            .collect();
        assert!(sent.len() > 2, "the tone went out");
        let last = sent.len() - 1;
        // Transmit: the halves that carried audio (the EOS packet's zero
        // padding records nothing). Receive: every echoed half.
        let tx: Vec<u8> = sent
            .iter()
            .enumerate()
            .flat_map(|(i, p)| halves(p).map(|h| (i, h.to_vec())))
            .filter(|(i, h)| *i != last || h.iter().any(|&b| b != 0))
            .flat_map(|(_, h)| h)
            .collect();
        let rx: Vec<u8> = sent
            .iter()
            .flat_map(|p| halves(p).map(<[u8]>::to_vec))
            .flatten()
            .collect();

        let rows = read_rows(&manifest).expect("manifest");
        assert_eq!(rows.len(), 2, "one tx and one rx row, got {rows:#?}");
        let air = dir.join("captured/codec2-3200");
        for (direction, want) in [("tx", &tx), ("rx", &rx)] {
            let row = rows
                .iter()
                .find(|r| r.astar.direction == direction)
                .unwrap_or_else(|| panic!("a {direction} row"));
            let stem = &row.key;
            let ambe = std::fs::read(air.join(format!("{stem}.ambe"))).expect("ambe");
            assert_eq!(
                &ambe, want,
                "{direction}: the .ambe is the halves on the wire"
            );
            assert_eq!(
                row.frames as usize,
                want.len() / 8,
                "{direction}: row.frames"
            );
            let (channels, rate, samples) = flac_info(&air.join(format!("{stem}.flac")));
            assert_eq!((channels, rate), (1, 8_000));
            assert_eq!(
                samples,
                u64::from(row.frames) * 160,
                "{direction}: 160 samples per frame"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
