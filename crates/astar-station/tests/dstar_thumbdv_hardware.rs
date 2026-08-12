// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! End-to-end hardware validation for the real `ThumbDV` D-Star vocoder path
//! (iax-b3e7 M0 spec §5). Every test here is a no-op unless
//! `IAX_THUMBDV_TESTS=1` is set AND a real `ThumbDV` is attached — a machine
//! with no dongle, and a normal `cargo test --features dstar`, must stay
//! green (the env-var check is the first statement in the test body, no
//! `#[ignore]`, mirroring `astar-station/tests/mint_token.rs`'s
//! `IAX_PORTAL_LIVE` precedent and `astar-codec`'s own
//! `ambe::hw_hardware_tests`).
//!
//! This is deliberately separate from `tests/dstar_station.rs`, whose own
//! tests already require a real dongle unconditionally once built with
//! `--features dstar` (D-Star is hardware-only post-M0, no software
//! fallback) — that file predates this one and is left as-is. This file
//! adds the specific, spec-named coverage: `dstar-listen`'s functional path
//! (`Station::dstar_connect` -> real reflector -> real `ThumbDV` decode)
//! against a LOCAL loopback parrot-capable reflector (127.0.0.1 only,
//! nothing transmits) decoding a canned stream to non-silent PCM, with an
//! explicit assertion that the backend really is `Hardware` — otherwise a
//! machine with no dongle but a stray env var set could pass this
//! vacuously.
//!
//! iax-2f6b added [`hardware_dstar_tx_rx_round_trip_through_the_real_thumbdv`]:
//! the TX-side counterpart, proving the encode path end to end against real
//! hardware — inject known mic PCM, key PTT, transmit into the SAME LOCAL
//! loopback parrot, let it replay, and assert the replay decodes back to
//! recognizable audio. Still 127.0.0.1-only; still nothing ever goes on the
//! air (see this workspace's ON-AIR SAFETY discipline).
//!
//! Only ONE process may hold the real `ThumbDV` at a time; do not run this
//! file concurrently with anything else that opens the dongle (including
//! `tests/dstar_station.rs` in the same crate — `hardware_lock` below plus
//! normal test-suite discipline is what keeps that true within this file).
#![cfg(feature = "dstar")]

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use astar_audio::{
    AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, OutputSource,
    StreamConfig, StreamHandle,
};
use astar_codec::ambe::AmbeBackend;
use astar_dstar::{DsvtPacket, LinkState, Reflector, RfHeader};
use astar_station::{Station, StationConfig};

/// `true` (and, on stderr, why) when this test should actually touch
/// hardware. See the module doc: checked first, no `#[ignore]`.
fn hardware_opted_in() -> bool {
    if std::env::var("IAX_THUMBDV_TESTS").ok().as_deref() == Some("1") {
        return true;
    }
    eprintln!(
        "skipping hardware ThumbDV test (set IAX_THUMBDV_TESTS=1 with a real dongle attached)"
    );
    false
}

/// Serializes the (one) test in this file against any other test binary
/// that might touch the same physical port. Within this process it's a
/// single-test guard; the real exclusivity discipline (don't run this
/// alongside `dstar_station.rs` or the `astar-codec`/`astar-console`
/// hardware suites) is on the invoker, per the task's hardware rules.
fn hardware_lock() -> HardwareGuard {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    HardwareGuard(Some(
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    ))
}

/// How long to let the vocoder worker actually close the serial fd after a
/// session drops, before the next test may open it. Dropping the stream only
/// closes the worker's request channel; the worker notices on its next loop
/// pass (up to 20 ms later) and only then drops the transport, so releasing
/// the lock the instant a test body ends lets the next opener race a port
/// that is still held.
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

// ---- helpers (duplicated from tests/dstar_station.rs on purpose: each ----
// ---- integration-test binary is its own compilation unit, and a shared --
// ---- `tests/common` crate wasn't judged worth it for one file's fixtures) --

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

/// Bind + run a real `astar_dstar::Reflector` in parrot mode — a LOCAL
/// (127.0.0.1-only) loopback target, never a live network. The handle is
/// intentionally leaked (`std::mem::forget`), matching `dstar_station.rs`'s
/// `spawn_reflector`: these tests only need it alive for the test binary's
/// process lifetime.
fn spawn_parrot_reflector() -> SocketAddr {
    let reflector =
        Reflector::bind_parrot("127.0.0.1:0".parse().unwrap()).expect("bind dstar parrot");
    let addr = reflector.local_addr();
    std::mem::forget(reflector.run());
    addr
}

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

