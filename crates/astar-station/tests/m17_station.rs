// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `Station` M17 surface (iax-f2b8 Task 4): the graft's end-to-end behavior
//! through `Station::m17_connect`/`m17_disconnect`/`m17_available`, against a
//! scripted fake reflector socket — offline and deterministic. Mirrors the
//! `astar-console` `tests/m17_session.rs` fake-reflector pattern (not
//! extracted to a shared crate: each integration-test binary is a separate
//! compilation unit, so sharing would need a new `tests/common` support crate
//! for a handful of small helpers — not worth it for one test file).
#![cfg(feature = "m17")]

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use astar_audio::{AudioBackend, AudioError, DeviceInfo, Direction, InputSink, OutputSource};
use astar_console::{CallStatus, ConsoleConfig, ConsoleError, ConsoleSession};
use astar_iax::CodecPolicy;
use astar_m17::{StreamPacket, decode_callsign};
use astar_station::{Station, StationConfig, StationError};

// ---- station + shared-session helpers --------------------------------------

fn test_station() -> Station {
    Station::with_backend_factory(
        StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    )
}

/// A station sharing its `ConsoleSession` with the test, so a dummy IAX2 call
/// can be pooled directly on the session (mirrors
/// `wireguard_transport.rs`'s `shared_station` helper) to exercise the
/// mutual-exclusion guard without a real IAX2 dial round-trip.
fn shared_station() -> (Station, Arc<Mutex<ConsoleSession>>) {
    let session = Arc::new(Mutex::new(ConsoleSession::new()));
    let station = Station::with_shared_session(
        StationConfig::default(),
        Arc::clone(&session),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    (station, session)
}

/// A bound-but-silent loopback socket: IAX2 dial traffic lands here and is
/// never answered. The caller keeps the returned `UdpSocket` alive for the
/// test's duration.
fn silent_peer() -> (UdpSocket, SocketAddr) {
    let s = UdpSocket::bind("127.0.0.1:0").expect("bind silent peer");
    let addr = s.local_addr().expect("local addr");
    (s, addr)
}

fn dummy_console_config(node: &str) -> ConsoleConfig {
    ConsoleConfig {
        node: node.to_string(),
        calling_node: node.to_string(),
        secret: "s".into(),
        name: "t".into(),
        input_device: None,
        output_device: None,
        codec_policy: CodecPolicy::default(),
    }
}

// ---- deadline-polling helper (house style: no fixed sleeps) --------------

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

// ---- fake M17 reflector -----------------------------------------------------

type CapturedPackets = Arc<Mutex<Vec<Vec<u8>>>>;

/// CONN(11B) -> ACKN; captures every 54-byte `"M17 "` stream packet it
/// receives into `captured` (unanswered — no parrot echo needed for these
/// tests).
fn spawn_capturing_reflector() -> (SocketAddr, CapturedPackets, thread::JoinHandle<()>) {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind fake reflector");
    sock.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set read timeout");
    let addr = sock.local_addr().expect("local addr");
    let captured: CapturedPackets = Arc::new(Mutex::new(Vec::new()));
    let captured_thread = Arc::clone(&captured);
    let t = thread::spawn(move || {
        let mut buf = [0u8; 4_096];
        let mut acked = false;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            let Ok((n, src)) = sock.recv_from(&mut buf) else {
                continue;
            };
            if !acked && n == 11 && &buf[0..4] == b"CONN" {
                acked = true;
                let _ = sock.send_to(b"ACKN", src);
            } else if n == 54 && &buf[0..4] == b"M17 " {
                captured_thread.lock().unwrap().push(buf[..n].to_vec());
            }
        }
    });
    (addr, captured, t)
}

/// CONN(11B) -> NACK, always — simulates a reflector rejecting/failing the
/// link (iax-f2b8-fix Fix 1's "fake reflector NACK" scenario).
fn spawn_nacking_reflector() -> SocketAddr {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind fake reflector");
    sock.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set read timeout");
    let addr = sock.local_addr().expect("local addr");
    thread::spawn(move || {
        let mut buf = [0u8; 4_096];
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            let Ok((n, src)) = sock.recv_from(&mut buf) else {
                continue;
            };
            if n == 11 && &buf[0..4] == b"CONN" {
                let _ = sock.send_to(b"NACK", src);
            }
        }
    });
    addr
}

