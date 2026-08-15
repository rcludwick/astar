// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `Station` D-Star surface (iax-a9d4 Task 6): the graft's end-to-end
//! behavior through `Station::dstar_connect`/`dstar_disconnect`/
//! `dstar_available`/`dstar_state`, against a real
//! `astar_dstar::Reflector`. Mirrors `tests/m17_station.rs`'s shape
//! (mutual exclusion, disconnect-clears-a-live-session, availability probe).
//!
//! D-Star is hardware-only (iax-b3e7 M0): `--features dstar` pulls
//! `astar-codec/ambe-hw` (the only AMBE backend), and `Station::dstar_connect`
//! requests the `ThumbDV` backend unconditionally — "no dongle, no D-Star"
//! applies here too. Every test below that calls `dstar_connect` for a
//! SUCCESSFUL connect therefore needs a real dongle, and is gated exactly as
//! spec §5 requires: [`hardware_opted_in`] is the first statement in the
//! body, so a dongle-less machine SKIPS (green) instead of failing with a
//! "no `ThumbDV` detected" panic indistinguishable from a real regression.
//!
//! The tests that do NOT need a working vocoder — argument validation, the
//! IAX2 mutual-exclusion refusal, the availability probe, and the
//! no-dongle error contract — stay ungated and run everywhere.
//!
//! Only ONE process/thread may hold the real `ThumbDV` at a time, but
//! `cargo test` runs a binary's tests on multiple threads by default.
//! [`hardware_lock`] serializes them so the suite passes deterministically
//! regardless of `--test-threads` (a spurious "no `ThumbDV` detected" from
//! two tests racing to open the same port is a test-harness artifact, not a
//! product bug). The guard also settles after a session drops: closing the
//! port is asynchronous (the vocoder worker notices its channel closed up to
//! 20 ms later), so releasing the lock the instant a test body ends would
//! let the next opener race a port that is still held.
#![cfg(feature = "dstar")]

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// See the module doc: every test that calls `Station::dstar_connect` must
/// hold this lock for its whole body so `cargo test`'s default parallel
/// execution never races two tests over the one real `ThumbDV` port.
/// `unwrap_or_else(PoisonError::into_inner)` recovers from a prior test
/// panicking while holding the lock — mutual exclusion is all that's needed
/// here, not the poisoned data (there is none).
fn hardware_lock() -> HardwareGuard {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    HardwareGuard(Some(
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    ))
}

/// `true` (and, on stderr, why) when a test may actually touch hardware —
/// spec §5's `IAX_THUMBDV_TESTS=1` gate. See the module doc.
fn hardware_opted_in() -> bool {
    if std::env::var("IAX_THUMBDV_TESTS").ok().as_deref() == Some("1") {
        return true;
    }
    eprintln!(
        "skipping hardware ThumbDV test (set IAX_THUMBDV_TESTS=1 with a real dongle attached)"
    );
    false
}

/// How long to let the vocoder worker actually close the serial fd after a
/// session drops, before the next test may open it. See the module doc.
const PORT_SETTLE: Duration = Duration::from_millis(40);

