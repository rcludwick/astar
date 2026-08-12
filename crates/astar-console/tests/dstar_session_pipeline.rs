// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Hardware-FREE coverage of `DstarSession`'s decode-path policy (iax-b3e7
//! M0 spec §2): the priming cushion, the raw-frame queue that absorbs
//! network bursts, the end-of-stream drain, the talker-change discard, the
//! abandoned-stream guard, and the bound on every one of those flushes.
//!
//! D-Star is hardware-only since M0, so `DstarSession::connect` opens a real
//! `ThumbDV`. That made every functional D-Star test in this repo require a
//! dongle — against spec §5's "normal CI and other machines must stay green
//! with no dongle attached", and it left the run loop's own logic (which has
//! nothing to do with the chip) untestable anywhere. `connect_with_stream`
//! is the seam that fixes it: the session takes an injected
//! [`AmbeStream`], so everything below runs against an in-process
//! [`FakeVocoder`] on any machine, deterministically, with no serial port
//! opened and nothing that could key a transmitter.
//!
//! Packets reach the session the same way the hardware tests deliver them —
//! through a REAL `astar_dstar::Reflector` on 127.0.0.1, with a second
//! raw UDP socket standing in for the talker.
#![cfg(feature = "dstar")]

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use astar_audio::{
    AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, OutputSource,
    StreamConfig, StreamHandle,
};
use astar_codec::ambe::{AMBE_STREAM_MAX_IN_FLIGHT, AmbeBackend, AmbeStream};
use astar_console::{DstarConfig, DstarSession};
use astar_dstar::{DsvtPacket, LinkState, NULL_AMBE, Reflector, RfHeader};

// ---- the injected vocoder ---------------------------------------------------

/// What a test wants to know about the vocoder afterwards. Shared with the
/// run-loop thread, which owns the [`FakeVocoder`] itself.
#[derive(Default)]
struct VocoderStats {
    /// Frames the session actually got into the pipeline.
    submitted: usize,
    /// Frames the session offered while the pipeline was already at
    /// [`AMBE_STREAM_MAX_IN_FLIGHT`] — i.e. 20 ms of audio thrown away.
    dropped: usize,
    /// Frames handed back out through `poll_decoded`.
    polled: usize,
    /// Mic frames the session got into the ENCODE pipeline (iax-2f6b).
    encode_submitted: usize,
    /// Mic frames offered to a full encode pipeline — 20 ms of the
    /// operator's own transmitted audio thrown away each.
    encode_dropped: usize,
}

/// An in-process stand-in for the pipelined `ThumbDV` worker: same bounded
/// in-flight contract, same "answers take a while" behaviour, no device.
///
/// `never_answers` models the failure this design has to survive — a worker
/// that is gone or wedged with frames outstanding, which is what turns an
/// unbounded drain loop into a hung run loop and a deadlocked `disconnect()`.
struct FakeVocoder {
    latency: Duration,
    never_answers: bool,
    /// Encode-direction-only wedge (iax-2f6b): `poll_encoded` never returns,
    /// modelling a `ThumbDV` worker that is gone/wedged mid-transmission —
    /// the TX-side twin of `never_answers`. Independent of `never_answers`
    /// so a test can wedge encode without also silencing decode.
    never_answers_encode: bool,
    /// Per-frame encode cost. Zero by default (most tests don't care), set
    /// to something like the real chip's measured ~15.9 ms by the sustained
    /// transmit test — an encoder that answers instantly hides every
    /// pipelining and cadence bug there is.
    encode_latency: Duration,
    /// Test-controlled encode STALL (iax-2f6b review): while set,
    /// `submit_encode` keeps accepting frames and `in_flight_encoded()` keeps
    /// growing — a chip that went quiet mid-transmission — but `poll_encoded`
    /// returns nothing, so an unkey hits `FLUSH_DEADLINE` with frames still
    /// owed. Clearing it releases them, which is how a test recreates "the
    /// previous over's encoded audio arrives after its flush gave up".
    stall_encode: Arc<AtomicBool>,
    queue: VecDeque<(Instant, [i16; 160])>,
    stats: Arc<Mutex<VocoderStats>>,
    /// Encode-side stand-in (iax-2f6b `AmbeStream::submit_encode`/
    /// `poll_encoded`): maps a PCM frame's first sample to a 2-byte "AMBE"
    /// payload, the exact inverse of `submit_decode`'s mapping — so a PCM
    /// level pushed in as TX audio round-trips back to the same level once
    /// decoded, letting TX tests reuse the same `contains_level`/`Recorder`
    /// helpers the RX tests already use.
    encode_queue: VecDeque<(Instant, [u8; 9])>,
}

impl FakeVocoder {
    fn new(latency: Duration, stats: &Arc<Mutex<VocoderStats>>) -> Self {
        FakeVocoder {
            latency,
            never_answers: false,
            never_answers_encode: false,
            encode_latency: Duration::ZERO,
            stall_encode: Arc::new(AtomicBool::new(false)),
            queue: VecDeque::new(),
            stats: Arc::clone(stats),
            encode_queue: VecDeque::new(),
        }
    }

    /// [`FakeVocoder::new`] with a realistic per-frame encode cost.
    fn with_encode_latency(
        latency: Duration,
        encode_latency: Duration,
        stats: &Arc<Mutex<VocoderStats>>,
    ) -> Self {
        FakeVocoder {
            encode_latency,
            ..FakeVocoder::new(latency, stats)
        }
    }

    /// [`FakeVocoder::new`] sharing a caller-held stall flag — see
    /// `stall_encode`.
    fn stallable(stall: &Arc<AtomicBool>, stats: &Arc<Mutex<VocoderStats>>) -> Self {
        FakeVocoder {
            stall_encode: Arc::clone(stall),
            ..FakeVocoder::new(Duration::from_millis(2), stats)
        }
    }

    fn wedged(stats: &Arc<Mutex<VocoderStats>>) -> Self {
        FakeVocoder {
            never_answers: true,
            ..FakeVocoder::new(Duration::ZERO, stats)
        }
    }

    /// Encode-side wedge (iax-2f6b): `submit_encode` still accepts frames
    /// (so `in_flight_encoded()` grows, exactly like a real worker that
    /// stopped answering mid-stream), but `poll_encoded` never returns
    /// anything — proving a TX unkey still terminates the DSVT stream (with
    /// a bare all-zero-AMBE EOT frame) rather than hanging.
    fn wedged_encode(stats: &Arc<Mutex<VocoderStats>>) -> Self {
        FakeVocoder {
            never_answers_encode: true,
            ..FakeVocoder::new(Duration::ZERO, stats)
        }
    }
}

impl AmbeStream for FakeVocoder {
    fn submit_decode(&mut self, frame: [u8; 9]) {
        let mut stats = self.stats.lock().unwrap();
        if self.queue.len() >= AMBE_STREAM_MAX_IN_FLIGHT {
            stats.dropped += 1;
            return;
        }
        stats.submitted += 1;
        // The frame's payload carries the PCM level the test wants back, so
        // decoded audio can be attributed to the exact frame that produced
        // it (see `voice_frame`).
        let value = i16::from_be_bytes([frame[0], frame[1]]);
        self.queue
            .push_back((Instant::now() + self.latency, [value; 160]));
    }