/// CONN(11B) -> ACKN; echoes any 54-byte `"M17 "` stream packet back to the
/// sender (a parrot) — iax-f2b8-fix Fix 6's RX-spectrum test needs the
/// far end to actually hear something back, unlike `spawn_capturing_reflector`.
fn spawn_parrot_reflector() -> (SocketAddr, thread::JoinHandle<()>) {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind fake reflector");
    sock.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set read timeout");
    let addr = sock.local_addr().expect("local addr");
    let t = thread::spawn(move || {
        let mut buf = [0u8; 4_096];
        let mut acked = false;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            let Ok((n, src)) = sock.recv_from(&mut buf) else {
                continue;
            };
            if !acked && n == 11 && &buf[0..4] == b"CONN" {
                acked = true;
                let _ = sock.send_to(b"ACKN", src);
            } else if n == 54 && &buf[0..4] == b"M17 " {
                let _ = sock.send_to(&buf[..n], src);
            }
        }
    });
    (addr, t)
}

// ---- Fix 6: TX/RX spectrum + input level reach the M17 router --------------
//
// `NullBackend` drops BOTH the mic sink and the output source — nothing ever
// pushes mic PCM in, and nothing ever calls `OutputSource::read` (which is
// where `AudioRouter`'s RX spectrum analyzer is actually fed), so it can't
// exercise this fix at all. This is a small push-capable INPUT + actively
// PULLED OUTPUT backend: `open_output` spawns a background thread that pulls
// from the source every 10ms, exactly as a real cpal output callback would,
// so the M17 parrot-echo path can be observed end to end.

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

/// Joins the background pull thread on `stop()` (bounds the 10ms pull loop).
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

fn station_with_push_pull_backend() -> (Station, MicSink) {
    let mic_sink: MicSink = Arc::new(Mutex::new(None));
    let sink_for_backend = Arc::clone(&mic_sink);
    let station = Station::with_backend_factory(
        StationConfig::default(),
        Box::new(move || {
            Box::new(PushPullBackend {
                mic_sink: Arc::clone(&sink_for_backend),
            }) as Box<dyn AudioBackend>
        }),
    );
    (station, mic_sink)
}

/// Push `ms` milliseconds of an 8 kHz mono tone at `freq_hz` straight into the
/// stashed mic sink (the router's real `MicLane`), exactly as a single cpal
/// capture callback would. No-op if the router hasn't opened the mic yet.
#[allow(clippy::cast_precision_loss)] // sample index -> f32 for a test tone; no precision concerns at these lengths
fn push_mic_tone(mic_sink: &MicSink, freq_hz: f32, ms: u32) {
    let n = (8_000 * ms / 1_000) as usize;
    let tone: Vec<f32> = (0..n)
        .map(|i| 0.6 * (std::f32::consts::TAU * freq_hz * i as f32 / 8_000.0).sin())
        .collect();
    if let Some(sink) = mic_sink.lock().unwrap().as_mut() {
        sink.write(&tone, 0.0);
    }
}

// ---- the tests --------------------------------------------------------------

#[test]
fn m17_connect_reaches_answered_and_reports_m17_active() {
    let (addr, _captured, _reflector) = spawn_capturing_reflector();
    let station = test_station();

    // Lowercase module + uppercase callsign both round-trip: the station
    // uppercases `module` before validating/dialing.
    station
        .m17_connect(&addr.ip().to_string(), addr.port(), 'a', "N0CALL")
        .expect("m17 connect");

    assert!(
        wait_until(
            || {
                let s = station.snapshot();
                s.status == CallStatus::Answered && s.m17_active
            },
            2_000
        ),
        "must reach Answered + m17_active once the reflector ACKs, got {:?}",
        station.snapshot()
    );

    station.m17_disconnect();
    assert!(
        wait_until(|| !station.snapshot().m17_active, 1_000),
        "m17_disconnect must clear m17_active"
    );
}

#[test]
fn m17_connect_rejects_invalid_module_and_empty_callsign() {
    let station = test_station();
    let err = station
        .m17_connect("127.0.0.1", 17_000, '1', "N0CALL")
        .expect_err("a digit is not A-Z");
    assert!(matches!(err, StationError::M17(_)), "got {err:?}");

    let err = station
        .m17_connect("127.0.0.1", 17_000, 'A', "")
        .expect_err("empty callsign must be rejected");
    assert!(matches!(err, StationError::M17(_)), "got {err:?}");
}

