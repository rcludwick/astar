// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! `Station::send_picture` (feature `sstv-tx`) on an `EchoLink` call.
//!
//! The station logs in to a fake directory and calls a fake station, both on
//! 127.0.0.1 (`astar_echolink::fake`). The fake station answers, echoes
//! nothing, and keeps every audio packet it receives whole: the 12-byte
//! header (sequence number at bytes 2–3, the time-stamp field at 4–7) and
//! four 33-byte GSM frames. Every socket the link opens refuses a
//! non-loopback address in a test build, so nothing here can reach a real
//! `EchoLink` server, directory or relay, and no test logs in anywhere.
//!
//! The station's audio backend has one microphone the test can push audio
//! into, so it can show that microphone audio never reaches the wire while
//! a picture is going out.
//!
//! A picture starts with a fresh GSM encoder (the link's "Sending a
//! picture"), so the packets it should produce can be worked out here, byte
//! for byte, from the audio `encode_picture` returns. A dropped, repeated,
//! reordered or altered frame anywhere in the picture fails that
//! comparison.
//!
//! Robot 36 takes about 41 s in real time, its 4 s lead included; only two
//! tests send a whole one.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use astar_audio::{
    AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, OutputSource,
    StreamConfig, StreamHandle,
};
use astar_codec::gsm::{FRAME_BYTES, FRAME_SAMPLES, GsmDecoder, GsmEncoder};
use astar_console::ConsoleSession;
use astar_console::echolink::{
    CallLink, EchoLinkNetwork, LoginState, PeerAddr, StationEntry, Timings,
};
use astar_echolink::fake::{FakeDirectory, FakeLogin, FakeStation};
use astar_echolink::voice::{AUDIO_HEADER_LEN, AUDIO_PACKET_LEN, FRAMES_PER_PACKET};
use astar_station::{
    ForcedReason, PICTURE_LEAD, PictureSend, PttSource, SendEnd, SendPictureError, SstvPicture,
    SstvTxMode, Station, StationConfig, encode_picture, sstv_tx_modes,
};