    fn poll_decoded(&mut self) -> Option<[i16; 160]> {
        if self.never_answers {
            return None;
        }
        if self
            .queue
            .front()
            .is_none_or(|(due, _)| Instant::now() < *due)
        {
            return None;
        }
        let (_, pcm) = self.queue.pop_front().expect("checked above");
        self.stats.lock().unwrap().polled += 1;
        Some(pcm)
    }

    fn in_flight(&self) -> usize {
        self.queue.len()
    }

    fn submit_encode(&mut self, pcm: [i16; 160]) {
        if self.encode_queue.len() >= AMBE_STREAM_MAX_IN_FLIGHT {
            self.stats.lock().unwrap().encode_dropped += 1;
            return;
        }
        self.stats.lock().unwrap().encode_submitted += 1;
        let b = pcm[0].to_be_bytes();
        self.encode_queue.push_back((
            Instant::now() + self.encode_latency,
            [b[0], b[1], 0, 0, 0, 0, 0, 0, 0],
        ));
    }

    fn poll_encoded(&mut self) -> Option<[u8; 9]> {
        if self.never_answers || self.never_answers_encode {
            return None;
        }
        if self.stall_encode.load(Ordering::Relaxed) {
            return None;
        }
        if self
            .encode_queue
            .front()
            .is_none_or(|(due, _)| Instant::now() < *due)
        {
            return None;
        }
        let (_, frame) = self.encode_queue.pop_front().expect("checked above");
        Some(frame)
    }

    fn in_flight_encoded(&self) -> usize {
        self.encode_queue.len()
    }
}

// ---- a push+pull test backend (mic sink in, output tap out) ---------------
//
// iax-2f6b: D-Star now opens a real mic lane on connect (TX), so every test
// backend needs a working `open_input` — mirrors `m17_session.rs`'s
// `PushBackend`. Renamed from `PullBackend`: this file's TX tests need to
// push mic PCM in, not just pull decoded PCM out.

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
/// Stashes the router's real `MicLane` (handed to `open_input` as a boxed
/// `InputSink`) so a test can push mic PCM directly by calling
/// `sink.write(...)`, exactly as a real cpal capture callback would.
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

/// Push a constant-level PCM "tone" straight into the stashed mic sink (the
/// router's real `MicLane`), exactly as a single cpal capture callback
/// would. A CONSTANT level (not a sine tone) is deliberate: `FakeVocoder`'s
/// encode mapping keys off a frame's first sample, so a constant level
/// round-trips through encode-then-decode to the exact same
/// `contains_level`/`expected_sample` value the RX tests already assert
/// against — no separate "tone survived" analysis needed for a fake codec
/// whose mapping is exact by construction (see the module doc's roundtrip
/// note on `FakeVocoder::encode_queue`).
fn push_mic_level(mic_sink: &MicSink, level: i16, ms: u32) {
    let n = (8_000 * ms / 1_000) as usize;
    let sample = f32::from(level) / 32_768.0;
    let buf = vec![sample; n];
    if let Some(sink) = mic_sink.lock().unwrap().as_mut() {
        sink.write(&buf, 0.0);
    }
}

/// Pulls `n` samples from the stashed tap. The mixer converts each `i16`
/// sample to `f32` as `s / 32768.0` (see `astar_audio::mixer::read`), so
/// a frame submitted with level `V` comes back as a run of `V / 32768.0`.
fn pull_samples(output_tap: &OutputTap, n: usize) -> Vec<f32> {
    let mut buf = vec![0.0f32; n];
    let mut guard = output_tap.lock().unwrap();
    let Some(source) = guard.as_mut() else {
        return Vec::new();
    };
    source.read(&mut buf);
    buf
}

/// The PCM level a `voice_frame(.., level, ..)` frame decodes to, as it
/// appears on the output bus.
fn expected_sample(level: i16) -> f32 {
    f32::from(level) / 32768.0
}

/// `true` when `samples` contains anything at `level`'s amplitude.
fn contains_level(samples: &[f32], level: i16) -> bool {
    let want = expected_sample(level);
    samples.iter().any(|s| (s - want).abs() < 1e-4)
}

/// Accumulates everything the output bus ever produced.
///
/// Reading the tap CONSUMES the mixer's residual, so a bare
/// "poll until the audio shows up" helper destroys the very samples a later
/// assertion needs to inspect. Every pull goes through here instead, and
/// assertions run against the accumulated history.
struct Recorder {
    tap: OutputTap,
    samples: Vec<f32>,
}

impl Recorder {
    fn new(tap: &OutputTap) -> Recorder {
        Recorder {
            tap: Arc::clone(tap),
            samples: Vec::new(),
        }
    }

    /// Pull whatever is available and keep the non-silent part of it.
    fn drain(&mut self) {
        let pulled = pull_samples(&self.tap, 8_000);
        self.samples.extend(pulled.iter().filter(|s| **s != 0.0));
    }

