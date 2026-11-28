// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The picture transmit path (`Manager::send_picture_audio`) against a fake
//! IAX2 peer on 127.0.0.1.
//!
//! The peer answers a no-auth NEW with ACCEPT, ACKs every full frame, and
//! records each voice frame the client sends: its full 32-bit time-stamp
//! (rebuilt from the mini frames' low 16 bits), its payload and when it
//! arrived. It can also hang up on request. Its shape follows
//! `wideband_station.rs`.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use astar_audio::{
    AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, MicId, OutputId,
    OutputSource, StreamConfig, StreamHandle,
};
use astar_codec::g711::{ulaw_decode, ulaw_encode};
use astar_iax::manager::DialSpec;
use astar_iax::{
    CallId, CallMode, CodecPolicy, CutReason, IaxError, Manager, PictureEnd, PictureTx,
};
use astar_iax_core::VoiceFormat;
use astar_iax_core::frame::{Frame, FullFrame, Subclass, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{ControlSubclass, FrameType, IaxCommand};

// ---------------------------------------------------------------------------
// Audio backend: retains mic sinks so a test can push microphone audio.
// ---------------------------------------------------------------------------

fn dev(dir: Direction, tag: &str) -> DeviceInfo {
    DeviceInfo {
        id: DeviceId::new(tag.to_string()),
        name: tag.to_string(),
        direction: dir,
        channels: 1,
        native_sample_rates: vec![8_000],
    }
}

struct StubHandle;
impl StreamHandle for StubHandle {
    fn stop(self: Box<Self>) {}
    fn pause(&self) -> Result<(), AudioError> {
        Ok(())
    }
    fn resume(&self) -> Result<(), AudioError> {
        Ok(())
    }
}

#[derive(Clone, Default)]
struct MicSinks(Arc<Mutex<HashMap<String, Box<dyn InputSink>>>>);

impl MicSinks {
    fn push(&self, device: &str, samples: &[f32]) {
        let mut g = self.0.lock().unwrap();
        if let Some(mut sink) = g.remove(device) {
            drop(g);
            sink.write(samples, 0.5);
            self.0.lock().unwrap().insert(device.to_string(), sink);
        }
    }
}

struct TestBackend {
    mic: MicSinks,
}

impl AudioBackend for TestBackend {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![
            dev(Direction::Input, "in:a"),
            dev(Direction::Output, "out:s"),
        ])
    }
    fn default_input(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Input, "in:a"))
    }
    fn default_output(&self) -> Option<DeviceInfo> {
        Some(dev(Direction::Output, "out:s"))
    }
    fn open_input(
        &self,
        d: &DeviceInfo,
        _c: StreamConfig,
        sink: Box<dyn InputSink>,
        _overruns: Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        self.mic
            .0
            .lock()
            .unwrap()
            .insert(d.id.as_str().to_string(), sink);
        Ok(Box::new(StubHandle))
    }
    fn open_output(
        &self,
        _d: &DeviceInfo,
        _c: StreamConfig,
        _source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        Ok(Box::new(StubHandle))
    }
}

// ---------------------------------------------------------------------------
// The fake peer.
// ---------------------------------------------------------------------------

/// One voice frame as the peer received it.
#[derive(Clone, Debug)]
struct RxVoice {
    ts: u32,
    payload: Vec<u8>,
    at: Instant,
}

/// What the peer thread and the test share.
#[derive(Clone, Default)]
struct PeerCells {
    voice: Arc<Mutex<Vec<RxVoice>>>,
    /// Every TEXT frame the client sent, without its trailing NUL.
    texts: Arc<Mutex<Vec<String>>>,
    hangup_now: Arc<AtomicBool>,
    hung_up_at: Arc<Mutex<Option<Instant>>>,
    stop: Arc<AtomicBool>,
}

struct Peer {
    addr: SocketAddr,
    cells: PeerCells,
    join: Option<thread::JoinHandle<()>>,
}

