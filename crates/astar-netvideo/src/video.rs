// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `video.mp4`: a finished session drawn as frame A and encoded by `ffmpeg`.
//!
//! [`render_session`] replays the session the way the recorder lived it. The
//! overs in `log.jsonl` become `over_start` and `over_end` calls at their
//! offsets, `session.flac` is decoded in 20 ms steps and fed in as audio, and
//! the [`SceneBuilder`] says what each frame at 30 fps shows. The
//! [`Renderer`] draws it, and the RGBA bytes go down a pipe into `ffmpeg`,
//! which muxes them with `session.flac` (as AAC) into `video.mp4.part`. Only
//! a finished, successful render is renamed to `video.mp4`; a failure or a
//! stop removes the part file.
//!
//! The video is as long as `session.flac`, not as the window: a session the
//! recorder never closed properly still renders what it holds.
//!
//! One render of a session runs at a time, on any machine that shares the
//! folder: a render holds an operating-system lock on [`RENDER_LOCK`] for as
//! long as it writes the part file, and a second one is refused with
//! [`VideoError::Busy`]. The kernel frees that lock however the process
//! ends, so [`remove_leftover_part`] can tell a crashed render's part file
//! from a live one's.

use std::collections::VecDeque;
use std::fmt;
use std::fs::{File, TryLockError};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use astar_netarchive::{LogKind, read_meta, read_peaks, read_rows};

use crate::ffmpeg::{Child, Ffmpeg};
use crate::render::{Renderer, Size};
use crate::scene::{Mode, SceneBuilder, SessionInfo};

/// One rung of the encoding ladder: a size, a frame rate and a video bit
/// rate. A live stream steps down it when the machine or the link cannot
/// keep up; the file render always uses the top rung.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    pub size: Size,
    pub fps: u32,
    pub video_kbps: u32,
}

impl Profile {
    /// How the state file and `status` name it: `1080p30`, `720p15`.
    #[must_use]
    pub fn name(&self) -> String {
        format!("{}p{}", self.size.height, self.fps)
    }
}

/// 1080p at 30 fps, 1080p at 15 fps, 720p at 15 fps.
pub const LADDER: [Profile; 3] = [
    Profile {
        size: Size::P1080,
        fps: 30,
        video_kbps: 4_500,
    },
    Profile {
        size: Size::P1080,
        fps: 15,
        video_kbps: 4_500,
    },
    Profile {
        size: Size::P720,
        fps: 15,
        video_kbps: 2_500,
    },
];

/// The finished video, in the session folder.
pub const VIDEO_FILE: &str = "video.mp4";
/// What `ffmpeg` writes while it works.
pub const VIDEO_PART: &str = "video.mp4.part";
/// The empty file a render locks while it works. It is never removed: the
/// lock on it is what counts, and removing it while another render was
/// opening it would let two renders lock two different files.
pub const RENDER_LOCK: &str = ".video.lock";
const AUDIO_FILE: &str = "session.flac";
/// Audio is fed to the scene in steps of this length.
const STEP_MS: u64 = 20;
/// How many of `ffmpeg`'s last stderr lines an error keeps.
const STDERR_TAIL: usize = 20;
/// No one stderr line is kept longer than this.
const STDERR_LINE_MAX: usize = 400;

/// What a finished render did.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoReport {
    /// Frames sent to `ffmpeg`.
    pub frames: u64,
    /// Of those, frames that were the previous one again.
    pub reused: u64,
    /// The video's length: `session.flac`'s.
    pub seconds: f64,
}

/// How a render runs, beyond the session it reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderOptions {
    /// The callsigns that count as "my side" when `session.json` names none:
    /// `[recorder] my_callsigns`, as the recordings page uses them.
    pub my_callsigns: Vec<String>,
    /// How many threads `ffmpeg` may encode with; `None` lets it choose. The
    /// recorder caps it so a render after one net cannot starve the next
    /// net's live encoder.
    pub threads: Option<u32>,
}

/// Why no `video.mp4` was written. In every case no part file of this
/// render is left.
#[derive(Debug)]
pub enum VideoError {
    /// The stop flag was set.
    Stopped,
    /// Another render of this session holds its lock.
    Busy,
    /// `session.json` or `log.jsonl` could not be read.
    Session(String),
    /// `session.flac` could not be read, or holds no audio.
    Audio(String),
    /// Writing in the session folder failed.
    Io(io::Error),
    /// `ffmpeg` could not be started, or failed. `tail` is the last lines it
    /// wrote to stderr.
    Ffmpeg { status: String, tail: Vec<String> },
}

