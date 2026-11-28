// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! `Station::send_picture` (feature `sstv-tx`) on a loopback call.
//!
//! The station dials a fake IAX2 peer bound to 127.0.0.1. The peer answers
//! a NEW with ACCEPT (µ-law), ACKs every full frame, and logs, in arrival
//! order, every voice frame and every radio key and unkey the station sends.
//! The station has no sound device: `NullBackend` gives it a microphone that
//! never produces a sample, so whatever voice reaches the peer came from the
//! picture path. Nothing here leaves this machine.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use astar_audio::{
    AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, OutputSource,
    StreamConfig, StreamHandle,
};
use astar_codec::g711::{ulaw_decode, ulaw_encode};
use astar_console::{ConsoleConfig, ConsoleSession};
use astar_iax::CodecPolicy;
use astar_iax_core::VoiceFormat;
use astar_iax_core::frame::{Frame, FullFrame, Subclass, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{ControlSubclass, FrameType, IaxCommand};
use astar_station::{
    CallStatus, ForcedReason, PICTURE_LEAD, PICTURE_PEAK_DBFS, PICTURE_TAIL, PictureSend,
    PttSource, SendEnd, SendPictureError, SstvPicture, SstvTxMode, Station, StationConfig,
    StationError, encode_picture, sstv_tx_modes,
};

const RATE: u32 = 8_000;
const FRAME: usize = 160;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn wait_until(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        thread::sleep(ms(5));
    }
    f()
}

// ---------------------------------------------------------------------------
// The fake peer.
// ---------------------------------------------------------------------------

/// What the peer heard, in arrival order.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Heard {
    Key,
    Unkey,
    Voice(Vec<u8>),
}

#[derive(Clone, Default)]
struct PeerCells {
    log: Arc<Mutex<Vec<Heard>>>,
    hangup_now: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

struct Peer {
    addr: SocketAddr,
    cells: PeerCells,
    join: Option<thread::JoinHandle<()>>,
}

impl Peer {
    fn start() -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(ms(5))).unwrap();
        let addr = sock.local_addr().unwrap();
        let cells = PeerCells::default();
        let join = {
            let cells = cells.clone();
            thread::spawn(move || run_peer(&sock, &cells))
        };
        Self {
            addr,
            cells,
            join: Some(join),
        }
    }

    fn log(&self) -> Vec<Heard> {
        self.cells.log.lock().unwrap().clone()
    }

    /// Hang the call up from the peer's side.
    fn hang_up(&self) {
        self.cells.hangup_now.store(true, Ordering::SeqCst);
    }

    fn keys(&self) -> usize {
        self.log().iter().filter(|h| **h == Heard::Key).count()
    }

    fn unkeys(&self) -> usize {
        self.log().iter().filter(|h| **h == Heard::Unkey).count()
    }

    fn voice_frames(&self) -> usize {
        self.log()
            .iter()
            .filter(|h| matches!(h, Heard::Voice(_)))
            .count()
    }

    /// Every voice sample the peer received, decoded, in arrival order.
    fn audio(&self) -> Vec<i16> {
        self.log()
            .iter()
            .filter_map(|h| match h {
                Heard::Voice(p) => Some(p.iter().map(|&b| ulaw_decode(b))),
                _ => None,
            })
            .flatten()
            .collect()
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
    subclass: Subclass,
    ies: Ies<'_>,
) -> Vec<u8> {
    let frame = Frame::Full(Box::new(FullFrame {
        source_call: SERVER_CALL,
        dest_call: client_call,
        retransmission: false,
        timestamp: ts,
        oseqno,
        iseqno,
        frame_type: FrameType::Iax,
        subclass,
        ies,
        payload: &[],
    }));
    let mut out = Vec::with_capacity(64);
    encode(&frame, &mut out).expect("test frame encodes");
    out
}