/// Output-only: `open_input` fails on purpose — see its comment below.
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
        // Deliberately FAILS (iax-2f6b review): the RX-only test below is
        // also this file's coverage of the no-usable-microphone machine —
        // mic permission denied, no input device, or one already held
        // exclusively. D-Star opens the capture device lazily, on the first
        // key-down, so a listen-only session must connect and decode through
        // the real dongle regardless of what `open_input` does.
        Err(AudioError::DeviceNotFound("no capture device".into()))
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

/// Pull `n` samples ONCE and report both their peak and how many of them
/// are non-silent.
///
/// One pull, two measures, deliberately: reading the tap CONSUMES the
/// mixer's residual, so two separate helpers would each throw away half the
/// audio the other needed. The non-silent COUNT is the point — `peak > 0.0`
/// alone is satisfied by a single decoded frame out of ten, which is exactly
/// how the frame-drop bug this milestone fixes stayed invisible.
fn pull_peak_and_nonsilent(output_tap: &OutputTap, n: usize) -> (f32, usize) {
    let mut buf = vec![0.0f32; n];
    let mut guard = output_tap.lock().unwrap();
    let Some(source) = guard.as_mut() else {
        return (0.0, 0);
    };
    source.read(&mut buf);
    (
        astar_audio::peak(&buf),
        buf.iter().filter(|s| **s != 0.0).count(),
    )
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

/// Fabricates distinct (non-zero) AMBE bytes per `seq` — real chip PCM
/// output for these bytes doesn't need to mean anything in particular, just
/// not be a run of frames the vocoder trivially decodes to literal digital
/// silence.
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

/// Number of voice frames in the canned stream.
const CANNED_FRAMES: u8 = 10;

/// Sends a header plus [`CANNED_FRAMES`] voice frames (the last with
/// `end: true`) from a fresh raw "talker" socket into `addr`'s module — the
/// canned stream the spec's end-to-end test decodes.
///
/// Paced at D-Star's real 20 ms cadence. An unpaced burst is a different
/// test (`dstar_session_pipeline.rs` covers burst absorption with a fake
/// vocoder); what this one is supposed to validate is that the session
/// SUSTAINS the 20 ms cadence end to end through the real chip, which an
/// unpaced feed cannot show.
fn feed_canned_stream(addr: SocketAddr, module: u8, stream_id: u16) {
    let (talker_sock, _) = raw_client();
    talker_sock
        .send_to(&connect_bytes("AJ7HR", module), addr)
        .unwrap();
    assert!(
        recv_packet(&talker_sock).ends_with(b"ACK\0"),
        "canned-stream talker must link before it can feed voice frames"
    );

    let header = talker_header("AJ7HR");
    talker_sock
        .send_to(&DsvtPacket::Header { stream_id, header }.encode(), addr)
        .unwrap();
    for seq in 0..CANNED_FRAMES {
        thread::sleep(Duration::from_millis(20));
        talker_sock
            .send_to(
                &voice_frame(stream_id, seq, seq == CANNED_FRAMES - 1).encode(),
                addr,
            )
            .unwrap();
    }
}

// ---- the test ---------------------------------------------------------

/// The spec's named end-to-end case: `dstar-listen`'s functional path
/// (`Station::dstar_connect` against a real, LOCAL loopback parrot
/// reflector) with the REAL `ThumbDV` dongle decodes a canned D-Star stream
/// to non-silent PCM. `dstar-listen` itself is a thin wrapper over exactly
/// this `Station` surface (see `astar-cli/src/dstar_listen.rs`'s own
/// module doc) — reaching past it to spawn the compiled binary would add
/// process-lifecycle complexity for no additional coverage.
#[test]
fn hardware_dstar_session_decodes_a_canned_stream_to_nonsilent_pcm() {
    if !hardware_opted_in() {
        return;
    }
    let _hw = hardware_lock();

    let addr = spawn_parrot_reflector();
    let (station, output_tap) = station_with_pull_backend();

    station
        .dstar_connect(&addr.ip().to_string(), addr.port(), 'A', "N0CALL")
        .expect("dstar connect against the local loopback parrot");
    assert!(
        wait_until(
            || station
                .dstar_state()
                .is_some_and(|s| s.link == LinkState::Linked),
            2_000
        ),
        "must reach Linked once the local reflector ACKs, got {:?}",
        station.dstar_state()
    );
    assert_eq!(
        station.dstar_state().and_then(|s| s.backend),
        Some(AmbeBackend::Hardware),
        "this hardware-gated test must actually be decoding through the real ThumbDV, \
         not a silently-substituted backend"
    );

    feed_canned_stream(addr, b'A', 0x4242);

    // Accumulate: every pull CONSUMES the mixer's residual, so counting has
    // to add up across pulls rather than sampling one of them.
    let mut nonsilent = 0usize;
    let mut peak = 0.0f32;
    let deadline = Instant::now() + Duration::from_secs(3);
    let want = usize::from(CANNED_FRAMES) * 160;
    while Instant::now() < deadline && nonsilent < want {
        let (p, n) = pull_peak_and_nonsilent(&output_tap, 4_000);
        peak = peak.max(p);
        nonsilent += n;
        thread::sleep(Duration::from_millis(10));
    }

    assert!(
        peak > 0.0,
        "PCM decoded through the real ThumbDV must not be silence"
    );
    // Within a frame or two of the whole canned stream: the session must
    // SUSTAIN the cadence, not decode a couple of frames and drop the rest.
    let floor = want - 2 * 160;
    assert!(
        nonsilent >= floor,
        "only {nonsilent} of {want} decoded samples reached the speaker (floor {floor}): the \
         session is losing frames somewhere between the socket and the vocoder"
    );

    station.dstar_disconnect();
    assert!(
        wait_until(|| station.dstar_state().is_none(), 1_000),
        "dstar_disconnect must clear the session, freeing the ThumbDV for the next test"
    );
}

// ---- TX round trip (iax-2f6b) ------------------------------------------

/// Test tone frequency: comfortably inside AMBE's voice passband, away from
/// DC and Nyquist/2 edge effects at 8kHz — matches
/// `astar-console/tests/m17_session.rs`'s own precedent.
const TONE_HZ: f32 = 400.0;
/// 500ms of tone = 25 complete 20ms frames: > `SYNC_INTERVAL` (21, so the
/// stream crosses one full superframe boundary) and < `MAX_PENDING_TX_FRAMES`
/// (32, `astar_console::dstar`, so the whole burst is guaranteed to reach
/// the encoder rather than being dropped for pipeline overflow).
const TONE_MS: u32 = 500;
const REAL_FRAMES: usize = (8_000 * TONE_MS as usize / 1_000) / 160;

/// Stashes the router's real `MicLane` input sink so a test can push PCM
/// straight into it, exactly as a cpal capture callback would — the TX-side
/// mirror of `OutputTap`. Mirrors `astar-console/tests/m17_session.rs`'s
/// `MicSink`/`PushBackend` (that file's own doc explains why `NullBackend`'s
/// push-capable twin isn't reachable from an external test crate).
type MicSink = Arc<Mutex<Option<Box<dyn InputSink>>>>;

struct PushPullBackend {
    mic_sink: MicSink,
    output_tap: OutputTap,
}

impl AudioBackend for PushPullBackend {
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
        sink: Box<dyn InputSink>,
        _overruns: Arc<AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        *self.mic_sink.lock().unwrap() = Some(sink);
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

fn station_with_push_pull_backend() -> (Station, MicSink, OutputTap) {
    let mic_sink: MicSink = Arc::new(Mutex::new(None));
    let output_tap: OutputTap = Arc::new(Mutex::new(None));
    let mic_for_backend = Arc::clone(&mic_sink);
    let tap_for_backend = Arc::clone(&output_tap);
    let station = Station::with_backend_factory(
        StationConfig::default(),
        Box::new(move || {
            Box::new(PushPullBackend {
                mic_sink: Arc::clone(&mic_for_backend),
                output_tap: Arc::clone(&tap_for_backend),
            }) as Box<dyn AudioBackend>
        }),
    );
    (station, mic_sink, output_tap)
}

/// Push `ms` milliseconds of an 8 kHz mono tone at `freq_hz` straight into the
/// stashed mic sink (the router's real `MicLane`), exactly as a single cpal
/// capture callback would — a direct port of
/// `astar-console/tests/m17_session.rs`'s `push_mic_tone`. No-op if the
/// router hasn't opened the mic yet (should not happen once `dstar_connect`
/// has succeeded — D-Star opens a mic lane unconditionally, iax-2f6b).
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

/// Links a second raw UDP client on `addr`'s module, standing in for a
/// listener on the wire: the reflector's plain relay forwards the station's
/// own outbound traffic to every OTHER same-module client immediately and
/// unconditionally (parrot mode doesn't change this — see
/// `astar_dstar::reflector`'s module/`handle_packet` docs), so this is
/// how a test observes exactly what the station transmitted, live, without
/// waiting on the parrot's own delayed echo (which only ever replays to the
/// ORIGINAL sender). Mirrors `tests/dstar_station.rs`'s identical use of
/// `raw_client` in `disconnect_while_keyed_flushes_and_terminates_the_stream_through_station`.
fn link_wire_listener(addr: SocketAddr, module: u8) -> UdpSocket {
    let (listener, _) = raw_client();
    listener
        .send_to(&connect_bytes("N7WIRE", module), addr)
        .expect("send connect");
    assert!(
        recv_packet(&listener).ends_with(b"ACK\0"),
        "wire listener must link before it can observe outbound traffic"
    );
    listener
}

/// Drains `sock` (a 500ms-read-timeout raw socket, see `raw_client`) for
/// DSVT `Voice` packets until `want` have been collected or `deadline`
/// passes. Non-DSVT-shaped datagrams (e.g. the reflector's own periodic
/// keepalive) are silently skipped rather than treated as a parse failure —
/// this socket is a live tap on everything addressed to its module, not a
/// protocol-filtered feed.
fn recv_voice_frames(sock: &UdpSocket, want: usize, deadline: Instant) -> Vec<DsvtPacket> {
    let mut out = Vec::new();
    while out.len() < want && Instant::now() < deadline {
        let mut buf = [0u8; 128];
        if let Ok((n, _)) = sock.recv_from(&mut buf)
            && let Ok(pkt @ DsvtPacket::Voice { .. }) = DsvtPacket::parse(&buf[..n])
        {
            out.push(pkt);
        }
    }
    out
}

/// Accumulates only the NON-SILENT samples pulled from `output_tap` (see
/// `pull_peak_and_nonsilent`'s doc for why `!= 0.0` is this codebase's
/// established "real decoded audio, not mixer padding" filter) until `want`
/// samples have been gathered or `deadline` passes.
///
/// Note the filter SPLICES: zero-crossing samples inside the tone are dropped
/// along with the mixer's padding, so the accumulated buffer is not
/// contiguous audio. Harmless for the Goertzel/RMS checks the caller makes
/// (at the measured ~51x tone-to-off-tone ratio, the discontinuities' broad-
/// band contribution is far below the margin), but it would NOT be safe for
/// any phase-sensitive analysis.
fn accumulate_nonsilent(output_tap: &OutputTap, want: usize, deadline: Instant) -> Vec<f32> {
    let mut acc = Vec::new();
    while acc.len() < want && Instant::now() < deadline {
        let mut buf = vec![0.0f32; 2_000];
        {
            let mut guard = output_tap.lock().unwrap();
            if let Some(source) = guard.as_mut() {
                source.read(&mut buf);
            }
        }
        acc.extend(buf.into_iter().filter(|s| *s != 0.0));
        thread::sleep(Duration::from_millis(10));
    }
    acc
}

/// Goertzel-algorithm magnitude of `samples` at `freq_hz` (`sample_rate` Hz) —
/// a single-bin DFT, exactly suited to "how much energy is at one known
/// frequency" without pulling in a full FFT crate. Unnormalized: only valid
/// for comparing two bins computed over the SAME `samples` slice, which is
/// all this test does (see the call site).
fn goertzel_magnitude(samples: &[f32], freq_hz: f32, sample_rate: f32) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = samples.len() as f32;
    let k = (0.5 + n * freq_hz / sample_rate).floor();
    let omega = 2.0 * std::f32::consts::PI * k / n;
    let coeff = 2.0 * omega.cos();
    let mut q1 = 0.0f32;
    let mut q2 = 0.0f32;
    for &x in samples {
        let q0 = coeff.mul_add(q1, x - q2);
        q2 = q1;
        q1 = q0;
    }
    (q1 * q1 + q2 * q2 - q1 * q2 * coeff).sqrt()
}

/// Root-mean-square amplitude of `samples` — a coarse, waveform-agnostic
/// energy floor check (see the call site for why bit/sample-exact comparison
/// is not appropriate for a lossy vocoder's round trip).
fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let mean_sq = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
    mean_sq.sqrt()
}

