// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! An SSTV picture heard on an `AllStar` call keeps the packets it arrived
//! in. A caller on 127.0.0.1 plays the Robot 36 test recording into a
//! loopback node, as µ-law mini frames in real time, with two frames swapped
//! and one never sent. The output bus is pulled on a real clock with no
//! sound device, so the decoder hears what an operator would. Nothing here
//! leaves this machine and nothing is transmitted.
#![cfg(feature = "sstv")]

use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use std::sync::atomic::AtomicU64;

use astar_audio::pull_backend::{PullBackend, PullSink};
use astar_audio::stream::StreamHandle;
use astar_audio::{AudioError, DeviceInfo, InputSink, OutputSource, StreamConfig};
use astar_iax::{IncomingAuthPolicy, IncomingCallPolicy};
use astar_iax_core::Subclass;
use astar_iax_core::frame::{Frame, FullFrame, encode, is_media, parse_lenient};
use astar_iax_core::ie::Ies;
use astar_iax_core::subclass::{FrameType, IaxCommand, VoiceFormat};
use astar_record::pcap::read_pcap;
use astar_station::{
    AnswerPolicy, CallStatus, ConsoleSession, NodeConfig, OperatingMode, SstvConfig, SstvContext,
    Station, StationConfig,
};
use serde_json::Value;

const PEER_CALL: u16 = 13885;
const FRAME: usize = 160;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn wait_for(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(ms(20));
    }
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("astar-sstv-pkts-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn files(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == ext))
        .collect();
    out.sort();
    out
}

fn robot36() -> Vec<i16> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../astar-sstv/tests/fixtures/robot36-french-logo-8k.flac");
    let mut reader = claxon::FlacReader::open(path).expect("open flac");
    assert_eq!(reader.streaminfo().sample_rate, 8_000);
    reader
        .samples()
        .map(|s| i16::try_from(s.expect("sample")).expect("16-bit"))
        .collect()
}

fn enc(frame: &Frame<'_>) -> Vec<u8> {
    let mut b = Vec::new();
    encode(frame, &mut b).expect("encode");
    b
}

fn new_datagram() -> Vec<u8> {
    enc(&Frame::Full(Box::new(FullFrame {
        source_call: PEER_CALL,
        dest_call: 0,
        retransmission: false,
        timestamp: 0,
        oseqno: 0,
        iseqno: 0,
        frame_type: FrameType::Iax,
        subclass: Subclass::Iax(IaxCommand::New),
        ies: Ies {
            called_number: Some("s"),
            calling_number: Some("1001"),
            calling_name: Some("Test"),
            capability: Some(VoiceFormat::G711U.as_u32()),
            format: Some(VoiceFormat::G711U.as_u32()),
            version: Some(2),
            calltoken: Some(b""),
            ..Ies::empty()
        },
        payload: &[],
    })))
}

/// Acknowledge everything the node sends, without waiting.
fn pump_acks(sock: &UdpSocket, node: SocketAddr) {
    let mut buf = [0u8; 4096];
    loop {
        let n = match sock.recv_from(&mut buf) {
            Ok((n, _)) => n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => return,
            Err(_) => return,
        };
        let Ok(Frame::Full(f)) = parse_lenient(&buf[..n]) else {
            continue;
        };
        if !matches!(f.subclass, Subclass::Iax(IaxCommand::Ack)) {
            let ack = enc(&Frame::Full(Box::new(FullFrame {
                source_call: PEER_CALL,
                dest_call: f.source_call,
                retransmission: false,
                timestamp: f.timestamp,
                oseqno: f.iseqno,
                iseqno: f.oseqno.wrapping_add(1),
                frame_type: FrameType::Iax,
                subclass: Subclass::Iax(IaxCommand::Ack),
                ies: Ies::empty(),
                payload: &[],
            })));
            let _ = sock.send_to(&ack, node);
        }
    }
}

/// No microphone, and an output bus pulled on a real clock.
struct PulledOutput {
    input: astar_audio::NullBackend,
    output: PullBackend,
}

impl astar_audio::AudioBackend for PulledOutput {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        let mut all = self.input.devices()?;
        all.extend(self.output.devices()?);
        Ok(all)
    }
    fn default_input(&self) -> Option<DeviceInfo> {
        self.input.default_input()
    }
    fn default_output(&self) -> Option<DeviceInfo> {
        self.output.default_output()
    }
    fn open_input(
        &self,
        device: &DeviceInfo,
        config: StreamConfig,
        sink: Box<dyn InputSink>,
        overruns: Arc<AtomicU64>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        self.input.open_input(device, config, sink, overruns)
    }
    fn open_output(
        &self,
        device: &DeviceInfo,
        config: StreamConfig,
        source: Box<dyn OutputSource>,
    ) -> Result<Box<dyn StreamHandle>, AudioError> {
        self.output.open_output(device, config, source)
    }
}

/// A loopback node whose output bus is pulled on a real clock.
fn node_station() -> Station {
    let cfg = StationConfig {
        mode: OperatingMode::Node,
        node: Some(NodeConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            policy: IncomingCallPolicy {
                auth: IncomingAuthPolicy::Off,
                ..IncomingCallPolicy::default()
            },
            answer: AnswerPolicy::Auto,
            register: None,
            max_calls: 4,
            allowlist: None,
        }),
        ..StationConfig::default()
    };
    let sink: PullSink = Arc::new(|_| {});
    Station::with_shared_session(
        cfg,
        Arc::new(Mutex::new(ConsoleSession::new())),
        Box::new(move || {
            Box::new(PulledOutput {
                input: astar_audio::NullBackend::new(),
                output: PullBackend::new(Arc::clone(&sink)),
            })
        }),
    )
}

