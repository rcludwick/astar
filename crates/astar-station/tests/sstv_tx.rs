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

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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
    encode_picture, sstv_tx_modes,
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

/// A station on an answered loopback call to a fresh peer.
fn on_a_call() -> (Station, Peer) {
    let peer = Peer::start();
    let (station, session) = station();
    session
        .lock()
        .unwrap()
        .connect(
            Box::new(astar_audio::NullBackend::new()),
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