/// Held for the whole body of any test that opens the real dongle; settles
/// on drop so the port is genuinely closed before the next waiter wakes.
struct HardwareGuard(Option<std::sync::MutexGuard<'static, ()>>);

impl Drop for HardwareGuard {
    fn drop(&mut self) {
        thread::sleep(PORT_SETTLE);
        drop(self.0.take());
    }
}

use astar_audio::{
    AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, OutputSource,
    StreamConfig, StreamHandle,
};
use astar_console::{ConsoleConfig, ConsoleError, ConsoleSession};
use astar_dstar::{DsvtPacket, LinkState, Reflector, RfHeader};
use astar_iax::CodecPolicy;
use astar_station::{Station, StationConfig, StationError};

// ---- station + shared-session helpers --------------------------------------

fn test_station() -> Station {
    Station::with_backend_factory(
        StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    )
}

/// A station sharing its `ConsoleSession` with the test, so a dummy IAX2
/// call can be pooled directly on the session (mirrors `m17_station.rs`'s
/// `shared_station` helper) to exercise the mutual-exclusion guard without a
/// real IAX2 dial round-trip.
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

/// Bind + run a real `astar_dstar::Reflector`; returns its address. The
/// handle is intentionally leaked (`std::mem::forget`) — these tests only
/// need the reflector alive for the process lifetime of the test binary, and
/// keeping a `ReflectorHandle` alive across every early-return assertion
/// above would otherwise need a guard type; the OS reclaims the socket at
/// process exit either way.
fn spawn_reflector() -> SocketAddr {
    let reflector = Reflector::bind("127.0.0.1:0".parse().unwrap()).expect("bind dstar reflector");
    let addr = reflector.local_addr();
    std::mem::forget(reflector.run());
    addr
}

// ---- the tests --------------------------------------------------------------

#[test]
fn dstar_connect_reaches_linked_and_reports_active() {
    if !hardware_opted_in() {
        return;
    }
    let _hw = hardware_lock();
    let addr = spawn_reflector();
    let station = test_station();

    // Lowercase module + uppercase callsign both round-trip: the station
    // uppercases `module` before validating/dialing.
    station
        .dstar_connect(&addr.ip().to_string(), addr.port(), 'a', "N0CALL")
        .expect("dstar connect");

    assert!(
        wait_until(
            || station
                .dstar_state()
                .is_some_and(|s| s.link == LinkState::Linked),
            2_000
        ),
        "must reach Linked once the reflector ACKs, got {:?}",
        station.dstar_state()
    );

    station.dstar_disconnect();
    assert!(
        wait_until(|| station.dstar_state().is_none(), 1_000),
        "dstar_disconnect must clear the session"
    );
}

#[test]
fn dstar_connect_rejects_invalid_module_and_empty_callsign() {
    let station = test_station();
    let err = station
        .dstar_connect("127.0.0.1", 30_001, '1', "N0CALL")
        .expect_err("a digit is not A-Z");
    assert!(matches!(err, StationError::Dstar(_)), "got {err:?}");

    let err = station
        .dstar_connect("127.0.0.1", 30_001, 'A', "")
        .expect_err("empty callsign must be rejected");
    assert!(matches!(err, StationError::Dstar(_)), "got {err:?}");
}

/// iax-2f6b: D-Star is now full-transceive — `Station::set_ptt` must key/
/// unkey a live D-Star session exactly like it does an M17/IAX2 one, and
/// `tx_capable`/`ptt` in the snapshot must reflect it. Supersedes the old
/// "D-Star is receive-only, PTT is rejected" contract this test used to pin.
#[test]
fn set_ptt_keys_and_unkeys_a_live_dstar_session() {
    if !hardware_opted_in() {
        return;
    }
    let _hw = hardware_lock();
    let addr = spawn_reflector();
    let station = test_station();
    station
        .dstar_connect(&addr.ip().to_string(), addr.port(), 'A', "N0CALL")
        .expect("dstar connect");
    assert!(
        wait_until(
            || station
                .dstar_state()
                .is_some_and(|s| s.link == LinkState::Linked),
            2_000
        ),
        "must link before attempting PTT"
    );
    assert!(
        station.dstar_state().is_some_and(|s| s.tx_capable),
        "iax-2f6b: D-Star sessions are tx_capable now"
    );
    assert!(
        station.dstar_state().is_some_and(|s| !s.ptt),
        "must start unkeyed"
    );

    station.set_ptt(true).expect("D-Star PTT must be accepted");
    assert!(
        wait_until(|| station.dstar_state().is_some_and(|s| s.ptt), 1_000),
        "set_ptt(true) must be applied by the run-loop"
    );

    station
        .set_ptt(false)
        .expect("D-Star unkey must be accepted");
    assert!(
        wait_until(|| station.dstar_state().is_some_and(|s| !s.ptt), 1_000),
        "set_ptt(false) must be applied by the run-loop"
    );
    assert!(
        station
            .dstar_state()
            .is_some_and(|s| s.link == LinkState::Linked),
        "a key/unkey cycle must not disturb the link"
    );

    station.dstar_disconnect();
}

/// iax-2f6b: PTT must do nothing (return [`StationError::NotConnected`],
/// exactly like an idle IAX2/M17 station — `station_null.rs`'s
/// `set_ptt_idle_is_not_connected` pins the generic contract) when no
/// D-Star session is live, both before any `dstar_connect` and again after
/// one has been torn down. Hardware-free: no session is ever opened.
#[test]
fn set_ptt_is_not_connected_when_no_dstar_session_is_live() {
    let station = test_station();
    assert!(station.dstar_state().is_none(), "no session yet");
    assert!(
        matches!(station.set_ptt(true), Err(StationError::NotConnected)),
        "PTT with no D-Star (or any) session live must be rejected, not silently accepted"
    );

    // `dstar_disconnect` on an already-empty station is a documented no-op
    // (see `Station::dstar_disconnect`'s docs) — PTT must still be refused
    // afterward, not left in some half-adopted state.
    station.dstar_disconnect();
    assert!(
        matches!(station.set_ptt(false), Err(StationError::NotConnected)),
        "PTT must stay rejected after a no-op dstar_disconnect"
    );
}

#[test]
fn iax2_connect_is_refused_while_dstar_is_live() {
    if !hardware_opted_in() {
        return;
    }
    let _hw = hardware_lock();
    let addr = spawn_reflector();
    let (station, session) = shared_station();

    station
        .dstar_connect(&addr.ip().to_string(), addr.port(), 'A', "N0CALL")
        .expect("dstar connect");
    assert!(
        wait_until(
            || station
                .dstar_state()
                .is_some_and(|s| s.link == LinkState::Linked),
            2_000
        ),
        "dstar session must come up before the exclusion check"
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
        .expect_err("IAX2 connect must be refused while D-Star is live");
    assert!(
        matches!(err, ConsoleError::AlreadyConnected),
        "expected AlreadyConnected, got {err:?}"
    );

    station.dstar_disconnect();
}

#[test]
fn dstar_connect_is_refused_while_an_iax2_call_is_live() {
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
        .dstar_connect("127.0.0.1", 30_001, 'A', "N0CALL")
        .expect_err("D-Star connect must be refused while an IAX2 call is live");
    assert!(
        matches!(err, StationError::AlreadyConnected),
        "expected AlreadyConnected, got {err:?}"
    );

    station.disconnect();
}

// ---- disconnect()/Drop must clear a live D-Star session (mirrors the -----
// ---- iax-f2b8-fix Fix 1 lesson for M17 at iax-a9d4 Task 6)               --

#[test]
fn disconnect_clears_a_live_dstar_session_so_a_fresh_dstar_connect_succeeds() {
    if !hardware_opted_in() {
        return;
    }
    let _hw = hardware_lock();
    let addr = spawn_reflector();
    let station = test_station();

    station
        .dstar_connect(&addr.ip().to_string(), addr.port(), 'A', "N0CALL")
        .expect("dstar connect");
    assert!(
        wait_until(
            || station
                .dstar_state()
                .is_some_and(|s| s.link == LinkState::Linked),
            2_000
        ),
        "must link before disconnect()"
    );

    station.disconnect();

    assert!(
        wait_until(|| station.dstar_state().is_none(), 1_000),
        "Station::disconnect() must clear a live D-Star session, not just IAX2/M17"
    );

    // Before the fix (mirroring iax-f2b8-fix Fix 1), a live D-Star session
    // would stay wedged forever behind ConsoleSession::dstar_connect's own
    // AlreadyConnected guard.
    let addr2 = spawn_reflector();
    station
        .dstar_connect(&addr2.ip().to_string(), addr2.port(), 'A', "N0CALL")
        .expect("a fresh dstar_connect must succeed once disconnect() cleared the prior session");
    assert!(
        wait_until(
            || station
                .dstar_state()
                .is_some_and(|s| s.link == LinkState::Linked),
            2_000
        ),
        "the fresh dstar session must reach Linked"
    );

    station.dstar_disconnect();
}

#[test]
fn disconnect_clears_a_live_dstar_session_so_an_iax2_connect_succeeds() {
    if !hardware_opted_in() {
        return;
    }
    let _hw = hardware_lock();
    let addr = spawn_reflector();
    let (station, session) = shared_station();

    station
        .dstar_connect(&addr.ip().to_string(), addr.port(), 'A', "N0CALL")
        .expect("dstar connect");
    assert!(
        wait_until(
            || station
                .dstar_state()
                .is_some_and(|s| s.link == LinkState::Linked),
            2_000
        ),
        "must link"
    );

    station.disconnect();

    assert!(
        wait_until(|| station.dstar_state().is_none(), 1_000),
        "disconnect() must clear a live D-Star session"
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
        .expect("a fresh IAX2 connect must succeed once disconnect() cleared the dstar session");

    station.disconnect();
}

/// iax-2f6b safety: `Station::disconnect()` — the public facade, not
/// `dstar_disconnect()` — while a D-Star session is STILL KEYED (no
/// explicit unkey first) must still flush queued audio and emit a properly
/// EOT-terminated DSVT stream before the unlink. `astar-console`'s
/// `disconnect_while_keyed_flushes_and_terminates_the_stream` pins the same
/// guarantee (`DstarSession::disconnect`/`unlink_flushing_eot_if_keyed`)
/// one layer down against an injected fake vocoder; this exercises it
/// through the public `Station` API against the real hardware pipeline,
/// which is the only way to reach `Station::dstar_connect` at all (D-Star
/// is hardware-only, iax-b3e7 M0 — there is no vocoder-injection seam at
/// the `Station` layer). `disconnect()` itself must not hang.
#[test]
fn disconnect_while_keyed_flushes_and_terminates_the_stream_through_station() {
    if !hardware_opted_in() {
        return;
    }
    let _hw = hardware_lock();
    let addr = spawn_reflector();
    let station = Arc::new(test_station());

    station
        .dstar_connect(&addr.ip().to_string(), addr.port(), 'A', "N0CALL")
        .expect("dstar connect");
    assert!(
        wait_until(
            || station
                .dstar_state()
                .is_some_and(|s| s.link == LinkState::Linked),
            2_000
        ),
        "must link before keying"
    );

    // A second raw client, linked on the same module, stands in for a
    // listener on the wire: the reflector's plain relay forwards the
    // station's own outbound traffic to it immediately (this is
    // `Reflector::bind`, not `bind_parrot` — no echo delay back to the
    // sender to wait out).
    let (listener, _listener_addr) = raw_client();
    listener
        .send_to(&connect_bytes("N7WIRE", b'A'), addr)
        .expect("send connect");
    assert!(recv_packet(&listener).ends_with(b"ACK\0"));

    station.set_ptt(true).expect("D-Star PTT must be accepted");
    assert!(
        wait_until(|| station.dstar_state().is_some_and(|s| s.ptt), 1_000),
        "must key before disconnecting while keyed"
    );
    let header_bytes = recv_packet(&listener);
    let header_pkt = DsvtPacket::parse(&header_bytes).expect("valid header packet");
    assert!(
        matches!(header_pkt, DsvtPacket::Header { .. }),
        "key-down must send the header first, got {header_pkt:?}"
    );

    // `Station::disconnect()` joins the D-Star run-loop thread; run it
    // off-thread so a hang is a test FAILURE, not a suite that never
    // finishes (iax-239a).
    let station_for_thread = Arc::clone(&station);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        station_for_thread.disconnect();
        let _ = done_tx.send(());
    });
    assert!(
        done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
        "Station::disconnect() while keyed hung: the shutdown branch must flush+unkey \
         unconditionally"
    );

    // Drain whatever the flush emitted after the header: it must end with
    // an EOT-marked voice frame.
    let mut tail = Vec::new();
    loop {
        let mut buf = [0u8; 128];
        match listener.recv_from(&mut buf) {
            Ok((n, _)) => tail.push(buf[..n].to_vec()),
            Err(_) => break, // read-timeout: no more traffic
        }
    }
    assert!(
        !tail.is_empty(),
        "disconnecting while keyed must still flush a terminating frame onto the wire"
    );
    let last =
        DsvtPacket::parse(tail.last().expect("checked non-empty above")).expect("valid DsvtPacket");
    let DsvtPacket::Voice { end, .. } = last else {
        panic!("expected the flushed tail's last packet to be a voice frame, got {last:?}");
    };
    assert!(
        end,
        "the last frame flushed by a disconnect-while-keyed must carry the EOT bit"
    );

    assert!(
        wait_until(|| station.dstar_state().is_none(), 1_000),
        "Station::disconnect() must clear the D-Star session"
    );
}

// ---- end-of-stream drain coverage (iax-b3e7 M0) ----------------------------
//
// A pull-capable, output-only test backend + a raw-socket "talker" client
// feeding a canned DSVT stream through a real reflector's plain relay —
// mirrors `astar-console`'s `tests/dstar_session.rs` (that file's own
// doc comment explains why a second raw client is what gets audio TO the
// session under test, which is full-transceive but not keyed in this
// group of tests). Reimplemented here rather than shared:
// each integration-test binary is a separate compilation unit, and
// `m17_station.rs` already notes a `tests/common` crate isn't worth it for
// one file's fixtures.

struct NullHandle;
impl StreamHandle for NullHandle {
    fn stop(self: Box<Self>) {}
    fn pause(&self) -> Result<(), AudioError> {
        Ok(())
    }
    fn resume(&self) -> Result<(), AudioError> {
        Ok(())
    }
}

fn dev(dir: Direction, tag: &str) -> DeviceInfo {
    DeviceInfo {
        id: DeviceId::new(tag.to_string()),
        name: tag.to_string(),
        direction: dir,
        channels: 1,
        native_sample_rates: vec![8_000],
    }
}

type OutputTap = Arc<Mutex<Option<Box<dyn OutputSource>>>>;

struct PullBackend {
    output_tap: OutputTap,
}

impl AudioBackend for PullBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![
            dev(Direction::Input, "dstar-in"),
            dev(Direction::Output, "dstar-out"),
        ])
    }
    fn default_input(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Input, "dstar-in"))
    }
    fn default_output(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Output, "dstar-out"))
    }
    fn open_input(
        &self,
        _device: &DeviceInfo,
        _config: StreamConfig,
        _sink: Box<dyn InputSink>,
        _overruns: Arc<AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        // iax-2f6b: D-Star opens its mic lane lazily, on the first key-down
        // (see `astar_console::dstar::MicLane`), so this only has to
        // succeed for the tests here that actually key. The sink is dropped:
        // no test in this file pushes TX audio. The no-capture-device path is
        // covered in `dstar_thumbdv_hardware.rs` (hardware) and
        // `astar-console/tests/dstar_session_pipeline.rs` (hardware-free).
        Ok(Box::new(NullHandle))
    }
    fn open_output(
        &self,
        _device: &DeviceInfo,
        _config: StreamConfig,
        source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        *self.output_tap.lock().unwrap() = Some(source);
        Ok(Box::new(NullHandle))
    }
}