#[test]
fn set_ptt_sends_a_stream_packet_carrying_our_callsign() {
    let (addr, captured, _reflector) = spawn_capturing_reflector();
    let station = test_station();
    station
        .m17_connect(&addr.ip().to_string(), addr.port(), 'A', "N0CALL")
        .expect("m17 connect");
    assert!(
        wait_until(|| station.snapshot().status == CallStatus::Answered, 2_000),
        "must link before keying"
    );

    // Key then unkey: the key-up flush always sends at least one
    // stream packet (an EOS-marked, possibly all-zero-payload packet), even
    // with no mic audio queued — see M17Session's `apply_ptt_edge`.
    station.set_ptt(true).expect("key");
    assert!(
        wait_until(|| station.snapshot().ptt, 1_000),
        "set_ptt(true) must be applied"
    );
    station.set_ptt(false).expect("unkey");
    assert!(
        wait_until(|| !station.snapshot().ptt, 1_000),
        "set_ptt(false) must be applied"
    );

    assert!(
        wait_until(|| !captured.lock().unwrap().is_empty(), 2_000),
        "the reflector must receive at least one stream packet"
    );
    let pkt_bytes = captured.lock().unwrap()[0].clone();
    let pkt = StreamPacket::parse(&pkt_bytes).expect("valid stream packet");
    assert_eq!(
        decode_callsign(&pkt.lsf.src),
        "N0CALL",
        "the stream packet's SRC must decode to the configured callsign"
    );

    station.m17_disconnect();
}

#[test]
fn iax2_connect_is_refused_while_m17_is_live() {
    let (addr, _captured, _reflector) = spawn_capturing_reflector();
    let (station, session) = shared_station();

    station
        .m17_connect(&addr.ip().to_string(), addr.port(), 'A', "N0CALL")
        .expect("m17 connect");
    assert!(
        wait_until(|| station.snapshot().m17_active, 2_000),
        "m17 session must come up before the exclusion check"
    );

    let (_peer, peer_addr) = silent_peer();
    let err = session
        .lock()
        .unwrap()
        .connect(
            Box::new(astar_audio::NullBackend::new()),
            peer_addr,
            dummy_console_config("9999"),
        )
        .expect_err("IAX2 connect must be refused while M17 is live");
    assert!(
        matches!(err, ConsoleError::AlreadyConnected),
        "expected AlreadyConnected, got {err:?}"
    );

    station.m17_disconnect();
}

#[test]
fn m17_connect_is_refused_while_an_iax2_call_is_live() {
    let (station, session) = shared_station();
    let (_peer, peer_addr) = silent_peer();
    session
        .lock()
        .unwrap()
        .connect(
            Box::new(astar_audio::NullBackend::new()),
            peer_addr,
            dummy_console_config("9999"),
        )
        .expect("dial pools a call");

    let err = station
        .m17_connect("127.0.0.1", 17_000, 'A', "N0CALL")
        .expect_err("M17 connect must be refused while an IAX2 call is live");
    assert!(
        matches!(err, StationError::AlreadyConnected),
        "expected AlreadyConnected, got {err:?}"
    );

    station.disconnect();
}

// ---- iax-f2b8-fix Fix 1: disconnect() must clear a live/failed M17 session --

#[test]
fn disconnect_clears_a_failed_m17_session_so_a_fresh_m17_connect_succeeds() {
    let bad_addr = spawn_nacking_reflector();
    let station = test_station();

    station
        .m17_connect(&bad_addr.ip().to_string(), bad_addr.port(), 'A', "N0CALL")
        .expect("m17 connect opens (the reflector rejects it after)");
    assert!(
        wait_until(
            || matches!(station.snapshot().status, CallStatus::Hangup { .. }),
            2_000
        ),
        "the reflector's NACK must fail the link, got {:?}",
        station.snapshot()
    );
    assert!(
        station.snapshot().m17_active,
        "m17 must still be reported active (wedged) until an explicit disconnect"
    );

    station.disconnect();

    assert!(
        wait_until(|| !station.snapshot().m17_active, 1_000),
        "disconnect() must clear a failed M17 session, not just an IAX2 call"
    );

    // The bug: before the fix, m17_connect's own AlreadyConnected guard
    // (`self.active.is_some() || self.m17.is_some()`) stayed tripped forever
    // because `self.m17` was never cleared, so this reconnect would fail
    // immediately with StationError::AlreadyConnected instead of dialing.
    let (good_addr, _captured, _reflector) = spawn_capturing_reflector();
    station
        .m17_connect(&good_addr.ip().to_string(), good_addr.port(), 'A', "N0CALL")
        .expect("a fresh m17_connect must succeed once disconnect() cleared the prior session");
    assert!(
        wait_until(
            || {
                let s = station.snapshot();
                s.status == CallStatus::Answered && s.m17_active
            },
            2_000
        ),
        "the fresh m17 session must reach Answered, got {:?}",
        station.snapshot()
    );

    station.m17_disconnect();
}