    /// Drain repeatedly until `level` shows up or `timeout_ms` elapses.
    fn wait_for_level(&mut self, level: i16, timeout_ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            self.drain();
            if contains_level(&self.samples, level) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn has(&self, level: i16) -> bool {
        contains_level(&self.samples, level)
    }

    fn forget(&mut self) {
        self.samples.clear();
    }
}

// ---- reflector plumbing -----------------------------------------------------

/// The read timeout every [`Talker`] socket carries by default.
const RECV_TIMEOUT: Duration = Duration::from_millis(500);

/// The mic PCM level [`FakeVocoder`] encoded into a transmitted frame's AMBE
/// payload — how a TX test recovers the injected audio level from the bytes
/// that actually reached the wire. Compared with a small tolerance by the
/// callers: the router's `f32` mic path round-trips `9_000` back as `8_999`.
fn decoded_tx_level(ambe: &[u8; 9]) -> i16 {
    i16::from_be_bytes([ambe[0], ambe[1]])
}

/// Parses `bytes` as a DSVT voice frame, panicking with context otherwise.
fn as_voice(bytes: &[u8]) -> (u16, u8, bool, [u8; 9]) {
    let pkt = DsvtPacket::parse(bytes).expect("every TX packet must be a valid DsvtPacket");
    match pkt {
        DsvtPacket::Voice {
            stream_id,
            seq,
            end,
            ambe,
            ..
        } => (stream_id, seq, end, ambe),
        other @ DsvtPacket::Header { .. } => panic!("expected a voice frame, got {other:?}"),
    }
}

fn wait_until(mut pred: impl FnMut() -> bool, timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    pred()
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

/// A voice frame whose AMBE payload encodes `level` — the PCM amplitude
/// [`FakeVocoder`] will decode it to, so decoded audio is attributable to
/// the frame (and therefore to the talker) that produced it.
fn voice_frame(stream_id: u16, seq: u8, level: i16, end: bool) -> DsvtPacket {
    let mut ambe = [0u8; 9];
    ambe[..2].copy_from_slice(&level.to_be_bytes());
    DsvtPacket::Voice {
        stream_id,
        seq,
        end,
        ambe,
        slow_data: [0x00, 0x00, 0x00],
    }
}

/// A linked raw-socket "talker": the reflector relays whatever it sends to
/// every OTHER client on the module, which is how the session under test
/// receives a stream.
struct Talker {
    sock: UdpSocket,
    addr: SocketAddr,
}

impl Talker {
    fn link(reflector: SocketAddr, callsign: &str, module: u8) -> Talker {
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind raw talker");
        sock.set_read_timeout(Some(RECV_TIMEOUT))
            .expect("set read timeout");
        sock.send_to(&connect_bytes(callsign, module), reflector)
            .expect("send link request");
        let mut buf = [0u8; 128];
        let (n, _) = sock.recv_from(&mut buf).expect("link ACK");
        assert!(buf[..n].ends_with(b"ACK\0"), "talker must link");
        Talker {
            sock,
            addr: reflector,
        }
    }

    fn send(&self, pkt: &DsvtPacket) {
        self.sock.send_to(&pkt.encode(), self.addr).expect("send");
    }

    fn header(&self, stream_id: u16, callsign: &str) {
        self.send(&DsvtPacket::Header {
            stream_id,
            header: talker_header(callsign),
        });
    }

    /// Blocks (bounded by the 500 ms read timeout set at `link()`) for the
    /// next packet the reflector relays to this socket — used as a "wire
    /// listener" in the TX tests: a `Talker` linked on the same module
    /// receives real-time relayed copies of whatever the [`DstarSession`]
    /// under test itself sends, which is the most direct way to assert on
    /// exactly what a TX-capable session put on the wire.
    fn recv_packet(&self) -> Vec<u8> {
        let mut buf = [0u8; 128];
        let (n, _) = self.sock.recv_from(&mut buf).expect("expected a packet");
        buf[..n].to_vec()
    }

    /// Collects every packet that arrives within `quiet_ms` of the previous
    /// one (or of the call, for the first), i.e. drains until the wire has
    /// been silent for `quiet_ms`. Used once a TX transmission is known to
    /// have finished, to gather the whole sequence of frames it emitted for
    /// bulk assertions (ordering, EOT placement) rather than counting exact
    /// arrivals one at a time.
    fn drain_quiet(&self, quiet_ms: u64) -> Vec<Vec<u8>> {
        self.sock
            .set_read_timeout(Some(Duration::from_millis(quiet_ms)))
            .expect("set read timeout");
        let mut out = Vec::new();
        let mut buf = [0u8; 128];
        while let Ok((n, _)) = self.sock.recv_from(&mut buf) {
            out.push(buf[..n].to_vec());
        }
        // Restore what `link()` established: leaving `quiet_ms` in place
        // would silently shorten a later `recv_packet()` on this socket into
        // a confusing "expected a packet" panic.
        self.sock
            .set_read_timeout(Some(RECV_TIMEOUT))
            .expect("restore read timeout");
        out
    }

    /// Like [`Talker::drain_quiet`], but timestamps each arrival — the TX
    /// cadence tests assert on inter-packet gaps, not just contents.
    fn drain_quiet_timed(&self, quiet_ms: u64) -> Vec<(Instant, Vec<u8>)> {
        self.sock
            .set_read_timeout(Some(Duration::from_millis(quiet_ms)))
            .expect("set read timeout");
        let mut out = Vec::new();
        let mut buf = [0u8; 128];
        while let Ok((n, _)) = self.sock.recv_from(&mut buf) {
            out.push((Instant::now(), buf[..n].to_vec()));
        }
        self.sock
            .set_read_timeout(Some(RECV_TIMEOUT))
            .expect("restore read timeout");
        out
    }
}

struct Fixture {
    session: Option<DstarSession>,
    reflector_addr: SocketAddr,
    mic_sink: MicSink,
    output_tap: OutputTap,
    stats: Arc<Mutex<VocoderStats>>,
    /// `None` when the session is pointed at a hand-rolled stand-in
    /// reflector (see `FakeReflector`) rather than the real loopback one.
    _reflector: Option<astar_dstar::ReflectorHandle>,
}

impl Fixture {
    fn start(vocoder: impl FnOnce(&Arc<Mutex<VocoderStats>>) -> FakeVocoder) -> Fixture {
        let reflector =
            Reflector::bind_parrot("127.0.0.1:0".parse().unwrap()).expect("bind reflector");
        let reflector_addr = reflector.local_addr();
        let handle = reflector.run();
        Fixture::connect(reflector_addr, Some(handle), vocoder)
    }

    /// [`Fixture::start`] against an already-running reflector stand-in — for
    /// the link-failure tests, which need to control when the link drops
    /// (the real loopback reflector never drops anyone).
    fn start_against(
        addr: SocketAddr,
        vocoder: impl FnOnce(&Arc<Mutex<VocoderStats>>) -> FakeVocoder,
    ) -> Fixture {
        Fixture::connect(addr, None, vocoder)
    }

    fn connect(
        reflector_addr: SocketAddr,
        handle: Option<astar_dstar::ReflectorHandle>,
        vocoder: impl FnOnce(&Arc<Mutex<VocoderStats>>) -> FakeVocoder,
    ) -> Fixture {
        let stats = Arc::new(Mutex::new(VocoderStats::default()));
        let mic_sink: MicSink = Arc::new(Mutex::new(None));
        let output_tap: OutputTap = Arc::new(Mutex::new(None));
        let sink_for_backend = Arc::clone(&mic_sink);
        let tap_for_backend = Arc::clone(&output_tap);
        let cfg = DstarConfig {
            host: reflector_addr.ip().to_string(),
            port: reflector_addr.port(),
            module: b'A',
            callsign: "N0CALL".into(),
            output: None,
            input: None,
            reflector_callsign: None,
        };
        let session = DstarSession::connect_with_stream(
            cfg,
            &move || {
                Box::new(PushPullBackend {
                    mic_sink: Arc::clone(&sink_for_backend),
                    output_tap: Arc::clone(&tap_for_backend),
                }) as Box<dyn AudioBackend>
            },
            Box::new(vocoder(&stats)),
            AmbeBackend::Hardware,
        )
        .expect("connect with an injected vocoder must not need hardware");

        let f = Fixture {
            session: Some(session),
            reflector_addr,
            mic_sink,
            output_tap,
            stats,
            _reflector: handle,
        };
        assert!(
            wait_until(|| f.session().state().link == LinkState::Linked, 2_000),
            "session must link to the local reflector"
        );
        f
    }

    fn session(&self) -> &DstarSession {
        self.session.as_ref().expect("session is live")
    }

    /// Mutable access for [`DstarSession::set_ptt`] (iax-2f6b).
    fn session_mut(&mut self) -> &mut DstarSession {
        self.session.as_mut().expect("session is live")
    }

    /// Takes the session out and disconnects it directly, bypassing
    /// `Fixture`'s own `Drop` — for tests that need to prove something about
    /// `disconnect()` itself (e.g. that it never hangs, or that it flushes a
    /// keyed transmission) rather than relying on teardown-at-end-of-test.
    fn take_session(&mut self) -> DstarSession {
        self.session.take().expect("session is live")
    }

    fn talker(&self, callsign: &str) -> Talker {
        Talker::link(self.reflector_addr, callsign, b'A')
    }

    fn stats(&self) -> (usize, usize, usize) {
        let s = self.stats.lock().unwrap();
        (s.submitted, s.dropped, s.polled)
    }

    /// `(encode_submitted, encode_dropped)` — the TX-side twin of
    /// [`Fixture::stats`].
    fn encode_stats(&self) -> (usize, usize) {
        let s = self.stats.lock().unwrap();
        (s.encode_submitted, s.encode_dropped)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(s) = self.session.take() {
            s.disconnect();
        }
    }
}

// ---- the tests --------------------------------------------------------------

/// Spec §2, and the reason the raw-frame queue exists: `handle_dsvt` used to
/// do exactly one `submit_decode` and one `poll_decoded` per arriving packet,
/// so a burst that outran the vocoder's [`AMBE_STREAM_MAX_IN_FLIGHT`] bound
/// silently discarded every frame past it. UDP delivers 2-3 frames back to
/// back routinely — that is the ordinary case jitter buffers exist for, and
/// the mixer's jitter buffer sits downstream of the vocoder where it cannot
/// help. Ten frames arriving at once must decode to ten frames of audio.
#[test]
fn a_burst_of_arrivals_loses_no_frames() {
    const FRAMES: u8 = 10;
    const LEVEL: i16 = 12_000;

    // 15 ms per frame: slower than the burst arrives, which is the whole
    // point (the real chip needs ~7.45 ms and the burst lands in ~1 ms).
    let f = Fixture::start(|s| FakeVocoder::new(Duration::from_millis(15), s));
    let talker = f.talker("AJ7HR");

    talker.header(0x4242, "AJ7HR");
    for seq in 0..FRAMES {
        talker.send(&voice_frame(0x4242, seq, LEVEL, seq == FRAMES - 1));
    }

    assert!(
        wait_until(|| f.stats().2 >= usize::from(FRAMES), 4_000),
        "only {:?} of {FRAMES} frames made it through the vocoder",
        f.stats()
    );
    let (submitted, dropped, _) = f.stats();
    assert_eq!(
        dropped, 0,
        "a back-to-back burst must be absorbed by the session's frame queue, not thrown at the \
         vocoder's in-flight bound and dropped"
    );
    assert_eq!(
        submitted,
        usize::from(FRAMES),
        "every frame must be decoded"
    );
}

/// Spec §2's end-of-stream drain, hardware-free: a transmission SHORTER than
/// the 3-frame priming cushion never reaches a normal in-band poll, so
/// without the drain on `end` its audio never reaches the speaker at all.
#[test]
fn a_stream_shorter_than_the_priming_window_is_still_drained() {
    const LEVEL: i16 = 9_000;

    let f = Fixture::start(|s| FakeVocoder::new(Duration::from_millis(10), s));
    let mut rec = Recorder::new(&f.output_tap);
    let talker = f.talker("AJ7HR");

    talker.header(0x1313, "AJ7HR");
    talker.send(&voice_frame(0x1313, 0, LEVEL, false));
    talker.send(&voice_frame(0x1313, 1, LEVEL, true));

    assert!(
        rec.wait_for_level(LEVEL, 2_000),
        "a 2-frame stream must still be drained at `end`, not clipped entirely"
    );
}

/// Findings 3/6/11: a new `Header` with no preceding end-of-stream.
///
/// D-Star streams routinely end without their `end`-flagged frame (last
/// frame lost, talker's link drops, reflector switches talkers). Talker A's
/// residue was left sitting in the vocoder, and because `poll_decoded` is
/// FIFO the next talker's first polls returned **A's audio** — played out
/// while the snapshot reported talker B.
///
/// A deliberately transmits FEWER frames than the priming cushion, so none
/// of A's audio can legitimately have been emitted before B keys up: any
/// sample at A's level in the output after B's header is misattributed
/// cross-talker audio.
#[test]
fn a_new_header_never_replays_the_previous_talkers_audio() {
    const A_LEVEL: i16 = 15_000;
    const B_LEVEL: i16 = 4_000;

    let f = Fixture::start(|s| FakeVocoder::new(Duration::from_millis(10), s));
    let mut rec = Recorder::new(&f.output_tap);
    let talker = f.talker("AJ7HR");

    // Talker A: header + 2 frames, then the stream is simply abandoned (its
    // `end` frame was lost). 2 < AMBE_STREAM_PRIME_FRAMES, so nothing of A's
    // has been polled out yet.
    talker.header(0x0A0A, "AJ7HR");
    talker.send(&voice_frame(0x0A0A, 0, A_LEVEL, false));
    talker.send(&voice_frame(0x0A0A, 1, A_LEVEL, false));
    assert!(
        wait_until(|| f.stats().0 >= 2, 2_000),
        "A's frames must reach the vocoder before B keys up, or this proves nothing"
    );
    rec.drain();
    assert!(
        !rec.has(A_LEVEL),
        "A transmitted fewer frames than the priming cushion: none of its audio should have \
         been emitted yet"
    );
    // Everything from here on is post-header output.
    rec.forget();

    // Talker B keys up. No `end` was ever seen for A.
    talker.header(0x0B0B, "W1AW");
    for seq in 0..5u8 {
        talker.send(&voice_frame(0x0B0B, seq, B_LEVEL, seq == 4));
    }

    assert!(
        rec.wait_for_level(B_LEVEL, 3_000),
        "B's own audio must be decoded and played"
    );
    // Give the tail of B's transmission time to land too, so the check below
    // sees the whole post-header output, not just up to B's first frame.
    let settle = Instant::now() + Duration::from_millis(300);
    while Instant::now() < settle {
        rec.drain();
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !rec.has(A_LEVEL),
        "the previous talker's residue was played out under the new talker's callsign"
    );
    assert_eq!(
        f.session().state().talker.as_deref(),
        Some("W1AW"),
        "the snapshot must report the new talker"
    );
}

/// The abandoned-stream guard: a tracked stream that simply goes quiet (no
/// `end`, no successor `Header`) must not hold frames in the vocoder
/// indefinitely. Without the idle timeout, `current_stream` stays `Some` and
/// those frames sit there until the next header — possibly forever.
#[test]
fn an_abandoned_stream_is_flushed_by_the_idle_timeout() {
    const LEVEL: i16 = 11_000;

    let f = Fixture::start(|s| FakeVocoder::new(Duration::from_millis(10), s));
    let mut rec = Recorder::new(&f.output_tap);
    let talker = f.talker("AJ7HR");

    talker.header(0x2222, "AJ7HR");
    talker.send(&voice_frame(0x2222, 0, LEVEL, false));
    talker.send(&voice_frame(0x2222, 1, LEVEL, false));

    // STREAM_IDLE_TIMEOUT is 400 ms; give the run loop room to notice.
    assert!(
        rec.wait_for_level(LEVEL, 3_000),
        "an abandoned stream's frames must be flushed out rather than stranded in the vocoder"
    );
    let (submitted, _, polled) = f.stats();
    assert_eq!(
        polled, submitted,
        "nothing may be left in flight once the stream has been declared abandoned"
    );
}

/// Finding 7: the flush must be bounded.
///
/// `flush_pipeline` runs on the run-loop thread. A vocoder whose worker died
/// with frames outstanding never decrements `in_flight` (`poll_decoded`
/// returns `None` forever), so an unbounded `while in_flight() > 0` loop
/// spins there for ever — and `disconnect()`/`Drop` `join()` that very
/// thread, deadlocking teardown from the caller's (UI) thread. iax-239a:
/// a wedged device surfaces as an error, never a hang.
#[test]
fn a_wedged_vocoder_cannot_hang_the_run_loop_or_disconnect() {
    let mut f = Fixture::start(FakeVocoder::wedged);
    let talker = f.talker("AJ7HR");

    talker.header(0x3333, "AJ7HR");
    for seq in 0..6u8 {
        talker.send(&voice_frame(0x3333, seq, 8_000, seq == 5));
    }
    assert!(
        wait_until(|| f.stats().0 > 0, 2_000),
        "frames must reach the wedged vocoder for this to prove anything"
    );

    // `disconnect()` joins the run-loop thread. Run it off-thread so a hang
    // is a test FAILURE rather than a suite that never finishes.
    let session = f.session.take().expect("session is live");
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        session.disconnect();
        let _ = tx.send(());
    });
    assert!(
        rx.recv_timeout(Duration::from_secs(5)).is_ok(),
        "disconnect() hung: the run loop is stuck draining a vocoder that will never answer"
    );
}