fn station_with_pull_backend() -> (Station, OutputTap) {
    let output_tap: OutputTap = Arc::new(Mutex::new(None));
    let tap_for_backend = Arc::clone(&output_tap);
    let station = Station::with_backend_factory(
        StationConfig::default(),
        Box::new(move || {
            Box::new(PullBackend {
                output_tap: Arc::clone(&tap_for_backend),
            }) as Box<dyn AudioBackend>
        }),
    );
    (station, output_tap)
}

/// Pulls up to `n` samples from the stashed `OutputTap` and returns their
/// peak amplitude (`0.0..=1.0`). `0.0` if the output device was never
/// opened.
fn pull_output_peak(output_tap: &OutputTap, n: usize) -> f32 {
    let mut buf = vec![0.0f32; n];
    let mut guard = output_tap.lock().unwrap();
    let Some(source) = guard.as_mut() else {
        return 0.0;
    };
    source.read(&mut buf);
    astar_audio::peak(&buf)
}

fn callsign8(cs: &str) -> [u8; 8] {
    let mut buf = [b' '; 8];
    buf[..cs.len()].copy_from_slice(cs.as_bytes());
    buf
}

fn connect_bytes(callsign: &str, dest_module: u8) -> Vec<u8> {
    let mut buf = Vec::with_capacity(11);
    buf.extend_from_slice(&callsign8(callsign));
    buf.push(b' ');
    buf.push(dest_module);
    buf.push(0x00);
    buf
}