#[test]
fn disconnect_clears_a_failed_m17_session_so_an_iax2_connect_succeeds() {
    let (station, session) = shared_station();
    let bad_addr = spawn_nacking_reflector();

    station
        .m17_connect(&bad_addr.ip().to_string(), bad_addr.port(), 'A', "N0CALL")
        .expect("m17 connect opens (the reflector rejects it after)");
    assert!(
        wait_until(
            || matches!(station.snapshot().status, CallStatus::Hangup { .. }),
            2_000
        ),
        "the reflector's NACK must fail the link, got {:?}",
        station.snapshot()
    );

    station.disconnect();

    assert!(
        wait_until(|| !station.snapshot().m17_active, 1_000),
        "disconnect() must clear a failed M17 session"
    );

    // Before the fix, `session.m17` (and thus `m17_is_active()`) stayed
    // `Some` forever, so every subsequent IAX2 `connect` (any UI's universal
    // pre-dial teardown is `Station::disconnect()`) kept returning
    // `ConsoleError::AlreadyConnected` — wedging ALL dialing, not just M17's.
    let (_peer, peer_addr) = silent_peer();
    session
        .lock()
        .unwrap()
        .connect(
            Box::new(astar_audio::NullBackend::new()),
            peer_addr,
            dummy_console_config("9999"),
        )
        .expect("a fresh IAX2 connect must succeed once disconnect() cleared the m17 session");

    station.disconnect();
}

// ---- iax-f2b8-fix Fix 2: snapshot's m17_available must be dirs-aware --------

#[test]
fn snapshot_m17_available_always_matches_station_m17_available() {
    // The documented contract (ffi.rs, the C header, and the Swift binding all
    // promise this): `Station::snapshot().m17_available` must mirror
    // `Station::m17_available()` exactly, INCLUDING after `set_codec_dirs`
    // repoints the probe — not the dirs-blind `astar_console::m17_available()`
    // process-wide cache `ConsoleSession::snapshot` populates it from.
    let station = test_station();

    assert_eq!(
        station.snapshot().m17_available,
        station.m17_available(),
        "snapshot().m17_available must match Station::m17_available() before any set_codec_dirs call"
    );
    assert_eq!(
        station
            .try_snapshot()
            .expect("uncontended lock")
            .m17_available,
        station.m17_available(),
        "try_snapshot().m17_available must match too"
    );

    // Repoint the probe at a directory that can't possibly hold a real
    // libcodec2 — this flips (or at least re-probes) the memoized dirs-aware
    // cache on `Station`, which the dirs-blind process-wide cache in
    // `astar_console::m17_available()` never sees.
    station.set_codec_dirs(vec![std::path::PathBuf::from(
        "/nonexistent/iax-f2b8-fix/codec2/dir",
    )]);

    assert_eq!(
        station.snapshot().m17_available,
        station.m17_available(),
        "snapshot().m17_available must still match Station::m17_available() after set_codec_dirs"
    );
    assert_eq!(
        station
            .try_snapshot()
            .expect("uncontended lock")
            .m17_available,
        station.m17_available(),
        "try_snapshot().m17_available must still match too"
    );
}

// ---- iax-f2b8-fix Fix 6: TX/RX spectrum + input level reach the M17 router -