// ---- TX coverage (iax-2f6b) -------------------------------------------------
//
// A `Talker` linked on the same module doubles as a wire listener here: the
// reflector's plain relay forwards whatever the `DstarSession` under test
// itself sends to every OTHER linked client in real time (independent of,
// and much sooner than, the parrot's own ~150 ms delayed echo back to the
// sender), which is the most direct way to assert on exactly what a keyed
// session put on the wire.

/// Happy path: keying PTT emits exactly one header packet before any voice
/// frame, the operator's OWN callsign is in that header, mic audio flows out
/// as a run of ordinary (`end = false`) voice frames sharing the header's
/// `stream_id`, and unkey emits a terminating (`end = true`) frame — and only
/// that one frame carries the EOT bit.
///
/// The frame COUNT and the AMBE PAYLOADS are asserted, not just "something
/// was sent" (iax-2f6b review): with only a non-empty check, deleting
/// `set_gate(mic, true)` from the key-down path — or breaking
/// `frame_to_array`/`drain_tx_mic_frames` — collapses the transmission to a
/// header plus one bare EOT frame and every hardware-free test still passes.
/// A station that transmits nothing but silence must not be green.
#[test]
fn keying_ptt_emits_header_then_voice_and_unkey_emits_one_terminating_frame() {
    const LEVEL: i16 = 9_000;
    const FRAMES: usize = 10;

    let mut f = Fixture::start(|s| FakeVocoder::new(Duration::from_millis(2), s));
    let listener = f.talker("N7WIRE");

    assert!(!f.session().state().ptt, "must start unkeyed");
    f.session_mut().set_ptt(true);
    assert!(
        wait_until(|| f.session().state().ptt, 1_000),
        "set_ptt(true) must be applied by the run-loop"
    );

    let header_bytes = listener.recv_packet();
    let header_pkt = DsvtPacket::parse(&header_bytes).expect("valid header packet");
    let DsvtPacket::Header { stream_id, header } = header_pkt else {
        panic!(
            "the FIRST packet a freshly keyed session sends must be its header, got {header_pkt:?}"
        );
    };
    assert_ne!(
        stream_id, 0,
        "a zero stream id is refused outright by real reflectors"
    );
    // Station identification: the one header field that legally must be
    // right. `general_call_header` takes three consecutive [u8; 8] params —
    // trivially transposable — and nothing else in the tree pins this.
    assert_eq!(
        &header.my, b"N0CALL  ",
        "the RF header must carry the operator's own callsign in MY"
    );
    assert_eq!(&header.ur, b"CQCQCQ  ", "a general call addresses CQCQCQ");

    // 200 ms = 10 complete 20 ms frames.
    push_mic_level(&f.mic_sink, LEVEL, 200);

    f.session_mut().set_ptt(false);
    assert!(
        wait_until(|| !f.session().state().ptt, 1_000),
        "set_ptt(false) must be applied by the run-loop"
    );

    let tail = listener.drain_quiet(300);
    assert_eq!(
        tail.len(),
        FRAMES,
        "every one of the {FRAMES} captured mic frames must reach the wire (got {})",
        tail.len()
    );
    let mut end_count = 0;
    for (i, bytes) in tail.iter().enumerate() {
        let (sid, _seq, end, ambe) = as_voice(bytes);
        assert_eq!(
            sid, stream_id,
            "every voice frame must share the header's stream_id"
        );
        let level = decoded_tx_level(&ambe);
        assert!(
            (i32::from(level) - i32::from(LEVEL)).abs() <= 2,
            "frame {i} decoded to level {level}, not the injected {LEVEL} — the mic gate or the \
             mic drain is broken and this station is transmitting silence"
        );
        if end {
            end_count += 1;
            assert_eq!(
                i,
                tail.len() - 1,
                "the end=true frame must be the LAST frame sent, not an earlier one"
            );
        }
    }
    assert_eq!(
        end_count, 1,
        "exactly one frame may carry the EOT bit — got {end_count} in {tail:?}"
    );
}