impl fmt::Display for VideoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stopped => f.write_str("the video render was stopped"),
            Self::Busy => f.write_str("another render of this session is running"),
            Self::Session(why) => write!(f, "cannot read the session: {why}"),
            Self::Audio(why) => write!(f, "cannot read {AUDIO_FILE}: {why}"),
            Self::Io(e) => write!(f, "cannot write the video: {e}"),
            Self::Ffmpeg { status, tail } => {
                write!(f, "ffmpeg {status}")?;
                if !tail.is_empty() {
                    write!(f, "; it said:\n{}", tail.join("\n"))?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for VideoError {}

impl From<io::Error> for VideoError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Render `dir`'s session to `dir/video.mp4` with `ff`. Only a finished
/// session renders: one still recording is refused. A set `stop` kills
/// `ffmpeg`, even one that has stopped reading frames, and returns
/// [`VideoError::Stopped`]. Synchronous and slow (minutes for a long net):
/// run it on a thread of its own.
///
/// # Errors
/// [`VideoError`]; whatever it is, this render's `video.mp4.part` is gone
/// afterwards and an existing `video.mp4` is untouched. [`VideoError::Busy`]
/// touches nothing, because the part file is the other render's.
pub fn render_session(
    dir: &Path,
    ff: &Ffmpeg,
    opts: &RenderOptions,
    stop: &AtomicBool,
) -> Result<VideoReport, VideoError> {
    render_session_with(dir, ff, opts, stop, &mut |_| {})
}

/// Take `dir`'s render lock without waiting: `Ok(None)` when another render
/// holds it.
fn lock_render(dir: &Path) -> io::Result<Option<File>> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(RENDER_LOCK))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// Remove `dir`'s `video.mp4.part` if a render that is gone left it there.
/// Returns whether one was removed; a part file a live render holds stays.
///
/// # Errors
/// The lock file cannot be opened, or the part file cannot be removed.
pub fn remove_leftover_part(dir: &Path) -> io::Result<bool> {
    let part = dir.join(VIDEO_PART);
    if std::fs::symlink_metadata(&part).is_err() {
        return Ok(false);
    }
    let Some(_lock) = lock_render(dir)? else {
        return Ok(false);
    };
    remove_if_there(&part)?;
    Ok(true)
}

/// [`render_session`], calling `on_frame` with the number of frames sent so
/// far after each one.
pub(crate) fn render_session_with(
    dir: &Path,
    ff: &Ffmpeg,
    opts: &RenderOptions,
    stop: &AtomicBool,
    on_frame: &mut dyn FnMut(u64),
) -> Result<VideoReport, VideoError> {
    let dir = std::path::absolute(dir)?;
    let meta = read_meta(&dir).map_err(|e| VideoError::Session(format!("session.json: {e}")))?;
    if meta.ended.is_none() {
        return Err(VideoError::Session("it is still recording".to_owned()));
    }
    // Held until this returns, so the part file is this render's alone.
    let Some(_lock) = lock_render(&dir)? else {
        return Err(VideoError::Busy);
    };
    let part = dir.join(VIDEO_PART);
    remove_if_there(&part)?;
    // Declared after the lock, so it drops first: the part file goes while
    // the lock still says it is this render's. That holds on a panic too,
    // which would otherwise leave the part file until the next render.
    let _cleanup = PartGuard(&part);
    // On an error `ffmpeg` is dead and reaped by now (its guard dropped
    // inside `render_to_part`), so nothing writes the part file again.
    let report = render_to_part(&dir, &part, ff, opts, stop, on_frame)?;
    std::fs::rename(&part, dir.join(VIDEO_FILE))?;
    Ok(report)
}

/// Removes a render's part file however the render ends. After a
/// successful rename there is nothing left to remove.
struct PartGuard<'a>(&'a Path);

impl Drop for PartGuard<'_> {
    fn drop(&mut self) {
        let _ = remove_if_there(self.0);
    }
}