#[test]
fn m17_tx_spectrum_reacts_while_keyed_with_pushed_tone() {
    let (addr, _captured, _reflector) = spawn_capturing_reflector();
    let (station, mic_sink) = station_with_push_pull_backend();

    station
        .m17_connect(&addr.ip().to_string(), addr.port(), 'A', "N0CALL")
        .expect("m17 connect");
    assert!(
        wait_until(|| station.snapshot().status == CallStatus::Answered, 2_000),
        "must link before keying"
    );

    station.set_ptt(true).expect("key");
    assert!(wait_until(|| station.snapshot().ptt, 1_000), "must key");

    // 400ms @ 8kHz fills several overlapping FFT windows (FFT_SIZE=2048,
    // HOP=256) so the TX spectrum has real signal to analyze, not just an
    // empty/floored buffer.
    push_mic_tone(&mic_sink, 400.0, 400);

    let mut bins = [0.0_f32; astar_audio::SPECTRUM_BINS];
    assert!(
        wait_until(
            || {
                let n = station.tx_spectrum(&mut bins);
                n == astar_audio::SPECTRUM_BINS
                    && bins.iter().copied().fold(f32::MIN, f32::max) > -30.0
            },
            2_000
        ),
        "TX spectrum must light up off the floor while keyed with a pushed tone \
         (before Fix 6, Station::tx_spectrum only ever read the IAX2 Manager, \
         which is None during an M17 call — always 0 bins)"
    );

    station.set_ptt(false).expect("unkey");
    station.m17_disconnect();
}

#[test]
fn m17_rx_spectrum_reacts_via_parrot_echo() {
    let (addr, _reflector) = spawn_parrot_reflector();
    let (station, mic_sink) = station_with_push_pull_backend();

    station
        .m17_connect(&addr.ip().to_string(), addr.port(), 'A', "N0CALL")
        .expect("m17 connect");
    assert!(
        wait_until(|| station.snapshot().status == CallStatus::Answered, 2_000),
        "must link before keying"
    );

    station.set_ptt(true).expect("key");
    assert!(wait_until(|| station.snapshot().ptt, 1_000), "must key");
    push_mic_tone(&mic_sink, 400.0, 400);
    // Unkey to flush the EOS-terminated burst — the reflector echoes every
    // stream packet it receives, keyed or not, but this mirrors a real
    // over-and-back exchange.
    station.set_ptt(false).expect("unkey");
    assert!(wait_until(|| !station.snapshot().ptt, 1_000), "must unkey");

    let mut bins = [0.0_f32; astar_audio::SPECTRUM_BINS];
    assert!(
        wait_until(
            || {
                let n = station.rx_spectrum(&mut bins);
                n == astar_audio::SPECTRUM_BINS
                    && bins.iter().copied().fold(f32::MIN, f32::max) > -30.0
            },
            3_000
        ),
        "RX spectrum must react once the reflector echoes the transmitted tone \
         back and the M17 router decodes it (before Fix 6, Station::rx_spectrum \
         only ever read the IAX2 Manager, which is None during an M17 call)"
    );

    station.m17_disconnect();
}

#[test]
fn input_level_db_mirrors_the_m17_router_while_unkeyed() {
    let (addr, _captured, _reflector) = spawn_capturing_reflector();
    let (station, mic_sink) = station_with_push_pull_backend();

    station
        .m17_connect(&addr.ip().to_string(), addr.port(), 'A', "N0CALL")
        .expect("m17 connect");
    assert!(
        wait_until(|| station.snapshot().status == CallStatus::Answered, 2_000),
        "must link"
    );
    assert!(
        !station.snapshot().ptt,
        "this test deliberately never keys — the mic input meter must be \
         continuous (VOX needs to key from silence) regardless of PTT"
    );

    push_mic_tone(&mic_sink, 400.0, 400);

    assert!(
        wait_until(|| station.snapshot().input_level_db > -30.0, 2_000),
        "input_level_db must mirror the M17 router's continuous mic input \
         meter even while unkeyed, got {}",
        station.snapshot().input_level_db
    );

    station.m17_disconnect();
}

#[test]
fn m17_available_matches_the_codec_probe() {
    // Hermetic: this asserts the two probes AGREE, not a fixed bool — the
    // static codec2 backend is forced on for this crate's own test builds
    // (see the Cargo.toml dev-dependency), but the equivalence itself must
    // hold regardless of what's actually installed on the machine.
    let station = test_station();
    assert_eq!(
        station.m17_available(),
        astar_console::m17_available(),
        "Station::m17_available must mirror astar_console::m17_available exactly"
    );
}