/// Cadence (iax-2f6b review findings 8/12): DSVT voice frames must egress on
/// the 20 ms frame clock `astar_dstar::tx` documents, NOT in bursts at
/// whatever rate the run loop happens to wake up.
///
/// Before the fix the socket read timeout stayed at `SOCKET_POLL_TIMEOUT`
/// (50 ms) for the whole transmission — `want_fast` consulted only the RX
/// backlog, which is empty by construction while keyed — so each pass emitted
/// every frame the encoder had finished, back to back, and then went silent
/// for ~50 ms. Average rate correct, instantaneous cadence wrong, which is
/// what a receiver's small isochronous ring actually hears.
#[test]
fn transmitted_voice_frames_are_paced_on_the_20ms_frame_clock() {
    const FRAMES: usize = 15;

    let mut f = Fixture::start(|s| FakeVocoder::new(Duration::from_millis(2), s));
    let listener = f.talker("N7WIRE");

    f.session_mut().set_ptt(true);
    assert!(wait_until(|| f.session().state().ptt, 1_000), "must key");
    let _header = listener.recv_packet();

    // One burst of 300 ms (15 frames) of capture, exactly like a cpal
    // callback delivering a block: the session — not the capture cadence — is
    // what has to pace the wire.
    push_mic_level(&f.mic_sink, 9_000, 300);

    let timed = listener.drain_quiet_timed(400);
    let arrivals: Vec<Instant> = timed.iter().map(|(t, _)| *t).collect();
    assert!(
        arrivals.len() >= FRAMES,
        "expected at least {FRAMES} paced voice frames, got {}",
        arrivals.len()
    );

    let span = arrivals[FRAMES - 1].duration_since(arrivals[0]);
    // 15 frames paced at 20 ms span ~280 ms. A burst-per-poll implementation
    // delivers them in a handful of ~50 ms clumps — under 150 ms total.
    assert!(
        span >= Duration::from_millis(200),
        "{FRAMES} voice frames spanned only {span:?}: they were bursted, not paced on the 20 ms \
         frame clock"
    );
    let worst = arrivals
        .windows(2)
        .map(|w| w[1].duration_since(w[0]))
        .max()
        .expect("at least two frames");
    assert!(
        worst < Duration::from_millis(45),
        "worst inter-frame gap was {worst:?}: the wire went quiet for more than two frame times, \
         which is the burst-then-silence pattern receivers stutter on"
    );
}