fn raw_client() -> (UdpSocket, SocketAddr) {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind raw client");
    sock.set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set read timeout");
    let addr = sock.local_addr().expect("local addr");
    (sock, addr)
}

fn recv_packet(sock: &UdpSocket) -> Vec<u8> {
    let mut buf = [0u8; 128];
    let (n, _) = sock.recv_from(&mut buf).expect("expected a packet");
    buf[..n].to_vec()
}

fn talker_header(my_callsign: &str) -> RfHeader {
    let mut my = [b' '; 8];
    my[..my_callsign.len()].copy_from_slice(my_callsign.as_bytes());
    RfHeader {
        flags: [0x00, 0x00, 0x00],
        rpt2: *b"XRF757 G",
        rpt1: *b"XRF757 A",
        ur: *b"CQCQCQ  ",
        my,
        suffix: *b"    ",
    }
}

fn voice_frame(stream_id: u16, seq: u8, end: bool) -> DsvtPacket {
    DsvtPacket::Voice {
        stream_id,
        seq,
        end,
        ambe: [
            0xA5 ^ seq,
            0x3C,
            0x91,
            0x77,
            0x2E,
            0xC4,
            0x58,
            0xDA,
            0x0F ^ seq,
        ],
        slow_data: [0x00, 0x00, 0x00],
    }
}