impl Peer {
    fn start(format: VoiceFormat) -> Self {
        Self::spawn(format, false)
    }

    /// A peer that opens the link with `!NEWKEY1!`, as an `app_rpt` node
    /// does, so our own hangup to it is held for the disconnect grace.
    fn start_app_rpt(format: VoiceFormat) -> Self {
        Self::spawn(format, true)
    }

    fn spawn(format: VoiceFormat, newkey: bool) -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();
        let addr = sock.local_addr().unwrap();
        let cells = PeerCells::default();
        let join = {
            let cells = cells.clone();
            thread::spawn(move || run_peer(&sock, format, newkey, &cells))
        };
        Self {
            addr,
            cells,
            join: Some(join),
        }
    }

    fn voice(&self) -> Vec<RxVoice> {
        self.cells.voice.lock().unwrap().clone()
    }

    fn texts(&self) -> Vec<String> {
        self.cells.texts.lock().unwrap().clone()
    }

    fn hang_up(&self) {
        self.cells.hangup_now.store(true, Ordering::SeqCst);
    }

    fn hung_up_at(&self) -> Option<Instant> {
        *self.cells.hung_up_at.lock().unwrap()
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.cells.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

const SERVER_CALL: u16 = 41;

fn full(
    client_call: u16,
    oseqno: u8,
    iseqno: u8,
    ts: u32,
    frame_type: FrameType,
    subclass: Subclass,
    ies: Ies<'_>,
) -> Vec<u8> {
    full_with_payload(
        client_call,
        oseqno,
        iseqno,
        ts,
        frame_type,
        subclass,
        ies,
        &[],
    )
}

#[allow(clippy::too_many_arguments)]
fn full_with_payload(
    client_call: u16,
    oseqno: u8,
    iseqno: u8,
    ts: u32,
    frame_type: FrameType,
    subclass: Subclass,
    ies: Ies<'_>,
    payload: &[u8],
) -> Vec<u8> {
    let frame = Frame::Full(Box::new(FullFrame {
        source_call: SERVER_CALL,
        dest_call: client_call,
        retransmission: false,
        timestamp: ts,
        oseqno,
        iseqno,
        frame_type,
        subclass,
        ies,
        payload,
    }));
    let mut out = Vec::with_capacity(64);
    encode(&frame, &mut out).expect("test frame encodes");
    out
}

#[allow(clippy::too_many_lines)]
fn run_peer(sock: &UdpSocket, format: VoiceFormat, newkey: bool, cells: &PeerCells) {
    let mut buf = [0u8; 4096];
    let mut client: Option<(SocketAddr, u16)> = None;
    // The peer's own outbound sequence number (ACCEPT is 0).
    let mut oseq: u8 = 0;
    // The next sequence number expected from the client.
    let mut next_in: u8 = 0;
    // The last full 32-bit voice time-stamp, to rebuild mini frames' upper bits.
    let mut last_ts: Option<u32> = None;
    let mut done = false;
    while !cells.stop.load(Ordering::SeqCst) {
        if !done
            && cells.hangup_now.swap(false, Ordering::SeqCst)
            && let Some((src, client_call)) = client
        {
            let hangup = full(
                client_call,
                oseq,
                next_in,
                0,
                FrameType::Iax,
                Subclass::Iax(IaxCommand::Hangup),
                Ies::empty(),
            );
            let _ = sock.send_to(&hangup, src);
            *cells.hung_up_at.lock().unwrap() = Some(Instant::now());
            done = true;
        }
        let Ok((n, src)) = sock.recv_from(&mut buf) else {
            continue;
        };
        let Ok(frame) = parse_lenient(&buf[..n]) else {
            continue;
        };
        let at = Instant::now();
        match frame {
            Frame::Mini(m) => {
                let low = u32::from(m.timestamp);
                let ts = match last_ts {
                    Some(prev) => {
                        let mut t = (prev & 0xFFFF_0000) | low;
                        if t < prev {
                            t = t.wrapping_add(0x1_0000);
                        }
                        t
                    }
                    None => low,
                };
                last_ts = Some(ts);
                cells.voice.lock().unwrap().push(RxVoice {
                    ts,
                    payload: m.payload.to_vec(),
                    at,
                });
            }
            Frame::Full(ff) => {
                if matches!(ff.subclass, Subclass::Iax(IaxCommand::Ack)) {
                    continue;
                }
                next_in = ff.oseqno.wrapping_add(1);
                let ack = full(
                    ff.source_call,
                    ff.iseqno,
                    next_in,
                    ff.timestamp,
                    FrameType::Iax,
                    Subclass::Iax(IaxCommand::Ack),
                    Ies::empty(),
                );
                let _ = sock.send_to(&ack, src);
                if ff.frame_type == FrameType::Voice {
                    last_ts = Some(ff.timestamp);
                    cells.voice.lock().unwrap().push(RxVoice {
                        ts: ff.timestamp,
                        payload: ff.payload.to_vec(),
                        at,
                    });
                }
                if ff.frame_type == FrameType::Text {
                    let text = String::from_utf8_lossy(ff.payload);
                    cells
                        .texts
                        .lock()
                        .unwrap()
                        .push(text.trim_end_matches('\0').to_string());
                }
                match ff.subclass {
                    Subclass::Iax(IaxCommand::New) if client.is_none() => {
                        client = Some((src, ff.source_call));
                        let accept = full(
                            ff.source_call,
                            oseq,
                            next_in,
                            0,
                            FrameType::Iax,
                            Subclass::Iax(IaxCommand::Accept),
                            Ies {
                                format: Some(format.as_u32()),
                                ..Ies::empty()
                            },
                        );
                        oseq = oseq.wrapping_add(1);
                        let _ = sock.send_to(&accept, src);
                        if newkey {
                            let text = full_with_payload(
                                ff.source_call,
                                oseq,
                                next_in,
                                40,
                                FrameType::Text,
                                Subclass::Raw(0),
                                Ies::empty(),
                                b"!NEWKEY1!\0",
                            );
                            oseq = oseq.wrapping_add(1);
                            let _ = sock.send_to(&text, src);
                        }
                    }
                    Subclass::Iax(IaxCommand::Hangup)
                    | Subclass::Control(ControlSubclass::Hangup) => done = true,
                    _ => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn spec(id: u64, peer: SocketAddr, codec_policy: CodecPolicy) -> DialSpec {
    DialSpec {
        id: CallId::from_raw(id),
        node: "1999".to_string(),
        peer,
        output: OutputId::new("out:s"),
        caller_id: "picture-tx".to_string(),
        secret: String::new(),
        mode: CallMode::Standard,
        dest: "1999".to_string(),
        frame_observer: None,
        codec_policy,
    }
}

fn wait_until(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    f()
}

fn wait_negotiated(mgr: &Manager, id: CallId) {
    assert!(
        wait_until(Duration::from_secs(6), || {
            mgr.snapshot()
                .calls
                .iter()
                .any(|c| c.id == id && c.is_active() && c.negotiated_format.is_some())
        }),
        "call never answered"
    );
}

/// A manager with one answered call, a routed mic, keyed.
fn keyed_call(mgr: &mut Manager, id: u64, peer: &Peer) -> CallId {
    keyed_call_with(mgr, id, peer, CodecPolicy::default())
}

fn keyed_call_with(mgr: &mut Manager, id: u64, peer: &Peer, policy: CodecPolicy) -> CallId {
    let call = mgr.dial(spec(id, peer.addr, policy)).unwrap();
    wait_negotiated(mgr, call);
    mgr.route(call, &MicId::new("in:a")).unwrap();
    mgr.key(call).unwrap();
    call
}

fn manager() -> (Manager, MicSinks) {
    let mic = MicSinks::default();
    let mgr = Manager::new(Box::new(TestBackend { mic: mic.clone() }));
    (mgr, mic)
}

/// A known ramp: every sample differs from its neighbour, so a dropped,
/// repeated or shifted frame cannot match.
fn ramp(samples: usize) -> Arc<[i16]> {
    (0..samples)
        .map(|i| i16::try_from((i * 37) % 20_000).unwrap() - 10_000)
        .collect()
}

/// What a μ-law peer should decode for frame `n` of `pcm` (zero-padded).
fn expected_ulaw_frame(pcm: &[i16], n: usize, frame_len: usize) -> Vec<i16> {
    (0..frame_len)
        .map(|i| {
            let s = pcm.get(n * frame_len + i).copied().unwrap_or(0);
            ulaw_decode(ulaw_encode(s))
        })
        .collect()
}

fn decode_ulaw(payload: &[u8]) -> Vec<i16> {
    payload.iter().map(|&b| ulaw_decode(b)).collect()
}

fn wait_ended(tx: &PictureTx, limit: Duration) -> Option<PictureEnd> {
    wait_until(limit, || tx.ended().is_some());
    tx.ended()
}

fn tone_frame(phase: &mut f32) -> Vec<f32> {
    (0..160)
        .map(|_| {
            let s = 0.5 * phase.sin();
            *phase += 2.0 * std::f32::consts::PI * 440.0 / 8000.0;
            s
        })
        .collect()
}

/// A microphone fed a 440 Hz tone in real time, the way a capture device
/// delivers it: frame `k` lands at `start + k × 20 ms`, on an absolute
/// schedule.
///
/// A loop that sleeps 20 ms after each push runs slow: every frame lands a
/// few milliseconds later than the last, and after ten or so frames the
/// wall clock is more than 80 ms ahead of the audio. The voice ladder then
/// re-anchors, as it should on a slow feed. That step belongs to the feed,
/// not to the code under test, so these tests feed in real time.
struct ToneFeed {
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl ToneFeed {
    fn start(mic: &MicSinks, mut phase: f32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let join = {
            let (mic, stop) = (mic.clone(), Arc::clone(&stop));
            thread::spawn(move || {
                let start = Instant::now();
                let mut k: u32 = 0;
                while !stop.load(Ordering::SeqCst) {
                    let due = start + Duration::from_millis(20) * k;
                    let now = Instant::now();
                    if due > now {
                        thread::sleep(due - now);
                    }
                    mic.push("in:a", &tone_frame(&mut phase));
                    k += 1;
                }
            })
        };
        Self {
            stop,
            join: Some(join),
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            j.join().unwrap();
        }
    }
}

/// A feed dropped without [`ToneFeed::stop`] (a failed assertion unwinding
/// past it) still stops its thread. The join's result is ignored here: a
/// second panic while unwinding would abort the test binary.
impl Drop for ToneFeed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Every picture frame arrived once, in order, each 20 later than the last,
/// and each decodes to the μ-law round trip of the input.
fn assert_whole_picture(rx: &[RxVoice], pcm: &[i16], frame_len: usize, total: usize) {
    assert!(rx.len() >= total, "got {} of {total} frames", rx.len());
    for (n, f) in rx.iter().take(total).enumerate() {
        assert_eq!(
            decode_ulaw(&f.payload),
            expected_ulaw_frame(pcm, n, frame_len),
            "frame {n} does not hold the picture's audio"
        );
    }
    for (n, w) in rx[..total].windows(2).enumerate() {
        assert_eq!(
            w[1].ts.wrapping_sub(w[0].ts),
            20,
            "time-stamp step between frames {n} and {}",
            n + 1
        );
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[test]
fn picture_arrives_whole_with_unbroken_timestamps() {
    let peer = Peer::start(VoiceFormat::G711U);
    let (mut mgr, _mic) = manager();
    let call = keyed_call(&mut mgr, 1, &peer);
    assert_eq!(mgr.tx_bus_rate(call), Some(8000));

    let pcm = ramp(3 * 8000);
    let tx = mgr.send_picture_audio(call, Arc::clone(&pcm)).unwrap();
    assert_eq!(tx.frames_total(), 150);
    assert_eq!(
        wait_ended(&tx, Duration::from_secs(6)),
        Some(PictureEnd::Sent)
    );
    assert_eq!(tx.frames_sent(), 150);
    thread::sleep(Duration::from_millis(100));

    let rx = peer.voice();
    assert_eq!(rx.len(), 150, "exactly the picture's frames were sent");
    assert_whole_picture(&rx, &pcm, 160, 150);
}

#[test]
fn a_stalled_pacer_does_not_step_the_timestamps() {
    let peer = Peer::start(VoiceFormat::G711U);
    let (mut mgr, _mic) = manager();
    let call = keyed_call(&mut mgr, 1, &peer);

    let pcm = ramp(3 * 8000);
    let tx = mgr.send_picture_audio(call, Arc::clone(&pcm)).unwrap();
    assert!(wait_until(Duration::from_secs(3), || tx.frames_sent() >= 50));
    tx.stall_pacer(Duration::from_millis(300));
    assert_eq!(
        wait_ended(&tx, Duration::from_secs(7)),
        Some(PictureEnd::Sent)
    );
    thread::sleep(Duration::from_millis(100));

    let rx = peer.voice();
    assert_eq!(rx.len(), 150, "no frame missing or repeated");
    assert_whole_picture(&rx, &pcm, 160, 150);

    // The stall shows on the wire as one late frame, not as a burst: the
    // pacer sends one frame per wake and catches up on its schedule no faster
    // than one frame per half frame (10 ms).
    let (gap_at, gap) = rx
        .windows(2)
        .enumerate()
        .map(|(i, w)| (i + 1, w[1].at.duration_since(w[0].at)))
        .max_by_key(|(_, d)| *d)
        .unwrap();
    assert!(gap >= Duration::from_millis(250), "stall not seen: {gap:?}");
    if gap_at + 6 <= rx.len() {
        let span = rx[gap_at + 5].at.duration_since(rx[gap_at].at);
        assert!(
            span >= Duration::from_millis(40),
            "frames after the stall were bunched: 5 frames in {span:?}"
        );
    }
    // The schedule is absolute: the stall is caught up, so the picture still
    // ends about 149 frames (2.98 s) after it began. A pacer that slipped by
    // the stall would take at least 3.26 s.
    let length = rx[149].at.duration_since(rx[0].at);
    assert!(
        length < Duration::from_millis(3_200),
        "the stall moved the schedule: picture took {length:?}"
    );
}

#[test]
fn mic_audio_is_not_sent_during_a_picture() {
    let peer = Peer::start(VoiceFormat::G711U);
    let (mut mgr, mic) = manager();
    let call = keyed_call(&mut mgr, 1, &peer);

    let pcm = ramp(2 * 8000);
    let tx = mgr.send_picture_audio(call, Arc::clone(&pcm)).unwrap();
    let feed = ToneFeed::start(&mic, 0.0);
    assert_eq!(
        wait_ended(&tx, Duration::from_secs(5)),
        Some(PictureEnd::Sent)
    );
    feed.stop();
    thread::sleep(Duration::from_millis(100));

    // The first 100 frames are the picture, exactly; no tone frame is mixed
    // in, slipped between them, or sent in their place.
    let rx = peer.voice();
    assert_whole_picture(&rx, &pcm, 160, 100);
}

#[test]
fn voice_resumes_without_a_step() {
    let peer = Peer::start(VoiceFormat::G711U);
    let (mut mgr, mic) = manager();
    let call = keyed_call(&mut mgr, 1, &peer);

    let pcm = ramp(8000);
    let tx = mgr.send_picture_audio(call, Arc::clone(&pcm)).unwrap();
    let feed = ToneFeed::start(&mic, 0.0);
    assert_eq!(
        wait_ended(&tx, Duration::from_secs(4)),
        Some(PictureEnd::Sent)
    );
    // No microphone goes out on the key-down a picture ran on: the operator
    // unkeys and keys again to talk, straight away, as a fresh press would.
    mgr.unkey(call).unwrap();
    mgr.key(call).unwrap();
    assert!(wait_until(Duration::from_secs(2), || peer.voice().len() > 52));
    feed.stop();

    let rx = peer.voice();
    assert_whole_picture(&rx, &pcm, 160, 50);
    assert_eq!(
        rx[50].ts,
        rx[49].ts.wrapping_add(20),
        "the first mic frame after the picture steps the time-stamps"
    );
    assert_ne!(
        decode_ulaw(&rx[50].payload),
        expected_ulaw_frame(&pcm, 50, 160),
        "frame 50 should be microphone audio"
    );
}

/// A picture keys the call for itself and a key press stops it, so no one
/// holds a key through one: once a picture has run on a key-down, the
/// microphone stays off the air until the call is unkeyed and keyed again.
/// Here the call stays keyed, with a live microphone, for 400 ms after the
/// picture's last frame.
#[test]
fn no_microphone_after_a_picture_until_keyed_again() {
    let peer = Peer::start(VoiceFormat::G711U);
    let (mut mgr, mic) = manager();
    let call = keyed_call(&mut mgr, 1, &peer);

    let pcm = ramp(8000 / 2);
    let tx = mgr.send_picture_audio(call, Arc::clone(&pcm)).unwrap();
    let feed = ToneFeed::start(&mic, 0.0);
    assert_eq!(
        wait_ended(&tx, Duration::from_secs(3)),
        Some(PictureEnd::Sent)
    );
    thread::sleep(Duration::from_millis(400));
    assert_eq!(
        peer.voice().len(),
        25,
        "the microphone went out after the picture, on its key-down"
    );
    // An unkey alone still sends nothing; the next key-down talks.
    mgr.unkey(call).unwrap();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(peer.voice().len(), 25);
    mgr.key(call).unwrap();
    assert!(
        wait_until(Duration::from_secs(2), || peer.voice().len() >= 30),
        "the microphone never came back after a fresh key"
    );
    feed.stop();
    let rx = peer.voice();
    assert_whole_picture(&rx, &pcm, 160, 25);
}

#[test]
fn call_end_cuts_the_picture() {
    let (mut mgr, _mic) = manager();
    let peer = Peer::start(VoiceFormat::G711U);
    let call = keyed_call(&mut mgr, 1, &peer);

    let pcm = ramp(3 * 8000);
    let tx = mgr.send_picture_audio(call, Arc::clone(&pcm)).unwrap();
    assert!(wait_until(Duration::from_secs(3), || tx.frames_sent() >= 30));
    peer.hang_up();
    assert_eq!(
        wait_ended(&tx, Duration::from_secs(3)),
        Some(PictureEnd::Cut(CutReason::CallEnded))
    );
    let sent_at_end = tx.frames_sent();
    assert!(sent_at_end < 150);
    thread::sleep(Duration::from_millis(200));
    assert_eq!(tx.frames_sent(), sent_at_end);
    let hung_up_at = peer.hung_up_at().expect("peer hung up");
    let late: Vec<_> = peer
        .voice()
        .into_iter()
        .filter(|f| f.at > hung_up_at + Duration::from_millis(30))
        .collect();
    assert!(
        late.is_empty(),
        "{} frames sent after the hangup",
        late.len()
    );

    // The ended call can carry no picture.
    assert!(matches!(
        mgr.send_picture_audio(call, Arc::clone(&pcm)),
        Err(IaxError::PictureNoCall)
    ));
    mgr.reap_hungup();
    drop(peer);

    // A new call's first picture starts at frame 0.
    let peer2 = Peer::start(VoiceFormat::G711U);
    let call2 = keyed_call(&mut mgr, 2, &peer2);
    let short = ramp(8000 / 2);
    let tx2 = mgr.send_picture_audio(call2, Arc::clone(&short)).unwrap();
    assert_eq!(
        wait_ended(&tx2, Duration::from_secs(3)),
        Some(PictureEnd::Sent)
    );
    thread::sleep(Duration::from_millis(100));
    let rx2 = peer2.voice();
    assert_eq!(rx2.len(), 25);
    assert_whole_picture(&rx2, &short, 160, 25);
}

#[test]
fn sixteen_khz_bus() {
    let peer = Peer::start(VoiceFormat::Slin16);
    let mic = MicSinks::default();
    let mut mgr = Manager::with_policy(
        Box::new(TestBackend { mic: mic.clone() }),
        CodecPolicy::PreferSlin16,
    );
    let call = keyed_call_with(&mut mgr, 1, &peer, CodecPolicy::PreferSlin16);
    assert_eq!(mgr.tx_bus_rate(call), Some(16_000));

    let pcm = ramp(16_000);
    let tx = mgr.send_picture_audio(call, Arc::clone(&pcm)).unwrap();
    assert_eq!(tx.frames_total(), 50);
    assert_eq!(
        wait_ended(&tx, Duration::from_secs(4)),
        Some(PictureEnd::Sent)
    );
    thread::sleep(Duration::from_millis(100));

    let rx = peer.voice();
    assert_eq!(rx.len(), 50);
    assert!(
        rx.iter().all(|f| f.payload.len() == 640),
        "320 samples of slin16 per frame"
    );
    assert!(rx.windows(2).all(|w| w[1].ts.wrapping_sub(w[0].ts) == 20));
}

#[test]
fn second_picture_is_refused_while_one_runs() {
    let peer = Peer::start(VoiceFormat::G711U);
    let (mut mgr, _mic) = manager();
    let call = keyed_call(&mut mgr, 1, &peer);

    let pcm = ramp(3 * 8000);
    let tx = mgr.send_picture_audio(call, Arc::clone(&pcm)).unwrap();
    assert!(matches!(
        mgr.send_picture_audio(call, Arc::clone(&pcm)),
        Err(IaxError::PictureBusy)
    ));
    tx.cancel();
    assert_eq!(tx.ended(), Some(PictureEnd::Cancelled));
}

#[test]
fn cancel_stops_at_once() {
    let peer = Peer::start(VoiceFormat::G711U);
    let (mut mgr, _mic) = manager();
    let call = keyed_call(&mut mgr, 1, &peer);

    let pcm = ramp(3 * 8000);
    let tx = mgr.send_picture_audio(call, Arc::clone(&pcm)).unwrap();
    assert!(wait_until(Duration::from_secs(3), || tx.frames_sent() >= 25));
    let at_cancel = tx.frames_sent();
    tx.cancel();
    assert_eq!(tx.ended(), Some(PictureEnd::Cancelled));
    thread::sleep(Duration::from_millis(300));
    assert!(
        tx.frames_sent() <= at_cancel + 1,
        "{} frames after cancel",
        tx.frames_sent() - at_cancel
    );
    assert_eq!(peer.voice().len(), tx.frames_sent());
}

#[test]
fn unkey_cuts_the_picture() {
    let peer = Peer::start(VoiceFormat::G711U);
    let (mut mgr, _mic) = manager();
    let call = keyed_call(&mut mgr, 1, &peer);

    let pcm = ramp(3 * 8000);
    let tx = mgr.send_picture_audio(call, Arc::clone(&pcm)).unwrap();
    assert!(wait_until(Duration::from_secs(3), || tx.frames_sent() >= 25));
    mgr.unkey(call).unwrap();
    assert_eq!(
        wait_ended(&tx, Duration::from_secs(1)),
        Some(PictureEnd::Cut(CutReason::Unkeyed))
    );

    // Not keyed at all: the picture ends before its first frame.
    let tx2 = mgr.send_picture_audio(call, Arc::clone(&pcm)).unwrap();
    assert_eq!(tx2.ended(), Some(PictureEnd::Cut(CutReason::Unkeyed)));
    assert_eq!(tx2.frames_sent(), 0);
}

#[test]
fn no_call_is_refused() {
    let (mut mgr, _mic) = manager();
    assert!(matches!(
        mgr.send_picture_audio(CallId::from_raw(9), ramp(160)),
        Err(IaxError::PictureNoCall)
    ));
    assert_eq!(mgr.tx_bus_rate(CallId::from_raw(9)), None);
}

#[test]
fn local_hangup_cuts_the_picture() {
    // An app_rpt peer, so our hangup takes the disconnect grace path: the
    // run loop says !!DISCONNECT!! and holds the HANGUP for 250 ms. The
    // picture must stop when the hangup is taken, not when the grace ends.
    let peer = Peer::start_app_rpt(VoiceFormat::G711U);
    let (mut mgr, _mic) = manager();
    let call = keyed_call(&mut mgr, 1, &peer);

    let pcm = ramp(3 * 8000);
    let tx = mgr.send_picture_audio(call, Arc::clone(&pcm)).unwrap();
    assert!(wait_until(Duration::from_secs(3), || tx.frames_sent() >= 25));
    let at_hangup = tx.frames_sent();
    mgr.hangup(call, None).unwrap();

    assert!(
        peer.texts().iter().any(|t| t == "!!DISCONNECT!!"),
        "the hangup did not take the app_rpt grace path: {:?}",
        peer.texts()
    );
    assert_eq!(tx.ended(), Some(PictureEnd::Cut(CutReason::CallEnded)));
    assert!(
        tx.frames_sent() <= at_hangup + 1,
        "{} frames sent after the hangup",
        tx.frames_sent() - at_hangup
    );
    thread::sleep(Duration::from_millis(50));
    assert!(
        peer.voice().len() <= at_hangup + 1,
        "the peer got {} frames after the hangup",
        peer.voice().len().saturating_sub(at_hangup)
    );
}

#[test]
fn a_preroll_lead_during_a_picture_does_not_step_voice_after_it() {
    // VOX pre-roll on: frames heard while unkeyed fill the look-back ring,
    // and the first keyed mic frame flushes it ahead of the live stream and
    // publishes the pre-roll lead. Here that happens while the picture runs,
    // so the flushed frames are thrown away with the rest of the mic audio,
    // and the lead must not carry over into the voice after the picture.
    let peer = Peer::start(VoiceFormat::G711U);
    let (mut mgr, mic) = manager();
    let call = mgr
        .dial(spec(1, peer.addr, CodecPolicy::default()))
        .unwrap();
    wait_negotiated(&mgr, call);
    mgr.route(call, &MicId::new("in:a")).unwrap();
    mgr.set_vox_preroll_ms(call, 100);
    let mut phase = 0.0f32;
    for _ in 0..8 {
        mic.push("in:a", &tone_frame(&mut phase));
    }
    mgr.key(call).unwrap();

    let pcm = ramp(8000);
    let tx = mgr.send_picture_audio(call, Arc::clone(&pcm)).unwrap();
    // Once a frame has gone out the run loop holds the picture, so the
    // first keyed push below, which flushes the ring and publishes the lead,
    // lands during the picture on any machine.
    assert!(wait_until(Duration::from_secs(1), || tx.frames_sent() >= 1));
    let feed = ToneFeed::start(&mic, phase);
    assert_eq!(
        wait_ended(&tx, Duration::from_secs(4)),
        Some(PictureEnd::Sent)
    );
    // The voice after it is a fresh over: unkey and key again at once.
    mgr.unkey(call).unwrap();
    mgr.key(call).unwrap();
    assert!(wait_until(Duration::from_secs(2), || peer.voice().len() > 65));
    feed.stop();

    let rx = peer.voice();
    assert_whole_picture(&rx, &pcm, 160, 50);
    for (i, w) in rx[49..66].windows(2).enumerate() {
        assert_eq!(
            w[1].ts.wrapping_sub(w[0].ts),
            20,
            "voice after the picture stepped between frames {} and {}",
            49 + i,
            50 + i
        );
    }
}