/// The TX round-trip proof: inject a known PCM tone as mic audio, key PTT,
/// transmit into the LOCAL loopback parrot, let it echo back, and assert
/// BOTH the protocol shape of what was actually put on the wire (header
/// first, the sync pattern at every superframe boundary, EOT on the final
/// frame only) AND that the parrot's replayed stream decodes — through the
/// SAME real `ThumbDV` chip, RX side this time — to audio whose dominant
/// frequency and energy correspond to the tone that was sent.
///
/// Frame budget: 500ms of tone = 25 complete 20ms frames, deliberately kept
/// under `MAX_PENDING_TX_FRAMES` (32, `astar_console::dstar`) so every
/// pushed frame is guaranteed to reach the encoder rather than being dropped
/// for pipeline overflow, and comfortably over `SYNC_INTERVAL` (21) so the
/// stream crosses one full superframe boundary — the exact case
/// `astar_dstar::tx`'s own unit tests already pin in isolation, now
/// proven against the real chip's actual encoded output.
///
/// Half-duplex handoff: this session encodes (TX) then, only once fully
/// unkeyed, decodes (RX) the parrot's delayed (150ms) reply — the run-loop's
/// own key-down handoff (`astar_console::dstar::apply_pending_ptt_edge`)
/// already flushes/discards any RX state before the first mic frame is ever
/// submitted to the encoder, and nothing in this test's own sequencing
/// (key -> push tone -> wait for it all to hit the wire -> unkey -> wait for
/// the delayed replay) ever asks the one physical `ThumbDV` link to encode
/// and decode at the same time.
#[test]
fn hardware_dstar_tx_rx_round_trip_through_the_real_thumbdv() {
    if !hardware_opted_in() {
        return;
    }
    let _hw = hardware_lock();

    let addr = spawn_parrot_reflector();
    let (station, mic_sink, output_tap) = station_with_push_pull_backend();

    station
        .dstar_connect(&addr.ip().to_string(), addr.port(), 'A', "AJ7HR")
        .expect("dstar connect against the local loopback parrot");
    assert!(
        wait_until(
            || station
                .dstar_state()
                .is_some_and(|s| s.link == LinkState::Linked),
            2_000
        ),
        "must reach Linked once the local reflector ACKs, got {:?}",
        station.dstar_state()
    );
    assert_eq!(
        station.dstar_state().and_then(|s| s.backend),
        Some(AmbeBackend::Hardware),
        "this hardware-gated test must actually be transmitting through the real ThumbDV, \
         not a silently-substituted backend"
    );

    // A second raw client, linked on the same module, observes exactly what
    // the station puts on the wire — independent of (and unaffected by) the
    // parrot's own delayed echo back to the station itself.
    let listener = link_wire_listener(addr, b'A');

    station.set_ptt(true).expect("D-Star PTT must be accepted");
    assert!(
        wait_until(|| station.dstar_state().is_some_and(|s| s.ptt), 1_000),
        "set_ptt(true) must be applied by the run-loop"
    );

    let header_bytes = recv_packet(&listener);
    let header_pkt = DsvtPacket::parse(&header_bytes).expect("valid header packet");
    let DsvtPacket::Header {
        stream_id: tx_stream_id,
        header: tx_header,
    } = header_pkt
    else {
        panic!("key-down must send the header first, got {header_pkt:?}");
    };
    // Station identification is the one TX header field that legally must be
    // right, and `general_call_header` takes three consecutive [u8; 8]
    // arguments — a transposition would transmit an unidentified stream with
    // every test in the workspace still green (iax-2f6b review).
    assert_eq!(
        &tx_header.my, b"AJ7HR   ",
        "the transmitted RF header must carry the connecting station's own callsign"
    );
    assert_eq!(
        &tx_header.ur, b"CQCQCQ  ",
        "a general call addresses CQCQCQ"
    );
    assert_ne!(
        tx_stream_id, 0,
        "stream id 0 is refused outright by real reflectors (xlxd's OpenStream)"
    );

    push_mic_tone(&mic_sink, TONE_HZ, TONE_MS);

    // Wait for every real frame to actually reach the wire (normal per-
    // iteration pump steps, NOT the 250ms unkey-flush deadline — that only
    // applies once we ask to unkey, which we deliberately haven't yet).
    let encode_deadline = Instant::now() + Duration::from_secs(10);
    let real_frames = recv_voice_frames(&listener, REAL_FRAMES, encode_deadline);
    assert_eq!(
        real_frames.len(),
        REAL_FRAMES,
        "all {REAL_FRAMES} pushed mic frames must reach the encoder and the wire before unkey \
         (got {}); MAX_PENDING_TX_FRAMES should make this lossless for a burst this size",
        real_frames.len()
    );

    // Now unkey. Since the encoder queue is already empty, the flush is a
    // near-instant no-op and the terminating frame is a bare zero-AMBE EOT
    // frame (see `apply_ptt_edge`'s doc) — still a real DsvtPacket::Voice
    // with `end = true` on the real wire, which is exactly what this test
    // asserts on below.
    station
        .set_ptt(false)
        .expect("D-Star unkey must be accepted");
    assert!(
        wait_until(|| station.dstar_state().is_some_and(|s| !s.ptt), 3_000),
        "set_ptt(false) must be applied by the run-loop (encoder flush)"
    );
    let eot_frames = recv_voice_frames(&listener, 1, Instant::now() + Duration::from_secs(2));
    assert_eq!(
        eot_frames.len(),
        1,
        "unkey must emit exactly one more (terminating) voice frame onto the wire"
    );

    // ---- protocol shape, asserted against what the real hardware path put
    // ---- on the wire (research §3/§6; astar_dstar::tx's own unit tests
    // ---- pin the same invariants in isolation — this proves them live).
    let mut voice = real_frames;
    voice.extend(eot_frames);
    assert_tx_protocol_shape(&voice, tx_stream_id);

    // ---- energy/frequency correspondence through the real chip's RX side.
    // ---- The parrot replays ~150ms after our own EOT; wait it out, then
    // ---- accumulate decoded PCM. NOT a bit-exact PCM comparison — AMBE at
    // ---- 2400bps is a lossy vocoder, so this asserts what a lossy round
    // ---- trip CAN promise: the dominant frequency and overall energy of a
    // ---- sustained tone survive.
    let want_samples = usize::from(astar_dstar::SYNC_INTERVAL) * 160; // > 1 superframe of real decoded audio
    let decode_deadline = Instant::now() + Duration::from_secs(5);
    let decoded = accumulate_nonsilent(&output_tap, want_samples, decode_deadline);
    assert_decoded_audio_matches_tone(&decoded, want_samples);

    station.dstar_disconnect();
    assert!(
        wait_until(|| station.dstar_state().is_none(), 1_000),
        "dstar_disconnect must clear the session, freeing the ThumbDV for the next test"
    );
}

