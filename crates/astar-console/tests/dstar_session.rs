// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Integration test for [`DstarSession`] (iax-a9d4 Task 6 RX; iax-2f6b adds
//! TX): a full-transceive D-Star `DExtra` reflector client, exercised against
//! the REAL
//! `astar_dstar::Reflector` (Task 5) — offline, deterministic, real UDP
//! sockets on `127.0.0.1`. Mirrors `tests/m17_session.rs`'s shape.
//!
//! The reflector's plain relay (same-module broadcast to every OTHER linked
//! client — unaffected by parrot mode, which is purely additive) is what
//! gets a canned D-Star stream to the [`DstarSession`] under test: a second
//! raw UDP socket stands in for the "talker" — it links to the SAME module
//! and feeds the reflector a header + voice frames, which the reflector
//! relays verbatim to the [`DstarSession`] (also linked on that module).
//!
//! D-Star is hardware-only (iax-b3e7 M0): `--features dstar` pulls
//! `astar-codec/ambe-hw` (the only AMBE backend), so both tests in this file
//! open a real `ThumbDV` dongle via [`DstarSession::connect`]. They are
//! therefore HARDWARE tests and are gated exactly as spec §5 requires —
//! [`hardware_opted_in`] is the first statement in each test body, so a
//! machine with no dongle SKIPS them (green) rather than failing with a
//! "no `ThumbDV` detected" panic indistinguishable from a real regression.
//!
//! The hardware-free coverage of the same run-loop logic (priming, the
//! burst-absorbing frame queue, the end-of-stream drain, the talker-change
//! discard, the flush bound) lives in `tests/dstar_session_pipeline.rs`,
//! which injects a fake vocoder through `DstarSession::connect_with_stream`.
//! What THIS file adds on top is the real chip in the loop.
//!
//! Only ONE process/thread may hold the real `ThumbDV` at a time, but
//! `cargo test` runs a binary's tests on multiple threads by default: both
//! tests below call `DstarSession::connect`, so without [`hardware_lock`]
//! serializing them they race to open the same physical serial port (a
//! spurious "no `ThumbDV` detected" from the loser is a test-harness
//! artifact, not a product bug). The guard also settles after the session
//! drops: closing the port is asynchronous (the vocoder worker notices its
//! channel closed up to 20 ms later), so releasing the lock the instant a
//! test body ends would let the next opener race a port still held.
#![cfg(feature = "dstar")]

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

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

/// See the module doc.
fn hardware_lock() -> HardwareGuard {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    HardwareGuard(Some(
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    ))
}

use astar_audio::{
    AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, OutputSource,
    StreamConfig, StreamHandle,
};
use astar_console::{DstarConfig, DstarSession};
use astar_dstar::{DsvtPacket, LinkState, Reflector, RfHeader};

// ---- a pull-capable test backend (with an inert input device) -------------
//
// iax-2f6b: D-Star now opens a real mic lane on connect (TX), so every
// backend `DstarSession::connect` is handed needs a working `open_input`
// even for tests that only exercise RX — mirrors `m17_session.rs`'s
// `PushBackend`, minus the mic-sink stash (nothing here ever pushes TX
// audio, so the sink handed to `open_input` is simply dropped).

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

/// Stashes the router's real output bus (handed to `open_output` as a boxed
/// `OutputSource`) so the test can pull decoded RX PCM directly by calling
/// `.read()` on it, exactly as a real cpal output callback would.
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
        // No test in this file pushes TX audio: the sink is simply dropped.
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