/// Regression coverage for the end-of-stream drain (iax-b3e7 M0 spec §2), at
/// the `Station` facade level (mirrors `astar-console`'s
/// `tests/dstar_session.rs::dstar_session_drains_a_short_stream_shorter_
/// than_the_priming_window`, one layer up). The pipeline primes 3 frames
/// before its first poll; a 2-frame stream never reaches that threshold
/// during normal per-frame handling — without an explicit drain when `end`
/// arrives, the whole (short) transmission's audio would simply never reach
/// the speaker.
#[test]
fn dstar_session_drains_a_short_stream_through_the_station_facade() {
    if !hardware_opted_in() {
        return;
    }
    let _hw = hardware_lock();
    let addr = spawn_reflector();
    let (station, output_tap) = station_with_pull_backend();

    station
        .dstar_connect(&addr.ip().to_string(), addr.port(), 'A', "N0CALL")
        .expect("dstar connect");
    assert!(
        wait_until(
            || station
                .dstar_state()
                .is_some_and(|s| s.link == LinkState::Linked),
            2_000
        ),
        "must link before feeding audio"
    );

    let (talker_sock, _) = raw_client();
    talker_sock
        .send_to(&connect_bytes("AJ7HR", b'A'), addr)
        .unwrap();
    assert!(recv_packet(&talker_sock).ends_with(b"ACK\0"));

    let stream_id = 0x1313;
    let header = talker_header("AJ7HR");
    talker_sock
        .send_to(&DsvtPacket::Header { stream_id, header }.encode(), addr)
        .unwrap();
    talker_sock
        .send_to(&voice_frame(stream_id, 0, false).encode(), addr)
        .unwrap();
    talker_sock
        .send_to(&voice_frame(stream_id, 1, true).encode(), addr)
        .unwrap();

    assert!(
        wait_until(|| pull_output_peak(&output_tap, 4_000) > 0.0, 2_000),
        "a stream shorter than the priming window must still be drained at `end`, not clipped"
    );

    station.dstar_disconnect();
}

