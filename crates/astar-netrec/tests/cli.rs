// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The `astar-netrec` binary itself. Only commands that never touch the
//! network run here: `check`, `status`, a `now` that names no net, and a `run`
//! whose config has no nets, so there is nothing it could ever join.

#[path = "common/sstv_signal.rs"]
mod sstv_signal;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use astar_netrec::daemon::{DaemonState, STATE_FILE};

/// How long a daemon gets to start or to exit. Far longer than either takes.
const PATIENCE: Duration = Duration::from_secs(20);

fn temp(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "netrec-cli-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Write a config with one daily net into `dir`, and return its path.
fn config(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("netrec.toml");
    let text = format!(
        r#"[recorder]
root = "{}"
callsign = "W1AW"

[[net]]
name = "Evening net"
network = "m17"
host = "127.0.0.1"
port = 17000
module = "C"
when = "daily 19:00"
duration = "1h"
tz = "America/Denver"
"#,
        dir.join("archive").display()
    );
    fs::write(&path, text).unwrap();
    path
}

/// Write a config with no nets into `dir`, and return its path.
fn config_without_nets(dir: &Path) -> PathBuf {
    let path = dir.join("netrec.toml");
    let root = dir.join("archive");
    let text = format!(
        "[recorder]\nroot = \"{}\"\ncallsign = \"W1AW\"\n",
        root.display()
    );
    fs::write(&path, text).unwrap();
    path
}

/// A running `astar-netrec run`, killed if the test ends first.
struct Daemon(Child);

impl Daemon {
    fn start(config: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_astar-netrec"))
            .arg("--config")
            .arg(config)
            .arg("run")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self(child)
    }

    /// Wait for `status` to say the daemon has come up.
    fn wait_until_idle(&mut self, config: &Path) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let line = stdout(&netrec(config, &["status"]));
            if line.trim() == "idle, nothing scheduled" {
                return;
            }
            let exited = self.0.try_wait().unwrap();
            assert!(exited.is_none(), "the daemon exited: {exited:?}");
            assert!(Instant::now() < deadline, "never came up: {line}");
            sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "the daemon did not exit");
            sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn netrec(config: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_astar-netrec"))
        .arg("--config")
        .arg(config)
        .args(args)
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn check_lists_next_windows() {
    let dir = temp("check");
    let out = netrec(&config(&dir), &["check"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 12, "{text}");
    // Which ffmpeg (or why none), then which keys are set: never a key.
    assert!(lines[0].starts_with("ffmpeg: "), "{text}");
    assert_eq!(lines[1], "live: YouTube key missing, Twitch key missing");
    assert!(
        lines[2..].iter().all(|l| l.contains("Evening net")),
        "{text}"
    );
}

#[cfg(unix)]
#[test]
fn check_says_a_key_is_set_and_never_shows_it() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp("checkkey");
    let path = config(&dir);
    let creds = dir.join("credentials.toml");
    fs::write(&creds, "youtube_stream_key = \"N0CALL-CLI-SENTINEL-KEY\"\n").unwrap();
    fs::set_permissions(&creds, fs::Permissions::from_mode(0o600)).unwrap();
    let out = netrec(&path, &["check"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("live: YouTube key set, Twitch key missing"),
        "{text}"
    );
    assert!(!text.contains("SENTINEL"), "{text}");
    assert!(!stderr(&out).contains("SENTINEL"), "{}", stderr(&out));
}

#[test]
fn check_prints_warnings_and_exits_1_on_a_bad_config() {
    let dir = temp("bad");
    let path = config(&dir);
    let text = fs::read_to_string(&path).unwrap();

    fs::write(&path, text.replace("W1AW", "N0CALL")).unwrap();
    let out = netrec(&path, &["check"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("N0CALL"), "{}", stderr(&out));

    fs::write(&path, text.replace("daily 19:00", "someday")).unwrap();
    let out = netrec(&path, &["check"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("someday"), "{}", stderr(&out));
}

#[test]
fn status_with_nothing_running() {
    let dir = temp("status");
    let out = netrec(&config(&dir), &["status"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "not running");
}

#[test]
fn status_without_a_config_fails_and_writes_nothing() {
    let dir = temp("noconfig");
    let path = dir.join("netrec.toml");
    let out = netrec(&path, &["status"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!path.exists());
}

#[test]
fn now_with_an_unknown_net_exits_1_and_lists_the_names() {
    let dir = temp("now");
    let out = netrec(&config(&dir), &["now", "Nobody's net"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("Evening net"), "{}", stderr(&out));
}

#[test]
fn now_refuses_a_bad_length() {
    let dir = temp("length");
    let out = netrec(&config(&dir), &["now", "Evening net", "--for", "soon"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("soon"), "{}", stderr(&out));
}

#[test]
fn lock_is_freed_when_the_holder_dies() {
    let dir = temp("lock");
    let path = config_without_nets(&dir);
    let state_file = dir.join("archive").join(STATE_FILE);

    let mut first = Daemon::start(&path);
    first.wait_until_idle(&path);

    // A second recorder is told why it cannot start.
    let mut second = Daemon::start(&path);
    assert_eq!(second.wait_for_exit().code(), Some(1));
    let mut refusal = String::new();
    std::io::Read::read_to_string(second.0.stderr.as_mut().unwrap(), &mut refusal).unwrap();
    assert!(refusal.contains("another recorder is running"), "{refusal}");

    // SIGKILL, so nothing gets to clean up: this is what a power cut leaves.
    first.0.kill().unwrap();
    first.0.wait().unwrap();
    assert!(state_file.exists(), "a killed daemon leaves its state file");

    // After the reboot, its pid belongs to some other live process.
    let mut state: DaemonState = serde_json::from_slice(&fs::read(&state_file).unwrap()).unwrap();
    state.pid = std::process::id();
    fs::write(&state_file, serde_json::to_vec(&state).unwrap()).unwrap();
    let line = stdout(&netrec(&path, &["status"]));
    assert!(line.starts_with("not running (last seen"), "{line}");

    let mut third = Daemon::start(&path);
    third.wait_until_idle(&path);

    // A clean stop, by the signal a service manager sends.
    let term = Command::new("kill")
        .arg(third.0.id().to_string())
        .status()
        .unwrap();
    assert!(term.success());
    assert!(third.wait_for_exit().success());
    assert_eq!(stdout(&netrec(&path, &["status"])).trim(), "not running");
}

// --- `astar-netrec sstv <file>` -------------------------------------------
//
// Every input here is synthetic or a freely licensed fixture; the command
// reads a file and writes pictures, and talks to no network.

const SSTV_PEER: &str = "192.0.2.10:4569";
const SSTV_US: &str = "192.0.2.20:4569";

fn sstv(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_astar-netrec"))
        .arg("sstv")
        .args(args)
        .output()
        .unwrap()
}

fn pngs_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| Path::new(n).extension().is_some_and(|e| e == "png"))
        .collect();
    names.sort();
    names
}

/// A RIFF PCM 16-bit WAV of `channels` interleaved channels.
fn write_wav(path: &Path, rate: u32, channels: u16, interleaved: &[i16]) {
    let data_len = u32::try_from(interleaved.len() * 2).unwrap();
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
    b.extend_from_slice(&(channels * 2).to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in interleaved {
        b.extend_from_slice(&s.to_le_bytes());
    }
    fs::write(path, b).unwrap();
}

/// A second of quiet, the test picture at `rate`, then a second of quiet.
fn quiet_picture_quiet(rate: u32) -> Vec<i16> {
    let mut audio = vec![0i16; rate as usize];
    audio.extend(sstv_signal::robot36(rate));
    audio.extend(std::iter::repeat_n(0, rate as usize));
    audio
}

#[test]
fn sstv_decodes_the_wikimedia_flac() {
    let dir = temp("sstv-flac");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../astar-sstv/tests/fixtures/robot36-french-logo-8k.flac");
    let out = sstv(&[fixture.to_str().unwrap(), "--out", dir.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        pngs_in(&dir),
        ["robot36-french-logo-8k-001-Robot36.png"],
        "{}",
        stdout(&out)
    );
    let text = stdout(&out);
    assert!(text.starts_with("saved "), "{text}");
    assert!(text.contains("(Robot 36, 240 lines)"), "{text}");
}

#[test]
fn sstv_reads_a_16bit_wav() {
    let dir = temp("sstv-wav");
    // Stereo at 11 025 Hz: the picture on the first channel, the second
    // silent, so reading the wrong channel finds nothing.
    let rate = 11_025;
    let mono = quiet_picture_quiet(rate);
    let interleaved: Vec<i16> = mono.iter().flat_map(|&s| [s, 0]).collect();
    let wav = dir.join("bars.wav");
    write_wav(&wav, rate, 2, &interleaved);
    // No --out: the pictures land beside the input.
    let out = sstv(&[wav.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(pngs_in(&dir), ["bars-001-Robot36.png"]);
}

#[test]
fn sstv_with_no_picture_exits_2() {
    let dir = temp("sstv-none");
    let wav = dir.join("quiet.wav");
    write_wav(&wav, 8_000, 1, &vec![0i16; 3 * 8_000]);
    let out = sstv(&[wav.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert_eq!(pngs_in(&dir), Vec::<String>::new());
    assert!(stderr(&out).contains("no SSTV picture"), "{}", stderr(&out));
}

#[test]
fn sstv_refuses_an_mp3_by_name() {
    let dir = temp("sstv-mp3");
    let mp3 = dir.join("net.mp3");
    fs::write(&mp3, [0xFFu8, 0xFB, 0x90, 0x00]).unwrap();
    let out = sstv(&[mp3.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    for format in ["WAV", "FLAC", ".pcap"] {
        assert!(err.contains(format), "{err}");
    }
    assert_eq!(pngs_in(&dir), Vec::<String>::new());
}

/// The test picture as a packet log would hold it: µ-law voice frames 20 ms
/// apart, all mini frames unless `full_first`, which names the codec.
fn write_sstv_pcap(path: &Path, full_first: bool) {
    write_two_way_pcap(path, full_first, 0);
}

/// The IAX2 voice frames of `audio` from `call`, µ-law, 20 ms apart: mini
/// frames, except the first when `full_first`.
fn voice_frames(audio: &[i16], call: u16, full_first: bool) -> Vec<Vec<u8>> {
    use astar_iax_core::frame::{FullFrame, MiniFrame, Subclass};
    use astar_iax_core::{Frame, FrameType, Ies, VoiceFormat, encode};

    audio
        .chunks(160)
        .enumerate()
        .map(|(k, chunk)| {
            let ts = 1_000 + 20 * u32::try_from(k).unwrap();
            let payload: Vec<u8> = chunk
                .iter()
                .map(|&s| astar_codec::g711::ulaw_encode(s))
                .collect();
            let frame = if k == 0 && full_first {
                Frame::Full(Box::new(FullFrame {
                    source_call: call,
                    dest_call: 3,
                    retransmission: false,
                    timestamp: ts,
                    oseqno: 0,
                    iseqno: 0,
                    frame_type: FrameType::Voice,
                    subclass: Subclass::Voice(VoiceFormat::G711U),
                    ies: Ies::empty(),
                    payload: &payload,
                }))
            } else {
                Frame::Mini(MiniFrame {
                    source_call: call,
                    timestamp: (ts & 0xFFFF) as u16,
                    payload: &payload,
                })
            };
            let mut bytes = Vec::new();
            encode(&frame, &mut bytes).unwrap();
            bytes
        })
        .collect()
}

/// The test picture received from the peer, and alongside it `sent` frames
/// of our own quiet transmitted to the peer, interleaved in arrival order.
fn write_two_way_pcap(path: &Path, full_first: bool, sent: usize) {
    use astar_record::pcap::PcapWriter;

    let received = voice_frames(&quiet_picture_quiet(8_000), 7, full_first);
    let ours = voice_frames(&vec![0i16; sent * 160], 3, false);
    let mut w = PcapWriter::new(fs::File::create(path).unwrap()).unwrap();
    let (peer, us) = (SSTV_PEER.parse().unwrap(), SSTV_US.parse().unwrap());
    for k in 0..received.len().max(ours.len()) {
        let arrival = 1_000_000_000 + k as u64 * 20_000_000;
        if let Some(bytes) = received.get(k) {
            w.write_packet(arrival, peer, us, bytes).unwrap();
        }
        if let Some(bytes) = ours.get(k) {
            w.write_packet(arrival + 1_000, us, peer, bytes).unwrap();
        }
    }
    w.flush().unwrap();
}

/// A packet log's companion: its header line names the codec.
fn write_companion(pcap: &Path, codec: &str) {
    let name = pcap.file_name().unwrap().to_str().unwrap();
    let companion = pcap.with_file_name(name.replace(".pcap", ".jitter.jsonl"));
    let header = format!(
        "{{\"astar_packet_log\":1,\"network\":\"allstar\",\"codec\":\"{codec}\",\
         \"label\":\"test\",\"local\":\"{SSTV_US}\",\"peer\":\"{SSTV_PEER}\",\
         \"started_at\":\"2026-11-14T00:00:00Z\",\"version\":\"test\",\"part\":1}}\n"
    );
    fs::write(companion, header).unwrap();
}

#[test]
fn sstv_rebuilds_a_pcap_using_the_companion_codec() {
    let dir = temp("sstv-pcap-companion");
    let pcap = dir.join("call.pcap");
    write_sstv_pcap(&pcap, false);
    write_companion(&pcap, "ulaw");
    let out = sstv(&[pcap.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(pngs_in(&dir), ["call-001-Robot36.png"]);
}

#[test]
fn sstv_pcap_of_mini_frames_without_a_companion_asks_for_the_codec() {
    let dir = temp("sstv-pcap-bare");
    let pcap = dir.join("call.pcap");
    write_sstv_pcap(&pcap, false);
    let out = sstv(&[pcap.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("--codec"), "{}", stderr(&out));

    // Named on the command line, it decodes.
    let out = sstv(&[pcap.to_str().unwrap(), "--codec", "ulaw"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(pngs_in(&dir), ["call-001-Robot36.png"]);
}

#[test]
fn sstv_pcap_takes_the_codec_from_a_full_frame() {
    let dir = temp("sstv-pcap-full");
    let pcap = dir.join("pic.pcap");
    write_sstv_pcap(&pcap, true);
    let out = sstv(&[pcap.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(pngs_in(&dir), ["pic-001-Robot36.png"]);
}

#[test]
fn sstv_never_overwrites_a_picture() {
    let dir = temp("sstv-twice");
    let pcap = dir.join("pic.pcap");
    write_sstv_pcap(&pcap, true);
    fs::write(dir.join("pic-001-Robot36.png"), b"mine").unwrap();
    let out = sstv(&[pcap.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(fs::read(dir.join("pic-001-Robot36.png")).unwrap(), b"mine");
    // The error names the file in the way and the way around it.
    let err = stderr(&out);
    assert!(err.contains("pic-001-Robot36.png"), "{err}");
    assert!(err.contains("already exists"), "{err}");
    assert!(err.contains("--out"), "{err}");
}

#[test]
fn sstv_pcap_leaves_out_our_own_louder_stream() {
    // Both directions captured, and we sent more frames than we heard. The
    // companion names our address, so our stream is never taken for the
    // call's received audio.
    let dir = temp("sstv-pcap-two-way");
    let pcap = dir.join("call.pcap");
    let received = quiet_picture_quiet(8_000).len().div_ceil(160);
    write_two_way_pcap(&pcap, false, received + 500);
    write_companion(&pcap, "ulaw");
    let out = sstv(&[pcap.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(pngs_in(&dir), ["call-001-Robot36.png"]);
}

#[test]
fn sstv_pcap_local_names_our_address_without_a_companion() {
    let dir = temp("sstv-pcap-local");
    let pcap = dir.join("call.pcap");
    let received = quiet_picture_quiet(8_000).len().div_ceil(160);
    write_two_way_pcap(&pcap, false, received + 500);
    let out = sstv(&[
        pcap.to_str().unwrap(),
        "--codec",
        "ulaw",
        "--local",
        SSTV_US,
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(pngs_in(&dir), ["call-001-Robot36.png"]);
}

#[test]
fn sstv_usage_errors_exit_1_not_2() {
    // 2 means "no picture found"; a mistyped command line is an error.
    let dir = temp("sstv-usage");
    let wav = dir.join("quiet.wav");
    write_wav(&wav, 8_000, 1, &[0i16; 800]);
    let out = sstv(&[wav.to_str().unwrap(), "--codec", "g729"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("ulaw"), "{}", stderr(&out));
    let out = sstv(&[]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    // Help is not an error.
    let out = sstv(&["--help"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

// A FLAC writer for the tests: STREAMINFO, then fixed-size frames of 16-bit
// mono VERBATIM subframes. Enough for claxon, and every byte is ours.

fn crc8(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |mut crc, &b| {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
        crc
    })
}

fn crc16(bytes: &[u8]) -> u16 {
    bytes.iter().fold(0u16, |mut crc, &b| {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
        }
        crc
    })
}

const FLAC_BLOCK: usize = 4_096;

fn flac_bytes(rate: u32, samples: &[i16]) -> Vec<u8> {
    let mut b = b"fLaC".to_vec();
    // The last metadata block, STREAMINFO, 34 bytes.
    b.extend_from_slice(&[0x80, 0, 0, 34]);
    b.extend_from_slice(&16u16.to_be_bytes());
    b.extend_from_slice(&u16::try_from(FLAC_BLOCK).unwrap().to_be_bytes());
    b.extend_from_slice(&[0; 6]); // frame sizes unknown
    let packed = (u64::from(rate) << 44) | (15u64 << 36) | samples.len() as u64;
    b.extend_from_slice(&packed.to_be_bytes());
    b.extend_from_slice(&[0; 16]); // MD5 unknown
    for (n, block) in samples.chunks(FLAC_BLOCK).enumerate() {
        let n = u8::try_from(n).expect("frame numbers above 127 need UTF-8 coding");
        assert!(n < 0x80);
        // Sync, fixed block size; block size in 16 bits at the end; rate
        // from STREAMINFO; mono; 16 bits.
        let mut f = vec![0xFF, 0xF8, 0x70, 0x08, n];
        f.extend_from_slice(&u16::try_from(block.len() - 1).unwrap().to_be_bytes());
        f.push(crc8(&f));
        f.push(0x02); // VERBATIM, no wasted bits
        for s in block {
            f.extend_from_slice(&s.to_be_bytes());
        }
        let crc = crc16(&f);
        f.extend_from_slice(&crc.to_be_bytes());
        b.extend(f);
    }
    b
}

/// The picture between quiet, as a FLAC cut off `secs_from_end` seconds
/// before its end.
fn truncated_flac(path: &Path, secs_from_end: f64) {
    let rate = 8_000;
    let mut audio = quiet_picture_quiet(rate);
    audio.extend(std::iter::repeat_n(0, 3 * rate as usize));
    let bytes = flac_bytes(rate, &audio);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let cut = bytes.len() - (secs_from_end * f64::from(rate) * 2.0) as usize;
    fs::write(path, &bytes[..cut]).unwrap();
}

#[test]
fn sstv_reads_a_whole_synthetic_flac() {
    let dir = temp("sstv-flac-whole");
    let flac = dir.join("whole.flac");
    truncated_flac(&flac, 0.0);
    let out = sstv(&[flac.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(pngs_in(&dir), ["whole-001-Robot36.png"]);
    assert!(!stderr(&out).contains("damaged"), "{}", stderr(&out));
}

#[test]
fn sstv_truncated_flac_keeps_the_pictures_before_the_damage() {
    let dir = temp("sstv-flac-cut");
    let flac = dir.join("cut.flac");
    // Cut in the quiet after the picture.
    truncated_flac(&flac, 2.3);
    let out = sstv(&[flac.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(pngs_in(&dir), ["cut-001-Robot36.png"]);
    assert!(stdout(&out).contains("saved "), "{}", stdout(&out));
    let err = stderr(&out);
    assert!(err.contains("damaged"), "{err}");
}

#[test]
fn sstv_truncated_flac_mid_picture_saves_it_as_partial() {
    let dir = temp("sstv-flac-mid");
    let flac = dir.join("mid.flac");
    // Robot 36 runs 36 s; four seconds of quiet follow it. Cut ten seconds
    // before the picture's end.
    truncated_flac(&flac, 14.0);
    let out = sstv(&[flac.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(pngs_in(&dir), ["mid-001-Robot36.png"]);
    assert!(stdout(&out).contains("partial"), "{}", stdout(&out));
    assert!(stderr(&out).contains("damaged"), "{}", stderr(&out));
}

#[test]
fn sstv_truncated_flac_with_no_picture_exits_1() {
    let dir = temp("sstv-flac-early");
    let flac = dir.join("early.flac");
    // Cut before the picture has begun.
    let rate = 8_000;
    let mut bytes = flac_bytes(rate, &quiet_picture_quiet(rate));
    bytes.truncate(42 + 8_000);
    fs::write(&flac, bytes).unwrap();
    let out = sstv(&[flac.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(pngs_in(&dir), Vec::<String>::new());
    assert!(stderr(&out).contains("damaged"), "{}", stderr(&out));
}