const RATE: u32 = 8_000;
const NODE: u32 = 9999;

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
// Audio backend: keeps the microphone's sink so a test can talk into it.
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
    fn push(&self, samples: &[f32]) {
        let mut g = self.0.lock().unwrap();
        if let Some(mut sink) = g.remove("in:a") {
            drop(g);
            sink.write(samples, 0.5);
            self.0.lock().unwrap().insert("in:a".to_string(), sink);
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

/// Talk into the microphone (a 440 Hz tone, 20 ms at a time) until the
/// returned flag is cleared.
fn talk(mic: &MicSinks) -> (Arc<AtomicBool>, thread::JoinHandle<()>) {
    let on = Arc::new(AtomicBool::new(true));
    let handle = {
        let (mic, on) = (mic.clone(), Arc::clone(&on));
        thread::spawn(move || {
            let mut phase = 0.0f32;
            while on.load(Ordering::SeqCst) {
                let frame: Vec<f32> = (0..FRAME_SAMPLES)
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

// ---------------------------------------------------------------------------
// The call.
// ---------------------------------------------------------------------------

/// A station on a connected `EchoLink` call to a fake station.
struct Call {
    station: Station,
    fake: FakeStation,
    mic: MicSinks,
    _dir: FakeDirectory,
    _session: Arc<Mutex<ConsoleSession>>,
}

impl Call {
    /// Every audio packet the fake station has received, in arrival order.
    fn packets(&self) -> Vec<Packet> {
        self.fake
            .audio_received()
            .into_iter()
            .map(|(at, bytes)| Packet::parse(at, &bytes))
            .collect()
    }

    /// PTT as the `EchoLink` link actually applied it.
    fn link_keyed(&self) -> bool {
        self.station
            .echolink_state()
            .and_then(|s| s.call)
            .is_some_and(|c| c.ptt)
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        self.station.echolink_disconnect();
        self.station.echolink_logout();
    }
}

fn on_an_echolink_call() -> Call {
    let fake = FakeStation::start("*ECHOTEST*");
    fake.set_echo(false);
    let dir = FakeDirectory::start(FakeLogin::Ok, vec![fake.entry(NODE)]);
    let mic = MicSinks::default();
    let session = Arc::new(Mutex::new(ConsoleSession::new()));
    let station = {
        let mic = mic.clone();
        Station::with_shared_session(
            StationConfig::default(),
            Arc::clone(&session),
            Box::new(move || Box::new(TestBackend { mic: mic.clone() })),
        )
    };
    let fake_addr = fake.addr();
    station.set_echolink_network(EchoLinkNetwork {
        audio_bind: "127.0.0.1:0".parse().unwrap(),
        control_bind: "127.0.0.1:0".parse().unwrap(),
        peer_addr: Arc::new(move |e: &StationEntry| {
            if e.node == NODE {
                fake_addr
            } else {
                PeerAddr::fixed(e.ip)
            }
        }),
        match_by_ip: false,
        timings: Timings {
            keepalive: ms(100),
            connection_timeout: ms(5_000),
            give_up_after: 20,
            talk_hold: ms(100),
            ..Timings::default()
        },
    });
    station
        .echolink_login("KX7ABC", "s3cret-pw".to_owned(), &[dir.addr().to_string()])
        .expect("login starts");
    assert!(
        wait_until(Duration::from_secs(3), || {
            station.echolink_state().map(|s| s.login.state) == Some(LoginState::LoggedIn)
                && !station.echolink_stations().is_empty()
        }),
        "never logged in to the fake directory"
    );
    station.echolink_connect("9999").expect("connect");
    assert!(
        wait_until(Duration::from_secs(3), || {
            station
                .echolink_state()
                .and_then(|s| s.call)
                .map(|c| c.link)
                == Some(CallLink::Connected)
        }),
        "the fake station never answered"
    );
    Call {
        station,
        fake,
        mic,
        _dir: dir,
        _session: session,
    }
}

// ---------------------------------------------------------------------------
// Packets.
// ---------------------------------------------------------------------------

type Frames = [[u8; FRAME_BYTES]; FRAMES_PER_PACKET];

#[derive(Clone, Debug)]
struct Packet {
    at: Instant,
    seq: u16,
    /// The header's time-stamp field (bytes 4–7).
    ts: u32,
    frames: Frames,
}

impl Packet {
    fn parse(at: Instant, bytes: &[u8]) -> Self {
        assert_eq!(
            bytes.len(),
            AUDIO_PACKET_LEN,
            "an audio packet is 144 bytes"
        );
        let mut frames = [[0u8; FRAME_BYTES]; FRAMES_PER_PACKET];
        for (i, f) in frames.iter_mut().enumerate() {
            let at = AUDIO_HEADER_LEN + i * FRAME_BYTES;
            f.copy_from_slice(&bytes[at..at + FRAME_BYTES]);
        }
        Self {
            at,
            seq: u16::from_be_bytes([bytes[2], bytes[3]]),
            ts: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            frames,
        }
    }
}

/// The packets a picture whose audio is `pcm` must produce: 160-sample
/// frames from a fresh encoder, the last one zero-padded, four to a packet,
/// the last packet filled out with silent frames.
fn expected_packets(pcm: &[i16]) -> Vec<Frames> {
    let mut enc = GsmEncoder::new();
    let mut frames: Vec<[u8; FRAME_BYTES]> = pcm
        .chunks(FRAME_SAMPLES)
        .map(|chunk| {
            let mut f = [0i16; FRAME_SAMPLES];
            f[..chunk.len()].copy_from_slice(chunk);
            enc.encode(&f)
        })
        .collect();
    while !frames.len().is_multiple_of(FRAMES_PER_PACKET) {
        frames.push(enc.encode(&[0; FRAME_SAMPLES]));
    }
    frames
        .chunks(FRAMES_PER_PACKET)
        .map(|c| [c[0], c[1], c[2], c[3]])
        .collect()
}

/// Where the picture's first packet is among `got`. Normally 0; a few
/// microphone frames queued in the instant between the key and the picture
/// would go out first, in a packet of their own.
fn picture_start(got: &[Packet], want: &[Frames]) -> usize {
    got.iter()
        .position(|p| p.frames == want[0])
        .expect("the picture's first packet arrived")
}

/// Every sequence number is the previous one plus one, and every step of
/// the time-stamp field is the same.
fn assert_unbroken(got: &[Packet]) {
    for (i, pair) in got.windows(2).enumerate() {
        assert_eq!(
            pair[1].seq,
            pair[0].seq.wrapping_add(1),
            "sequence number step between packets {i} and {}",
            i + 1
        );
    }
    // EchoLink's header carries no time-stamp (bytes 4–7 are zero by
    // protocol), so on this wire the check below cannot fail; it is here for
    // the day the field is filled in.
    if let [first, second, ..] = got {
        let step = second.ts.wrapping_sub(first.ts);
        for (i, pair) in got.windows(2).enumerate() {
            assert_eq!(
                pair[1].ts.wrapping_sub(pair[0].ts),
                step,
                "time-stamp step between packets {i} and {}",
                i + 1
            );
        }
    }
}

/// Packets `got[k]` equal `want[k]` exactly, for every `k` in `got`.
fn assert_picture_prefix(got: &[Packet], want: &[Frames]) {
    assert!(
        got.len() <= want.len(),
        "{} packets for a {}-packet picture",
        got.len(),
        want.len()
    );
    for (k, (g, w)) in got.iter().zip(want).enumerate() {
        assert!(
            g.frames == *w,
            "packet {k} of {} is not the picture's packet {k}",
            want.len()
        );
    }
}

fn decode(got: &[Packet]) -> Vec<i16> {
    decode_frames(&got.iter().map(|p| p.frames).collect::<Vec<_>>())
}

fn decode_frames(packets: &[Frames]) -> Vec<i16> {
    let mut dec = GsmDecoder::new();
    packets
        .iter()
        .flatten()
        .flat_map(|f| dec.decode(f))
        .collect()
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

fn picture_for(mode: SstvTxMode) -> SstvPicture {
    let (w, h) = mode.resolution();
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

fn robot36() -> SstvTxMode {
    sstv_tx_modes::ROBOT_36
}

/// The packets a Robot 36 test picture must produce on the wire.
fn robot36_packets() -> Vec<Frames> {
    let pcm = encode_picture(&picture_for(robot36()), robot36(), RATE).expect("encode");
    expected_packets(&pcm)
}

fn wait_ended(send: &PictureSend, limit: Duration) -> Option<SendEnd> {
    wait_until(limit, || send.ended().is_some());
    send.ended()
}

/// Nothing held, nothing latched, and the link not keyed.
fn let_go(call: &Call) {
    assert!(
        wait_until(Duration::from_secs(1), || {
            let ptt = call.station.ptt_snapshot();
            (ptt.sources, ptt.latched) == (0, 0)
        }),
        "the picture still holds or latches its source: {:?}",
        call.station.ptt_snapshot()
    );
    assert!(
        wait_until(Duration::from_secs(1), || !call.link_keyed()),
        "the EchoLink link is still keyed"
    );
}

// ---------------------------------------------------------------------------
// Sending.
// ---------------------------------------------------------------------------

#[test]
fn robot36_round_trip_over_gsm() {
    let call = on_an_echolink_call();
    let send = call
        .station
        .send_picture(&picture_for(robot36()), robot36())
        .expect("send");
    assert_eq!(
        wait_ended(&send, Duration::from_secs(60)),
        Some(SendEnd::Sent)
    );
    let_go(&call);
    thread::sleep(ms(200));

    let wire = mean_error(decode(&call.packets()));
    // The same audio through GSM 06.10 with no network at all: what the
    // codec alone costs.
    let pcm = encode_picture(&picture_for(robot36()), robot36(), RATE).expect("encode");
    let offline = mean_error(decode_frames(&expected_packets(&pcm)));
    eprintln!(
        "robot36_round_trip_over_gsm: mean per-channel error {wire:.2} \
         (GSM alone, no network: {offline:.2})"
    );
    // GSM 06.10 is lossy, so this bound is its own, not the AllStarLink
    // test's (11). Measured at 33.1 on the first run, and the same at any
    // level from -26 to 0 dBFS, so it is the codec and not the level; the
    // bound is twice that.
    assert!(wire < 66.0, "mean per-channel error {wire:.2}");
    // The link adds nothing to what the codec costs.
    assert!(
        wire <= offline + 0.5,
        "the link made the picture worse: {wire:.2} against {offline:.2} for GSM alone"
    );
}

/// Decode `audio` as SSTV and return the mean per-channel error against the
/// test picture.
fn mean_error(mut audio: Vec<i16>) -> f64 {
    let mut decoder = astar_sstv::Decoder::new(RATE).expect("decoder");
    audio.extend(std::iter::repeat_n(0, RATE as usize));
    let mut finished = None;
    for chunk in audio.chunks(FRAME_SAMPLES) {
        for event in decoder.push(chunk) {
            if let astar_sstv::Event::Finished(p) = event {
                finished = Some(p);
            }
        }
    }
    let got = finished.expect("the audio decodes to a whole picture");
    assert_eq!((got.width, got.height), (320, 240));
    let want = picture_for(robot36()).rgb;
    let total: u64 = want
        .iter()
        .zip(&got.rgb)
        .map(|(a, b)| u64::from(a.abs_diff(*b)))
        .sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = total as f64 / want.len() as f64;
    mean
}

#[test]
fn sequence_numbers_are_unbroken() {
    let call = on_an_echolink_call();
    let want = robot36_packets();
    let send = call
        .station
        .send_picture(&picture_for(robot36()), robot36())
        .expect("send");
    assert_eq!(
        wait_ended(&send, Duration::from_secs(60)),
        Some(SendEnd::Sent)
    );
    let_go(&call);
    thread::sleep(ms(200));

    let got = call.packets();
    let start = picture_start(&got, &want);
    let picture = &got[start..];
    assert_eq!(
        picture.len(),
        want.len(),
        "every packet of the picture arrived, and nothing after it"
    );
    assert_picture_prefix(picture, &want);
    assert_unbroken(&got);
}

#[test]
fn a_stalled_pacer_does_not_step_them() {
    let call = on_an_echolink_call();
    let want = robot36_packets();
    let send = call
        .station
        .send_picture(&picture_for(robot36()), robot36())
        .expect("send");
    // One second past the lead, so the stalled frames are picture, not
    // silence: a dropped or repeated silent frame would look like any other.
    thread::sleep(PICTURE_LEAD + ms(1_000));
    send.stall_pacer_for_test(ms(300));
    thread::sleep(ms(2_000));
    // Everything that has arrived by now is whole picture packets; the one
    // in flight at the cancel may be cut short and padded.
    let before_cancel = call.packets().len();
    send.cancel();
    assert_eq!(send.ended(), Some(SendEnd::Cancelled));
    let_go(&call);
    thread::sleep(ms(200));

    let got = call.packets();
    let start = picture_start(&got, &want);
    let whole = &got[start..before_cancel.saturating_sub(1)];
    assert!(
        whole.len() > 20,
        "only {} packets before the cancel",
        whole.len()
    );
    assert_picture_prefix(whole, &want);
    assert_unbroken(&got);
    // The stall shows on the wire as one late packet, not as a lost one.
    let longest = got
        .windows(2)
        .map(|w| w[1].at.duration_since(w[0].at))
        .max()
        .unwrap();
    assert!(
        longest >= ms(200),
        "the stall never showed: longest gap {longest:?}"
    );
}

#[test]
fn mic_audio_is_not_sent_during_a_picture() {
    let call = on_an_echolink_call();
    let want = robot36_packets();
    let (talking, talker) = talk(&call.mic);
    let send = call
        .station
        .send_picture(&picture_for(robot36()), robot36())
        .expect("send");
    // Into the header, past the lead, so the mic has picture to compete with.
    thread::sleep(PICTURE_LEAD + ms(3_000));
    let before_cancel = call.packets().len();
    send.cancel();
    talking.store(false, Ordering::SeqCst);
    talker.join().unwrap();
    let_go(&call);

    // Every packet of the picture is exactly the picture's: no microphone
    // frame is mixed in, slipped between them, or sent in their place.
    let got = call.packets();
    let start = picture_start(&got, &want);
    let whole = &got[start..before_cancel.saturating_sub(1)];
    assert!(
        whole.len() > 25,
        "only {} packets before the cancel",
        whole.len()
    );
    assert_picture_prefix(whole, &want);
}

#[test]
fn voice_resumes_without_a_step() {
    let call = on_an_echolink_call();
    let want = robot36_packets();
    let (talking, talker) = talk(&call.mic);
    let send = call
        .station
        .send_picture(&picture_for(robot36()), robot36())
        .expect("send");
    thread::sleep(ms(1_500));
    send.cancel();
    let_go(&call);
    let picture_packets = call.packets().len();

    // The operator talks after the picture.
    call.station
        .ptt_source(PttSource::Manual, true)
        .expect("key");
    assert!(
        wait_until(Duration::from_secs(2), || call.packets().len()
            >= picture_packets + 10),
        "no voice after the picture"
    );
    call.station.ptt_source(PttSource::Manual, false).unwrap();
    talking.store(false, Ordering::SeqCst);
    talker.join().unwrap();
    let_go(&call);

    let got = call.packets();
    let start = picture_start(&got, &want);
    assert_picture_prefix(&got[start..picture_packets.saturating_sub(1)], &want);
    // The voice's packets number on from the picture's last one.
    assert_unbroken(&got);
    let voice = decode(&got[picture_packets..]);
    let loud = voice.iter().filter(|s| s.unsigned_abs() > 2_000).count();
    assert!(
        loud > voice.len() / 4,
        "the packets after the picture are not the voice"
    );
}

// ---------------------------------------------------------------------------
// Ending early.
// ---------------------------------------------------------------------------

#[test]
fn disconnect_cuts_the_picture() {
    let call = on_an_echolink_call();
    let send = call
        .station
        .send_picture(&picture_for(robot36()), robot36())
        .expect("send");
    assert!(wait_until(Duration::from_secs(2), || call.packets().len() >= 5));
    call.fake.send_bye();
    assert!(
        wait_until(Duration::from_secs(3), || {
            let _ = call.station.snapshot();
            send.ended().is_some()
        }),
        "the picture never ended"
    );
    assert_eq!(send.ended(), Some(SendEnd::CallEnded));
    let_go(&call);
    let after = call.packets().len();
    thread::sleep(ms(300));
    assert_eq!(call.packets().len(), after, "packets after the call ended");
}

#[test]
fn timeout_cuts_a_picture() {
    let call = on_an_echolink_call();
    let send = call
        .station
        .send_picture(&picture_for(robot36()), robot36())
        .expect("send");
    thread::sleep(ms(1_200));
    call.station
        .set_tx_timeout_for_test(Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        wait_ended(&send, Duration::from_secs(3)),
        Some(SendEnd::TimedOut)
    );
    let ptt = call.station.ptt_snapshot();
    assert_eq!(ptt.forced.reason, Some(ForcedReason::TxTimeout));
    assert_eq!(ptt.forced.sources, PttSource::Picture.bit());
    let_go(&call);
    assert!(!call.station.snapshot().ptt);
}

#[test]
fn refuses_when_longer_than_the_timeout() {
    let call = on_an_echolink_call();
    let minute = Duration::from_secs(60);
    call.station.set_tx_timeout_for_test(minute).unwrap();
    let scottie2 = sstv_tx_modes::SCOTTIE_2;
    assert!(Station::picture_duration(scottie2) > minute);
    assert_eq!(
        call.station
            .send_picture(&picture_for(scottie2), scottie2)
            .err(),
        Some(SendPictureError::TooLong {
            picture: Station::picture_duration(scottie2),
            timeout: minute,
        })
    );
    assert_eq!(
        call.station.tx_timeout(),
        minute,
        "the time-out is never changed"
    );
    thread::sleep(ms(200));
    let ptt = call.station.ptt_snapshot();
    assert_eq!((ptt.sources, ptt.latched), (0, 0));
    assert!(!call.link_keyed(), "the EchoLink call was never keyed");
    assert_eq!(call.fake.audio_packets(), 0, "nothing was sent");
}

#[test]
fn echolink_is_accepted() {
    let call = on_an_echolink_call();
    let send = call
        .station
        .send_picture(&picture_for(robot36()), robot36())
        .expect("an EchoLink call carries a picture");
    assert_eq!(
        call.station.ptt_snapshot().sources,
        PttSource::Picture.bit()
    );
    assert!(
        wait_until(Duration::from_secs(1), || call.link_keyed()),
        "the EchoLink link never keyed"
    );
    assert!(
        wait_until(Duration::from_secs(2), || call.packets().len() >= 5),
        "no picture packets arrived"
    );
    // A second picture is refused while the first runs.
    assert_eq!(
        call.station
            .send_picture(&picture_for(robot36()), robot36())
            .err(),
        Some(SendPictureError::OnAir)
    );
    send.cancel();
    assert_eq!(send.ended(), Some(SendEnd::Cancelled));
    let_go(&call);
}