fn run_peer(sock: &UdpSocket, cells: &PeerCells) {
    let mut buf = [0u8; 4096];
    let mut answered = false;
    let mut oseq: u8 = 0;
    // The client's address and call number, and the next sequence number
    // expected from it, for a hangup of our own.
    let mut client: Option<(SocketAddr, u16)> = None;
    let mut next_from_client: u8 = 0;
    while !cells.stop.load(Ordering::SeqCst) {
        if cells.hangup_now.swap(false, Ordering::SeqCst)
            && let Some((src, client_call)) = client
        {
            let hangup = full(
                client_call,
                oseq,
                next_from_client,
                0,
                Subclass::Iax(IaxCommand::Hangup),
                Ies::empty(),
            );
            oseq = oseq.wrapping_add(1);
            let _ = sock.send_to(&hangup, src);
        }
        let Ok((n, src)) = sock.recv_from(&mut buf) else {
            continue;
        };
        let Ok(frame) = parse_lenient(&buf[..n]) else {
            continue;
        };
        match frame {
            Frame::Mini(m) => cells
                .log
                .lock()
                .unwrap()
                .push(Heard::Voice(m.payload.to_vec())),
            Frame::Full(ff) => {
                if matches!(ff.subclass, Subclass::Iax(IaxCommand::Ack)) {
                    continue;
                }
                let next_in = ff.oseqno.wrapping_add(1);
                next_from_client = next_in;
                let ack = full(
                    ff.source_call,
                    ff.iseqno,
                    next_in,
                    ff.timestamp,
                    Subclass::Iax(IaxCommand::Ack),
                    Ies::empty(),
                );
                let _ = sock.send_to(&ack, src);
                if ff.retransmission {
                    continue;
                }
                let heard = match (ff.frame_type, ff.subclass) {
                    (FrameType::Voice, _) => Some(Heard::Voice(ff.payload.to_vec())),
                    (_, Subclass::Control(ControlSubclass::RadioKey)) => Some(Heard::Key),
                    (_, Subclass::Control(ControlSubclass::RadioUnkey)) => Some(Heard::Unkey),
                    _ => None,
                };
                if let Some(h) = heard {
                    cells.log.lock().unwrap().push(h);
                }
                if matches!(ff.subclass, Subclass::Iax(IaxCommand::New)) && !answered {
                    answered = true;
                    client = Some((src, ff.source_call));
                    let accept = full(
                        ff.source_call,
                        oseq,
                        next_in,
                        0,
                        Subclass::Iax(IaxCommand::Accept),
                        Ies {
                            format: Some(VoiceFormat::G711U.as_u32()),
                            ..Ies::empty()
                        },
                    );
                    oseq = oseq.wrapping_add(1);
                    let _ = sock.send_to(&accept, src);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The station.
// ---------------------------------------------------------------------------

/// A station with no sound device, sharing its session with the test so the
/// test can dial without a registrar lookup or a portal token.
fn station() -> (Station, Arc<Mutex<ConsoleSession>>) {
    let session = Arc::new(Mutex::new(ConsoleSession::new()));
    let station = Station::with_shared_session(
        StationConfig {
            codec_policy: CodecPolicy::UlawOnly,
            ..StationConfig::default()
        },
        Arc::clone(&session),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
    (station, session)
}

// ---------------------------------------------------------------------------
// A microphone the test can talk into.
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

/// The microphone's sink, kept so a test can push audio into it.
#[derive(Clone, Default)]
struct MicSinks(Arc<Mutex<HashMap<String, Box<dyn InputSink>>>>);

impl MicSinks {
    fn push(&self, samples: &[f32]) {
        let mut g = self.0.lock().unwrap();
        if let Some(mut sink) = g.remove("in:a") {
            drop(g);
            sink.write(samples, 0.5);
            self.0.lock().unwrap().insert("in:a".to_string(), sink);
        }
    }
}

struct MicBackend {
    mic: MicSinks,
}

impl AudioBackend for MicBackend {
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

/// Talk into the microphone (a loud 440 Hz tone, 20 ms at a time) until the
/// returned flag is cleared.
fn talk(mic: &MicSinks) -> (Arc<AtomicBool>, thread::JoinHandle<()>) {
    let on = Arc::new(AtomicBool::new(true));
    let handle = {
        let (mic, on) = (mic.clone(), Arc::clone(&on));
        thread::spawn(move || {
            let mut phase = 0.0f32;
            while on.load(Ordering::SeqCst) {
                let frame: Vec<f32> = (0..FRAME)
                    .map(|_| {
                        let s = 0.5 * phase.sin();
                        phase += 2.0 * std::f32::consts::PI * 440.0 / 8000.0;
                        s
                    })
                    .collect();
                mic.push(&frame);
                thread::sleep(ms(20));
            }
        })
    };
    (on, handle)
}

/// A station on an answered loopback call to a fresh peer, allowed to send
/// pictures.
fn on_a_call() -> (Station, Peer) {
    let (station, peer) = on_a_call_not_allowed();
    station.set_picture_sending_allowed(true);
    (station, peer)
}

/// A station on an answered loopback call to a fresh peer, as a new station
/// is: not allowed to send pictures.
fn on_a_call_not_allowed() -> (Station, Peer) {
    dial(Box::new(astar_audio::NullBackend::new()))
}

/// A station allowed to send pictures, on a call whose microphone the test
/// can talk into.
fn on_a_call_with_a_mic() -> (Station, Peer, MicSinks) {
    let mic = MicSinks::default();
    let (station, peer) = dial(Box::new(MicBackend { mic: mic.clone() }));
    station.set_picture_sending_allowed(true);
    (station, peer, mic)
}

/// Dial a fresh peer with `backend` as the call's audio, and wait for the
/// answer.
fn dial(backend: Box<dyn AudioBackend>) -> (Station, Peer) {
    let peer = Peer::start();
    let (station, session) = station();
    session
        .lock()
        .unwrap()
        .connect(
            backend,
            peer.addr,
            ConsoleConfig {
                node: "1999".into(),
                calling_node: "1999".into(),
                secret: String::new(),
                name: "sstv-tx-test".into(),
                input_device: None,
                output_device: None,
                codec_policy: CodecPolicy::UlawOnly,
            },
        )
        .expect("the loopback dial starts");
    assert!(
        wait_until(Duration::from_secs(6), || station.snapshot().status
            == CallStatus::Answered),
        "the loopback call was never answered: {:?}",
        station.snapshot().status
    );
    (station, peer)
}

// ---------------------------------------------------------------------------
// The test picture.
// ---------------------------------------------------------------------------

/// Seven colour bars over the top two thirds, a grey ramp across the bottom
/// third, at (`x`, `y`) of a `w` × `h` picture.
fn pixel(x: u32, y: u32, w: u32, h: u32) -> [u8; 3] {
    const BARS: [[u8; 3]; 7] = [
        [191, 191, 191],
        [191, 191, 0],
        [0, 191, 191],
        [0, 191, 0],
        [191, 0, 191],
        [191, 0, 0],
        [0, 0, 191],
    ];
    if y < h * 2 / 3 {
        BARS[(x * 7 / w) as usize]
    } else {
        let grey = u8::try_from(x * 255 / (w - 1)).unwrap();
        [grey, grey, grey]
    }
}

fn picture(w: u32, h: u32) -> SstvPicture {
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            rgb.extend_from_slice(&pixel(x, y, w, h));
        }
    }
    SstvPicture {
        width: w,
        height: h,
        rgb,
    }
}

fn picture_for(mode: SstvTxMode) -> SstvPicture {
    let (w, h) = mode.resolution();
    picture(w, h)
}

fn robot36() -> SstvTxMode {
    sstv_tx_modes::ROBOT_36
}

fn wait_ended(send: &PictureSend, limit: Duration) -> Option<SendEnd> {
    wait_until(limit, || send.ended().is_some());
    send.ended()
}

/// Nothing held, nothing latched, and the console unkeyed.
fn never_keyed(station: &Station) {
    let ptt = station.ptt_snapshot();
    assert_eq!((ptt.sources, ptt.latched), (0, 0), "arbiter: {ptt:?}");
    assert!(!station.snapshot().ptt, "the console is not keyed");
}

// ---------------------------------------------------------------------------
// Sending.
// ---------------------------------------------------------------------------

#[test]
fn robot36_round_trip() {
    let (station, peer) = on_a_call();
    let send = station
        .send_picture(&picture_for(robot36()), robot36())
        .expect("send");
    assert_eq!(
        wait_ended(&send, Duration::from_secs(60)),
        Some(SendEnd::Sent)
    );
    // The source is let go of: nothing held, nothing latched, one unkey.
    assert!(
        wait_until(Duration::from_secs(1), || {
            let ptt = station.ptt_snapshot();
            (ptt.sources, ptt.latched) == (0, 0)
        }),
        "the picture still holds the key: {:?}",
        station.ptt_snapshot()
    );
    assert!(
        wait_until(Duration::from_secs(1), || peer.unkeys() == 1),
        "the peer saw {} unkeys",
        peer.unkeys()
    );

    let mut decoder = astar_sstv::Decoder::new(RATE).expect("decoder");
    let mut audio = peer.audio();
    audio.extend(std::iter::repeat_n(0, RATE as usize));
    let mut finished = None;
    for chunk in audio.chunks(FRAME) {
        for event in decoder.push(chunk) {
            if let astar_sstv::Event::Finished(p) = event {
                finished = Some(p);
            }
        }
    }
    let got = finished.expect("the peer's audio decodes to a whole picture");
    assert_eq!((got.width, got.height), (320, 240));
    let want = picture(320, 240).rgb;
    let total: u64 = want
        .iter()
        .zip(&got.rgb)
        .map(|(a, b)| u64::from(a.abs_diff(*b)))
        .sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = total as f64 / want.len() as f64;
    eprintln!("robot36_round_trip: mean per-channel error {mean:.2}");
    // Measured at 5.5 on the first run; the bound is twice that.
    assert!(mean < 11.0, "mean per-channel error {mean:.2}");
}

#[test]
fn skips_mic_processing() {
    let (station, peer) = on_a_call();
    station.set_noise_reduction(true);
    station.set_compression(true);
    station.set_tx_trim(10f32.powf(-12.0 / 20.0));
    let pic = picture_for(robot36());
    let send = station.send_picture(&pic, robot36()).expect("send");
    assert_eq!(
        wait_ended(&send, Duration::from_secs(60)),
        Some(SendEnd::Sent)
    );

    let mut want: Vec<i16> = encode_picture(&pic, robot36(), RATE)
        .expect("encode")
        .iter()
        .map(|&s| ulaw_decode(ulaw_encode(s)))
        .collect();
    want.resize(want.len().div_ceil(FRAME) * FRAME, 0);
    let got = peer.audio();
    assert!(
        got.len() >= want.len(),
        "{} of {} samples",
        got.len(),
        want.len()
    );
    let first_bad = got.iter().zip(&want).position(|(a, b)| a != b);
    assert_eq!(first_bad, None, "the picture reached the peer changed");
}

#[test]
fn peak_level() {
    let audio = encode_picture(&picture_for(robot36()), robot36(), RATE).expect("encode");
    let peak = audio.iter().map(|s| s.unsigned_abs()).max().unwrap();
    let dbfs = 20.0 * (f32::from(peak) / 32_767.0).log10();
    assert!(
        (dbfs - PICTURE_PEAK_DBFS).abs() <= 0.2,
        "peak {dbfs:.2} dBFS"
    );
    // The lead and the tail are silence, and the whole is as long as the
    // station says the picture lasts.
    let lead = usize::try_from(PICTURE_LEAD.as_millis()).unwrap() * RATE as usize / 1000;
    let tail = usize::try_from(PICTURE_TAIL.as_millis()).unwrap() * RATE as usize / 1000;
    assert!(audio[..lead].iter().all(|&s| s == 0));
    assert!(audio[audio.len() - tail..].iter().all(|&s| s == 0));
    let secs = Station::picture_duration(robot36()).as_secs_f64();
    #[allow(clippy::cast_precision_loss)]
    let len = audio.len() as f64 / f64::from(RATE);
    assert!(
        (len - secs).abs() < 0.02,
        "{len} s of audio, {secs} s claimed"
    );
}

// ---------------------------------------------------------------------------
// Refusals.
// ---------------------------------------------------------------------------

#[test]
fn refuses_without_a_call() {
    let (station, _session) = station();
    station.set_picture_sending_allowed(true);
    assert_eq!(
        station
            .send_picture(&picture_for(robot36()), robot36())
            .err(),
        Some(SendPictureError::NoCall)
    );
    never_keyed(&station);
}

#[test]
fn refuses_wrong_size() {
    let (station, peer) = on_a_call();
    assert_eq!(
        station.send_picture(&picture(100, 100), robot36()).err(),
        Some(SendPictureError::WrongSize {
            expected: (320, 240),
            got: (100, 100)
        })
    );
    // The right size with the wrong number of bytes is refused the same way.
    let mut short = picture_for(robot36());
    short.rgb.pop();
    assert!(matches!(
        station.send_picture(&short, robot36()),
        Err(SendPictureError::WrongSize { .. })
    ));
    thread::sleep(ms(100));
    never_keyed(&station);
    assert_eq!(peer.keys(), 0, "the peer never saw a key");
}

#[test]
fn refuses_zero_size() {
    let (station, peer) = on_a_call();
    let empty = SstvPicture {
        width: 0,
        height: 0,
        rgb: Vec::new(),
    };
    assert_eq!(
        station.send_picture(&empty, robot36()).err(),
        Some(SendPictureError::WrongSize {
            expected: (320, 240),
            got: (0, 0)
        })
    );
    thread::sleep(ms(100));
    never_keyed(&station);
    assert_eq!(peer.keys(), 0, "the peer never saw a key");
}

#[test]
fn refuses_when_longer_than_the_timeout() {
    let (station, peer) = on_a_call();
    let minute = Duration::from_secs(60);
    station.set_tx_timeout_for_test(minute).unwrap();
    let scottie2 = sstv_tx_modes::SCOTTIE_2;
    assert!(Station::picture_duration(scottie2) > minute);
    assert_eq!(
        station.send_picture(&picture_for(scottie2), scottie2).err(),
        Some(SendPictureError::TooLong {
            picture: Station::picture_duration(scottie2),
            timeout: minute,
        })
    );
    assert_eq!(
        station.tx_timeout(),
        minute,
        "the time-out is never changed"
    );
    thread::sleep(ms(100));
    never_keyed(&station);
    assert_eq!(peer.keys(), 0);
}

#[test]
fn refuses_while_ptt_is_held() {
    let (station, _peer) = on_a_call();
    station.ptt_source(PttSource::Manual, true).expect("key");
    assert_eq!(
        station
            .send_picture(&picture_for(robot36()), robot36())
            .err(),
        Some(SendPictureError::OnAir)
    );
    assert_eq!(station.ptt_snapshot().sources, PttSource::Manual.bit());
    station.ptt_source(PttSource::Manual, false).unwrap();
}

#[test]
fn refuses_a_second_picture() {
    let (station, _peer) = on_a_call();
    let pic = picture_for(robot36());
    let first = station.send_picture(&pic, robot36()).expect("first");
    assert_eq!(
        station.send_picture(&pic, robot36()).err(),
        Some(SendPictureError::OnAir)
    );
    assert_eq!(first.ended(), None, "the first picture carries on");
    first.cancel();
}

#[cfg(feature = "m17")]
#[test]
fn refuses_on_m17() {
    let reflector = UdpSocket::bind("127.0.0.1:0").unwrap();
    reflector.set_read_timeout(Some(ms(100))).unwrap();
    let addr = reflector.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let ack = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while !stop.load(Ordering::SeqCst) {
                if let Ok((n, src)) = reflector.recv_from(&mut buf)
                    && n == 11
                    && &buf[..4] == b"CONN"
                {
                    let _ = reflector.send_to(b"ACKN", src);
                }
            }
        })
    };
    let (station, _session) = station();
    station.set_picture_sending_allowed(true);
    station
        .m17_connect(&addr.ip().to_string(), addr.port(), 'A', "N0CALL")
        .expect("m17 connect");
    assert!(wait_until(Duration::from_secs(2), || station
        .snapshot()
        .status
        == CallStatus::Answered));
    assert_eq!(
        station
            .send_picture(&picture_for(robot36()), robot36())
            .err(),
        Some(SendPictureError::WrongNetwork("M17"))
    );
    never_keyed(&station);
    station.m17_disconnect();
    stop.store(true, Ordering::SeqCst);
    ack.join().unwrap();
}

// ---------------------------------------------------------------------------
// Ending early.
// ---------------------------------------------------------------------------

#[test]
fn timeout_cuts_a_picture() {
    let (station, peer) = on_a_call();
    let send = station
        .send_picture(&picture_for(robot36()), robot36())
        .expect("send");
    thread::sleep(ms(1_200));
    station
        .set_tx_timeout_for_test(Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        wait_ended(&send, Duration::from_secs(3)),
        Some(SendEnd::TimedOut)
    );
    assert!(wait_until(Duration::from_secs(2), || !station
        .snapshot()
        .ptt));
    let ptt = station.ptt_snapshot();
    assert_eq!(ptt.forced.reason, Some(ForcedReason::TxTimeout));
    assert_eq!(ptt.forced.sources, PttSource::Picture.bit());
    assert!(
        wait_until(Duration::from_secs(1), || station.ptt_snapshot().latched
            == 0),
        "the ended picture's latch is cleared"
    );
    assert!(wait_until(Duration::from_secs(1), || peer.unkeys() == 1));
}

#[test]
fn cancel_then_send_again() {
    let (station, peer) = on_a_call();
    let pic = picture_for(robot36());
    let started = Instant::now();
    let first = station.send_picture(&pic, robot36()).expect("first");
    // Cancel two seconds into the header's tones, past the lead's silence,
    // so a second picture that carried on from the first would not be
    // silent where its own lead should be.
    thread::sleep((PICTURE_LEAD + Duration::from_secs(2)).saturating_sub(started.elapsed()));
    first.cancel();
    assert_eq!(first.ended(), Some(SendEnd::Cancelled));
    let (sent, total) = first.progress();
    assert!(sent < total, "{sent:?} of {total:?}");

    let second = station
        .send_picture(&pic, robot36())
        .expect("a second picture right after a cancel");
    assert!(
        wait_until(Duration::from_secs(2), || {
            let log = peer.log();
            let second_key = log.iter().rposition(|h| *h == Heard::Key);
            second_key.is_some_and(|k| {
                log[k..]
                    .iter()
                    .filter(|h| matches!(h, Heard::Voice(_)))
                    .count()
                    >= 20
            })
        }),
        "the second picture's frames never arrived"
    );
    second.cancel();
    assert_eq!(second.ended(), Some(SendEnd::Cancelled));
    thread::sleep(ms(200));

    let log = peer.log();
    let keys: Vec<usize> = log
        .iter()
        .enumerate()
        .filter(|(_, h)| **h == Heard::Key)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(keys.len(), 2, "one key per picture: {:?}", keys.len());
    let unkeys_between = log[keys[0]..keys[1]]
        .iter()
        .filter(|h| **h == Heard::Unkey)
        .count();
    assert_eq!(unkeys_between, 1, "exactly one unkey for the cancel");
    let lead_frames = usize::try_from(PICTURE_LEAD.as_millis() / 20).unwrap();
    let second_voice: Vec<&Vec<u8>> = log[keys[1]..]
        .iter()
        .filter_map(|h| match h {
            Heard::Voice(p) => Some(p),
            _ => None,
        })
        .collect();
    for (n, frame) in second_voice.iter().take(lead_frames).enumerate() {
        assert!(
            frame.iter().all(|&b| ulaw_decode(b) == 0),
            "frame {n} of the second picture's lead is not silence"
        );
    }
}

#[test]
fn snapshot_shows_the_picture_source() {
    let (station, _peer) = on_a_call();
    let send = station
        .send_picture(&picture_for(robot36()), robot36())
        .expect("send");
    let ptt = station.ptt_snapshot();
    assert_ne!(ptt.sources & (1 << 5), 0, "bit 5 is set: {ptt:?}");
    assert_eq!(ptt.sources, PttSource::Picture.bit());
    assert!(station.snapshot().ptt);
    send.cancel();
    assert!(wait_until(Duration::from_secs(1), || station
        .ptt_snapshot()
        .sources
        == 0));
}

#[test]
fn call_end_cuts_a_picture_and_lets_go() {
    let (station, peer) = on_a_call();
    let send = station
        .send_picture(&picture_for(robot36()), robot36())
        .expect("send");
    thread::sleep(ms(1_000));
    peer.hang_up();
    // The station notices the call is gone through its snapshot poll.
    assert!(
        wait_until(Duration::from_secs(5), || {
            let _ = station.snapshot();
            send.ended().is_some()
        }),
        "the picture never ended"
    );
    assert_eq!(send.ended(), Some(SendEnd::CallEnded));
    assert!(
        wait_until(Duration::from_secs(1), || {
            let _ = station.snapshot();
            let ptt = station.ptt_snapshot();
            (ptt.sources, ptt.latched) == (0, 0)
        }),
        "the picture still holds or latches its source: {:?}",
        station.ptt_snapshot()
    );
}

// ---------------------------------------------------------------------------
// Allowed or not.
// ---------------------------------------------------------------------------

#[test]
fn a_new_station_does_not_allow_pictures() {
    let (station, peer) = on_a_call_not_allowed();
    assert!(!station.picture_sending_allowed());
    assert_eq!(
        station
            .send_picture(&picture_for(robot36()), robot36())
            .err(),
        Some(SendPictureError::NotAllowed)
    );
    thread::sleep(ms(200));
    never_keyed(&station);
    assert_eq!(peer.keys(), 0, "the peer never saw a key");
    assert_eq!(peer.voice_frames(), 0, "nothing was sent");
}

#[test]
fn not_allowed_is_checked_first() {
    let (station, _session) = station();
    // No call, and a picture of the wrong size: still `NotAllowed`.
    assert_eq!(
        station
            .send_picture(&picture_for(robot36()), robot36())
            .err(),
        Some(SendPictureError::NotAllowed)
    );
    assert_eq!(
        station.send_picture(&picture(100, 100), robot36()).err(),
        Some(SendPictureError::NotAllowed)
    );
    never_keyed(&station);
}

#[test]
fn snapshot_shows_allowed() {
    let (station, _session) = station();
    assert!(!station.ptt_snapshot().picture_allowed);
    station.set_picture_sending_allowed(true);
    assert!(station.picture_sending_allowed());
    assert!(station.ptt_snapshot().picture_allowed);
    station.set_picture_sending_allowed(false);
    assert!(!station.picture_sending_allowed());
    assert!(!station.ptt_snapshot().picture_allowed);
}

#[test]
fn turning_it_off_cancels_a_picture() {
    let (station, peer) = on_a_call();
    let send = station
        .send_picture(&picture_for(robot36()), robot36())
        .expect("send");
    assert!(
        wait_until(Duration::from_secs(2), || peer.voice_frames() >= 10),
        "the picture never started"
    );
    station.set_picture_sending_allowed(false);
    assert_eq!(send.ended(), Some(SendEnd::Cancelled));
    assert!(!station.snapshot().ptt, "unkeyed at once");
    never_keyed(&station);
    thread::sleep(ms(300));
    assert_eq!(peer.keys(), 1);
    assert_eq!(peer.unkeys(), 1, "exactly one unkey");
    assert_eq!(
        station
            .send_picture(&picture_for(robot36()), robot36())
            .err(),
        Some(SendPictureError::NotAllowed)
    );
    thread::sleep(ms(100));
    assert_eq!(peer.keys(), 1, "the refused picture keyed nothing");
}

// ---------------------------------------------------------------------------
// A key press during a picture.
// ---------------------------------------------------------------------------

/// A picture going out, with its first frames at the peer.
fn picture_going_out(station: &Station, peer: &Peer) -> PictureSend {
    let send = station
        .send_picture(&picture_for(robot36()), robot36())
        .expect("send");
    assert!(
        wait_until(Duration::from_secs(2), || peer.voice_frames() >= 10),
        "the picture never started"
    );
    send
}

/// After a press stopped the picture: it ended `Cancelled`, nothing holds
/// the key, `src` alone is latched, the station is unkeyed and stays so for
/// 300 ms, and no more audio reaches the peer.
fn stopped_and_quiet(station: &Station, peer: &Peer, send: &PictureSend, src: PttSource) {
    assert_eq!(send.ended(), Some(SendEnd::Cancelled));
    let ptt = station.ptt_snapshot();
    assert_eq!(ptt.sources, 0, "nothing holds the key: {ptt:?}");
    assert_eq!(ptt.latched, src.bit(), "the press is used up: {ptt:?}");
    assert!(!station.snapshot().ptt, "unkeyed");
    // At most one frame was already on its way.
    thread::sleep(ms(60));
    let frames = peer.voice_frames();
    thread::sleep(ms(300));
    assert!(!station.snapshot().ptt, "still unkeyed 300 ms later");
    assert_eq!(peer.voice_frames(), frames, "no audio after the press");
    assert_eq!(peer.keys(), 1, "the press keyed nothing");
    assert_eq!(peer.unkeys(), 1, "exactly one unkey");
}

#[test]
fn a_manual_press_stops_the_picture_and_does_not_key() {
    let (station, peer) = on_a_call();
    let send = picture_going_out(&station, &peer);
    assert!(matches!(
        station.ptt_source(PttSource::Manual, true),
        Err(StationError::PttLatched)
    ));
    stopped_and_quiet(&station, &peer, &send, PttSource::Manual);
    // Pressing again without letting go is still used up.
    assert!(matches!(
        station.ptt_source(PttSource::Manual, true),
        Err(StationError::PttLatched)
    ));
    assert!(!station.snapshot().ptt);
    // A fresh press after a release keys.
    station.ptt_source(PttSource::Manual, false).unwrap();
    station.ptt_source(PttSource::Manual, true).expect("keys");
    assert!(station.snapshot().ptt);
    assert!(wait_until(Duration::from_secs(1), || peer.keys() == 2));
    station.ptt_source(PttSource::Manual, false).unwrap();
}

#[test]
fn a_set_ptt_press_stops_the_picture_and_does_not_key() {
    let (station, peer) = on_a_call();
    let send = picture_going_out(&station, &peer);
    assert!(matches!(
        station.set_ptt(true),
        Err(StationError::PttLatched)
    ));
    stopped_and_quiet(&station, &peer, &send, PttSource::Legacy);
    // `set_ptt(false)` is Legacy's release; then `set_ptt(true)` keys.
    let _ = station.set_ptt(false);
    assert_eq!(station.ptt_snapshot().latched, 0);
    station.set_ptt(true).expect("keys");
    assert!(station.snapshot().ptt);
    assert!(wait_until(Duration::from_secs(1), || peer.keys() == 2));
    let _ = station.set_ptt(false);
}

#[test]
fn a_bluetooth_press_stops_the_picture_and_renewals_key_nothing() {
    let (station, peer) = on_a_call();
    let send = picture_going_out(&station, &peer);
    let ttl = ms(1_500);
    assert!(matches!(
        station.ptt_lease_press(PttSource::Bluetooth, ttl),
        Err(StationError::PttLatched)
    ));
    // Two renewals, as a held button's client sends them: no lease was
    // armed, so neither keys anything.
    for _ in 0..2 {
        assert!(matches!(
            station.ptt_lease_renew(PttSource::Bluetooth),
            Err(StationError::NotLeased)
        ));
        thread::sleep(ms(20));
    }
    stopped_and_quiet(&station, &peer, &send, PttSource::Bluetooth);
    assert_eq!(station.ptt_snapshot().forced.seq, 0, "nothing was forced");
    // Released, then pressed fresh with a lease: it keys.
    station.ptt_source(PttSource::Bluetooth, false).unwrap();
    station
        .ptt_lease_press(PttSource::Bluetooth, ttl)
        .expect("keys");
    assert!(station.snapshot().ptt);
    assert!(wait_until(Duration::from_secs(1), || peer.keys() == 2));
    station.ptt_source(PttSource::Bluetooth, false).unwrap();
}

#[test]
fn held_key_then_picture_then_press() {
    let (station, peer) = on_a_call();
    station.ptt_source(PttSource::Manual, true).expect("key");
    assert_eq!(
        station
            .send_picture(&picture_for(robot36()), robot36())
            .err(),
        Some(SendPictureError::OnAir)
    );
    station.ptt_source(PttSource::Manual, false).unwrap();
    assert!(wait_until(Duration::from_secs(1), || peer.unkeys() == 1));
    let send = station
        .send_picture(&picture_for(robot36()), robot36())
        .expect("send once the key is up");
    assert!(wait_until(Duration::from_secs(2), || peer.keys() == 2));
    assert!(matches!(
        station.ptt_source(PttSource::Manual, true),
        Err(StationError::PttLatched)
    ));
    assert_eq!(send.ended(), Some(SendEnd::Cancelled));
    assert!(!station.snapshot().ptt);
    assert_eq!(station.ptt_snapshot().latched, PttSource::Manual.bit());
    // Not latched forever: after a release Manual keys normally.
    station.ptt_source(PttSource::Manual, false).unwrap();
    station.ptt_source(PttSource::Manual, true).expect("keys");
    assert!(station.snapshot().ptt);
    station.ptt_source(PttSource::Manual, false).unwrap();
    let ptt = station.ptt_snapshot();
    assert_eq!((ptt.sources, ptt.latched), (0, 0), "{ptt:?}");
}

/// A source latched while its key is still down (here by the universal
/// stop) is not on air, so a picture can start. That source's next press
/// must still stop the picture, not be swallowed by its latch.
#[test]
fn a_latched_source_still_stops_a_picture() {
    let (station, peer) = on_a_call();
    station.ptt_source(PttSource::Manual, true).expect("key");
    station.ptt_stop_all();
    assert!(wait_until(Duration::from_secs(1), || peer.unkeys() == 1));
    assert_eq!(station.ptt_snapshot().latched, PttSource::Manual.bit());
    assert_eq!(station.ptt_snapshot().sources, 0);

    let send = picture_going_out(&station, &peer);
    assert_eq!(peer.keys(), 2, "the picture keyed");
    assert!(matches!(
        station.ptt_source(PttSource::Manual, true),
        Err(StationError::PttLatched)
    ));
    assert_eq!(send.ended(), Some(SendEnd::Cancelled));
    let ptt = station.ptt_snapshot();
    assert_eq!(ptt.sources, 0, "nothing holds the key: {ptt:?}");
    assert_eq!(ptt.latched, PttSource::Manual.bit(), "{ptt:?}");
    assert!(!station.snapshot().ptt, "unkeyed");
    thread::sleep(ms(60));
    let frames = peer.voice_frames();
    thread::sleep(ms(300));
    assert!(!station.snapshot().ptt, "still unkeyed 300 ms later");
    assert_eq!(peer.voice_frames(), frames, "no audio after the press");
    assert_eq!(peer.keys(), 2, "the press keyed nothing");
    assert_eq!(peer.unkeys(), 2, "exactly one unkey for the picture");

    // After a release, Manual keys normally.
    station.ptt_source(PttSource::Manual, false).unwrap();
    station.ptt_source(PttSource::Manual, true).expect("keys");
    assert!(station.snapshot().ptt);
    assert!(wait_until(Duration::from_secs(1), || peer.keys() == 3));
    station.ptt_source(PttSource::Manual, false).unwrap();
}

#[test]
fn vox_cannot_stop_a_picture() {
    let (station, peer) = on_a_call();
    let send = picture_going_out(&station, &peer);
    // VOX opening and closing during the picture changes nothing.
    station
        .ptt_source(PttSource::Vox, true)
        .expect("not an error");
    assert_eq!(station.ptt_snapshot().sources, PttSource::Picture.bit());
    assert_eq!(station.ptt_snapshot().latched, 0);
    station.ptt_source(PttSource::Vox, false).unwrap();
    station
        .ptt_source(PttSource::Vox, true)
        .expect("not an error");
    assert_eq!(send.ended(), None, "the picture carries on");
    assert_eq!(
        wait_ended(&send, Duration::from_secs(60)),
        Some(SendEnd::Sent)
    );
    assert!(
        wait_until(Duration::from_secs(1), || {
            let ptt = station.ptt_snapshot();
            (ptt.sources, ptt.latched) == (0, 0) && !station.snapshot().ptt
        }),
        "the stale VOX press keyed after the picture: {:?}",
        station.ptt_snapshot()
    );
    assert_eq!(peer.keys(), 1, "only the picture keyed");
    // The next VOX press keys as usual.
    station.ptt_source(PttSource::Vox, true).expect("keys");
    assert!(station.snapshot().ptt);
    assert_eq!(station.ptt_snapshot().sources, PttSource::Vox.bit());
    station.ptt_source(PttSource::Vox, false).unwrap();
}

#[test]
fn engine_failures_are_not_no_call() {
    let (station, peer) = on_a_call();
    station.fail_next_picture_pacer_for_test();
    assert!(matches!(
        station.send_picture(&picture_for(robot36()), robot36()),
        Err(SendPictureError::Engine(_))
    ));
    thread::sleep(ms(200));
    never_keyed(&station);
    assert_eq!(peer.keys(), peer.unkeys(), "any key was taken back");
    assert_eq!(peer.voice_frames(), 0, "nothing was sent");
    // The hook fires once; the next picture goes out.
    let send = station
        .send_picture(&picture_for(robot36()), robot36())
        .expect("send");
    send.cancel();
    assert_eq!(send.ended(), Some(SendEnd::Cancelled));
}

// ---------------------------------------------------------------------------
// The microphone stays off the air when a picture is stopped early.
// ---------------------------------------------------------------------------

/// Talk into the microphone throughout; start a picture; stop it with
/// `stop` while it is still in its silent lead; and check the peer heard
/// nothing but silence. The console's unkey is held back 300 ms by the test
/// hook, so if the picture stopped before the call unkeyed, the microphone
/// would have 300 ms on the air.
fn mic_stays_off_the_air(stop: impl FnOnce(&Station, &PictureSend)) {
    let (station, peer, mic) = on_a_call_with_a_mic();
    station.delay_picture_unkey_for_test(ms(300));
    let (talking, talker) = talk(&mic);
    let send = picture_going_out(&station, &peer);
    stop(&station, &send);
    assert_eq!(send.ended(), Some(SendEnd::Cancelled));
    thread::sleep(ms(500));
    talking.store(false, Ordering::SeqCst);
    talker.join().unwrap();
    assert!(!station.snapshot().ptt, "unkeyed");
    let audio = peer.audio();
    assert!(audio.len() >= 10 * FRAME, "the picture's lead went out");
    let loudest = audio.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    assert!(
        loudest < 100,
        "the microphone reached the peer: peak {loudest} in {} samples",
        audio.len()
    );
}

#[test]
fn a_manual_press_keeps_the_mic_off_the_air() {
    mic_stays_off_the_air(|station, _| {
        assert!(matches!(
            station.ptt_source(PttSource::Manual, true),
            Err(StationError::PttLatched)
        ));
    });
}

#[test]
fn turning_sending_off_keeps_the_mic_off_the_air() {
    mic_stays_off_the_air(|station, _| station.set_picture_sending_allowed(false));
}

#[test]
fn cancel_keeps_the_mic_off_the_air() {
    mic_stays_off_the_air(|_, send| send.cancel());
}

// ---------------------------------------------------------------------------
// The microphone stays off the air after a picture.
// ---------------------------------------------------------------------------

/// A picture that runs to its end: the call stays keyed after its last
/// frame until the picture is reaped (held open 400 ms here by the test
/// hook), then unkeys. A microphone talking throughout is never heard: not
/// during the picture, not in those 400 ms, and not in the 300 ms after.
#[test]
fn a_picture_that_ends_by_itself_keeps_the_mic_off_the_air() {
    let (station, peer, mic) = on_a_call_with_a_mic();
    station.delay_picture_reap_for_test(ms(400));
    let (talking, talker) = talk(&mic);
    let send = station
        .send_picture(&picture_for(robot36()), robot36())
        .expect("send");
    let frames = usize::try_from(send.progress().1.as_millis() / 20).unwrap();
    assert_eq!(
        wait_ended(&send, Duration::from_secs(60)),
        Some(SendEnd::Sent)
    );
    assert!(
        wait_until(Duration::from_secs(2), || peer.unkeys() == 1),
        "the call never unkeyed"
    );
    thread::sleep(ms(300));
    talking.store(false, Ordering::SeqCst);
    talker.join().unwrap();
    assert!(!station.snapshot().ptt, "unkeyed");
    assert_eq!(peer.keys(), 1);
    assert_eq!(peer.unkeys(), 1, "exactly one unkey");
    let audio = peer.audio();
    assert!(
        audio.len() >= frames * FRAME,
        "{} of {} picture samples",
        audio.len(),
        frames * FRAME
    );
    let after = &audio[frames * FRAME..];
    let loudest = after.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    assert!(
        loudest < 100,
        "the microphone reached the peer after the picture: peak {loudest} in {} samples",
        after.len()
    );
    assert_eq!(
        peer.voice_frames(),
        frames,
        "frames went out after the picture's last"
    );
}

/// Talk into the microphone throughout; start a picture; end it with `stop`
/// (a path that unkeys the call itself, not through the early stop); and
/// check the picture ended `Cancelled`, the peer heard only silence and saw
/// exactly one key and one unkey.
fn unkeyed_mic_stays_off_the_air(stop: impl FnOnce(&Station)) {
    let (station, peer, mic) = on_a_call_with_a_mic();
    let (talking, talker) = talk(&mic);
    let send = picture_going_out(&station, &peer);
    stop(&station);
    assert_eq!(
        wait_ended(&send, Duration::from_secs(2)),
        Some(SendEnd::Cancelled)
    );
    thread::sleep(ms(500));
    talking.store(false, Ordering::SeqCst);
    talker.join().unwrap();
    assert!(!station.snapshot().ptt, "unkeyed");
    let ptt = station.ptt_snapshot();
    assert_eq!(ptt.sources, 0, "nothing holds the key: {ptt:?}");
    let audio = peer.audio();
    assert!(audio.len() >= 10 * FRAME, "the picture's lead went out");
    let loudest = audio.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    assert!(
        loudest < 100,
        "the microphone reached the peer: peak {loudest} in {} samples",
        audio.len()
    );
    assert_eq!(peer.keys(), 1);
    assert_eq!(peer.unkeys(), 1, "exactly one unkey");
}

#[test]
fn set_ptt_off_during_a_picture_keeps_the_mic_off_the_air() {
    unkeyed_mic_stays_off_the_air(|station| {
        station.set_ptt(false).expect("unkey");
    });
}

#[test]
fn releasing_the_picture_source_keeps_the_mic_off_the_air() {
    unkeyed_mic_stays_off_the_air(|station| {
        station
            .ptt_source(PttSource::Picture, false)
            .expect("release");
    });
}

#[test]
fn stop_all_during_a_picture_keeps_the_mic_off_the_air() {
    unkeyed_mic_stays_off_the_air(Station::ptt_stop_all);
}