fn remove_if_there(p: &Path) -> io::Result<()> {
    match std::fs::remove_file(p) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// One step of the replay, at a media offset.
enum Step {
    End {
        seq: u32,
        duration_ms: u64,
        peaks: Option<Vec<u8>>,
    },
    Start {
        seq: u32,
        talker: Option<String>,
    },
}

/// The overs of `log.jsonl` as starts and ends, in offset order (an end
/// before a start at the same moment). Each over's `peaks.json` entry rides
/// on its end, so the bubble shows what the recordings page shows.
fn replay_steps(dir: &Path) -> Result<Vec<(u64, Step)>, VideoError> {
    let rows = read_rows(dir).map_err(|e| VideoError::Session(format!("log.jsonl: {e}")))?;
    let peaks = read_peaks(dir).ok().flatten();
    let mut overs: Vec<(u64, u64, Option<String>, String)> = rows
        .into_iter()
        .filter_map(|r| match r.kind {
            LogKind::Over {
                duration_ms,
                offset_ms,
                talker,
                file,
                ..
            } => Some((offset_ms, duration_ms, talker, file)),
            _ => None,
        })
        .collect();
    overs.sort_by_key(|o| o.0);
    let mut steps = Vec::with_capacity(overs.len() * 2);
    for (i, (offset_ms, duration_ms, talker, file)) in overs.into_iter().enumerate() {
        let seq = u32::try_from(i + 1).unwrap_or(u32::MAX);
        let peaks = peaks.as_ref().and_then(|p| p.overs.get(&file).cloned());
        steps.push((offset_ms, Step::Start { seq, talker }));
        steps.push((
            offset_ms.saturating_add(duration_ms),
            Step::End {
                seq,
                duration_ms,
                peaks,
            },
        ));
    }
    steps.sort_by_key(|(ms, s)| (*ms, matches!(s, Step::Start { .. })));
    Ok(steps)
}

fn apply(b: &mut SceneBuilder, at_ms: u64, step: Step) {
    match step {
        Step::Start { seq, talker } => b.over_start(seq, talker.as_deref(), at_ms),
        Step::End {
            seq,
            duration_ms,
            peaks,
        } => {
            b.over_end(seq, duration_ms);
            if let Some(p) = peaks {
                b.over_peaks(seq, &p);
            }
        }
    }
}

/// `session.flac` as mono 16-bit PCM, a few samples at a time. A stream that
/// stops decoding part way (a crash mid-write) ends there.
struct Pcm {
    reader: claxon::FlacReader<File>,
    channels: u32,
    bits: u32,
    block: Option<claxon::Block>,
    pos: u32,
    spare: Vec<i32>,
    done: bool,
}

impl Pcm {
    fn open(path: &Path) -> Result<Self, VideoError> {
        let reader =
            claxon::FlacReader::open(path).map_err(|e| VideoError::Audio(e.to_string()))?;
        let info = reader.streaminfo();
        Ok(Self {
            channels: info.channels.max(1),
            bits: info.bits_per_sample,
            reader,
            block: None,
            pos: 0,
            spare: Vec::new(),
            done: false,
        })
    }

    fn rate(&self) -> u32 {
        self.reader.streaminfo().sample_rate
    }

    fn sample(&self, block: &claxon::Block, i: u32) -> i16 {
        let sum: i64 = (0..self.channels)
            .map(|c| i64::from(block.sample(c, i)))
            .sum();
        let mean = sum / i64::from(self.channels);
        let s = if self.bits >= 16 {
            mean >> (self.bits - 16)
        } else {
            mean << (16 - self.bits)
        };
        i16::try_from(s.clamp(i64::from(i16::MIN), i64::from(i16::MAX))).unwrap_or(0)
    }

    /// Append up to `n` samples to `out`; fewer only at the end.
    fn read(&mut self, n: u64, out: &mut Vec<i16>) {
        let mut want = n;
        while want > 0 && !self.done {
            if let Some(block) = self.block.take() {
                let end = block.duration().min(
                    self.pos
                        .saturating_add(u32::try_from(want).unwrap_or(u32::MAX)),
                );
                for i in self.pos..end {
                    out.push(self.sample(&block, i));
                }
                want -= u64::from(end - self.pos);
                self.pos = end;
                if end < block.duration() {
                    self.block = Some(block);
                } else {
                    self.spare = block.into_buffer();
                }
                continue;
            }
            let buf = std::mem::take(&mut self.spare);
            match self.reader.blocks().read_next_or_eof(buf) {
                Ok(Some(block)) => {
                    self.block = Some(block);
                    self.pos = 0;
                }
                Ok(None) | Err(_) => self.done = true,
            }
        }
    }
}

/// How many samples `session.flac` holds: its header's count, or (for a file
/// whose header was never finished) a count of what decodes.
fn audio_length(path: &Path) -> Result<(u32, u64), VideoError> {
    let mut pcm = Pcm::open(path)?;
    let rate = pcm.rate();
    if rate == 0 {
        return Err(VideoError::Audio("its sample rate is 0".to_owned()));
    }
    if let Some(n) = pcm.reader.streaminfo().samples.filter(|&n| n > 0) {
        return Ok((rate, n));
    }
    let mut n = 0u64;
    let mut scratch = Vec::new();
    while !pcm.done {
        scratch.clear();
        pcm.read(u64::from(rate), &mut scratch);
        n += scratch.len() as u64;
    }
    Ok((rate, n))
}

/// `file:` keeps `ffmpeg` from reading a path as an option or a protocol.
pub(crate) fn file_url(p: &Path) -> std::ffi::OsString {
    let mut s = std::ffi::OsString::from("file:");
    s.push(p.as_os_str());
    s
}

/// The last [`STDERR_TAIL`] lines of `ffmpeg`'s stderr, read on a thread of
/// their own so a chatty `ffmpeg` never blocks on a full pipe.
pub(crate) fn read_tail(
    stderr: Option<std::process::ChildStderr>,
) -> thread::JoinHandle<Vec<String>> {
    thread::spawn(move || {
        let mut tail = VecDeque::with_capacity(STDERR_TAIL);
        if let Some(stderr) = stderr {
            let mut r = BufReader::new(stderr);
            let mut line = Vec::new();
            loop {
                line.clear();
                match r.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let text = String::from_utf8_lossy(&line);
                        let text = text.trim_end();
                        if text.is_empty() {
                            continue;
                        }
                        if tail.len() == STDERR_TAIL {
                            tail.pop_front();
                        }
                        tail.push_back(text.chars().take(STDERR_LINE_MAX).collect());
                    }
                }
            }
        }
        tail.into_iter().collect()
    })
}