/// Asserts the protocol shape of one transmission's voice frames (header
/// packet already checked by the caller): all share `stream_id`, the sync
/// pattern lands at every superframe boundary, and the EOT bit is set on
/// (and only on) the last frame. Mirrors the invariants
/// `astar_dstar::tx`'s own unit tests pin in isolation, now checked
/// against bytes that actually round-tripped through the real hardware path.
fn assert_tx_protocol_shape(voice: &[DsvtPacket], stream_id: u16) {
    assert!(
        voice
            .iter()
            .all(|p| matches!(p, DsvtPacket::Voice { stream_id: sid, .. } if *sid == stream_id)),
        "every voice frame must share the header's stream id"
    );
    let DsvtPacket::Voice {
        seq: seq0,
        slow_data: slow0,
        end: end0,
        ..
    } = voice[0]
    else {
        unreachable!("filtered to Voice above")
    };
    assert_eq!(seq0, 0, "the first voice frame must be seq 0");
    assert_eq!(
        slow0,
        astar_dstar::slowdata::SYNC_PATTERN,
        "the first frame of a superframe must carry the sync pattern"
    );
    assert!(!end0, "the first frame must not carry EOT");

    assert!(
        voice.len() > usize::from(astar_dstar::SYNC_INTERVAL),
        "this test's own frame budget must cross a superframe boundary (got {} frames)",
        voice.len()
    );
    let DsvtPacket::Voice {
        seq: seq21,
        slow_data: slow21,
        ..
    } = voice[usize::from(astar_dstar::SYNC_INTERVAL)]
    else {
        unreachable!("filtered to Voice above")
    };
    assert_eq!(
        seq21, 0,
        "seq must wrap back to 0 exactly at the second superframe boundary"
    );
    assert_eq!(
        slow21,
        astar_dstar::slowdata::SYNC_PATTERN,
        "the second superframe's first frame must ALSO carry the sync pattern"
    );

    // Walk the WHOLE run, not just frames 0 and SYNC_INTERVAL: a regression
    // in `pump_tx`/`flush_tx_pipeline` that duplicated a seq, skipped one or
    // restarted the counter mid-stream would lose a receiving radio's
    // slow-data sync while every reflector-side test stayed happy (the
    // loopback parrot echoes our bytes verbatim, so it can never tell correct
    // slow data from self-consistent wrong slow data — research §8's lesson).
    for (i, pkt) in voice.iter().enumerate() {
        let DsvtPacket::Voice {
            seq,
            end,
            slow_data,
            ..
        } = pkt
        else {
            unreachable!("filtered to Voice above")
        };
        let want_seq = u8::try_from(i % usize::from(astar_dstar::SYNC_INTERVAL))
            .expect("SYNC_INTERVAL fits in u8");
        assert_eq!(
            *seq,
            want_seq,
            "frame {i} carries seq {seq}, breaking the consecutive 0..{} run a receiver's \
             slow-data decoder tracks",
            astar_dstar::SYNC_INTERVAL
        );
        let want_slow = if want_seq == 0 {
            astar_dstar::slowdata::SYNC_PATTERN
        } else {
            astar_dstar::SCRAMBLE
        };
        assert_eq!(
            *slow_data, want_slow,
            "frame {i} (seq {seq}) carries the wrong slow-data slot: every superframe starts with \
             the sync pattern and every other frame carries the scrambled null filler"
        );
        if i == voice.len() - 1 {
            assert!(*end, "the LAST frame transmitted must carry the EOT bit");
        } else {
            assert!(!*end, "frame {i} of {} must NOT carry EOT", voice.len());
        }
    }
}