// ---- availability probe -----------------------------------------------------

#[test]
fn dstar_available_matches_the_ambe_probe() {
    // Hermetic: this asserts the station-level probe agrees with the
    // console-level one it delegates to, not a fixed bool — whether a
    // ThumbDV is attached depends on the machine, but the two probes must
    // always agree. No hardware lock needed since iax-b3e7's review fixes:
    // `dstar_available()` is now a VID/PID enumeration that opens no port
    // (it used to run a full `open_ambe` detect + init cookbook, which both
    // contended for the dongle and cached a permanently-wrong `false` if a
    // live session happened to hold it).
    let station = test_station();
    assert_eq!(
        station.dstar_available(),
        astar_console::dstar_available(),
        "Station::dstar_available must mirror astar_console::dstar_available exactly"
    );
}

// ---- error contract: no dongle -> StationError::Dstar ---------------------

/// Regression guard for `Station::dstar_connect`'s documented error
/// contract. Before iax-b3e7 the doc promised [`StationError::Audio`] when
/// no `ThumbDV` was detected; M0 made D-Star hardware-only and the session
/// now returns `ConsoleError::Dstar(classify_thumbdv_failure().message())`,
/// which maps to [`StationError::Dstar`]. The only test that pinned the old
/// mapping was deleted in the same commit, so nothing caught the drift — and
/// a UI matching on `StationError::Audio` (as the doc instructed) would send
/// every "dongle busy"/"no dongle" case to a generic error arm, hiding the
/// classified message spec §4 exists to produce.
///
/// Hardware-free and hardware-SAFE: `IAX_THUMBDV_PORT` is pointed at a path
/// no VID/PID scan can ever return, so the candidate list comes back empty
/// and nothing is opened — not the `ThumbDV`, and certainly not a radio
/// interface's serial port.
#[test]
fn dstar_connect_without_a_thumbdv_fails_with_stationerror_dstar() {
    // Serialized against every dongle-touching test in this binary: the env
    // var is process-global, and a concurrent `dstar_connect` must not see
    // the pinned path.
    let _hw = hardware_lock();
    // SAFETY: serialized by `hardware_lock`; no other test in this binary
    // reads or writes `IAX_THUMBDV_PORT` while the guard is held.
    unsafe {
        std::env::set_var("IAX_THUMBDV_PORT", "/dev/cu.usbserial-NOSUCHDEVICE");
    }
    let station = test_station();
    let err = station.dstar_connect("127.0.0.1", 30_001, 'A', "N0CALL");
    // SAFETY: same serialization as the `set_var` above.
    unsafe {
        std::env::remove_var("IAX_THUMBDV_PORT");
    }

    let err = err.expect_err("a connect with no ThumbDV available must fail");
    match err {
        StationError::Dstar(msg) => assert!(
            msg.contains("ThumbDV"),
            "the error must carry spec §4's classified, operator-facing message, got {msg:?}"
        ),
        other => panic!("expected StationError::Dstar, got {other:?}"),
    }
}