/// `ffmpeg` reading RGBA frames on stdin and `audio` from its file, writing
/// an MP4 to `part`.
fn spawn_ffmpeg(
    ff: &Ffmpeg,
    profile: Profile,
    threads: Option<u32>,
    audio: &Path,
    part: &Path,
) -> Result<Child, VideoError> {
    let size = format!("{}x{}", profile.size.width, profile.size.height);
    let mut cmd = Command::new(&ff.path);
    cmd.args(["-hide_banner", "-nostats", "-loglevel", "warning", "-y"])
        .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-s", &size])
        .args(["-r", &profile.fps.to_string(), "-i", "pipe:0", "-i"])
        .arg(file_url(audio))
        .args(["-map", "0:v", "-map", "1:a"])
        .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20"]);
    if let Some(n) = threads {
        cmd.args(["-threads", &n.to_string()]);
    }
    cmd.args(["-pix_fmt", "yuv420p"])
        .args(["-c:a", "aac", "-b:a", "128k", "-ar", "48000", "-ac", "2"])
        .args(["-shortest", "-movflags", "+faststart", "-f", "mp4"])
        .arg(file_url(part))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    Child::spawn(&mut cmd).map_err(|e| VideoError::Ffmpeg {
        status: format!("could not be started from {}: {e}", ff.path.display()),
        tail: Vec::new(),
    })
}