/// Asserts `decoded` (non-silent PCM pulled back from the real `ThumbDV`'s
/// decode of the parrot's replay) corresponds to the injected [`TONE_HZ`]
/// tone — energy/frequency correspondence, deliberately NOT bit-exact PCM
/// (see the call site's doc for why). Two checks: the dominant frequency
/// (via a Goertzel single-bin filter, comparing energy at the tone frequency
/// against an off-tone bin) and an overall RMS energy floor, catching a
/// "keyed but no useful audio ever left the mic" bug that a frequency check
/// alone could miss on near-silent input.
fn assert_decoded_audio_matches_tone(decoded: &[f32], want_samples: usize) {
    // A one-frame floor is not a floor (iax-2f6b review): if the replay burst
    // overran the RX pipeline and 24 of 25 frames were dropped between the
    // socket and the vocoder, a single surviving frame still clears both
    // checks below. The RX-only sibling in this file deliberately asserts
    // within a frame or two of the whole stream for exactly this reason —
    // `peak > 0.0` alone "is exactly how the frame-drop bug this milestone
    // fixes stayed invisible".
    let floor = want_samples.saturating_sub(2 * 160);
    assert!(
        decoded.len() >= floor,
        "only {} of {want_samples} decoded samples came back from the parrot's replay (floor \
         {floor}): the RX path is losing frames",
        decoded.len()
    );

    let mag_tone = goertzel_magnitude(decoded, TONE_HZ, 8_000.0);
    let mag_off = goertzel_magnitude(decoded, 1_000.0, 8_000.0);
    let rms_decoded = rms(decoded);
    eprintln!(
        "iax-2f6b TX round trip: {} decoded samples, Goertzel({TONE_HZ}Hz)={mag_tone:.1} \
         Goertzel(1000Hz off-tone)={mag_off:.1} rms={rms_decoded:.4}",
        decoded.len()
    );
    assert!(
        mag_tone > mag_off * 4.0,
        "decoded audio's dominant energy must be at {TONE_HZ}Hz (the injected tone), not \
         1000Hz off-tone: tone={mag_tone:.1} off={mag_off:.1} (want tone > 4x off, ~6dB)"
    );
    assert!(
        rms_decoded > 0.01,
        "decoded RMS energy is implausibly low for a keyed, tone-fed transmission: {rms_decoded:.4}"
    );
}
