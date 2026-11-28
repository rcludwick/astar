// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The C interface for sending a picture (feature `sstv-tx`), on a loopback
//! call.
//!
//! Each test's station dials its own fake IAX2 peer bound to 127.0.0.1. The
//! peer answers a NEW with ACCEPT (µ-law), ACKs every full frame, and logs,
//! in arrival order, every voice frame and every radio key and unkey the
//! station sends. The station has no sound device (`NullBackend`), so any
//! voice the peer hears came from the picture path. Nothing here leaves this
//! machine. Only `prepare_send_progress_end` sends a whole picture (Robot 36,
//! about 41 s); the others stop early or are refused.

use std::ffi::{c_char, c_int};
use std::net::{SocketAddr, UdpSocket};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use astar_codec::g711::ulaw_decode;
use astar_console::{ConsoleConfig, ConsoleSession};
use astar_iax_core::VoiceFormat;
use astar_iax_core::frame::{Frame, FullFrame, Subclass, encode, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{ControlSubclass, FrameType, IaxCommand};
use astar_station::{CodecPolicy, PICTURE_LEAD, Station, StationConfig, sstv_tx_modes};
use astar_sys::*;
use serde_json::Value;

const RATE: u32 = 8_000;
const FRAME: usize = 160;
const ROBOT_36: u32 = 0;
const SCOTTIE_2: u32 = 2;

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
// The fake peer (the same one as astar-station's tests/sstv_tx.rs).
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum Heard {
    Key,
    Unkey,
    Voice(Vec<u8>),
}

#[derive(Clone, Default)]
struct PeerCells {
    log: Arc<Mutex<Vec<Heard>>>,
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
    while !cells.stop.load(Ordering::SeqCst) {
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
// The station, behind the C handle.
// ---------------------------------------------------------------------------

/// A C handle on a station with no sound device, answered on a loopback call
/// to a fresh peer. Sending pictures is off, as on any new station.
struct OnACall {
    st: *mut IaxStation,
    peer: Peer,
}

impl Drop for OnACall {
    fn drop(&mut self) {
        unsafe { iax_station_free(self.st) };
    }
}

fn on_a_call() -> OnACall {
    on_a_call_with(|_| {})
}

/// [`on_a_call`], with `before` run on the station before it is wrapped.
fn on_a_call_with(before: impl FnOnce(&Station)) -> OnACall {
    let peer = Peer::start();
    let session = Arc::new(Mutex::new(ConsoleSession::new()));
    let station = Station::with_shared_session(
        StationConfig {
            codec_policy: CodecPolicy::UlawOnly,
            ..StationConfig::default()
        },
        Arc::clone(&session),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    );
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
                name: "picture-ffi-test".into(),
                input_device: None,
                output_device: None,
                codec_policy: CodecPolicy::UlawOnly,
            },
        )
        .expect("the loopback dial starts");
    before(&station);
    let st = iax_station_from_station_for_test(station);
    let call = OnACall { st, peer };
    assert!(
        wait_until(Duration::from_secs(6), || matches!(
            snapshot(call.st).status,
            IaxStatus::Answered
        )),
        "the loopback call was never answered"
    );
    call
}

/// A C handle on a station with no call at all.
fn idle_station() -> *mut IaxStation {
    iax_station_from_station_for_test(Station::with_backend_factory(
        StationConfig::default(),
        Box::new(|| Box::new(astar_audio::NullBackend::new())),
    ))
}

fn snapshot(st: *mut IaxStation) -> IaxState {
    let mut out = std::mem::MaybeUninit::<IaxState>::uninit();
    assert_eq!(
        unsafe { iax_station_snapshot(st, out.as_mut_ptr()) },
        IAX_OK
    );
    unsafe { out.assume_init() }
}

/// The peer never heard a key. Waits a short settle window first, so a key
/// frame still in flight when the refusal returned would be counted.
fn nothing_keyed_at(peer: &Peer) {
    thread::sleep(ms(150));
    assert_eq!(peer.keys(), 0, "nothing keyed at the peer");
    assert_eq!(peer.voice_frames(), 0, "no voice reached the peer");
}

/// Nothing held, nothing latched, the console unkeyed.
fn never_keyed(st: *mut IaxStation) {
    let s = snapshot(st);
    assert_eq!((s.ptt_sources, s.ptt_latched), (0, 0));
    assert!(!s.ptt, "the console is not keyed");
}

fn allowed(st: *mut IaxStation) -> bool {
    let mut out = false;
    assert_eq!(
        unsafe { iax_station_picture_allowed(st, &raw mut out) },
        IAX_OK
    );
    out
}

fn allow(st: *mut IaxStation) {
    assert_eq!(unsafe { iax_station_picture_set_allowed(st, true) }, IAX_OK);
}

/// Read a JSON document through the sizing-query-then-fill convention. A
/// sending picture's document can grow between the two calls (`9.9` becomes
/// `10.0`), so size again whenever the fill says the buffer was short.
fn read_json(
    st: *mut IaxStation,
    f: unsafe extern "C" fn(*mut IaxStation, *mut c_char, usize) -> c_int,
) -> Value {
    let mut needed = unsafe { f(st, ptr::null_mut(), 0) };
    let buf = loop {
        assert!(needed > 0, "sizing query returned {needed}");
        let mut buf = vec![0 as c_char; usize::try_from(needed).unwrap() + 1];
        let got = unsafe { f(st, buf.as_mut_ptr(), buf.len()) };
        if got <= needed {
            break buf;
        }
        needed = got;
    };
    let text = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
        .to_str()
        .unwrap()
        .to_owned();
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

fn state(st: *mut IaxStation) -> Value {
    read_json(st, iax_station_picture_state_json)
}

fn modes(st: *mut IaxStation) -> Value {
    read_json(st, iax_station_picture_modes_json)
}

/// RGBA, `cols` × `rows`, each pixel `color(x, y)`, opaque.
fn rgba_from(cols: u32, rows: u32, color: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
    let mut out = Vec::with_capacity((cols * rows * 4) as usize);
    for y in 0..rows {
        for x in 0..cols {
            let [r, g, b] = color(x, y);
            out.extend_from_slice(&[r, g, b, 255]);
        }
    }
    out
}

/// Seven colour bars over the top two thirds, a grey ramp below.
fn bars(x: u32, y: u32, w: u32, h: u32) -> [u8; 3] {
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

struct Prepared {
    rc: c_int,
    rgb: Vec<u8>,
    w: u32,
    h: u32,
}

#[allow(clippy::too_many_arguments)]
fn prepare_raw(
    st: *mut IaxStation,
    rgba: *const u8,
    rgba_len: usize,
    width: u32,
    height: u32,
    mode: u32,
    fit: u32,
    rgb_out: *mut u8,
    rgb_cap: usize,
) -> (c_int, u32, u32) {
    let (mut w, mut h) = (0u32, 0u32);
    let rc = unsafe {
        iax_station_picture_prepare(
            st, rgba, rgba_len, width, height, mode, fit, rgb_out, rgb_cap, &raw mut w, &raw mut h,
        )
    };
    (rc, w, h)
}

fn prepare(st: *mut IaxStation, rgba: &[u8], width: u32, height: u32, mode: u32) -> Prepared {
    let mut rgb = vec![0xABu8; 640 * 496 * 3];
    let (rc, w, h) = prepare_raw(
        st,
        rgba.as_ptr(),
        rgba.len(),
        width,
        height,
        mode,
        0,
        rgb.as_mut_ptr(),
        rgb.len(),
    );
    rgb.truncate((w * h * 3) as usize);
    Prepared { rc, rgb, w, h }
}

/// Prepare a mode-sized colour-bar picture for `mode`.
fn prepare_bars(st: *mut IaxStation, mode: u32, w: u32, h: u32) -> Prepared {
    let p = prepare(st, &rgba_from(w, h, |x, y| bars(x, y, w, h)), w, h, mode);
    assert_eq!(p.rc, IAX_OK);
    p
}

fn wait_state(st: *mut IaxStation, limit: Duration, want: &str) -> Value {
    let mut last = Value::Null;
    let hit = wait_until(limit, || {
        last = state(st);
        last["state"] == want
    });
    assert!(hit, "never reached {want}: {last}");
    last
}

// ---------------------------------------------------------------------------
// The tests.
// ---------------------------------------------------------------------------

#[test]
fn compiled_is_one() {
    assert_eq!(iax_station_picture_compiled(), 1);
}

#[test]
fn allowed_starts_off_and_round_trips() {
    let st = idle_station();
    assert!(!allowed(st), "a new station does not send pictures");
    assert_eq!(state(st)["allowed"], false);
    allow(st);
    assert!(allowed(st));
    assert_eq!(state(st)["allowed"], true);
    assert_eq!(
        unsafe { iax_station_picture_set_allowed(st, false) },
        IAX_OK
    );
    assert!(!allowed(st));
    assert_eq!(
        unsafe { iax_station_picture_allowed(st, ptr::null_mut()) },
        IAX_ERR_NULL
    );
    unsafe { iax_station_free(st) };
}

#[test]
fn modes_json_lists_the_eight() {
    let st = idle_station();
    let doc = modes(st);
    eprintln!("modes: {doc}");
    assert_eq!(doc["default"], 2, "{doc}");
    let lead = doc["lead_secs"].as_f64().expect("lead_secs");
    assert!(
        (lead - PICTURE_LEAD.as_secs_f64()).abs() < 1e-9,
        "lead_secs {lead}"
    );
    let list = doc["modes"].as_array().expect("a modes array");
    let want = [
        ("Robot 36", sstv_tx_modes::ROBOT_36, 320, 240),
        ("Scottie 1", sstv_tx_modes::SCOTTIE_1, 320, 256),
        ("Scottie 2", sstv_tx_modes::SCOTTIE_2, 320, 256),
        ("Scottie DX", sstv_tx_modes::SCOTTIE_DX, 320, 256),
        ("Martin 1", sstv_tx_modes::MARTIN_1, 320, 256),
        ("Martin 2", sstv_tx_modes::MARTIN_2, 320, 256),
        ("PD120", sstv_tx_modes::PD_120, 640, 496),
        ("PD180", sstv_tx_modes::PD_180, 640, 496),
    ];
    assert_eq!(list.len(), want.len());
    for (i, (entry, (name, mode, w, h))) in list.iter().zip(want).enumerate() {
        assert_eq!(entry["index"], i, "{entry}");
        assert_eq!(entry["name"], name, "{entry}");
        assert_eq!(entry["width"], w, "{entry}");
        assert_eq!(entry["height"], h, "{entry}");
        let secs = entry["seconds"].as_f64().expect("seconds");
        let real = Station::picture_duration(mode).as_secs_f64();
        assert!((secs - real).abs() <= 0.1, "{name}: {secs} vs {real}");
        assert!(secs > lead, "{name}: seconds include the lead-in");
    }
    unsafe { iax_station_free(st) };
}

#[test]
fn prepare_returns_the_preview() {
    const GREEN: [u8; 3] = [0, 255, 0];
    let st = idle_station();
    // Red, green, blue in 1:2:1 across 640 × 256.
    let band = |x: u32, _| {
        if x < 160 {
            [255, 0, 0]
        } else if x < 480 {
            GREEN
        } else {
            [0, 0, 255]
        }
    };
    let p = prepare(st, &rgba_from(640, 256, band), 640, 256, SCOTTIE_2);
    assert_eq!(p.rc, IAX_OK);
    assert_eq!((p.w, p.h), (320, 256));
    assert_eq!(p.rgb.len(), 320 * 256 * 3);
    for (i, px) in p.rgb.chunks(3).enumerate() {
        assert!(
            px.iter().zip(GREEN).all(|(a, b)| a.abs_diff(b) <= 1),
            "pixel {i} is {px:?}"
        );
    }
    unsafe { iax_station_free(st) };
}

#[test]
fn send_without_allowed_is_refused() {
    let call = on_a_call();
    prepare_bars(call.st, ROBOT_36, 320, 240);
    assert_eq!(
        unsafe { iax_station_picture_send(call.st) },
        IAX_ERR_PICTURE
    );
    let doc = state(call.st);
    eprintln!("refused: {doc}");
    assert_eq!(doc["refused"], "not_allowed", "{doc}");
    assert_eq!(doc["state"], "idle", "{doc}");
    assert_eq!(doc["allowed"], false, "{doc}");
    nothing_keyed_at(&call.peer);
    never_keyed(call.st);
}

#[test]
fn prepare_send_progress_end() {
    let call = on_a_call();
    allow(call.st);
    let prepared = prepare_bars(call.st, ROBOT_36, 320, 240);
    assert_eq!(unsafe { iax_station_picture_send(call.st) }, IAX_OK);

    let real = Station::picture_duration(sstv_tx_modes::ROBOT_36).as_secs_f64();
    let first = wait_state(call.st, Duration::from_secs(2), "sending");
    eprintln!("sending: {first}");
    assert_eq!(first["mode"], "Robot 36");
    assert_eq!(first["allowed"], true);
    let total = first["total_secs"].as_f64().unwrap();
    assert!((total - real).abs() <= 0.1, "total {total} vs {real}");
    let s1 = first["sent_secs"].as_f64().unwrap();
    thread::sleep(Duration::from_secs(2));
    let second = state(call.st);
    assert_eq!(second["state"], "sending", "{second}");
    let s2 = second["sent_secs"].as_f64().unwrap();
    assert!(s2 > s1, "sent_secs rises: {s1} then {s2}");

    let end = wait_state(call.st, Duration::from_secs(60), "ended");
    eprintln!("ended: {end}");
    assert_eq!(end["mode"], "Robot 36");
    assert_eq!(end["end"], "sent");
    assert!(end.get("refused").is_none(), "{end}");
    assert!(
        wait_until(Duration::from_secs(1), || call.peer.unkeys() == 1),
        "the peer saw {} unkeys",
        call.peer.unkeys()
    );
    // Still ended on a later read: it stays until the next prepare or send.
    assert_eq!(state(call.st)["state"], "ended");

    let mut decoder = astar_sstv::Decoder::new(RATE).expect("decoder");
    let mut audio = call.peer.audio();
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
    let sum: u64 = prepared
        .rgb
        .iter()
        .zip(&got.rgb)
        .map(|(a, b)| u64::from(a.abs_diff(*b)))
        .sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = sum as f64 / prepared.rgb.len() as f64;
    eprintln!("prepare_send_progress_end: mean per-channel error {mean:.2}");
    // The bound astar-station's robot36_round_trip uses.
    assert!(mean < 11.0, "mean per-channel error {mean:.2}");

    // A prepare clears the ended state.
    prepare_bars(call.st, ROBOT_36, 320, 240);
    assert_eq!(state(call.st)["state"], "idle");
}

#[test]
fn cancel_ends_it_cancelled() {
    let call = on_a_call();
    allow(call.st);
    prepare_bars(call.st, ROBOT_36, 320, 240);
    assert_eq!(unsafe { iax_station_picture_send(call.st) }, IAX_OK);
    wait_state(call.st, Duration::from_secs(2), "sending");
    thread::sleep(ms(500));
    assert_eq!(unsafe { iax_station_picture_cancel(call.st) }, IAX_OK);
    let end = wait_state(call.st, Duration::from_secs(2), "ended");
    assert_eq!(end["end"], "cancelled", "{end}");
    assert_eq!(end["mode"], "Robot 36");
    assert!(
        wait_until(Duration::from_secs(1), || call.peer.unkeys() == 1),
        "the peer saw {} unkeys",
        call.peer.unkeys()
    );
    assert_eq!(call.peer.keys(), 1);
    never_keyed(call.st);
    // A second cancel, with nothing on the air, does nothing.
    assert_eq!(unsafe { iax_station_picture_cancel(call.st) }, IAX_OK);
    assert_eq!(state(call.st)["end"], "cancelled");
}

#[test]
fn send_with_nothing_pending() {
    let call = on_a_call();
    allow(call.st);
    assert_eq!(
        unsafe { iax_station_picture_send(call.st) },
        IAX_ERR_INVALID
    );
    assert_eq!(state(call.st)["state"], "idle");
    nothing_keyed_at(&call.peer);
    never_keyed(call.st);
}

#[test]
fn a_second_send_needs_a_second_prepare() {
    let call = on_a_call();
    // A refused send uses the pending picture up too.
    prepare_bars(call.st, ROBOT_36, 320, 240);
    assert_eq!(
        unsafe { iax_station_picture_send(call.st) },
        IAX_ERR_PICTURE
    );
    allow(call.st);
    assert_eq!(
        unsafe { iax_station_picture_send(call.st) },
        IAX_ERR_INVALID
    );

    // So does one that goes out.
    prepare_bars(call.st, ROBOT_36, 320, 240);
    assert_eq!(unsafe { iax_station_picture_send(call.st) }, IAX_OK);
    wait_state(call.st, Duration::from_secs(2), "sending");
    assert_eq!(unsafe { iax_station_picture_cancel(call.st) }, IAX_OK);
    wait_state(call.st, Duration::from_secs(2), "ended");
    assert_eq!(
        unsafe { iax_station_picture_send(call.st) },
        IAX_ERR_INVALID
    );
    assert!(
        wait_until(Duration::from_secs(1), || call.peer.unkeys() == 1),
        "the peer saw {} unkeys",
        call.peer.unkeys()
    );
    assert_eq!(call.peer.keys(), 1, "only the one picture keyed");
    never_keyed(call.st);
}

/// One bad call to `iax_station_picture_prepare`: the one thing that differs
/// from a good Scottie 2 prepare of a 320 × 256 image.
#[derive(Clone, Copy, Debug)]
enum Bad {
    NullRgba,
    NullRgbOut,
    RgbCapOneShort,
    ModeIndex8,
    Fit2,
    RgbaLenWrong,
    NullW,
}

impl Bad {
    const ALL: [Self; 7] = [
        Self::NullRgba,
        Self::NullRgbOut,
        Self::RgbCapOneShort,
        Self::ModeIndex8,
        Self::Fit2,
        Self::RgbaLenWrong,
        Self::NullW,
    ];

    fn want(self) -> c_int {
        match self {
            Self::NullRgba | Self::NullRgbOut | Self::NullW => IAX_ERR_NULL,
            _ => IAX_ERR_INVALID,
        }
    }

    /// Make the call, with `out` as the caller's buffer.
    fn prepare(self, st: *mut IaxStation, rgba: &[u8], out: &mut [u8]) -> c_int {
        let need = 320 * 256 * 3;
        let (mut w, mut h) = (0u32, 0u32);
        let rc = unsafe {
            iax_station_picture_prepare(
                st,
                if matches!(self, Self::NullRgba) {
                    ptr::null()
                } else {
                    rgba.as_ptr()
                },
                rgba.len()
                    - if matches!(self, Self::RgbaLenWrong) {
                        4
                    } else {
                        0
                    },
                320,
                256,
                if matches!(self, Self::ModeIndex8) {
                    8
                } else {
                    SCOTTIE_2
                },
                if matches!(self, Self::Fit2) { 2 } else { 0 },
                if matches!(self, Self::NullRgbOut) {
                    ptr::null_mut()
                } else {
                    out.as_mut_ptr()
                },
                need - usize::from(matches!(self, Self::RgbCapOneShort)),
                if matches!(self, Self::NullW) {
                    ptr::null_mut()
                } else {
                    &raw mut w
                },
                &raw mut h,
            )
        };
        assert_eq!((w, h), (0, 0), "{self:?}: the size was written");
        rc
    }
}

#[test]
fn bad_arguments() {
    let call = on_a_call();
    allow(call.st);
    let st = call.st;
    let rgba = rgba_from(320, 256, |x, y| bars(x, y, 320, 256));
    let mut out = vec![0xABu8; 640 * 496 * 3];
    for bad in Bad::ALL {
        // A good prepare first, so the bad one must also clear it.
        prepare_bars(st, SCOTTIE_2, 320, 256);
        assert_eq!(bad.prepare(st, &rgba, &mut out), bad.want(), "{bad:?}");
        assert!(
            out.iter().all(|&b| b == 0xAB),
            "{bad:?}: rgb_out was written"
        );
        assert_eq!(
            unsafe { iax_station_picture_send(st) },
            IAX_ERR_INVALID,
            "{bad:?}: something was left pending"
        );
    }
    nothing_keyed_at(&call.peer);
    never_keyed(st);
}

#[test]
fn too_long_is_reported_with_both_times() {
    let call = on_a_call();
    allow(call.st);
    assert_eq!(
        unsafe { iax_station_set_tx_timeout_ms(call.st, 60_000) },
        IAX_OK
    );
    prepare_bars(call.st, SCOTTIE_2, 320, 256);
    assert_eq!(
        unsafe { iax_station_picture_send(call.st) },
        IAX_ERR_PICTURE
    );
    let doc = state(call.st);
    eprintln!("too long: {doc}");
    assert_eq!(doc["refused"], "too_long", "{doc}");
    let picture = doc["picture_secs"].as_f64().expect("picture_secs");
    let real = Station::picture_duration(sstv_tx_modes::SCOTTIE_2).as_secs_f64();
    assert!((picture - real).abs() <= 0.1, "{picture} vs {real}");
    assert_eq!(doc["timeout_secs"].as_f64(), Some(60.0), "{doc}");
    assert_eq!(
        snapshot(call.st).ptt_tx_timeout_ms,
        60_000,
        "the time-out is unchanged"
    );
    nothing_keyed_at(&call.peer);
    never_keyed(call.st);
}

#[test]
fn a_refusal_without_a_call_is_no_call() {
    let st = idle_station();
    allow(st);
    prepare_bars(st, ROBOT_36, 320, 240);
    assert_eq!(unsafe { iax_station_picture_send(st) }, IAX_ERR_PICTURE);
    assert_eq!(state(st)["refused"], "no_call");
    // The next prepare clears the refusal.
    prepare_bars(st, ROBOT_36, 320, 240);
    assert!(state(st).get("refused").is_none());
    never_keyed(st);
    unsafe { iax_station_free(st) };
}

/// What a held prepare returned: its code, size and buffer.
type Older = (c_int, u32, u32, Vec<u8>);

/// Start a prepare of a 320 × 256 picture for Scottie 2 on another thread,
/// held after it has taken its number and before it fits its image. Returns
/// the pause and the thread, which yields the call's code, size and buffer.
fn held_older_prepare(st: *mut IaxStation) -> (Arc<PreparePause>, thread::JoinHandle<Older>) {
    let pause = unsafe { iax_station_pause_next_prepare_for_test(st) };
    let st_addr = st as usize;
    let older = thread::spawn(move || {
        let rgba = rgba_from(320, 256, |x, y| bars(x, y, 320, 256));
        let mut out = vec![0xABu8; 320 * 256 * 3];
        let (rc, w, h) = prepare_raw(
            st_addr as *mut IaxStation,
            rgba.as_ptr(),
            rgba.len(),
            320,
            256,
            SCOTTIE_2,
            0,
            out.as_mut_ptr(),
            out.len(),
        );
        (rc, w, h, out)
    });
    pause.wait_until_held();
    (pause, older)
}

/// Two prepares at once: the newer one (started while the older one was
/// fitting) wins, the older is told it was overtaken and writes nothing, and
/// the picture that goes out is the newer one.
#[test]
fn two_prepares_at_once_keep_the_newer_one() {
    let call = on_a_call();
    allow(call.st);
    let (pause, older) = held_older_prepare(call.st);
    let small = rgba_from(320, 240, |x, y| bars(x, y, 320, 240));
    let newer = prepare(call.st, &small, 320, 240, ROBOT_36);
    pause.release();
    let (rc, w, h, out) = older.join().unwrap();
    assert_eq!(newer.rc, IAX_OK, "the newer prepare lands");
    assert_eq!((newer.w, newer.h), (320, 240));
    assert_eq!(rc, IAX_ERR_SUPERSEDED, "the overtaken prepare says so");
    assert_eq!((w, h), (0, 0), "and writes no size");
    assert!(out.iter().all(|&b| b == 0xAB), "and no preview");
    // What goes out is the newer, Robot 36 picture.
    assert_eq!(unsafe { iax_station_picture_send(call.st) }, IAX_OK);
    let doc = wait_state(call.st, Duration::from_secs(2), "sending");
    assert_eq!(doc["mode"], "Robot 36", "{doc}");
    assert_eq!(unsafe { iax_station_picture_cancel(call.st) }, IAX_OK);
    wait_state(call.st, Duration::from_secs(2), "ended");
}

/// A newer prepare that fails still overtakes the older one: both lose, and
/// nothing is pending.
#[test]
fn a_failed_newer_prepare_leaves_nothing_pending() {
    let st = idle_station();
    let (pause, older) = held_older_prepare(st);
    let small = rgba_from(320, 240, |x, y| bars(x, y, 320, 240));
    let newer = prepare(st, &small, 320, 240, 8);
    pause.release();
    let (rc, _, _, out) = older.join().unwrap();
    assert_eq!(newer.rc, IAX_ERR_INVALID);
    assert_eq!(rc, IAX_ERR_SUPERSEDED);
    assert!(out.iter().all(|&b| b == 0xAB));
    assert_eq!(unsafe { iax_station_picture_send(st) }, IAX_ERR_INVALID);
    unsafe { iax_station_free(st) };
}

fn last_error(st: *mut IaxStation) -> String {
    let mut buf = vec![0 as c_char; 512];
    unsafe { iax_station_last_error(st, buf.as_mut_ptr(), buf.len()) };
    unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
        .to_str()
        .unwrap()
        .to_owned()
}

/// `pending` says whether a prepared picture is waiting, and a discard drops
/// it, so a send then has nothing to send.
#[test]
fn discard_drops_the_pending_picture() {
    let st = idle_station();
    assert_eq!(state(st)["pending"], false);
    prepare_bars(st, ROBOT_36, 320, 240);
    assert_eq!(state(st)["pending"], true);
    assert_eq!(unsafe { iax_station_picture_discard(st) }, IAX_OK);
    assert_eq!(state(st)["pending"], false);
    allow(st);
    assert_eq!(unsafe { iax_station_picture_send(st) }, IAX_ERR_INVALID);
    // With nothing pending it does nothing.
    assert_eq!(unsafe { iax_station_picture_discard(st) }, IAX_OK);
    never_keyed(st);
    unsafe { iax_station_free(st) };
}

/// A discard leaves a picture that is going out alone.
#[test]
fn discard_does_not_touch_a_picture_going_out() {
    let call = on_a_call();
    allow(call.st);
    prepare_bars(call.st, ROBOT_36, 320, 240);
    assert_eq!(unsafe { iax_station_picture_send(call.st) }, IAX_OK);
    let doc = wait_state(call.st, Duration::from_secs(2), "sending");
    assert_eq!(doc["pending"], false, "the send took it: {doc}");
    prepare_bars(call.st, ROBOT_36, 320, 240);
    assert_eq!(state(call.st)["pending"], true);
    assert_eq!(unsafe { iax_station_picture_discard(call.st) }, IAX_OK);
    let doc = state(call.st);
    assert_eq!(doc["state"], "sending", "{doc}");
    assert_eq!(doc["pending"], false, "{doc}");
    thread::sleep(ms(200));
    assert!(snapshot(call.st).ptt, "still keyed for the picture");
    assert_eq!(unsafe { iax_station_picture_cancel(call.st) }, IAX_OK);
    wait_state(call.st, Duration::from_secs(2), "ended");
    assert_eq!(call.peer.keys(), 1);
}

/// A discard while a prepare is still fitting overtakes it: the prepare
/// keeps nothing, so the sheet the person closed cannot leave a picture
/// pending behind it.
#[test]
fn discard_overtakes_a_prepare_still_fitting() {
    let st = idle_station();
    let (pause, older) = held_older_prepare(st);
    assert_eq!(unsafe { iax_station_picture_discard(st) }, IAX_OK);
    pause.release();
    let (rc, w, h, out) = older.join().unwrap();
    assert_eq!(rc, IAX_ERR_SUPERSEDED);
    assert_eq!((w, h), (0, 0));
    assert!(out.iter().all(|&b| b == 0xAB));
    assert_eq!(state(st)["pending"], false);
    unsafe { iax_station_free(st) };
}

/// A send refused while a picture is going out: the state reads `sending`
/// and `refused: on_air` together, and the running picture carries on.
#[test]
fn a_refusal_while_sending_reads_sending_and_on_air() {
    let call = on_a_call();
    allow(call.st);
    prepare_bars(call.st, ROBOT_36, 320, 240);
    assert_eq!(unsafe { iax_station_picture_send(call.st) }, IAX_OK);
    wait_state(call.st, Duration::from_secs(2), "sending");
    prepare_bars(call.st, ROBOT_36, 320, 240);
    assert_eq!(
        unsafe { iax_station_picture_send(call.st) },
        IAX_ERR_PICTURE
    );
    let doc = state(call.st);
    eprintln!("on air: {doc}");
    assert_eq!(doc["state"], "sending", "{doc}");
    assert_eq!(doc["refused"], "on_air", "{doc}");
    assert_eq!(unsafe { iax_station_picture_cancel(call.st) }, IAX_OK);
    let end = wait_state(call.st, Duration::from_secs(2), "ended");
    assert_eq!(end["end"], "cancelled", "{end}");
    assert_eq!(call.peer.keys(), 1, "the refused send keyed nothing");
}

/// A key press during a picture stops it, and the PTT function says so with
/// `IAX_ERR_PTT_LATCHED`.
#[test]
fn a_key_press_stops_the_picture_with_ptt_latched() {
    let call = on_a_call();
    allow(call.st);
    prepare_bars(call.st, ROBOT_36, 320, 240);
    assert_eq!(unsafe { iax_station_picture_send(call.st) }, IAX_OK);
    wait_state(call.st, Duration::from_secs(2), "sending");
    assert_eq!(
        unsafe { iax_station_ptt_source(call.st, IAX_PTT_MANUAL, true) },
        IAX_ERR_PTT_LATCHED
    );
    let end = state(call.st);
    assert_eq!(end["state"], "ended", "{end}");
    assert_eq!(end["end"], "cancelled", "{end}");
    assert!(!snapshot(call.st).ptt, "unkeyed");
    assert!(
        wait_until(Duration::from_secs(1), || call.peer.unkeys() == 1),
        "the peer saw {} unkeys",
        call.peer.unkeys()
    );
    assert_eq!(call.peer.keys(), 1, "the press keyed nothing");
    assert_eq!(
        unsafe { iax_station_ptt_source(call.st, IAX_PTT_MANUAL, false) },
        IAX_OK
    );
}

/// An engine failure is refused as `engine`, and its text is in the last
/// error.
#[test]
fn an_engine_refusal_has_its_text_in_the_last_error() {
    let call = on_a_call_with(Station::fail_next_picture_pacer_for_test);
    allow(call.st);
    prepare_bars(call.st, ROBOT_36, 320, 240);
    assert_eq!(
        unsafe { iax_station_picture_send(call.st) },
        IAX_ERR_PICTURE
    );
    let doc = state(call.st);
    assert_eq!(doc["refused"], "engine", "{doc}");
    let text = last_error(call.st);
    eprintln!("engine: {text}");
    assert!(
        text.starts_with("the picture could not be started: "),
        "{text}"
    );
    thread::sleep(ms(200));
    never_keyed(call.st);
    assert_eq!(
        call.peer.keys(),
        call.peer.unkeys(),
        "any key was taken back"
    );
    assert_eq!(call.peer.voice_frames(), 0, "nothing was sent");
}

/// A picture on an M17 call is refused as `wrong_network`, naming M17. The
/// reflector is a loopback socket that answers the connect and nothing else.
#[test]
fn wrong_network_names_the_network() {
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
    let st = idle_station();
    allow(st);
    let host = std::ffi::CString::new(addr.ip().to_string()).unwrap();
    let callsign = std::ffi::CString::new("N0CALL").unwrap();
    assert_eq!(
        unsafe {
            iax_station_connect_m17(
                st,
                host.as_ptr(),
                addr.port(),
                c_char::from_ne_bytes(*b"A"),
                callsign.as_ptr(),
            )
        },
        IAX_OK
    );
    assert!(
        wait_until(Duration::from_secs(2), || matches!(
            snapshot(st).status,
            IaxStatus::Answered
        )),
        "the M17 reflector never answered"
    );
    prepare_bars(st, ROBOT_36, 320, 240);
    assert_eq!(unsafe { iax_station_picture_send(st) }, IAX_ERR_PICTURE);
    let doc = state(st);
    eprintln!("wrong network: {doc}");
    assert_eq!(doc["refused"], "wrong_network", "{doc}");
    assert_eq!(doc["network"], "M17", "{doc}");
    never_keyed(st);
    unsafe {
        iax_station_m17_disconnect(st);
        iax_station_free(st);
    }
    stop.store(true, Ordering::SeqCst);
    ack.join().unwrap();
}