/// Sets its flag when dropped, however the scope it guards ends.
struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Kill `child` if `stop` is set before `done` is.
fn kill_on_stop(child: &Mutex<Child>, stop: &AtomicBool, done: &AtomicBool) {
    while !done.load(Ordering::Acquire) {
        if stop.load(Ordering::Acquire) {
            if let Ok(mut c) = child.lock() {
                c.kill();
            }
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Wait for `ffmpeg` to finish writing, or for `stop`.
fn wait_for_exit(
    child: &mut Child,
    stop: &AtomicBool,
) -> Result<std::process::ExitStatus, VideoError> {
    loop {
        if stop.load(Ordering::Acquire) {
            return Err(VideoError::Stopped);
        }
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn render_to_part(
    dir: &Path,
    part: &Path,
    ff: &Ffmpeg,
    opts: &RenderOptions,
    stop: &AtomicBool,
    on_frame: &mut dyn FnMut(u64),
) -> Result<VideoReport, VideoError> {
    let meta = read_meta(dir).map_err(|e| VideoError::Session(format!("session.json: {e}")))?;
    let mut info =
        SessionInfo::from_meta(&meta, &opts.my_callsigns).map_err(VideoError::Session)?;
    let mut steps = replay_steps(dir)?.into_iter().peekable();
    let audio = dir.join(AUDIO_FILE);
    let (rate, samples) = audio_length(&audio)?;
    if samples == 0 {
        return Err(VideoError::Audio("it holds no audio".to_owned()));
    }
    // The audio's own rate is the one the scene must count samples at.
    info.sample_rate = rate;
    let profile = LADDER[0];
    let fps = u64::from(profile.fps);
    let rate64 = u64::from(rate);
    let total_ms = samples * 1_000 / rate64;
    let frames = (samples * fps).div_ceil(rate64);
    #[allow(clippy::cast_precision_loss)] // a session is far below 2^52 samples
    let seconds = samples as f64 / f64::from(rate);

    let mut renderer = Renderer::new(profile.size);
    let mut scene = SceneBuilder::new(info, Mode::Recorded { total_ms });
    scene.set_capacity(renderer.column_capacity());
    let mut pcm = Pcm::open(&audio)?;

    let mut child = spawn_ffmpeg(ff, profile, opts.threads, &audio, part)?;
    let tail = read_tail(child.take_stderr());
    let mut stdin = child.take_stdin();

    // A frame write blocks while `ffmpeg` is not reading, and an `ffmpeg`
    // that has wedged never reads again. So a watcher kills it on `stop`,
    // which ends the write with a broken pipe. Nothing reaps the child
    // while the watcher runs, so the process it kills is still ours.
    let child = Mutex::new(child);
    let frames_done = AtomicBool::new(false);
    let at = |ms: u64| ms * rate64 / 1_000;
    let (sent, pipe_broke) = thread::scope(|scope| {
        scope.spawn(|| kill_on_stop(&child, stop, &frames_done));
        let _done = SetOnDrop(&frames_done);
        let mut fed_ms = 0u64;
        let mut block = Vec::new();
        let mut sent = 0u64;
        let mut pipe_broke = false;
        for k in 0..frames {
            if stop.load(Ordering::Acquire) {
                return Err(VideoError::Stopped);
            }
            let t = k * 1_000 / fps;
            // Feed audio up to `t` in steps that never cross an over's start or
            // end, so each over's peaks see exactly its own samples.
            loop {
                while let Some((ms, s)) = steps.next_if(|(ms, _)| *ms <= fed_ms) {
                    apply(&mut scene, ms, s);
                }
                if fed_ms >= t {
                    break;
                }
                let mut end = (fed_ms + STEP_MS).min(t);
                if let Some((ms, _)) = steps.peek() {
                    end = end.min(*ms);
                }
                block.clear();
                pcm.read(at(end) - at(fed_ms), &mut block);
                if !block.is_empty() {
                    scene.audio(fed_ms, &block);
                }
                fed_ms = end;
            }
            let state = scene.frame_at(t);
            let pixels = renderer.render(&state);
            if let Some(w) = stdin.as_mut()
                && w.write_all(pixels).is_err()
            {
                // `ffmpeg` has gone; its status and stderr say why.
                pipe_broke = true;
                break;
            }
            sent += 1;
            on_frame(sent);
        }
        // Closing stdin is ffmpeg's end of input.
        drop(stdin.take());
        Ok((sent, pipe_broke))
    })?;
    let mut child = child
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let status = wait_for_exit(&mut child, stop)?;
    let tail = tail.join().unwrap_or_default();
    // `-shortest` lets `ffmpeg` finish when the audio does, which can be a
    // frame or two before the last frame is written: a broken pipe followed
    // by a clean exit is a finished video.
    if !status.success() {
        let status = if pipe_broke {
            format!("stopped reading frames and failed ({status})")
        } else {
            format!("failed ({status})")
        };
        return Err(VideoError::Ffmpeg { status, tail });
    }
    // That slop is a frame or two. An `ffmpeg` that exits 0 having read far
    // fewer frames than the audio needs has written a video that stops
    // early, or freezes, which is not a finished one.
    let missing = frames.saturating_sub(sent);
    if missing > fps {
        return Err(VideoError::Ffmpeg {
            status: format!(
                "exited after reading {sent} of {frames} frames, {}.{} s short",
                missing / fps,
                missing % fps * 10 / fps
            ),
            tail,
        });
    }
    Ok(VideoReport {
        frames: sent,
        reused: renderer.stats().reused,
        seconds,
    })
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use crate::demo;
    use crate::ffmpeg::ffmpeg_for_tests;
    use crate::ffmpeg::tests::{Scratch, script};
    use astar_netarchive::{Ended, LogRow, append_row, net_dir_name, write_meta};
    use astar_record::FlacWriter;
    use std::path::PathBuf;

    /// The fixture's Tuesday session (90 s, ten overs), under `root`.
    fn fixture_session(root: &Path) -> PathBuf {
        let _ = astar_netarchive::fixture::write_fixture(root);
        let net = root.join(net_dir_name("Tuesday M17 net"));
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&net)
            .expect("the net's folder")
            .map(|e| e.expect("an entry").path())
            .filter(|p| p.join(AUDIO_FILE).is_file())
            .collect();
        assert_eq!(dirs.len(), 1, "{dirs:?}");
        dirs.remove(0)
    }

    /// A finished session of `len_ms` at 8 kHz with these overs
    /// (`start_ms`, `duration_ms`, talker); silence between them.
    fn synthetic_session(dir: &Path, len_ms: u64, overs: &[(u64, u64, &str)]) {
        std::fs::create_dir_all(dir).expect("session dir");
        let mut meta = demo::meta("Synthetic net", "M17-XXX C");
        meta.ended = Some(Ended::Window);
        meta.ended_at = Some("2026-11-04T03:00:00Z".to_owned());
        write_meta(dir, &meta).expect("session.json");
        let mut pcm = vec![0i16; usize::try_from(len_ms * 8).expect("small")];
        for (i, &(start, dur, talker)) in overs.iter().enumerate() {
            let seed = u32::try_from(i).expect("few");
            for k in 0..dur * 8 {
                if let Some(s) = pcm.get_mut(usize::try_from(start * 8 + k).expect("small")) {
                    *s = demo::voice(seed, k);
                }
            }
            append_row(
                dir,
                &LogRow {
                    t: "2026-11-04T02:00:00Z".to_owned(),
                    kind: LogKind::Over {
                        start: "2026-11-04T02:00:00Z".to_owned(),
                        end: "2026-11-04T02:00:00Z".to_owned(),
                        duration_ms: dur,
                        offset_ms: start,
                        talker: Some(talker.to_owned()),
                        file: format!("{i:04}.flac"),
                    },
                },
            )
            .expect("log row");
        }
        let mut w = FlacWriter::create(&dir.join(AUDIO_FILE), 8_000, 1).expect("flac");
        w.push_interleaved(&pcm).expect("samples");
        w.finish().expect("finish");
    }

    /// What `ffmpeg -i` says about a file: (duration s, video line, audio line).
    pub(crate) fn describe(ff: &Ffmpeg, file: &Path) -> (f64, String, String) {
        let out = Command::new(&ff.path)
            .args(["-hide_banner", "-i"])
            .arg(file)
            .output()
            .expect("run ffmpeg -i");
        let text = String::from_utf8_lossy(&out.stderr).into_owned();
        let dur = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("Duration: "))
            .and_then(|d| d.split(',').next())
            .expect("a duration")
            .to_owned();
        let parts: Vec<f64> = dur
            .split(':')
            .map(|p| p.parse().expect("hh:mm:ss"))
            .collect();
        let secs = parts[0] * 3600.0 + parts[1] * 60.0 + parts[2];
        let line = |kind: &str| {
            text.lines()
                .find(|l| l.contains(kind))
                .unwrap_or_else(|| panic!("no {kind} stream in:\n{text}"))
                .to_owned()
        };
        (secs, line("Video:"), line("Audio:"))
    }

    fn flac_seconds(p: &Path) -> f64 {
        let r = claxon::FlacReader::open(p).expect("flac");
        let i = r.streaminfo();
        #[allow(clippy::cast_precision_loss)] // a short test file
        let n = i.samples.expect("a length") as f64;
        n / f64::from(i.sample_rate)
    }

    fn assert_good_mp4(ff: &Ffmpeg, dir: &Path, report: &VideoReport) {
        let mp4 = dir.join(VIDEO_FILE);
        assert!(mp4.is_file(), "no video.mp4");
        assert!(!dir.join(VIDEO_PART).exists(), "part file left behind");
        let (secs, video, audio) = describe(ff, &mp4);
        let want = flac_seconds(&dir.join(AUDIO_FILE));
        assert!(
            (secs - want).abs() <= 0.2,
            "{secs} s vs the FLAC's {want} s"
        );
        assert!((report.seconds - want).abs() < 0.01, "{report:?}");
        assert!(video.contains("h264"), "{video}");
        assert!(video.contains("1920x1080"), "{video}");
        assert!(video.contains("30 fps"), "{video}");
        assert!(audio.contains("aac"), "{audio}");
        assert!(
            audio.contains("48000 Hz") && audio.contains("stereo"),
            "{audio}"
        );
    }

    #[test]
    fn renders_the_fixture() {
        let Some(ff) = ffmpeg_for_tests() else { return };
        let s = Scratch::new("fixture");
        let dir = fixture_session(&s.0);
        let report = render_session(
            &dir,
            &ff,
            &RenderOptions::default(),
            &AtomicBool::new(false),
        )
        .expect("a render");
        assert_good_mp4(&ff, &dir, &report);
        assert_eq!(report.frames, 2_700, "{report:?}");
        assert!(report.reused > 0, "{report:?}");
        if let Ok(keep) = std::env::var("NETVIDEO_KEEP_MP4") {
            std::fs::copy(dir.join(VIDEO_FILE), keep).expect("keep the mp4");
        }
    }

    #[test]
    fn a_silent_session_renders() {
        let Some(ff) = ffmpeg_for_tests() else { return };
        let s = Scratch::new("silent");
        let dir = s.0.join("session");
        synthetic_session(&dir, 6_000, &[]);
        let report = render_session(
            &dir,
            &ff,
            &RenderOptions::default(),
            &AtomicBool::new(false),
        )
        .expect("a render");
        assert_good_mp4(&ff, &dir, &report);
        assert_eq!(report.frames, 180);
    }

    #[test]
    fn a_failed_render_leaves_no_part_file() {
        let s = Scratch::new("fail");
        let dir = fixture_session(&s.0);
        // Writes the part file (its last argument), complains, and fails.
        let fake = script(
            &s.0,
            "ffmpeg",
            "for a; do last=$a; done\necho junk > \"${last#file:}\"\n\
             echo 'Unknown encoder libx264' >&2\nexit 1\n",
        );
        let ff = Ffmpeg {
            path: fake,
            version: "fake".to_owned(),
        };
        let err = render_session(
            &dir,
            &ff,
            &RenderOptions::default(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        match &err {
            VideoError::Ffmpeg { tail, .. } => {
                assert_eq!(tail, &["Unknown encoder libx264".to_owned()]);
            }
            other => panic!("{other:?}"),
        }
        assert!(err.to_string().contains("Unknown encoder libx264"), "{err}");
        assert!(!dir.join(VIDEO_PART).exists());
        assert!(!dir.join(VIDEO_FILE).exists());
    }

    #[test]
    fn an_ffmpeg_that_will_not_start_is_an_error() {
        let s = Scratch::new("nostart");
        let dir = fixture_session(&s.0);
        let ff = Ffmpeg {
            path: s.0.join("no-such-ffmpeg"),
            version: "none".to_owned(),
        };
        let err = render_session(
            &dir,
            &ff,
            &RenderOptions::default(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(matches!(err, VideoError::Ffmpeg { .. }), "{err:?}");
        assert!(!dir.join(VIDEO_PART).exists());
    }

    #[test]
    fn stop_cancels_a_render() {
        let s = Scratch::new("stop");
        let dir = fixture_session(&s.0);
        // Writes the part file, then swallows frames until killed.
        let fake = script(
            &s.0,
            "ffmpeg",
            "for a; do last=$a; done\necho junk > \"${last#file:}\"\nexec cat > /dev/null\n",
        );
        let ff = Ffmpeg {
            path: fake,
            version: "fake".to_owned(),
        };
        let stop = AtomicBool::new(false);
        let mut seen = 0;
        let err = render_session_with(&dir, &ff, &RenderOptions::default(), &stop, &mut |n| {
            seen = n;
            if n == 10 {
                stop.store(true, Ordering::Release);
            }
        })
        .unwrap_err();
        assert!(matches!(err, VideoError::Stopped), "{err:?}");
        assert_eq!(seen, 10);
        assert!(!dir.join(VIDEO_PART).exists());
        assert!(!dir.join(VIDEO_FILE).exists());
    }

    #[test]
    fn a_session_without_audio_is_refused() {
        let s = Scratch::new("noaudio");
        let dir = fixture_session(&s.0);
        std::fs::remove_file(dir.join(AUDIO_FILE)).expect("rm");
        let ff = Ffmpeg {
            path: PathBuf::from("/bin/false"),
            version: "none".to_owned(),
        };
        let err = render_session(
            &dir,
            &ff,
            &RenderOptions::default(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(matches!(err, VideoError::Audio(_)), "{err:?}");
    }

    fn fake(path: PathBuf) -> Ffmpeg {
        Ffmpeg {
            path,
            version: "fake".to_owned(),
        }
    }

    #[test]
    fn stop_interrupts_an_ffmpeg_that_stopped_reading() {
        let s = Scratch::new("wedged");
        let dir = fixture_session(&s.0);
        // Writes the part file, then never reads a frame: the pipe fills and
        // the frame write blocks.
        let ff = fake(script(
            &s.0,
            "ffmpeg",
            "for a; do last=$a; done\necho junk > \"${last#file:}\"\nexec sleep 30\n",
        ));
        let stop = AtomicBool::new(false);
        let t = std::time::Instant::now();
        let err = thread::scope(|sc| {
            sc.spawn(|| {
                thread::sleep(Duration::from_millis(500));
                stop.store(true, Ordering::Release);
            });
            render_session(&dir, &ff, &RenderOptions::default(), &stop).unwrap_err()
        });
        assert!(matches!(err, VideoError::Stopped), "{err:?}");
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
        assert!(!dir.join(VIDEO_PART).exists());
        assert!(!dir.join(VIDEO_FILE).exists());
    }

    /// A fake `ffmpeg` that writes `mp4` to its part file, reads `frames`
    /// 1080p frames, and exits 0 without reading the rest.
    fn reads_then_exits(dir: &Path, frames: u64) -> Ffmpeg {
        let Size {
            width: w,
            height: h,
        } = LADDER[0].size;
        let bytes = frames * u64::from(w) * u64::from(h) * 4;
        fake(script(
            dir,
            "ffmpeg",
            &format!(
                "for a; do last=$a; done\necho mp4 > \"${{last#file:}}\"\nhead -c {bytes} > /dev/null\nexit 0\n"
            ),
        ))
    }

    #[test]
    fn a_clean_exit_after_it_stops_reading_is_a_video() {
        let s = Scratch::new("shortest");
        let dir = s.0.join("session");
        // A 6 s session: 180 frames.
        synthetic_session(&dir, 6_000, &[]);
        // As `-shortest` does: the output is done half a second early, and
        // it exits 0 without reading the rest of the frames.
        let ff = reads_then_exits(&s.0, 165);
        render_session(
            &dir,
            &ff,
            &RenderOptions::default(),
            &AtomicBool::new(false),
        )
        .expect("a finished video");
        assert_eq!(
            std::fs::read(dir.join(VIDEO_FILE)).expect("video"),
            b"mp4\n"
        );
        assert!(!dir.join(VIDEO_PART).exists());
    }

    #[test]
    fn a_clean_exit_far_short_of_the_end_is_an_error() {
        let s = Scratch::new("tooshort");
        let dir = s.0.join("session");
        synthetic_session(&dir, 6_000, &[]);
        // Two seconds of frames never read: more than `-shortest` explains.
        let ff = reads_then_exits(&s.0, 120);
        let err = render_session(
            &dir,
            &ff,
            &RenderOptions::default(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(matches!(err, VideoError::Ffmpeg { .. }), "{err:?}");
        assert!(
            err.to_string()
                .contains("exited after reading 120 of 180 frames, 2.0 s short"),
            "{err}"
        );
        assert!(!dir.join(VIDEO_PART).exists());
        assert!(!dir.join(VIDEO_FILE).exists());
    }

    #[test]
    fn exactly_one_second_short_is_still_a_video() {
        let s = Scratch::new("onesec");
        let dir = s.0.join("session");
        // 180 frames at 30 fps; 30 never read is the most `-shortest` may
        // explain.
        synthetic_session(&dir, 6_000, &[]);
        let ff = reads_then_exits(&s.0, 150);
        render_session(
            &dir,
            &ff,
            &RenderOptions::default(),
            &AtomicBool::new(false),
        )
        .expect("30 frames short is a finished video");
        assert!(dir.join(VIDEO_FILE).exists());
    }

    #[test]
    fn one_frame_past_a_second_short_is_an_error() {
        let s = Scratch::new("onesecplus");
        let dir = s.0.join("session");
        synthetic_session(&dir, 6_000, &[]);
        let ff = reads_then_exits(&s.0, 149);
        let err = render_session(
            &dir,
            &ff,
            &RenderOptions::default(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("exited after reading 149 of 180 frames, 1.0 s short"),
            "{err}"
        );
        assert!(!dir.join(VIDEO_PART).exists());
        assert!(!dir.join(VIDEO_FILE).exists());
    }

    #[test]
    fn a_panicking_render_leaves_no_part_file() {
        let s = Scratch::new("panic");
        let dir = fixture_session(&s.0);
        // Writes the part file, then swallows frames until killed.
        let fake = script(
            &s.0,
            "ffmpeg",
            "for a; do last=$a; done\necho junk > \"${last#file:}\"\nexec cat > /dev/null\n",
        );
        let ff = Ffmpeg {
            path: fake,
            version: "fake".to_owned(),
        };
        let stop = AtomicBool::new(false);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            render_session_with(&dir, &ff, &RenderOptions::default(), &stop, &mut |n| {
                assert!(n < 10, "a render panics part way");
            })
        }));
        assert!(caught.is_err(), "the render did not panic");
        assert!(!dir.join(VIDEO_PART).exists(), "part file left behind");
        assert!(!dir.join(VIDEO_FILE).exists());
        // The lock went with it: the next render is not refused as busy.
        assert!(remove_leftover_part(&dir).is_ok());
    }

    #[test]
    fn a_session_still_recording_is_refused() {
        let s = Scratch::new("open");
        let dir = fixture_session(&s.0);
        let mut meta = read_meta(&dir).expect("meta");
        meta.ended = None;
        meta.ended_at = None;
        write_meta(&dir, &meta).expect("meta");
        let ff = fake(PathBuf::from("/bin/false"));
        let err = render_session(
            &dir,
            &ff,
            &RenderOptions::default(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(err.to_string().contains("still recording"), "{err}");
    }

    #[test]
    fn a_second_render_of_one_session_is_refused() {
        let s = Scratch::new("busy");
        let dir = fixture_session(&s.0);
        std::fs::write(dir.join(VIDEO_PART), b"theirs").expect("part");
        let held = lock_render(&dir).expect("lock").expect("free");
        let ff = fake(PathBuf::from("/bin/false"));
        let err = render_session(
            &dir,
            &ff,
            &RenderOptions::default(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(matches!(err, VideoError::Busy), "{err:?}");
        // The other render's part file is its own business.
        assert!(dir.join(VIDEO_PART).exists());
        assert!(!remove_leftover_part(&dir).expect("check"), "held");
        assert!(dir.join(VIDEO_PART).exists());
        drop(held);
        assert!(remove_leftover_part(&dir).expect("remove"));
        assert!(!dir.join(VIDEO_PART).exists());
        assert!(!remove_leftover_part(&dir).expect("nothing left"));
    }

    #[test]
    fn threads_are_capped_when_asked() {
        let s = Scratch::new("threads");
        let dir = fixture_session(&s.0);
        let args = s.0.join("args");
        let body = format!("echo \"$@\" > '{}'\nexit 1\n", args.display());
        let ff = fake(script(&s.0, "ffmpeg", &body));
        let opts = RenderOptions {
            my_callsigns: vec!["N0CALL".to_owned()],
            threads: Some(2),
        };
        let _ = render_session(&dir, &ff, &opts, &AtomicBool::new(false));
        let said = std::fs::read_to_string(&args).expect("args");
        assert!(said.contains(" -threads 2 "), "{said}");
        let _ = render_session(
            &dir,
            &ff,
            &RenderOptions::default(),
            &AtomicBool::new(false),
        );
        let said = std::fs::read_to_string(&args).expect("args");
        assert!(!said.contains(" -threads "), "{said}");
    }

    #[test]
    #[ignore = "perf: run with `just netvideo-perf` (release)"]
    fn perf_video_faster_than_real_time() {
        let Some(ff) = ffmpeg_for_tests() else { return };
        let s = Scratch::new("perf");
        let dir = s.0.join("session");
        let overs: Vec<(u64, u64, &str)> = (0..12u64)
            .map(|i| {
                let calls = ["NC0NTL", "N0CALL", "KX0AAA", "KX0BBB-1"];
                (5_000 + i * 24_000, 18_000, calls[(i % 4) as usize])
            })
            .collect();
        synthetic_session(&dir, 300_000, &overs);
        let wall = std::time::Instant::now();
        let report = render_session(
            &dir,
            &ff,
            &RenderOptions::default(),
            &AtomicBool::new(false),
        )
        .expect("a render");
        let wall = wall.elapsed().as_secs_f64();
        println!(
            "video: 300 s session in {wall:.1} s wall ({:.2}× real time); frames {} reused {}",
            wall / 300.0,
            report.frames,
            report.reused
        );
        assert_good_mp4(&ff, &dir, &report);
        assert!(wall < 0.8 * 300.0, "{wall:.1} s");
    }
}