// ---- the session mutex must not be held across the connect ----------------

/// An audio backend whose device enumeration takes its time — standing in,
/// from inside `DstarSession::connect`, for the slow parts of a real connect
/// (the `ThumbDV` candidate scan and init cookbook, then the output-device
/// open), which together can run for seconds against a flaky dongle.
struct SlowBackend {
    delay: Duration,
    inner: astar_audio::NullBackend,
}

impl AudioBackend for SlowBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        thread::sleep(self.delay);
        self.inner.devices()
    }
    fn default_input(&self) -> Option<DeviceInfo> {
        self.inner.default_input()
    }
    fn default_output(&self) -> Option<DeviceInfo> {
        thread::sleep(self.delay);
        self.inner.default_output()
    }
    fn open_input(
        &self,
        device: &DeviceInfo,
        config: StreamConfig,
        sink: Box<dyn InputSink>,
        overruns: Arc<AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        self.inner.open_input(device, config, sink, overruns)
    }
    fn open_output(
        &self,
        device: &DeviceInfo,
        config: StreamConfig,
        source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        self.inner.open_output(device, config, source)
    }
}

fn slow_backend_station(delay: Duration) -> Station {
    Station::with_backend_factory(
        StationConfig::default(),
        Box::new(move || {
            Box::new(SlowBackend {
                delay,
                inner: astar_audio::NullBackend::new(),
            }) as Box<dyn AudioBackend>
        }),
    )
}