/// Regression, iax-2f6b review's Critical finding: encoded frames left in
/// flight by a deadline-truncated unkey must NEVER be transmitted as the next
/// transmission's first voice frames.
///
/// The interleaving: the vocoder stalls mid-over, so the unkey flush hits
/// `FLUSH_DEADLINE` with frames still owed and gives up on them; the chip
/// then answers (or times them out into substitutes) with nobody polling; the
/// operator keys again, and — without the key-down drain — the first frames
/// of the NEW stream id carry the PREVIOUS over's audio, with the whole
/// transmission running a permanent lag behind the mic. RX has always been
/// guarded against exactly this on a fresh header; this is the TX twin.
#[test]
fn a_second_keying_never_transmits_the_previous_overs_leftover_audio() {
    const FIRST: i16 = 15_000;
    const SECOND: i16 = 4_000;

    let stall = Arc::new(AtomicBool::new(false));
    let stall_for_vocoder = Arc::clone(&stall);
    let mut f = Fixture::start(move |s| FakeVocoder::stallable(&stall_for_vocoder, s));
    let listener = f.talker("N7WIRE");

    // First over: key, speak, and stall the encoder so the unkey flush is
    // truncated by FLUSH_DEADLINE with frames still owed.
    f.session_mut().set_ptt(true);
    assert!(wait_until(|| f.session().state().ptt, 1_000), "must key");
    let _first_header = listener.recv_packet();
    stall.store(true, Ordering::Relaxed);
    push_mic_level(&f.mic_sink, FIRST, 160);
    thread::sleep(Duration::from_millis(60));
    f.session_mut().set_ptt(false);
    assert!(
        wait_until(|| !f.session().state().ptt, 2_000),
        "unkey must complete even with the encoder stalled (FLUSH_DEADLINE)"
    );
    let _first_tail = listener.drain_quiet(300);

    // The chip comes back: the abandoned frames are now sitting in the
    // encoder's output queue with nobody polling them.
    stall.store(false, Ordering::Relaxed);
    thread::sleep(Duration::from_millis(50));

    // Second over.
    f.session_mut().set_ptt(true);
    assert!(
        wait_until(|| f.session().state().ptt, 1_000),
        "must key a second time"
    );
    let second_header = listener.recv_packet();
    let DsvtPacket::Header {
        stream_id: second_id,
        ..
    } = DsvtPacket::parse(&second_header).expect("valid header")
    else {
        panic!("the second keying must start with its own header");
    };
    push_mic_level(&f.mic_sink, SECOND, 200);
    f.session_mut().set_ptt(false);
    assert!(
        wait_until(|| !f.session().state().ptt, 2_000),
        "second unkey must complete"
    );

    let tail = listener.drain_quiet(300);
    assert!(!tail.is_empty(), "the second over must transmit something");
    for (i, bytes) in tail.iter().enumerate() {
        let (sid, _seq, _end, ambe) = as_voice(bytes);
        assert_eq!(sid, second_id, "frame {i} must belong to the new stream");
        let level = decoded_tx_level(&ambe);
        assert!(
            (i32::from(level) - i32::from(FIRST)).abs() > 2,
            "frame {i} of the SECOND transmission decoded to level {level} — the FIRST over's \
             audio: stale encoded frames survived the key-down"
        );
        assert!(
            (i32::from(level) - i32::from(SECOND)).abs() <= 2,
            "frame {i} of the SECOND transmission decoded to level {level}, not the {SECOND} \
             actually spoken into it"
        );
    }
}

/// Sustained transmit (iax-2f6b review finding 4): a long over against a
/// realistically slow encoder must not silently drop mic frames.
///
/// `MAX_PENDING_TX_FRAMES` is 32 frames — 640 ms — and overflow discards the
/// newest mic frame with nothing but a log line: audible gaps punched into the
/// middle of a live transmission. The hardware round trip deliberately sizes
/// its burst UNDER that bound, so only a sustained feed exercises it. Feeding
/// in real time (as a capture device does) with a ~16 ms/frame encoder, the
/// session must keep up: every pushed frame encoded, none dropped.
#[test]
fn a_sustained_transmission_drops_no_mic_frames() {
    const CHUNK_MS: u32 = 100;
    const CHUNKS: usize = 12; // 1.2 s — well past the 640 ms queue bound
    const FRAMES: usize = (8_000 * CHUNK_MS as usize / 1_000) / 160 * CHUNKS;

    let mut f = Fixture::start(|s| {
        FakeVocoder::with_encode_latency(
            Duration::from_millis(2),
            // The real ThumbDV's measured synchronous encode cost.
            Duration::from_millis(16),
            s,
        )
    });
    let listener = f.talker("N7WIRE");

    f.session_mut().set_ptt(true);
    assert!(wait_until(|| f.session().state().ptt, 1_000), "must key");
    let _header = listener.recv_packet();

    // Real-time-ish feed: a capture callback's worth of audio every CHUNK_MS.
    for _ in 0..CHUNKS {
        push_mic_level(&f.mic_sink, 9_000, CHUNK_MS);
        thread::sleep(Duration::from_millis(u64::from(CHUNK_MS)));
    }

    f.session_mut().set_ptt(false);
    assert!(
        wait_until(|| !f.session().state().ptt, 2_000),
        "unkey must complete"
    );
    let _tail = listener.drain_quiet(400);

    let (submitted, dropped) = f.encode_stats();
    assert_eq!(
        dropped, 0,
        "{dropped} mic frame(s) were dropped mid-transmission (submitted {submitted} of \
         {FRAMES}): the TX queue overflowed, punching audible gaps into a live over"
    );
    assert!(
        submitted >= FRAMES - 2,
        "only {submitted} of {FRAMES} mic frames reached the encoder"
    );
}