// ---- dial-wedge repro: disconnect mid-Connecting (not just post-Failed) ----
//
// Rob's live bug report: hitting hang-up WHILE a connection is still trying
// to connect (Dialing/Connecting, before any Answered/Failed/Hangup) wedges
// the engine — a subsequent connect refuses AlreadyConnected. The existing
// `disconnect_clears_a_failed_m17_session_*` tests only prove the
// post-FAILED path works; this exercises the mid-CONNECTING path, which
// never reaches Failed on its own (the silent peer never NACKs OR ACKs, so
// the FSM just keeps resending CONN once a second — see
// `SessionFsm::tick`'s `CONN_RESEND_INTERVAL`).

#[test]
fn disconnect_mid_connecting_m17_session_lets_a_fresh_m17_connect_succeed() {
    let (_silent, silent_addr) = silent_peer();
    let station = test_station();

    station
        .m17_connect(
            &silent_addr.ip().to_string(),
            silent_addr.port(),
            'A',
            "N0CALL",
        )
        .expect("m17 connect opens (the silent peer never answers)");
    assert!(
        wait_until(|| station.snapshot().status == CallStatus::Dialing, 2_000),
        "the link must sit in Dialing/Connecting against a silent peer, got {:?}",
        station.snapshot()
    );
    assert!(
        station.snapshot().m17_active,
        "m17 must be reported active while still connecting"
    );

    station.disconnect();

    assert!(
        wait_until(|| !station.snapshot().m17_active, 1_000),
        "disconnect() must clear an in-flight (Connecting) M17 session, not just a Failed one"
    );

    let (good_addr, _captured, _reflector) = spawn_capturing_reflector();
    station
        .m17_connect(&good_addr.ip().to_string(), good_addr.port(), 'A', "N0CALL")
        .expect("a fresh m17_connect must succeed once disconnect() cleared the in-flight session");
    assert!(
        wait_until(
            || {
                let s = station.snapshot();
                s.status == CallStatus::Answered && s.m17_active
            },
            2_000
        ),
        "the fresh m17 session must reach Answered, got {:?}",
        station.snapshot()
    );

    station.m17_disconnect();
}

#[test]
fn disconnect_mid_connecting_m17_session_lets_a_fresh_iax2_connect_succeed() {
    let (_silent, silent_addr) = silent_peer();
    let (station, session) = shared_station();

    station
        .m17_connect(
            &silent_addr.ip().to_string(),
            silent_addr.port(),
            'A',
            "N0CALL",
        )
        .expect("m17 connect opens (the silent peer never answers)");
    assert!(
        wait_until(|| station.snapshot().status == CallStatus::Dialing, 2_000),
        "the link must sit in Dialing/Connecting against a silent peer, got {:?}",
        station.snapshot()
    );

    station.disconnect();

    assert!(
        wait_until(|| !station.snapshot().m17_active, 1_000),
        "disconnect() must clear an in-flight M17 session"
    );

    let (_peer, peer_addr) = silent_peer();
    session
        .lock()
        .unwrap()
        .connect(
            Box::new(astar_audio::NullBackend::new()),
            peer_addr,
            dummy_console_config("9999"),
        )
        .expect("a fresh IAX2 connect must succeed once disconnect() cleared the m17 session");

    station.disconnect();
}

#[test]
fn disconnect_mid_dialing_iax2_call_lets_a_fresh_iax2_connect_succeed() {
    // The IAX2/WT analog: dial a silent peer (never answers -> Dialing
    // forever), disconnect while still Dialing, then a fresh IAX2 connect
    // must succeed (this is Station::connect's plain dial path, not M17 at
    // all — proving disconnect-mid-dial isn't an M17-specific gap).
    let (station, session) = shared_station();
    let (_peer, peer_addr) = silent_peer();

    session
        .lock()
        .unwrap()
        .connect(
            Box::new(astar_audio::NullBackend::new()),
            peer_addr,
            dummy_console_config("9999"),
        )
        .expect("dial pools a call");
    assert_eq!(
        station.snapshot().status,
        CallStatus::Dialing,
        "a dial to a silent peer must sit in Dialing"
    );

    station.disconnect();

    let (_peer2, peer_addr2) = silent_peer();
    session
        .lock()
        .unwrap()
        .connect(
            Box::new(astar_audio::NullBackend::new()),
            peer_addr2,
            dummy_console_config("9999"),
        )
        .expect("a fresh IAX2 connect must succeed once disconnect() cleared the Dialing call");

    station.disconnect();
}