/// Regression guard for the `AstarStation` "poll + snapshot, never blocks"
/// contract. `Station::dstar_connect` used to run the whole of
/// `DstarSession::connect` — device probe, init handshake, audio open —
/// with the session mutex held, and EVERY other `Station` method takes that
/// same mutex (`snapshot`, `dstar_state`, `set_ptt`, `is_active`,
/// `call_count`). A UI polling on its usual tick froze for the entire
/// connect.
///
/// Hardware-free: a deliberately slow audio-backend factory supplies the
/// delay, and `IAX_THUMBDV_PORT` is pinned at a path no VID/PID scan can
/// return, so the connect fails (after the slow part) without opening any
/// serial device at all.
#[test]
fn dstar_connect_does_not_block_snapshot_polling() {
    const CONNECT_DELAY: Duration = Duration::from_millis(600);
    /// Generous: the lock is taken only for a precheck and an install, both
    /// a handful of instructions. Anything near `CONNECT_DELAY` means the
    /// connect is holding it across its slow work again.
    const POLL_BUDGET: Duration = Duration::from_millis(150);

    let _hw = hardware_lock();
    // SAFETY: serialized by `hardware_lock`; no other test in this binary
    // reads or writes `IAX_THUMBDV_PORT` while the guard is held.
    unsafe {
        std::env::set_var("IAX_THUMBDV_PORT", "/dev/cu.usbserial-NOSUCHDEVICE");
    }

    let station = Arc::new(slow_backend_station(CONNECT_DELAY));
    let connector = Arc::clone(&station);
    let handle = thread::spawn(move || {
        let _ = connector.dstar_connect("127.0.0.1", 30_001, 'A', "N0CALL");
    });

    // Poll the way a menu-bar UI does while the connect is in flight.
    let deadline = Instant::now() + CONNECT_DELAY;
    let mut worst = Duration::ZERO;
    let mut polls = 0usize;
    while Instant::now() < deadline {
        let t = Instant::now();
        let _ = station.snapshot();
        let _ = station.dstar_state();
        worst = worst.max(t.elapsed());
        polls += 1;
        thread::sleep(Duration::from_millis(10));
    }
    handle.join().expect("connect thread");
    // SAFETY: same serialization as the `set_var` above.
    unsafe {
        std::env::remove_var("IAX_THUMBDV_PORT");
    }

    assert!(
        worst < POLL_BUDGET,
        "a snapshot/dstar_state poll blocked for {worst:?} (budget {POLL_BUDGET:?}) while \
         dstar_connect was running: the session mutex is being held across the connect again"
    );
    assert!(polls > 5, "the poller must have actually run, got {polls}");
}