/// A hand-rolled stand-in reflector: ACKs the link like the real one, records
/// every DSVT packet it receives, and — on the test's command — sends the
/// `"DISCONNECTED"` datagram a restarting or unlinking reflector sends, which
/// is what drives a client's [`LinkState`] out of `Linked` without waiting
/// out the 30 s keepalive timeout.
struct FakeReflector {
    addr: SocketAddr,
    drop_link: Arc<AtomicBool>,
    dsvt_seen: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl FakeReflector {
    fn spawn() -> FakeReflector {
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind fake reflector");
        sock.set_read_timeout(Some(Duration::from_millis(20)))
            .expect("set read timeout");
        let addr = sock.local_addr().expect("local addr");
        let drop_link = Arc::new(AtomicBool::new(false));
        let dsvt_seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let thread_drop = Arc::clone(&drop_link);
        let thread_seen = Arc::clone(&dsvt_seen);
        thread::spawn(move || {
            let mut peer: Option<SocketAddr> = None;
            let mut dropped = false;
            let mut buf = [0u8; 256];
            loop {
                if let Ok((n, src)) = sock.recv_from(&mut buf) {
                    peer = Some(src);
                    let data = &buf[..n];
                    if n == 11 {
                        // 14-byte connect ACK: only the trailing "ACK\0"
                        // is checked by DextraFsm (see its module docs).
                        let mut ack = [0u8; 14];
                        ack[..8].copy_from_slice(&data[..8]);
                        ack[8] = data[8];
                        ack[9] = data[9];
                        ack[10..].copy_from_slice(b"ACK\0");
                        let _ = sock.send_to(&ack, src);
                    } else if data.starts_with(b"DSVT") {
                        thread_seen.lock().unwrap().push(data.to_vec());
                    }
                }
                if !dropped
                    && thread_drop.load(Ordering::Relaxed)
                    && let Some(p) = peer
                {
                    let _ = sock.send_to(b"DISCONNECTED", p);
                    dropped = true;
                }
            }
        });
        FakeReflector {
            addr,
            drop_link,
            dsvt_seen,
        }
    }

    fn voice_frames_seen(&self) -> usize {
        self.dsvt_seen
            .lock()
            .unwrap()
            .iter()
            .filter(|b| matches!(DsvtPacket::parse(b), Ok(DsvtPacket::Voice { .. })))
            .count()
    }
}

/// Safety (iax-2f6b review finding 2): losing the reflector link while keyed
/// must force an unkey — gate shut, stream terminated, transmit stopped.
///
/// Before the fix nothing in the run loop reacted to the FSM leaving
/// `Linked`: the session kept the gate open, kept feeding the encoder and
/// kept sending DSVT voice frames at a peer that no longer had it linked,
/// indefinitely, with `ptt` still reporting `true`. A transmit path that can
/// get stuck keyed is a safety defect, not a bug.
#[test]
fn losing_the_link_while_keyed_forces_an_unkey_and_stops_transmitting() {
    let reflector = FakeReflector::spawn();
    let mut f = Fixture::start_against(reflector.addr, |s| {
        FakeVocoder::new(Duration::from_millis(2), s)
    });

    f.session_mut().set_ptt(true);
    assert!(wait_until(|| f.session().state().ptt, 1_000), "must key");
    push_mic_level(&f.mic_sink, 9_000, 200);
    assert!(
        wait_until(|| reflector.voice_frames_seen() > 0, 2_000),
        "the session must actually be transmitting before the link is dropped"
    );

    // The reflector goes away mid-transmission.
    reflector.drop_link.store(true, Ordering::Relaxed);
    assert!(
        wait_until(|| !f.session().state().ptt, 3_000),
        "losing the link while keyed must force an unkey, got link {:?}",
        f.session().state().link
    );
    assert_ne!(
        f.session().state().link,
        LinkState::Linked,
        "the link must be reported as down"
    );

    // Terminated, then silent: the last voice frame carries EOT and nothing
    // more goes out, even though PTT is still held down.
    let after = reflector.voice_frames_seen();
    let last = reflector
        .dsvt_seen
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("frames were transmitted");
    let (_, _, end, _) = as_voice(&last);
    assert!(
        end,
        "a forced unkey must still terminate the stream with an EOT frame"
    );
    push_mic_level(&f.mic_sink, 9_000, 200);
    thread::sleep(Duration::from_millis(250));
    assert_eq!(
        reflector.voice_frames_seen(),
        after,
        "nothing may be transmitted after a forced unkey while PTT is still held"
    );
}

/// The other half of the same rule: PTT is REFUSED, not silently accepted,
/// while the link is not up. Before the fix `set_ptt(true)` sent an RF header
/// and a full voice stream at a reflector that had already dropped the
/// client, with the operator's UI happily reporting "KEYED — transmitting".
#[test]
fn keying_is_refused_while_the_link_is_not_up() {
    let reflector = FakeReflector::spawn();
    let mut f = Fixture::start_against(reflector.addr, |s| {
        FakeVocoder::new(Duration::from_millis(2), s)
    });

    reflector.drop_link.store(true, Ordering::Relaxed);
    assert!(
        wait_until(|| f.session().state().link != LinkState::Linked, 2_000),
        "the fake reflector must drop the link first"
    );
    let before = reflector.voice_frames_seen();

    f.session_mut().set_ptt(true);
    thread::sleep(Duration::from_millis(250));
    assert!(
        !f.session().state().ptt,
        "a key-down must be refused while the link is down"
    );
    assert_eq!(
        reflector.voice_frames_seen(),
        before,
        "a refused key-down must put nothing on the wire"
    );
    f.session_mut().set_ptt(false);
}

/// A backend with NO capture device at all (and whose `open_input` fails
/// outright, belt and braces) — a machine with no microphone, one whose only
/// input is exclusively held, or a macOS app whose mic permission was denied.
struct OutputOnlyBackend {
    output_tap: OutputTap,
}

impl AudioBackend for OutputOnlyBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![dev(Direction::Output, "dstar-out")])
    }
    fn default_input(&self) -> Option<DeviceInfo> {
        None
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

/// Receive-only D-Star must work on a machine with no usable microphone
/// (iax-2f6b review finding 5).
///
/// The TX work made `connect` resolve AND open a capture device
/// unconditionally, so a user who denied mic permission — or has no input
/// device at all — could no longer even LISTEN to a reflector, a capability
/// that worked before. Connect must succeed, audio must decode and play, and
/// a key request must simply be refused (no header, no orphan stream) rather
/// than failing the whole session.
#[test]
fn a_session_with_no_capture_device_still_connects_and_receives() {
    const LEVEL: i16 = 11_000;

    let reflector = Reflector::bind_parrot("127.0.0.1:0".parse().unwrap()).expect("bind reflector");
    let reflector_addr = reflector.local_addr();
    let handle = reflector.run();

    let stats = Arc::new(Mutex::new(VocoderStats::default()));
    let output_tap: OutputTap = Arc::new(Mutex::new(None));
    let tap_for_backend = Arc::clone(&output_tap);
    let cfg = DstarConfig {
        host: reflector_addr.ip().to_string(),
        port: reflector_addr.port(),
        module: b'A',
        callsign: "N0CALL".into(),
        output: None,
        input: None,
        reflector_callsign: None,
    };
    let mut session = DstarSession::connect_with_stream(
        cfg,
        &move || {
            Box::new(OutputOnlyBackend {
                output_tap: Arc::clone(&tap_for_backend),
            }) as Box<dyn AudioBackend>
        },
        Box::new(FakeVocoder::new(Duration::from_millis(2), &stats)),
        AmbeBackend::Hardware,
    )
    .expect("a machine with no microphone must still be able to LISTEN to a reflector");

    assert!(
        wait_until(|| session.state().link == LinkState::Linked, 2_000),
        "the receive-only session must link"
    );

    // RX still works end to end.
    let talker = Talker::link(reflector_addr, "AJ7HR", b'A');
    talker.header(0x7070, "AJ7HR");
    for seq in 0..5u8 {
        talker.send(&voice_frame(0x7070, seq, LEVEL, seq == 4));
    }
    let mut rec = Recorder::new(&output_tap);
    assert!(
        rec.wait_for_level(LEVEL, 3_000),
        "a session with no capture device must still decode and play received audio"
    );

    // And PTT is refused rather than half-applied.
    session.set_ptt(true);
    thread::sleep(Duration::from_millis(200));
    assert!(
        !session.state().ptt,
        "with no capture device there is nothing to transmit — the key must be refused"
    );
    session.disconnect();
    drop(handle);
}

/// Half-duplex mute: while keyed, an inbound DSVT stream from another talker
/// must be ignored entirely (no talker/decode activity) — proving this
/// session never decodes and encodes against the one `ThumbDV` link at once.
/// Once unkeyed, the exact same traffic (fresh header/frames) IS processed
/// normally, proving this is a deliberate mute, not a broken RX path.
#[test]
fn half_duplex_ignores_inbound_traffic_while_transmitting() {
    let mut f = Fixture::start(|s| FakeVocoder::new(Duration::from_millis(2), s));
    let talker = f.talker("AJ7HR");

    f.session_mut().set_ptt(true);
    assert!(
        wait_until(|| f.session().state().ptt, 1_000),
        "must key before sending inbound traffic"
    );

    talker.header(0x5151, "AJ7HR");
    for seq in 0..5u8 {
        talker.send(&voice_frame(0x5151, seq, 7_000, seq == 4));
    }
    // No fixed sleep: give the run-loop several ticks' worth of real time to
    // have processed this (it won't) before asserting the negative.
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        f.session().state().talker,
        None,
        "inbound DSVT traffic must be ignored entirely while transmitting"
    );

    f.session_mut().set_ptt(false);
    assert!(
        wait_until(|| !f.session().state().ptt, 1_000),
        "set_ptt(false) must be applied"
    );

    // The SAME kind of traffic, sent again after unkey, must now be heard —
    // proving RX was muted, not broken. A DIFFERENT callsign for the second
    // stream is what makes that provable: with one callsign for both, "the
    // mute worked and the new stream was heard" and "the muted stream's
    // buffered header was decoded late" satisfy the assertion identically.
    talker.header(0x5252, "W7VRY");
    for seq in 0..5u8 {
        talker.send(&voice_frame(0x5252, seq, 7_000, seq == 4));
    }
    assert!(
        wait_until(
            || f.session().state().talker.as_deref() == Some("W7VRY"),
            2_000
        ),
        "once unkeyed, inbound traffic must be processed normally again, got {:?}",
        f.session().state().talker
    );
}