struct Caller {
    sock: UdpSocket,
    node: SocketAddr,
}

fn answered_caller(station: &Station) -> Caller {
    let node = station.node_bind_addr().expect("node listener bound");
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(ms(20))).unwrap();
    sock.send_to(&new_datagram(), node).unwrap();
    assert!(
        wait_for(5, || {
            pump_acks(&sock, node);
            station.snapshot().status == CallStatus::Answered
        }),
        "the loopback call must be answered"
    );
    sock.set_nonblocking(true).unwrap();
    Caller { sock, node }
}

impl Caller {
    /// Send `audio` as µ-law mini frames, one every 20 ms on a deadline
    /// clock. Frames `swap` and `swap + 1` go the wrong way round; frame
    /// `lost` (if any) is never sent.
    fn play(&self, audio: &[i16], swap: Option<usize>, lost: Option<usize>) {
        let frames: Vec<Vec<u8>> = audio
            .chunks(FRAME)
            .enumerate()
            .map(|(i, pcm)| {
                let stamp = u16::try_from((40 + i * 20) % 65_536).unwrap();
                let mut b = PEER_CALL.to_be_bytes().to_vec();
                b.extend_from_slice(&stamp.to_be_bytes());
                b.extend(pcm.iter().map(|&x| astar_codec::g711::ulaw_encode(x)));
                b.resize(4 + FRAME, 0xff);
                b
            })
            .collect();
        let mut order: Vec<usize> = (0..frames.len()).collect();
        if let Some(k) = swap {
            order.swap(k, k + 1);
        }
        let start = Instant::now();
        for (slot, &i) in order.iter().enumerate() {
            let due = start + ms(20) * u32::try_from(slot).unwrap();
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
            if Some(i) != lost {
                self.sock.send_to(&frames[i], self.node).unwrap();
            }
            pump_acks(&self.sock, self.node);
        }
    }
}

fn sstv_on(station: &Station, dir: &Path) {
    station
        .sstv_configure(Some(SstvConfig {
            dir: dir.to_path_buf(),
            utc_offset_secs: 0,
        }))
        .unwrap();
    station.sstv_set_context(SstvContext {
        network: "AllStar".to_owned(),
        target: "1999".to_owned(),
        talker: Some("N0CALL".to_owned()),
    });
}

#[test]
fn an_allstar_picture_keeps_its_packets_beside_the_png() {
    let dir = scratch("kept");
    let station = node_station();
    sstv_on(&station, &dir);
    let caller = answered_caller(&station);

    let mut audio = vec![0i16; 8_000];
    audio.extend(robot36());
    audio.extend(std::iter::repeat_n(0, 3 * 8_000));
    // Swapped and lost frames both inside the picture.
    let swap = 8_000 / FRAME + 600;
    let lost = 8_000 / FRAME + 1_100;
    caller.play(&audio, Some(swap), Some(lost));

    assert!(
        wait_for(20, || station.sstv_state().last.is_some_and(|l| !l.lost)),
        "the picture never ended: {:?}",
        station.sstv_state()
    );
    let state = station.sstv_state();
    let newest = state.last.clone().unwrap();
    assert_eq!(newest.mode, "Robot 36");
    assert!(state.error.is_none(), "{:?}", state.error);
    let png = PathBuf::from(newest.file.expect("saved"));
    let pcap = png.with_extension("pcap");
    assert!(pcap.exists(), "the packets beside {}", png.display());
    assert_eq!(files(&dir, "png").len(), 1);
    assert_eq!(files(&dir, "pcap"), vec![pcap.clone()]);

    let packets = read_pcap(&std::fs::read(&pcap).unwrap()).expect("a whole pcap");
    assert!(packets.iter().all(|p| is_media(&p.payload)), "voice only");
    // The picture is about 36 s of 20 ms frames, and the window reaches
    // a few seconds before its header.
    assert!(packets.len() > 1_700, "{} packets", packets.len());
    assert!(
        packets.windows(2).all(|w| w[0].ts_ns <= w[1].ts_ns),
        "in arrival order"
    );

    let lines: Vec<Value> = std::fs::read_to_string(dir.join("pictures.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert_eq!(line["codec"], "ulaw");
    assert_eq!(line["packets"], pcap.file_name().unwrap().to_str().unwrap());
    let source = line["source"].as_str().expect("a source");
    assert!(source == "live" || source == "packets", "{source}");
    eprintln!("kept the {source} decode from {} packets", packets.len());

    station.disconnect();
    station.sstv_configure(None).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// SSTV on, then off, then the same picture the test above keeps: with
/// the decoder and its packet tap gone, nothing is written. Were either
/// left in place, the picture and its packets would land in `dir`.
#[test]
fn with_sstv_off_no_packets_are_kept() {
    let dir = scratch("off");
    let station = node_station();
    sstv_on(&station, &dir);
    station.sstv_configure(None).unwrap();
    let caller = answered_caller(&station);
    let mut audio = vec![0i16; 8_000];
    audio.extend(robot36());
    audio.extend(std::iter::repeat_n(0, 3 * 8_000));
    caller.play(&audio, None, None);
    // As long as the test above gives its picture to land.
    let landed = wait_for(20, || {
        station.sstv_state().last.is_some() || !files(&dir, "png").is_empty()
    });
    station.disconnect();
    assert!(!landed, "nothing decoded: {:?}", station.sstv_state());
    assert_eq!(files(&dir, "pcap"), Vec::<PathBuf>::new());
    assert_eq!(files(&dir, "png"), Vec::<PathBuf>::new());
    let _ = std::fs::remove_dir_all(&dir);
}