/// Pulls up to `n` samples from the stashed `OutputTap` and returns their
/// peak amplitude (`0.0..=1.0`) — proof that decoded RX PCM actually reached
/// the audio backend, not just that a `talker`/link flag flipped. `0.0` if
/// the output device was never opened.
fn pull_output_peak(output_tap: &OutputTap, n: usize) -> f32 {
    let mut buf = vec![0.0f32; n];
    let mut guard = output_tap.lock().unwrap();
    let Some(source) = guard.as_mut() else {
        return 0.0;
    };
    source.read(&mut buf);
    astar_audio::peak(&buf)
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

// ---- raw-socket "talker" client + canned DSVT stream ----------------------

fn raw_client() -> (UdpSocket, SocketAddr) {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind raw client");
    sock.set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set read timeout");
    let addr = sock.local_addr().expect("local addr");
    (sock, addr)
}

fn callsign8(cs: &str) -> [u8; 8] {
    let mut buf = [b' '; 8];
    buf[..cs.len()].copy_from_slice(cs.as_bytes());
    buf
}

/// 11-byte connect request (per `astar_dstar::fsm`'s wire layout): the
/// SAME shape [`DextraFsm::connect`] sends — used here from a raw socket to
/// link the "talker" client onto the same module as the [`DstarSession`]
/// under test.
fn connect_bytes(callsign: &str, dest_module: u8) -> Vec<u8> {
    let mut buf = Vec::with_capacity(11);
    buf.extend_from_slice(&callsign8(callsign));
    buf.push(b' '); // own module: a plain (non-repeater) client
    buf.push(dest_module);
    buf.push(0x00); // revision
    buf
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

/// One canned 20 ms voice frame: arbitrary non-zero AMBE bytes (this test
/// doesn't need the decoded audio to be MEANINGFUL, just non-silent) plus
/// silent slow-data.
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

// ---- the test ---------------------------------------------------------------

#[test]
fn dstar_session_tracks_talker_decodes_rx_and_reports_tx_capable() {
    if !hardware_opted_in() {
        return;
    }
    let _hw = hardware_lock();
    let reflector =
        Reflector::bind_parrot("127.0.0.1:0".parse().unwrap()).expect("bind parrot reflector");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let output_tap: OutputTap = Arc::new(Mutex::new(None));
    let tap_for_backend = Arc::clone(&output_tap);
    let cfg = DstarConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        module: b'A',
        callsign: "N0CALL".into(),
        output: None,
        input: None,
        reflector_callsign: None,
    };
    let mut session = DstarSession::connect(cfg, &move || {
        Box::new(PullBackend {
            output_tap: Arc::clone(&tap_for_backend),
        }) as Box<dyn AudioBackend>
    })
    .expect("dstar session connect");

    assert!(
        wait_until(|| session.state().link == LinkState::Linked, 2_000),
        "session must link to the reflector, got {:?}",
        session.state().link
    );
    assert!(
        session.state().tx_capable,
        "iax-2f6b: D-Star is full-transceive now, tx_capable must be true"
    );
    assert!(!session.state().ptt, "must start unkeyed");
    assert_eq!(
        session.state().talker,
        None,
        "no talker has transmitted yet"
    );

    // A quick key/unkey cycle must be applied by the run-loop and never
    // disturb the link (a smoke test — the framing/safety details have
    // dedicated hardware-free coverage in dstar_session_pipeline.rs).
    session.set_ptt(true);
    assert!(
        wait_until(|| session.state().ptt, 1_000),
        "set_ptt(true) must be applied by the run-loop"
    );
    session.set_ptt(false);
    assert!(
        wait_until(|| !session.state().ptt, 1_000),
        "set_ptt(false) must be applied by the run-loop"
    );
    assert_eq!(
        session.state().link,
        LinkState::Linked,
        "a key/unkey cycle must not disturb the link"
    );

    // A second raw client links onto the SAME module and feeds the
    // reflector a canned stream; the reflector's plain relay (same-module
    // broadcast, unaffected by parrot mode) forwards it verbatim to our
    // DstarSession.
    let (talker_sock, _) = raw_client();
    talker_sock
        .send_to(&connect_bytes("AJ7HR", b'A'), addr)
        .unwrap();
    assert!(recv_packet(&talker_sock).ends_with(b"ACK\0"));

    let stream_id = 0x4242;
    let header = talker_header("AJ7HR");
    talker_sock
        .send_to(&DsvtPacket::Header { stream_id, header }.encode(), addr)
        .unwrap();
    for seq in 0..5u8 {
        let end = seq == 4;
        talker_sock
            .send_to(&voice_frame(stream_id, seq, end).encode(), addr)
            .unwrap();
    }

    assert!(
        wait_until(|| session.state().talker.as_deref() == Some("AJ7HR"), 2_000),
        "the header's MY callsign must surface as the current talker, got {:?}",
        session.state().talker
    );

    assert!(
        wait_until(|| pull_output_peak(&output_tap, 4_000) > 0.0, 2_000),
        "decoded PCM must reach the audio backend's output"
    );

    // tx_capable/link stay well-formed after RX traffic too.
    assert!(session.state().tx_capable);
    assert_eq!(session.state().link, LinkState::Linked);

    session.disconnect();
    handle.shutdown();
}

/// Regression coverage for the end-of-stream drain (iax-b3e7 M0 spec §2):
/// the AMBE pipeline primes 3 frames (`AMBE_STREAM_PRIME_FRAMES`, private to
/// `astar_console::dstar`) before its first poll, so a transmission
/// SHORTER than that priming window would never reach a single
/// `poll_decoded` call during normal per-frame handling — without an
/// explicit drain on `end`, its audio would simply never reach the speaker,
/// clipping the entire (short) transmission rather than just its tail. A
/// 2-frame stream exercises exactly that: fewer voice frames than the
/// pipeline's own priming cushion.
#[test]
fn dstar_session_drains_a_short_stream_shorter_than_the_priming_window() {
    if !hardware_opted_in() {
        return;
    }
    let _hw = hardware_lock();
    let reflector =
        Reflector::bind_parrot("127.0.0.1:0".parse().unwrap()).expect("bind parrot reflector");
    let addr = reflector.local_addr();
    let handle = reflector.run();

    let output_tap: OutputTap = Arc::new(Mutex::new(None));
    let tap_for_backend = Arc::clone(&output_tap);
    let cfg = DstarConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        module: b'A',
        callsign: "N0CALL".into(),
        output: None,
        input: None,
        reflector_callsign: None,
    };
    let session = DstarSession::connect(cfg, &move || {
        Box::new(PullBackend {
            output_tap: Arc::clone(&tap_for_backend),
        }) as Box<dyn AudioBackend>
    })
    .expect("dstar session connect");

    assert!(
        wait_until(|| session.state().link == LinkState::Linked, 2_000),
        "session must link to the reflector"
    );

    let (talker_sock, _) = raw_client();
    talker_sock
        .send_to(&connect_bytes("AJ7HR", b'A'), addr)
        .unwrap();
    assert!(recv_packet(&talker_sock).ends_with(b"ACK\0"));

    // Only 2 voice frames, well under the pipeline's own priming cushion —
    // never enough to trigger a normal in-band poll before `end`.
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

    session.disconnect();
    handle.shutdown();
}