/// Safety: disconnecting while STILL KEYED (no explicit unkey first) must
/// still flush the queued audio and emit a properly terminated (`end =
/// true`) DSVT stream before the unlink — mirroring the RX-side
/// `a_wedged_vocoder_cannot_hang_the_run_loop_or_disconnect` test's shape,
/// but for the TX teardown path (`unlink_flushing_eot_if_keyed`).
/// `disconnect()` itself must not hang.
#[test]
fn disconnect_while_keyed_flushes_and_terminates_the_stream() {
    let mut f = Fixture::start(|s| FakeVocoder::new(Duration::from_millis(2), s));
    let listener = f.talker("N7WIRE");

    f.session_mut().set_ptt(true);
    assert!(
        wait_until(|| f.session().state().ptt, 1_000),
        "must key before disconnecting while keyed"
    );
    let _header_bytes = listener.recv_packet();
    push_mic_level(&f.mic_sink, 6_000, 100); // 5 frames, never explicitly unkeyed

    // disconnect() joins the run-loop thread; run it off-thread so a hang is
    // a test FAILURE rather than a suite that never finishes (iax-239a).
    let session = f.take_session();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        session.disconnect();
        let _ = done_tx.send(());
    });
    assert!(
        done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
        "disconnect() while keyed hung: the shutdown branch must flush+unkey unconditionally"
    );

    let tail = listener.drain_quiet(300);
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
}

/// Safety: an encoder that stops answering mid-transmission (the TX-side
/// twin of `a_wedged_vocoder_cannot_hang_the_run_loop_or_disconnect`) must
/// still let unkey terminate — bounded by `FLUSH_DEADLINE` — rather than
/// leaving the stream open on the wire forever. With nothing ever
/// successfully encoded, the terminating frame is a bare all-zero-AMBE EOT.
#[test]
fn unkey_terminates_even_when_the_encoder_never_answers() {
    let mut f = Fixture::start(FakeVocoder::wedged_encode);
    let listener = f.talker("N7WIRE");

    f.session_mut().set_ptt(true);
    assert!(
        wait_until(|| f.session().state().ptt, 1_000),
        "must key before pushing audio into the wedged encoder"
    );
    let _header_bytes = listener.recv_packet();
    push_mic_level(&f.mic_sink, 5_000, 100); // submitted, but never comes back

    // set_ptt(false) itself never blocks (it only stores a request atomic);
    // the run-loop's own unkey flush is bounded by FLUSH_DEADLINE (250ms) —
    // `wait_until`'s 1s budget gives that comfortable margin without hanging
    // the test if the bound were ever broken.
    let before_unkey = Instant::now();
    f.session_mut().set_ptt(false);
    assert!(
        wait_until(|| !f.session().state().ptt, 2_000),
        "unkey must still complete (bounded by FLUSH_DEADLINE) even when the encoder never answers"
    );
    // The budget above and the bound below must NOT be the same number: with
    // `wait_until(.., 1_000)` followed by `elapsed() < 1s`, the assertion is
    // satisfied by construction and could never catch FLUSH_DEADLINE being
    // raised — the exact regression it exists for. 600 ms is comfortably
    // above the 250 ms deadline plus one 50 ms poll and the snapshot's own
    // polling granularity, and comfortably below the 2 s wait budget.
    assert!(
        before_unkey.elapsed() < Duration::from_millis(600),
        "unkey took {:?}, longer than FLUSH_DEADLINE (250 ms) plus a poll should ever allow",
        before_unkey.elapsed()
    );

    let tail = listener.drain_quiet(600);
    assert!(
        !tail.is_empty(),
        "unkey must still emit a terminating frame even when the encoder never answers"
    );
    let last =
        DsvtPacket::parse(tail.last().expect("checked non-empty above")).expect("valid packet");
    let DsvtPacket::Voice { end, ambe, .. } = last else {
        panic!("expected a voice frame, got {last:?}");
    };
    assert!(end, "the bailout frame must still carry the EOT bit");
    assert_eq!(
        ambe, NULL_AMBE,
        "with nothing ever successfully encoded, the bare EOT frame must carry D-Star's NULL \
         codeword (an all-zero payload is not silence to an AMBE decoder)"
    );
}

// ---- iax-4c8e: the ConsoleSession snapshot mirrors D-Star -------------------

/// A `ConsoleState` snapshot must report a live D-Star session the same way it
/// reports an M17 one: `dstar_active` set, `status` tracking the link, and
/// both cleared again after disconnect.
///
/// This is what a front-end reads to decide whether to draw a connected
/// station and offer PTT, so a snapshot that never mentions D-Star leaves the
/// UI blind to a session that is genuinely transmitting and receiving.
#[test]
fn the_console_snapshot_tracks_a_dstar_session_and_its_link() {
    use astar_console::{CallStatus, ConsoleSession};

    let reflector = Reflector::bind_parrot("127.0.0.1:0".parse().unwrap()).expect("bind reflector");
    let reflector_addr = reflector.local_addr();
    let handle = reflector.run();

    let mut console = ConsoleSession::new();

    // Idle: no session, and D-Star reports itself inactive.
    let idle = console.snapshot();
    assert!(!idle.dstar_active, "no session yet");
    assert_eq!(idle.status, CallStatus::Idle);

    let stats = Arc::new(Mutex::new(VocoderStats::default()));
    let output_tap: OutputTap = Arc::new(Mutex::new(None));
    let tap_for_backend = Arc::clone(&output_tap);
    let cfg = DstarConfig {
        host: reflector_addr.ip().to_string(),
        port: reflector_addr.port(),
        module: b'A',
        callsign: "N0CALL".into(),
        output: None,
        input: None,
        reflector_callsign: None,
    };
    let session = DstarSession::connect_with_stream(
        cfg,
        &move || {
            Box::new(OutputOnlyBackend {
                output_tap: Arc::clone(&tap_for_backend),
            }) as Box<dyn AudioBackend>
        },
        Box::new(FakeVocoder::new(Duration::from_millis(2), &stats)),
        AmbeBackend::Hardware,
    )
    .expect("connect to the loopback reflector");
    console.dstar_adopt(session).expect("adopt the session");

    // Linked → Answered, exactly as the M17 branch maps its own link state,
    // so a front-end runs ONE connection state machine for every network.
    assert!(
        wait_until(|| console.snapshot().status == CallStatus::Answered, 2_000),
        "status must reach Answered once the reflector ACKs the link"
    );
    let linked = console.snapshot();
    assert!(linked.dstar_active, "a live session must report as active");
    assert!(
        !linked.ptt,
        "a freshly linked session is not keyed — nothing may report transmit before a key-down"
    );

    console.dstar_disconnect();
    let after = console.snapshot();
    assert!(!after.dstar_active, "disconnect must clear the flag");
    assert_eq!(after.status, CallStatus::Idle);

    handle.shutdown();
}
