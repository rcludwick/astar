// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! SSTV pictures decoded from what the operator hears, saved as PNG files
//! beside a `pictures.jsonl` index. Receive only: nothing here can send a
//! picture or key a radio.
//!
//! The decoder (slowrx, through `astar-sstv`) draws a picture only once all
//! of its audio has arrived, so while one airs the state says which mode is
//! coming in and how far along it is, and the pixels appear when it ends.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use astar_iax::rebuild::rebuild_received;
use astar_record::naming::{app_base_name, unique_stem};
use astar_record::packet_tap::{PacketEvent, PacketNetwork, packet_tap_channel};
use astar_record::pcap::PcapWriter;
use astar_record::tap::{PcmChunk, Tap, tap_channel};
use astar_sstv::{Event, Mode, Outcome, Picture, SSTV_TAP_CAPACITY, Worker, decode_rebuilt};
use serde::Serialize;

use crate::sstv_packets::{Captured, Keeper, SSTV_PACKET_BUFFERS, SSTV_PACKET_EVENTS, Then};

/// The index of saved pictures, one JSON line each, in the SSTV folder.
pub const SSTV_INDEX_FILE: &str = "pictures.jsonl";

/// How many pictures [`SstvState::recent`] carries.
pub const SSTV_RECENT_MAX: usize = 10;

/// Where pictures go and how their names read the time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SstvConfig {
    /// The folder pictures and `pictures.jsonl` are written to. Created if
    /// it does not exist.
    pub dir: PathBuf,
    /// The local zone's offset from UTC, for the time in file names.
    pub utc_offset_secs: i32,
}

/// What the station is connected to, captured when a picture starts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SstvContext {
    /// The network, as the app names it: `"AllStar"`, `"M17"`, …
    pub network: String,
    /// The node, reflector or talkgroup.
    pub target: String,
    /// Who is talking, when the network says.
    pub talker: Option<String>,
}

/// One picture that came in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SstvPictureRow {
    /// The picture's number in [`SstvState::seq`]'s count.
    pub seq: u64,
    /// The PNG's full path. `None` when the picture was lost, or when it
    /// could not be written (then [`SstvState::error`] says why).
    pub file: Option<String>,
    /// The mode's display name: `"PD120"`, `"Robot 36"`.
    pub mode: String,
    pub network: String,
    pub target: String,
    pub talker: Option<String>,
    /// Unix milliseconds when the picture started.
    pub time_ms: u64,
    /// The signal ended before the last row; the rows heard were kept.
    pub partial: bool,
    /// Nothing worth keeping arrived: noise, or under half the rows.
    pub lost: bool,
}

/// What [`crate::Station::sstv_state`] reports.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SstvState {
    /// Decoding is on.
    pub enabled: bool,
    /// A picture is airing now.
    pub receiving: bool,
    /// Pictures started since the station was made; the newest one's
    /// number. A change means a new picture.
    pub seq: u64,
    /// The airing picture's mode (display name).
    pub mode: Option<String>,
    pub width: u32,
    pub height: u32,
    /// Rows of the airing picture whose audio has arrived, by the audio's
    /// own clock. The rows themselves are drawn only at the end.
    pub lines: u32,
    /// How long the airing picture takes, in seconds.
    pub expected_secs: Option<f64>,
    /// How long it has been airing, in seconds.
    pub elapsed_secs: Option<f64>,
    /// Unix milliseconds when the airing picture started.
    pub started_ms: Option<u64>,
    /// Who was talking when the airing picture started.
    pub talker: Option<String>,
    /// The picture that ended last, kept or lost.
    pub last: Option<SstvPictureRow>,
    /// Pictures received (not lost), newest first, at most
    /// [`SSTV_RECENT_MAX`].
    pub recent: Vec<SstvPictureRow>,
    /// 20 ms audio chunks the decoder could not keep up with.
    pub dropped: u64,
    /// Why the last picture could not be saved, or the folder not made.
    /// Cleared by the next picture saved.
    pub error: Option<String>,
}

/// The pixels of the picture that ended last, as RGBA.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SstvFrame {
    /// The picture's [`SstvPictureRow::seq`].
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    /// Rows heard: `height` for a whole picture, fewer for a partial one,
    /// whose missing rows are black.
    pub lines: u32,
    /// `width * height * 4` bytes, alpha always 255.
    pub rgba: Vec<u8>,
}

/// The slot's state, shared with the sink on the decoder thread. Every lock
/// on it is short: no file is written and no thread joined while it is held.
#[derive(Default)]
struct Shared {
    context: SstvContext,
    seq: u64,
    receiving: Option<Receiving>,
    last: Option<SstvPictureRow>,
    recent: VecDeque<SstvPictureRow>,
    frame: Option<Arc<SstvFrame>>,
    error: Option<String>,
}

/// The picture airing now.
struct Receiving {
    /// Which worker it belongs to ([`SstvSlot::runs`]), and that worker's
    /// own number for it.
    run: u64,
    worker_seq: u64,
    seq: u64,
    mode: Mode,
    width: u32,
    height: u32,
    started_ms: u64,
    started_at: Instant,
    expected_secs: f64,
    talker: Option<String>,
}

/// When a hold stops: half again the picture's length, and five seconds
/// for the decoder to say it ended.
fn hold_deadline(r: &Receiving) -> Instant {
    r.started_at + Duration::from_secs_f64((r.expected_secs.max(0.0) * 1.5 + 5.0).min(86_400.0))
}

/// The latest a picture's packets are kept to: half again its length, and
/// ten seconds, from when it started.
fn packets_cap(started_at: Instant, expected_secs: f64) -> Instant {
    started_at + Duration::from_secs_f64((expected_secs.max(0.0) * 1.5 + 10.0).min(86_400.0))
}

/// Whether a picture heard on `network` (as the app names it) can have
/// packets: on `AllStar`, or with no network named (a node's own calls,
/// which are all `AllStar`).
fn keeps_packets(network: &str) -> bool {
    network.is_empty() || network.eq_ignore_ascii_case(PacketNetwork::AllStar.as_str())
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A picture between its start and its end, with what it was captured with.
struct Pending {
    seq: u64,
    mode: Mode,
    context: SstvContext,
    start: SystemTime,
}

/// Saves a picture that has ended and reports it. Runs on the worker's
/// thread, or for a picture with packets, on a thread of the keeper's.
#[derive(Clone)]
struct Finisher {
    dir: PathBuf,
    utc_offset_secs: i32,
    shared: Arc<Mutex<Shared>>,
}

/// Turns a worker's outcomes into files and state. Runs on the worker's
/// thread.
pub(crate) struct Sink {
    run: u64,
    finisher: Finisher,
    shared: Arc<Mutex<Shared>>,
    /// The session's "hold shrink" cell. Written only under `shared`'s lock,
    /// and always to "is a picture airing", so two workers (one stopping,
    /// one new) can never leave it set with nothing airing.
    hold: Arc<AtomicBool>,
    pending: Option<Pending>,
    /// The `AllStar` packets behind each picture, while SSTV is on. Dropped
    /// with the sink, which hands on every picture that has ended.
    keeper: Option<Keeper>,
}

impl Sink {
    pub(crate) fn handle(&mut self, outcome: Outcome, now: SystemTime, at: Instant) {
        match outcome {
            Outcome::Started {
                seq: worker_seq,
                mode,
                width,
                height,
                expected_secs,
                ..
            } => {
                let mut sh = lock(&self.shared);
                sh.seq += 1;
                let seq = sh.seq;
                let context = sh.context.clone();
                sh.receiving = Some(Receiving {
                    run: self.run,
                    worker_seq,
                    seq,
                    mode,
                    width,
                    height,
                    started_ms: unix_ms(now),
                    started_at: at,
                    expected_secs,
                    talker: context.talker.clone(),
                });
                // Lock-free for the audio thread, which only loads it.
                self.hold.store(true, Ordering::Relaxed);
                drop(sh);
                // Only an AllStar picture has packets to keep. One heard on
                // another network while an AllStar call carries traffic is
                // finished exactly as before, never pinned.
                if let Some(keeper) = &self.keeper
                    && keeps_packets(&context.network)
                {
                    keeper.pin(seq, at, packets_cap(at, expected_secs));
                }
                self.pending = Some(Pending {
                    seq,
                    mode,
                    context,
                    start: now,
                });
            }
            Outcome::Finished { picture, .. } | Outcome::Abandoned { picture, .. } => {
                self.ended(Some(picture), now, at);
            }
            Outcome::Rejected { .. } => self.ended(None, now, at),
        }
    }

    /// The picture's end. It stops airing now. It is saved (when worth
    /// keeping) now too, unless `AllStar` packets arrived while it aired:
    /// then the keeper waits for the last of them, and the picture rebuilt
    /// from them is decoded and weighed against this one first.
    fn ended(&mut self, picture: Option<Picture>, now: SystemTime, at: Instant) {
        let pending = self.pending.take().unwrap_or_else(|| {
            // An end with no start never happens; number it all the same.
            let mut sh = lock(&self.shared);
            sh.seq += 1;
            Pending {
                seq: sh.seq,
                mode: picture.as_ref().map_or(Mode::Other, |p| p.mode),
                context: sh.context.clone(),
                start: now,
            }
        });
        {
            let mut sh = lock(&self.shared);
            if sh
                .receiving
                .as_ref()
                .is_some_and(|r| r.run == self.run && r.seq == pending.seq)
            {
                sh.receiving = None;
            }
            self.hold.store(sh.receiving.is_some(), Ordering::Relaxed);
        }
        let seq = pending.seq;
        let finisher = self.finisher.clone();
        let then: Then =
            Box::new(move |captured| finisher.finish(&pending, picture, captured.as_ref()));
        let handed_back = match &self.keeper {
            Some(keeper) => keeper.finish(seq, at, then).err(),
            None => Some(then),
        };
        // No packets (another network, or none kept): saved here and now,
        // exactly as before packets were kept.
        if let Some(then) = handed_back {
            then(None);
        }
    }
}

impl Drop for Sink {
    /// The worker thread is gone, however it went (a panic included): a
    /// picture it started but never ended is over, so release the hold.
    fn drop(&mut self) {
        if self.pending.is_some() {
            self.ended(None, SystemTime::now(), Instant::now());
        }
    }
}

impl Finisher {
    /// Save `live` (or the picture rebuilt from `captured`, when that is
    /// better) and report it. With no packets this is exactly what happened
    /// before any were kept.
    fn finish(&self, pending: &Pending, live: Option<Picture>, captured: Option<&Captured>) {
        let mut errors = Vec::new();
        // A rebuilt picture is held to the same bar as a live one: the
        // packets of a picture the sender gave up on rebuild it no better.
        let rebuilt = captured
            .and_then(|c| match rebuild(c, pending.mode) {
                Ok(p) => p,
                Err(e) => {
                    errors.push(e);
                    None
                }
            })
            .filter(Picture::worth_keeping);
        let live_kept = live.as_ref().is_some_and(Picture::worth_keeping);
        let mut row = SstvPictureRow {
            seq: pending.seq,
            file: None,
            mode: pending.mode.name().to_owned(),
            network: pending.context.network.clone(),
            target: pending.context.target.clone(),
            talker: pending.context.talker.clone(),
            time_ms: unix_ms(pending.start),
            partial: live.as_ref().is_some_and(|p| p.partial),
            lost: true,
        };
        let saved = match (live.filter(|_| live_kept), rebuilt) {
            (None, None) => None,
            (live, rebuilt) => Some(save(
                &self.dir,
                self.utc_offset_secs,
                pending,
                Choice { live, rebuilt },
                captured,
            )),
        };
        let frame = saved
            .as_ref()
            .and_then(|s| s.picture.as_ref())
            .map(|p| Arc::new(rgba_frame(pending.seq, p)));

        let mut sh = lock(&self.shared);
        // Pictures rebuilt from packets finish on threads of their own, so
        // a later one can finish first. Everything here goes by the
        // picture's number: an older picture finishing late joins `recent`
        // in its place and leaves `last` and the frame to the newer one,
        // and `error` too unless the newer one had none.
        let newest = sh.last.as_ref().is_none_or(|l| l.seq <= row.seq);
        let saved_ok = saved.is_some();
        if let Some(saved) = saved {
            if let Some(p) = &saved.picture {
                row.partial = p.partial;
            }
            row.lost = false;
            row.file = saved.file.map(|f| f.to_string_lossy().into_owned());
            errors.extend(saved.error);
            let at = sh
                .recent
                .iter()
                .position(|r| r.seq < row.seq)
                .unwrap_or(sh.recent.len());
            sh.recent.insert(at, row.clone());
            sh.recent.truncate(SSTV_RECENT_MAX);
        }
        if newest && (saved_ok || !errors.is_empty()) {
            sh.error = (!errors.is_empty()).then(|| errors.join("; "));
        } else if !newest && !errors.is_empty() && sh.error.is_none() {
            // An older picture's trouble is still news when the newest had
            // none; it says which picture, since `last` is another one.
            sh.error = Some(format!("picture {}: {}", row.seq, errors.join("; ")));
        }
        // A lost picture (a false start, a short abandon) leaves the last
        // good picture on screen.
        if let Some(frame) = frame
            && sh.frame.as_ref().is_none_or(|f| f.seq <= frame.seq)
        {
            sh.frame = Some(frame);
        }
        if newest {
            sh.last = Some(row);
        }
    }
}

/// Rebuild the picture from its packets: put them in the sender's order,
/// turn each lost frame into a known hole, and decode that. The picture is
/// the first one of `mode` (any, for [`Mode::Other`]) the audio holds.
fn rebuild(captured: &Captured, mode: Mode) -> Result<Option<Picture>, String> {
    let failed = |e: &dyn std::fmt::Display| {
        format!("couldn't rebuild an SSTV picture from its packets: {e}")
    };
    let audio = rebuild_received(&captured.records, captured.codec(), captured.local)
        .map_err(|e| failed(&e))?;
    let events =
        decode_rebuilt(audio.rate, &audio.samples, &audio.holes).map_err(|e| failed(&e))?;
    Ok(events.into_iter().find_map(|e| match e {
        Event::Finished(p) | Event::Abandoned(p) if mode == Mode::Other || p.mode == mode => {
            Some(p)
        }
        _ => None,
    }))
}

/// Whether the picture rebuilt from packets beats the live one: clearer
/// sync (quality), then more rows heard, then fewer rows made up. A tie
/// keeps the live one.
fn rebuilt_is_better(rebuilt: &Picture, live: Option<&Picture>) -> bool {
    if !rebuilt.worth_keeping() {
        return false;
    }
    let Some(live) = live else {
        return true;
    };
    match rebuilt.quality.total_cmp(&live.quality) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => {
            (rebuilt.lines, std::cmp::Reverse(rebuilt.repaired_rows))
                > (live.lines, std::cmp::Reverse(live.repaired_rows))
        }
    }
}

/// The two decodes of one picture: the live one and the one rebuilt from
/// packets, each only when worth keeping. At least one is set.
struct Choice {
    live: Option<Picture>,
    rebuilt: Option<Picture>,
}

/// Which of a [`Choice`]'s two pictures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Which {
    Live,
    Rebuilt,
}

impl Which {
    /// Where the picture came from, as `pictures.jsonl` says it.
    fn source(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Rebuilt => "packets",
        }
    }
}

impl Choice {
    /// The picture to keep. With `packets_saved` false the live one is
    /// kept whenever there is one: a failed write keeps the live picture.
    fn pick(&self, packets_saved: bool) -> Which {
        let better = self
            .rebuilt
            .as_ref()
            .is_some_and(|r| rebuilt_is_better(r, self.live.as_ref()));
        match (&self.live, &self.rebuilt) {
            (Some(_), Some(_)) if better && packets_saved => Which::Rebuilt,
            (Some(_), _) => Which::Live,
            (None, Some(_)) => Which::Rebuilt,
            (None, None) => unreachable!("a choice holds at least one picture"),
        }
    }

    fn get(&self, which: Which) -> &Picture {
        match which {
            Which::Live => self.live.as_ref(),
            Which::Rebuilt => self.rebuilt.as_ref(),
        }
        .expect("picked from what the choice holds")
    }

    fn take(self, which: Which) -> Picture {
        match which {
            Which::Live => self.live,
            Which::Rebuilt => self.rebuilt,
        }
        .expect("picked from what the choice holds")
    }
}

/// What became of writing a picture: the PNG if it was written, the
/// picture kept, and why something failed.
struct Saved {
    file: Option<PathBuf>,
    picture: Option<Picture>,
    error: Option<String>,
}

/// `<network> <mode token>[ <talker>]`; `app_base_name` sanitises it.
fn file_label(network: &str, mode: Mode, talker: Option<&str>) -> String {
    [network, mode.token(), talker.unwrap_or("")]
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// One line of `pictures.jsonl`. The packet fields are left out for a
/// picture with none, so its line is what it always was.
#[derive(Serialize)]
struct IndexLine<'a> {
    file: &'a str,
    time_ms: u64,
    network: &'a str,
    target: &'a str,
    mode: &'a str,
    talker: Option<&'a str>,
    partial: bool,
    /// The `.pcap` beside the PNG, holding the packets the picture aired in.
    #[serde(skip_serializing_if = "Option::is_none")]
    packets: Option<&'a str>,
    /// The codec the call agreed on, by its wire name (`"ulaw"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    codec: Option<&'a str>,
    /// Which decode was kept: `"live"` (the audio as heard) or `"packets"`
    /// (rebuilt from the packets).
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'a str>,
}

/// `path` with `ext` added (`stem` + `.png`).
fn with_ext(stem: &Path, ext: &str) -> PathBuf {
    let mut p = stem.as_os_str().to_owned();
    p.push(ext);
    PathBuf::from(p)
}

/// Write `captured` as a pcap at `path`, which must not exist yet; a
/// half-written file is removed.
fn write_packets(path: &Path, captured: &Captured) -> io::Result<()> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let written = (|| {
        let mut pcap = PcapWriter::new(io::BufWriter::new(file))?;
        for r in &captured.records {
            match pcap.write_packet(r.ts_ns, r.src, r.dst, &r.payload) {
                // A packet the format can't hold is left out, as the
                // packet log does.
                Err(e) if e.kind() == io::ErrorKind::InvalidInput => {}
                other => {
                    other?;
                }
            }
        }
        let mut out = pcap.into_inner();
        io::Write::flush(&mut out)?;
        out.into_inner()
            .map_err(io::IntoInnerError::into_error)?
            .sync_all()
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(path);
    }
    written
}

/// Write the PNG under a name no other file has (and the packets beside
/// it, first, when there are some), then index it.
fn save(
    dir: &Path,
    utc_offset_secs: i32,
    pending: &Pending,
    choice: Choice,
    captured: Option<&Captured>,
) -> Saved {
    save_with(
        dir,
        utc_offset_secs,
        pending,
        choice,
        captured,
        &mut write_packets,
        &mut Picture::write_png,
    )
}

/// [`save`], with the two writes passed in, so a test can fail them.
fn save_with(
    dir: &Path,
    utc_offset_secs: i32,
    pending: &Pending,
    choice: Choice,
    captured: Option<&Captured>,
    write_packets: &mut dyn FnMut(&Path, &Captured) -> io::Result<()>,
    write_png: &mut dyn FnMut(&Picture, &Path) -> io::Result<()>,
) -> Saved {
    let failed = |e: &io::Error| format!("couldn't save an SSTV picture in {}: {e}", dir.display());
    let label = file_label(
        &pending.context.network,
        pending.mode,
        pending.context.talker.as_deref(),
    );
    let base = app_base_name(pending.start, utc_offset_secs, &label);
    let exts: &[&str] = if captured.is_some() {
        &[".png", ".pcap"]
    } else {
        &[".png"]
    };
    let mut which = choice.pick(false);
    let mut written = None;
    let mut packets = None;
    let mut packets_err = None;
    let mut last_err = None;
    // `unique_stem` only looks, so a file made between the look and the
    // write takes another turn.
    for _ in 0..8 {
        let stem = unique_stem(dir, &base, exts);
        if let Some(c) = captured
            && packets_err.is_none()
        {
            let path = with_ext(&stem, ".pcap");
            match write_packets(&path, c) {
                Ok(()) => packets = Some(path),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    packets_err = Some(format!(
                        "couldn't save an SSTV picture's packets in {}: {e}",
                        dir.display()
                    ));
                }
            }
        }
        // Picked again on every turn: the packets a rebuilt picture stands
        // on may have been saved on one turn and not the next.
        which = choice.pick(packets.is_some());
        let path = with_ext(&stem, ".png");
        match write_png(choice.get(which), &path) {
            Ok(()) => {
                written = Some(path);
                break;
            }
            Err(e) => {
                // The packets go with the picture; a new name takes both.
                if let Some(p) = packets.take() {
                    let _ = std::fs::remove_file(p);
                }
                let again = e.kind() == io::ErrorKind::AlreadyExists;
                last_err = Some(e);
                // `write_png` removes a half-written file it made itself,
                // and never touches one it did not make.
                if !again {
                    break;
                }
            }
        }
    }
    let Some(path) = written else {
        let e = last_err.unwrap_or_else(|| io::Error::other("no free file name"));
        let error = [packets_err, Some(failed(&e))]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("; ");
        // Received, only not saved: it still shows.
        return Saved {
            file: None,
            picture: Some(choice.take(which)),
            error: Some(error),
        };
    };
    let picture = choice.take(which);
    let name_of = |p: &Path| {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let name = name_of(&path);
    let packets_name = packets.as_deref().map(name_of);
    let line = IndexLine {
        file: &name,
        time_ms: unix_ms(pending.start),
        network: &pending.context.network,
        target: &pending.context.target,
        mode: pending.mode.name(),
        talker: pending.context.talker.as_deref(),
        partial: picture.partial,
        packets: packets_name.as_deref(),
        codec: captured.and_then(|c| c.codec_name),
        source: captured.map(|_| which.source()),
    };
    let index_err = append_index(dir, &line).err().map(|e| failed(&e));
    let errors: Vec<String> = [packets_err, index_err].into_iter().flatten().collect();
    Saved {
        file: Some(path),
        picture: Some(picture),
        error: (!errors.is_empty()).then(|| errors.join("; ")),
    }
}

/// One line onto `pictures.jsonl`: a link in its place is refused, a torn
/// last line trimmed first, and the line synced.
fn append_index(dir: &Path, line: &IndexLine<'_>) -> io::Result<()> {
    let text = serde_json::to_vec(line).map_err(io::Error::other)?;
    astar_record::jsonl::append_line(&dir.join(SSTV_INDEX_FILE), &text)
}

fn rgba_frame(seq: u64, picture: &Picture) -> SstvFrame {
    let mut rgba = Vec::with_capacity(picture.rgb.len() / 3 * 4);
    for px in picture.rgb.as_chunks::<3>().0 {
        rgba.extend_from_slice(px);
        rgba.push(255);
    }
    SstvFrame {
        seq,
        width: picture.width,
        height: picture.height,
        lines: picture.lines,
        rgba,
    }
}

/// A decoder running for one config.
struct Running {
    cfg: SstvConfig,
    run: u64,
    worker: Worker,
    dropped: Arc<AtomicU64>,
}

/// The station's SSTV decoding. Lock order is `Station::sstv_change` →
/// `Station::sstv` → `Shared`, and `Station::sstv_change` → `session`; the
/// slot itself is never held across a file system call, a join or the
/// session lock, so [`crate::Station::sstv_state`] never waits on them.
#[derive(Default)]
pub(crate) struct SstvSlot {
    running: Option<Running>,
    /// Workers started so far; numbers each one.
    runs: u64,
    shared: Arc<Mutex<Shared>>,
    /// The session's "hold shrink" cell: `true` while a picture airs, so the
    /// receive jitter buffers shed no frame from it. See
    /// [`SstvSlot::share_hold`].
    hold: Arc<AtomicBool>,
    /// Chunks dropped by the taps of workers since replaced.
    dropped_before: u64,
    /// Threads stopping replaced workers.
    stoppers: Vec<JoinHandle<()>>,
}

impl SstvSlot {
    /// Drive `hold` (the session's "hold shrink" cell) from now on. Takes
    /// effect for workers started after this.
    ///
    /// `sstv_configure` calls this on every configure, though the session's
    /// cell is the same one for the session's whole life; re-sharing it is
    /// harmless and keeps the constructors unchanged.
    pub(crate) fn share_hold(&mut self, hold: Arc<AtomicBool>) {
        self.hold = hold;
    }

    pub(crate) fn is_same(&self, cfg: Option<&SstvConfig>) -> bool {
        match (cfg, &self.running) {
            (Some(cfg), Some(run)) => *cfg == run.cfg,
            _ => false,
        }
    }

    /// A sink for a new worker on `cfg`.
    fn sink(&mut self, cfg: &SstvConfig) -> Sink {
        self.runs += 1;
        Sink {
            run: self.runs,
            finisher: Finisher {
                dir: cfg.dir.clone(),
                utc_offset_secs: cfg.utc_offset_secs,
                shared: Arc::clone(&self.shared),
            },
            shared: Arc::clone(&self.shared),
            hold: Arc::clone(&self.hold),
            pending: None,
            keeper: None,
        }
    }

    /// Replace the running worker with one on `cfg` (none for `None`, or
    /// when `made` says the folder could not be made). Returns the tap the
    /// session should carry from now on, and the old worker, which the
    /// caller stops with [`stop_in_background`] once this slot's lock is
    /// released: stopping finishes the picture in flight.
    pub(crate) fn replace(&mut self, cfg: Option<SstvConfig>, made: io::Result<()>) -> Replaced {
        self.stoppers.retain(|h| !h.is_finished());
        let old = self.running.take().map(|old| {
            self.dropped_before += old.dropped.load(Ordering::Relaxed);
            old.worker
        });
        let (taps, result) = self.start(cfg, made);
        let (tap, packet_tap) = taps.unzip();
        let packet_tap = packet_tap.flatten();
        if tap.is_none() {
            // Off (or failed to start): nothing listens any more, so voice
            // goes back to shedding at once, not when the old worker has
            // finished the picture it was decoding.
            self.release_hold();
        }
        Replaced {
            tap,
            packet_tap,
            result,
            old,
        }
    }

    /// Release the hold when the airing picture is long overdue: a decoder
    /// that wedged, or never reported an end, must not stop the jitter
    /// buffers shedding for good. Runs off the audio thread (state polls);
    /// the audio thread only ever loads the cell.
    fn expire_hold(&self, now: Instant) {
        let sh = lock(&self.shared);
        if let Some(r) = &sh.receiving
            && now >= hold_deadline(r)
        {
            self.hold.store(false, Ordering::Relaxed);
        }
    }

    /// Let the receive jitter buffers shrink again.
    ///
    /// Takes the `Shared` lock, as `Sink::ended` does, so a late `Started`
    /// from an old worker cannot interleave. Lock order: the caller holds
    /// at most `Station::sstv`/`sstv_change`; `Shared` is always innermost.
    /// Also runs from `Drop`, where nothing else of ours is held.
    fn release_hold(&self) {
        let _sh = lock(&self.shared);
        self.hold.store(false, Ordering::Relaxed);
    }

    fn start(
        &mut self,
        cfg: Option<SstvConfig>,
        made: io::Result<()>,
    ) -> (Option<StartedTaps>, io::Result<()>) {
        let Some(cfg) = cfg else {
            return (None, Ok(()));
        };
        let started = made.and_then(|()| {
            let (tap, rx) = tap_channel(SSTV_TAP_CAPACITY);
            let mut sink = self.sink(&cfg);
            // The packets behind each AllStar picture. Without a thread for
            // them, pictures are decoded and saved just as before.
            let (packet_tap, packet_rx) =
                packet_tap_channel(SSTV_PACKET_EVENTS, SSTV_PACKET_BUFFERS);
            let packet_tap = Keeper::spawn(packet_rx).ok().map(|keeper| {
                sink.keeper = Some(keeper);
                packet_tap
            });
            let worker = Worker::spawn(
                rx,
                Box::new(move |o| sink.handle(o, SystemTime::now(), Instant::now())),
            )?;
            Ok(((tap, packet_tap), worker))
        });
        match started {
            Ok(((tap, packet_tap), worker)) => {
                lock(&self.shared).error = None;
                self.running = Some(Running {
                    cfg,
                    run: self.runs,
                    worker,
                    dropped: tap.dropped_counter(),
                });
                (Some((tap, packet_tap)), Ok(()))
            }
            Err(e) => {
                lock(&self.shared).error = Some(format!(
                    "couldn't start SSTV decoding in {}: {e}",
                    cfg.dir.display()
                ));
                (None, Err(e))
            }
        }
    }

    /// Keep a stopping thread, so tests can wait for it.
    pub(crate) fn add_stopper(&mut self, h: JoinHandle<()>) {
        self.stoppers.retain(|h| !h.is_finished());
        self.stoppers.push(h);
    }

    /// The threads still stopping old workers, for a caller that wants to
    /// wait for them with no lock held.
    #[cfg(test)]
    pub(crate) fn take_stoppers(&mut self) -> Vec<JoinHandle<()>> {
        std::mem::take(&mut self.stoppers)
    }

    pub(crate) fn set_context(&self, ctx: SstvContext) {
        lock(&self.shared).context = ctx;
    }

    /// Is the receive path holding off its jitter-buffer shrink?
    #[cfg(test)]
    pub(crate) fn holding(&self) -> bool {
        self.hold.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn run(&self) -> Option<u64> {
        self.running.as_ref().map(|r| r.run)
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub(crate) fn state(&self) -> SstvState {
        self.state_at(Instant::now())
    }

    /// [`SstvSlot::state`] as of `now`, which also lets go of a hold that
    /// has outlived its picture (see [`hold_deadline`]).
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub(crate) fn state_at(&self, now: Instant) -> SstvState {
        self.expire_hold(now);
        let live = self
            .running
            .as_ref()
            .and_then(|r| r.worker.live_lines().map(|l| (r.run, l)));
        let dropped = self.dropped_before
            + self
                .running
                .as_ref()
                .map_or(0, |r| r.dropped.load(Ordering::Relaxed));
        let sh = lock(&self.shared);
        let mut st = SstvState {
            enabled: self.running.is_some(),
            seq: sh.seq,
            last: sh.last.clone(),
            recent: sh.recent.iter().cloned().collect(),
            dropped,
            error: sh.error.clone(),
            ..SstvState::default()
        };
        if let Some(r) = &sh.receiving {
            let elapsed = r.started_at.elapsed().as_secs_f64();
            let by_clock = if r.expected_secs > 0.0 {
                (elapsed / r.expected_secs * f64::from(r.height)).min(f64::from(r.height)) as u32
            } else {
                0
            };
            st.receiving = true;
            st.mode = Some(r.mode.name().to_owned());
            st.width = r.width;
            st.height = r.height;
            st.lines = match live {
                Some((run, (worker_seq, lines))) if run == r.run && worker_seq == r.worker_seq => {
                    lines
                }
                _ => by_clock,
            };
            st.expected_secs = Some(r.expected_secs);
            st.elapsed_secs = Some(elapsed);
            st.started_ms = Some(r.started_ms);
            st.talker.clone_from(&r.talker);
        }
        st
    }

    /// The shared frame; the caller copies it out with no lock held.
    pub(crate) fn frame_ref(&self) -> Option<Arc<SstvFrame>> {
        lock(&self.shared).frame.clone()
    }

    #[cfg(feature = "test-hooks")]
    pub(crate) fn set_frame_for_test(&self, frame: SstvFrame) {
        lock(&self.shared).frame = Some(Arc::new(frame));
    }

    #[cfg(test)]
    pub(crate) fn frame(&self) -> Option<SstvFrame> {
        self.frame_ref().map(|f| (*f).clone())
    }
}

/// A new worker's heard-audio tap, and its packet tap when the keeper
/// started.
type StartedTaps = (Tap<PcmChunk>, Option<Tap<PacketEvent>>);

/// What [`SstvSlot::replace`] hands back.
pub(crate) struct Replaced {
    /// The tap the session carries from now on.
    pub(crate) tap: Option<Tap<PcmChunk>>,
    /// The packet tap every `AllStar` call offers to from now on.
    pub(crate) packet_tap: Option<Tap<PacketEvent>>,
    pub(crate) result: io::Result<()>,
    /// The worker replaced, still to be stopped.
    pub(crate) old: Option<Worker>,
}

/// Stop `worker` on a thread of its own: `Worker::stop` drains the tap and
/// decodes the picture in flight, which can take seconds. Call it with no
/// lock held.
///
/// The rare fallback: when the OS refuses a new thread, `spawn` drops the
/// closure, and with it the worker, whose `Drop` stops it right here. That
/// blocks the caller for the decode, which is why no lock may be held — in
/// particular not the slot's, so [`crate::Station::sstv_state`] never
/// waits on a decode.
pub(crate) fn stop_in_background(worker: Worker) -> Option<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("astar-sstv-stop".to_owned())
        .spawn(move || worker.stop())
        .ok()
}

impl Drop for SstvSlot {
    /// A station going away must not wait out a picture's decode either.
    /// Nothing else can hold the slot by now, so the fallback in
    /// [`stop_in_background`] blocks no reader.
    fn drop(&mut self) {
        self.release_hold();
        if let Some(old) = self.running.take() {
            let _ = stop_in_background(old.worker);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Station, StationConfig};
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::thread::sleep;
    use std::time::Duration;

    /// 2026-11-12 02:04:12 UTC, which is 19:04:12 at UTC−7.
    const START_SECS: u64 = 1_794_449_052;
    const MST: i32 = -25_200;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "astar-station-sstv-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx(talker: Option<&str>) -> SstvContext {
        SstvContext {
            network: "AllStar".to_owned(),
            target: "1999".to_owned(),
            talker: talker.map(str::to_owned),
        }
    }

    fn picture(mode: Mode, width: u32, height: u32, lines: u32) -> Picture {
        let mut rgb = vec![0u8; (width * height * 3) as usize];
        for (i, b) in rgb
            .iter_mut()
            .enumerate()
            .take((width * lines * 3) as usize)
        {
            *b = u8::try_from(i % 251).unwrap();
        }
        Picture {
            mode,
            width,
            height,
            rgb,
            lines,
            quality: 0.6,
            at_sample: 0,
            partial: lines < height,
            repaired_rows: 0,
        }
    }

    fn started(seq: u64, mode: Mode, width: u32, height: u32) -> Outcome {
        Outcome::Started {
            seq,
            mode,
            width,
            height,
            at_sample: 0,
            rate: 8_000,
            expected_secs: 126.1,
        }
    }

    /// A slot with nothing running and a sink writing to `dir`.
    fn rig(dir: &Path) -> (SstvSlot, Sink) {
        let mut slot = SstvSlot::default();
        let sink = slot.sink(&SstvConfig {
            dir: dir.to_owned(),
            utc_offset_secs: MST,
        });
        (slot, sink)
    }

    /// Start and finish one picture at `secs`.
    fn air(sink: &mut Sink, seq: u64, secs: u64, pic: Picture) {
        sink.handle(
            started(seq, pic.mode, pic.width, pic.height),
            at(secs),
            Instant::now(),
        );
        let outcome = if pic.partial {
            Outcome::Abandoned {
                seq,
                picture: pic,
                rate: 8_000,
            }
        } else {
            Outcome::Finished {
                seq,
                picture: pic,
                rate: 8_000,
            }
        };
        sink.handle(outcome, at(secs + 120), Instant::now());
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| Path::new(n).extension().is_some_and(|e| e == "png"))
            .collect();
        v.sort();
        v
    }

    fn index_lines(dir: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(dir.join(SSTV_INDEX_FILE))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn wait_for(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        while !ok() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            sleep(Duration::from_millis(20));
        }
    }

    fn station() -> Station {
        Station::with_backend_factory(
            StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        )
    }

    #[test]
    fn a_finished_picture_is_saved_named_and_indexed() {
        let dir = tmp("named");
        let (slot, mut sink) = rig(&dir);
        slot.set_context(ctx(Some("N0CALL")));
        air(&mut sink, 1, START_SECS, picture(Mode::Pd120, 4, 2, 2));

        let name = "2026-11-11 19-04-12 AllStar PD120 N0CALL.png";
        assert_eq!(names(&dir), [name]);
        let png = dir.join(name);
        let st = slot.state();
        let last = st.last.clone().expect("last");
        assert_eq!(
            last,
            SstvPictureRow {
                seq: 1,
                file: Some(png.to_string_lossy().into_owned()),
                mode: "PD120".to_owned(),
                network: "AllStar".to_owned(),
                target: "1999".to_owned(),
                talker: Some("N0CALL".to_owned()),
                time_ms: START_SECS * 1000,
                partial: false,
                lost: false,
            }
        );
        assert_eq!(st.recent, [last]);
        assert_eq!(st.seq, 1);
        assert!(!st.receiving);
        assert_eq!(st.error, None);
        let raw = std::fs::read_to_string(dir.join(SSTV_INDEX_FILE)).unwrap();
        assert_eq!(
            raw,
            format!(
                "{{\"file\":\"{name}\",\"time_ms\":{},\"network\":\"AllStar\",\"target\":\"1999\",\"mode\":\"PD120\",\"talker\":\"N0CALL\",\"partial\":false}}\n",
                START_SECS * 1000
            )
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_pictures_in_one_second_do_not_collide() {
        let dir = tmp("collide");
        let (slot, mut sink) = rig(&dir);
        slot.set_context(ctx(Some("N0CALL")));
        air(&mut sink, 1, START_SECS, picture(Mode::Pd120, 4, 2, 2));
        air(&mut sink, 2, START_SECS, picture(Mode::Pd120, 4, 2, 2));
        assert_eq!(
            names(&dir),
            [
                "2026-11-11 19-04-12 AllStar PD120 N0CALL 2.png",
                "2026-11-11 19-04-12 AllStar PD120 N0CALL.png",
            ]
        );
        let files: Vec<_> = index_lines(&dir)
            .iter()
            .map(|v| v["file"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            files,
            [
                "2026-11-11 19-04-12 AllStar PD120 N0CALL.png",
                "2026-11-11 19-04-12 AllStar PD120 N0CALL 2.png",
            ]
        );
        let st = slot.state();
        assert_eq!(st.seq, 2);
        assert_eq!(st.recent.len(), 2);
        assert_eq!(st.recent[0].seq, 2, "newest first");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hostile_talker_makes_a_safe_file_name() {
        let dir = tmp("hostile");
        let (slot, mut sink) = rig(&dir);
        slot.set_context(ctx(Some("../../<x>\u{202e}gnp.exe")));
        air(&mut sink, 1, START_SECS, picture(Mode::Robot36, 4, 2, 2));
        let st = slot.state();
        let file = PathBuf::from(st.last.unwrap().file.expect("saved"));
        assert_eq!(
            file.parent(),
            Some(dir.as_path()),
            "stays inside the folder"
        );
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!name.contains('/') && !name.contains('\\'), "{name}");
        assert!(
            !name
                .chars()
                .any(|c| c.is_control() || ('\u{2000}'..='\u{206f}').contains(&c)),
            "{name:?}"
        );
        assert_eq!(file.extension().and_then(|e| e.to_str()), Some("png"));
        assert!(file.exists());
        // The index keeps what was heard, safely escaped as JSON.
        assert_eq!(
            index_lines(&dir)[0]["talker"].as_str(),
            Some("../../<x>\u{202e}gnp.exe")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_talker_leaves_it_out_of_the_name() {
        let dir = tmp("notalker");
        let (slot, mut sink) = rig(&dir);
        slot.set_context(ctx(None));
        air(&mut sink, 1, START_SECS, picture(Mode::Robot36, 4, 2, 2));
        assert_eq!(names(&dir), ["2026-11-11 19-04-12 AllStar Robot36.png"]);
        let line = &index_lines(&dir)[0];
        assert!(line["talker"].is_null());
        assert_eq!(line["mode"], "Robot 36");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rejected_picture_is_lost_not_saved() {
        let dir = tmp("rejected");
        let (slot, mut sink) = rig(&dir);
        slot.set_context(ctx(Some("N0CALL")));
        sink.handle(
            started(1, Mode::Martin1, 320, 256),
            at(START_SECS),
            Instant::now(),
        );
        assert!(slot.state().receiving);
        sink.handle(
            Outcome::Rejected { seq: 1 },
            at(START_SECS + 115),
            Instant::now(),
        );
        // And an abandon with under half the rows is lost too.
        sink.handle(
            started(2, Mode::Martin1, 4, 4),
            at(START_SECS + 200),
            Instant::now(),
        );
        let short = picture(Mode::Martin1, 4, 4, 1);
        sink.handle(
            Outcome::Abandoned {
                seq: 2,
                picture: short,
                rate: 8_000,
            },
            at(START_SECS + 210),
            Instant::now(),
        );
        assert_eq!(names(&dir), Vec::<String>::new());
        assert_eq!(index_lines(&dir), Vec::<serde_json::Value>::new());
        let st = slot.state();
        assert!(!st.receiving);
        let last = st.last.expect("last");
        assert!(last.lost);
        assert_eq!(last.file, None);
        assert_eq!(last.seq, 2);
        assert_eq!(last.mode, "Martin 1");
        assert!(st.recent.is_empty(), "lost pictures are not received ones");
        assert_eq!(slot.frame(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lost_picture_keeps_the_last_good_frame() {
        let dir = tmp("keepframe");
        let (slot, mut sink) = rig(&dir);
        slot.set_context(ctx(Some("N0CALL")));
        air(&mut sink, 1, START_SECS, picture(Mode::Pd120, 4, 2, 2));
        // A false start: rejected.
        sink.handle(
            started(2, Mode::Martin1, 4, 4),
            at(START_SECS + 200),
            Instant::now(),
        );
        sink.handle(
            Outcome::Rejected { seq: 2 },
            at(START_SECS + 210),
            Instant::now(),
        );
        assert_eq!(slot.frame().map(|f| f.seq), Some(1), "after a rejection");
        // A short abandon: under half the rows.
        sink.handle(
            started(3, Mode::Martin1, 4, 4),
            at(START_SECS + 300),
            Instant::now(),
        );
        sink.handle(
            Outcome::Abandoned {
                seq: 3,
                picture: picture(Mode::Martin1, 4, 4, 1),
                rate: 8_000,
            },
            at(START_SECS + 310),
            Instant::now(),
        );
        let st = slot.state();
        assert!(st.last.expect("last").lost);
        assert_eq!(
            slot.frame().map(|f| f.seq),
            Some(1),
            "after a short abandon"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_index_after_a_torn_tail_gives_two_parseable_rows() {
        let dir = tmp("torn");
        let (slot, mut sink) = rig(&dir);
        slot.set_context(ctx(Some("N0CALL")));
        air(&mut sink, 1, START_SECS, picture(Mode::Pd120, 4, 2, 2));
        // A write cut short leaves half a line.
        let mut f = OpenOptions::new()
            .append(true)
            .open(dir.join(SSTV_INDEX_FILE))
            .unwrap();
        f.write_all(b"{\"file\":\"half").unwrap();
        drop(f);
        air(
            &mut sink,
            2,
            START_SECS + 300,
            picture(Mode::Pd120, 4, 2, 2),
        );
        let rows = index_lines(&dir);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(slot.state().error, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_index_is_refused_and_its_target_untouched() {
        let dir = tmp("symlink");
        let target = dir.join("elsewhere.txt");
        std::fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, dir.join(SSTV_INDEX_FILE)).unwrap();
        let (slot, mut sink) = rig(&dir);
        slot.set_context(ctx(Some("N0CALL")));
        air(&mut sink, 1, START_SECS, picture(Mode::Pd120, 4, 2, 2));
        let st = slot.state();
        assert!(st.error.is_some(), "{st:?}");
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        assert!(
            st.last.expect("last").file.is_some(),
            "the PNG is still saved"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn an_unwritable_folder_reports_and_keeps_decoding() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("unwritable");
        let (slot, mut sink) = rig(&dir);
        slot.set_context(ctx(Some("N0CALL")));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        air(&mut sink, 1, START_SECS, picture(Mode::Pd120, 4, 2, 2));
        let st = slot.state();
        assert!(st.error.is_some(), "{st:?}");
        let last = st.last.expect("last");
        assert_eq!(last.file, None);
        assert!(!last.lost, "it was received, only not saved");
        assert_eq!(slot.frame().map(|f| f.seq), Some(1));

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        air(
            &mut sink,
            2,
            START_SECS + 300,
            picture(Mode::Pd120, 4, 2, 2),
        );
        let st = slot.state();
        assert_eq!(st.error, None, "the next picture saved clears it");
        assert!(st.last.unwrap().file.is_some());
        assert_eq!(names(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_is_secret_free_and_idle_by_default() {
        let st = station().sstv_state();
        assert_eq!(st, SstvState::default());
        assert!(!st.enabled && !st.receiving);
        let json = serde_json::to_value(&st).unwrap();
        let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "dropped",
                "elapsed_secs",
                "enabled",
                "error",
                "expected_secs",
                "height",
                "last",
                "lines",
                "mode",
                "receiving",
                "recent",
                "seq",
                "started_ms",
                "talker",
                "width",
            ]
        );
        let text = json.to_string().to_lowercase();
        for word in ["secret", "pass", "token", "key"] {
            assert!(!text.contains(word), "{word} in {text}");
        }
        assert_eq!(station().sstv_frame(), None);
    }

    #[test]
    fn receiving_reports_mode_and_progress() {
        let dir = tmp("receiving");
        let (slot, mut sink) = rig(&dir);
        slot.set_context(ctx(Some("N0CALL")));
        sink.handle(
            started(1, Mode::Pd120, 640, 496),
            at(START_SECS),
            Instant::now().checked_sub(Duration::from_secs(3)).unwrap(),
        );
        // The talker changing mid-picture does not rewrite the picture's.
        slot.set_context(ctx(Some("KX0AAA")));
        let st = slot.state();
        assert!(st.receiving);
        assert_eq!(st.seq, 1);
        assert_eq!(st.mode.as_deref(), Some("PD120"));
        assert_eq!((st.width, st.height), (640, 496));
        assert_eq!(st.expected_secs, Some(126.1));
        assert!(st.elapsed_secs.unwrap() >= 3.0);
        assert_eq!(st.started_ms, Some(START_SECS * 1000));
        assert_eq!(st.talker.as_deref(), Some("N0CALL"));
        // No worker here, so rows come from the wall clock: 3 s of 126 s.
        assert!(st.lines >= 11 && st.lines < 40, "{}", st.lines);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn frame_is_rgba_of_the_last_finished_picture() {
        let dir = tmp("frame");
        let (slot, mut sink) = rig(&dir);
        assert_eq!(slot.frame(), None);
        let pic = picture(Mode::Robot36, 3, 2, 2);
        let rgb = pic.rgb.clone();
        air(&mut sink, 1, START_SECS, pic);
        let frame = slot.frame().expect("frame");
        assert_eq!(
            (frame.seq, frame.width, frame.height, frame.lines),
            (1, 3, 2, 2)
        );
        assert_eq!(frame.rgba.len(), 3 * 2 * 4);
        for (px, rgba) in rgb.chunks(3).zip(frame.rgba.chunks(4)) {
            assert_eq!(&rgba[..3], px);
            assert_eq!(rgba[3], 255);
        }
        // A picture starting does not take the last one's pixels away.
        sink.handle(
            started(2, Mode::Robot36, 3, 2),
            at(START_SECS + 60),
            Instant::now(),
        );
        assert_eq!(slot.frame().map(|f| f.seq), Some(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn configure_same_twice_keeps_the_worker() {
        let dir = tmp("same");
        let st = station();
        let cfg = SstvConfig {
            dir: dir.join("pictures"),
            utc_offset_secs: MST,
        };
        st.sstv_configure(Some(cfg.clone())).unwrap();
        assert!(cfg.dir.is_dir(), "the folder is made");
        assert!(st.sstv_state().enabled);
        let run = st.sstv_run();
        let tap = st.sstv_tap().expect("a tap on the session");
        st.sstv_configure(Some(cfg.clone())).unwrap();
        assert_eq!(st.sstv_run(), run, "same config is a no-op");
        assert!(
            tap.offer(PcmChunk {
                rate: 8_000,
                samples: vec![0; 160],
            }),
            "the same tap still feeds a live worker"
        );

        st.sstv_configure(Some(SstvConfig {
            utc_offset_secs: 0,
            ..cfg
        }))
        .unwrap();
        assert_ne!(st.sstv_run(), run, "a new config starts a new worker");

        st.sstv_configure(None).unwrap();
        assert!(!st.sstv_state().enabled);
        assert!(st.sstv_tap().is_none(), "off removes the tap");
        st.sstv_wait_stopped();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn configure_reports_a_folder_it_cannot_make() {
        let dir = tmp("cannot");
        let file = dir.join("a-file");
        std::fs::write(&file, b"x").unwrap();
        let st = station();
        let err = st
            .sstv_configure(Some(SstvConfig {
                dir: file.join("pictures"),
                utc_offset_secs: 0,
            }))
            .unwrap_err();
        assert!(matches!(err, crate::StationError::Record(_)), "{err:?}");
        let state = st.sstv_state();
        assert!(!state.enabled);
        assert!(state.error.is_some());
        assert!(st.sstv_tap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn robot36_fixture() -> (u32, Vec<i16>) {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../astar-sstv/tests/fixtures/robot36-french-logo-8k.flac");
        let mut reader = claxon::FlacReader::open(path).expect("open flac");
        let rate = reader.streaminfo().sample_rate;
        let samples = reader
            .samples()
            .map(|s| i16::try_from(s.expect("sample")).expect("16-bit"))
            .collect();
        (rate, samples)
    }

    fn feed(tap: &Tap<PcmChunk>, samples: &[i16], rate: u32) {
        for c in samples.chunks((rate / 50) as usize) {
            let chunk = PcmChunk {
                rate,
                samples: c.to_vec(),
            };
            while !tap.offer(chunk.clone()) {
                sleep(Duration::from_millis(5));
            }
        }
    }

    /// The receive jitter buffers hold off shedding exactly while a
    /// picture airs: Started sets the hold, every kind of end clears it.
    #[test]
    fn a_picture_airing_holds_the_jitter_shrink() {
        let dir = tmp("hold");
        let (slot, mut sink) = rig(&dir);
        assert!(!slot.holding(), "idle");
        let ends: [fn(u64, Picture) -> Outcome; 3] = [
            |seq, picture| Outcome::Finished {
                seq,
                picture,
                rate: 8_000,
            },
            |seq, picture| Outcome::Abandoned {
                seq,
                picture,
                rate: 8_000,
            },
            |seq, _| Outcome::Rejected { seq },
        ];
        for (i, end) in ends.iter().enumerate() {
            let seq = i as u64 + 1;
            let secs = START_SECS + 200 * seq;
            sink.handle(started(seq, Mode::Pd120, 4, 2), at(secs), Instant::now());
            assert!(slot.holding(), "airing #{seq}");
            sink.handle(
                end(seq, picture(Mode::Pd120, 4, 2, 2)),
                at(secs + 120),
                Instant::now(),
            );
            assert!(!slot.holding(), "ended #{seq}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Turning SSTV off mid-picture releases the hold at once, before the
    /// stopping worker has finished the half it heard.
    #[test]
    fn a_hold_with_no_end_lapses_after_its_deadline() {
        let dir = tmp("hold-deadline");
        let (slot, mut sink) = rig(&dir);
        let t0 = Instant::now();
        // 126.1 s picture: deadline 126.1 * 1.5 + 5 = 194.15 s.
        sink.handle(started(1, Mode::Pd120, 4, 2), at(START_SECS), t0);
        assert!(slot.state_at(t0 + Duration::from_secs(194)).receiving);
        assert!(slot.holding(), "still inside the deadline");
        slot.state_at(t0 + Duration::from_secs(195));
        assert!(!slot.holding(), "released once it is overdue");
        // A picture that does start again holds again.
        sink.handle(started(2, Mode::Pd120, 4, 2), at(START_SECS + 400), t0);
        assert!(slot.holding());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sink_that_panics_mid_picture_releases_the_hold() {
        let dir = tmp("hold-panic");
        let (slot, mut sink) = rig(&dir);
        let t = std::thread::spawn(move || {
            sink.handle(
                started(1, Mode::Pd120, 4, 2),
                at(START_SECS),
                Instant::now(),
            );
            panic!("decoder blew up mid-picture");
        });
        assert!(t.join().is_err());
        assert!(!slot.holding(), "released by the thread's exit");
        let st = slot.state();
        assert!(!st.receiving, "no picture airs any more");
        assert!(st.last.is_some_and(|r| r.lost), "it is recorded as lost");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn turning_off_mid_picture_releases_the_hold() {
        let dir = tmp("hold-off");
        let st = station();
        assert!(!st.sstv_shrink_hold());
        st.sstv_configure(Some(SstvConfig {
            dir: dir.clone(),
            utc_offset_secs: MST,
        }))
        .unwrap();
        let (rate, samples) = robot36_fixture();
        let tap = st.sstv_tap().expect("tap");
        feed(&tap, &samples[..samples.len() * 6 / 10], rate);
        drop(tap);
        wait_for("the picture to start", 30, || st.sstv_state().receiving);
        assert!(st.sstv_shrink_hold(), "held while the picture airs");
        st.sstv_configure(None).unwrap();
        assert!(!st.sstv_shrink_hold(), "released when SSTV is off");
        st.sstv_wait_stopped();
        assert!(!st.sstv_shrink_hold(), "and still released once it stops");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn turning_off_mid_picture_keeps_the_half() {
        let dir = tmp("half");
        let st = station();
        st.sstv_set_context(ctx(Some("N0CALL")));
        st.sstv_configure(Some(SstvConfig {
            dir: dir.clone(),
            utc_offset_secs: MST,
        }))
        .unwrap();
        let (rate, samples) = robot36_fixture();
        let tap = st.sstv_tap().expect("tap");
        feed(&tap, &samples[..samples.len() * 6 / 10], rate);
        drop(tap);
        wait_for("the picture to start", 30, || st.sstv_state().receiving);
        let begun = Instant::now();
        st.sstv_configure(None).unwrap();
        assert!(
            begun.elapsed() < Duration::from_millis(500),
            "turning off does not wait for the worker"
        );
        st.sstv_wait_stopped();

        let state = st.sstv_state();
        assert!(!state.enabled && !state.receiving);
        let last = state.last.expect("last");
        assert!(last.partial && !last.lost, "{last:?}");
        assert_eq!(last.mode, "Robot 36");
        let file = PathBuf::from(last.file.expect("saved"));
        assert!(file.exists());
        assert!(
            file.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(" AllStar Robot36 N0CALL.png")
        );
        let line = &index_lines(&dir)[0];
        assert_eq!(line["partial"], true);
        let frame = st.sstv_frame().expect("frame");
        assert_eq!((frame.width, frame.height), (320, 240));
        assert!(frame.lines * 2 >= frame.height && frame.lines < frame.height);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- the packets behind an AllStar picture --------------------------

    use astar_record::pcap::{PcapRecord, read_pcap};
    use std::net::SocketAddr;

    const PEER_CALL: u16 = 0x1234;

    fn peer() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 4570))
    }

    fn local() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 4569))
    }

    /// The Robot 36 fixture as an `AllStar` peer would send it: µ-law mini
    /// frames, 20 ms apart. Frames `swap` and `swap + 1` arrive the wrong way
    /// round, and frame `lost` never arrives.
    fn robot36_packets(swap: usize, lost: usize) -> Vec<PcapRecord> {
        let (rate, samples) = robot36_fixture();
        assert_eq!(rate, 8_000);
        let wall = 1_794_449_052_000_000_000u64;
        let mut frames: Vec<(u64, Vec<u8>)> = samples
            .chunks(160)
            .enumerate()
            .map(|(i, pcm)| {
                let stamp = u16::try_from((i * 20) % 65_536).unwrap();
                let mut b = PEER_CALL.to_be_bytes().to_vec();
                b.extend_from_slice(&stamp.to_be_bytes());
                b.extend(pcm.iter().map(|&x| astar_codec::g711::ulaw_encode(x)));
                (wall + i as u64 * 20_000_000, b)
            })
            .collect();
        frames.swap(swap, swap + 1);
        frames.remove(lost);
        frames
            .into_iter()
            .enumerate()
            .map(|(i, (_, payload))| PcapRecord {
                // Arrival order is the order sent here, 20 ms apart.
                ts_ns: wall + i as u64 * 20_000_000,
                src: peer(),
                dst: local(),
                payload,
            })
            .collect()
    }

    fn captured(codec: Option<&'static str>) -> Captured {
        Captured {
            records: robot36_packets(400, 900),
            codec_name: codec,
            local: Some(local()),
        }
    }

    fn pending(seq: u64) -> Pending {
        Pending {
            seq,
            mode: Mode::Robot36,
            context: ctx(Some("N0CALL")),
            start: at(START_SECS),
        }
    }

    fn finisher(dir: &Path) -> (SstvSlot, Finisher) {
        let (slot, sink) = rig(dir);
        (slot, sink.finisher.clone())
    }

    fn files(dir: &Path, ext: &str) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == ext))
            .collect();
        v.sort();
        v
    }

    fn with_quality(mut p: Picture, quality: f32, repaired_rows: u32) -> Picture {
        p.quality = quality;
        p.repaired_rows = repaired_rows;
        p
    }

    #[test]
    fn the_rebuilt_picture_wins_on_quality_then_rows_then_repairs() {
        let full = picture(Mode::Robot36, 320, 240, 240);
        let live = with_quality(full.clone(), 0.8, 4);
        assert!(rebuilt_is_better(
            &with_quality(full.clone(), 0.9, 9),
            Some(&live)
        ));
        assert!(!rebuilt_is_better(
            &with_quality(full.clone(), 0.7, 0),
            Some(&live)
        ));
        assert!(rebuilt_is_better(
            &with_quality(full.clone(), 0.8, 2),
            Some(&live)
        ));
        assert!(
            !rebuilt_is_better(&with_quality(full.clone(), 0.8, 4), Some(&live)),
            "a tie keeps live"
        );
        let short = picture(Mode::Robot36, 320, 240, 200);
        assert!(!rebuilt_is_better(
            &with_quality(short, 0.8, 0),
            Some(&live)
        ));
        assert!(rebuilt_is_better(&full, None), "nothing live to beat");
        let scrap = picture(Mode::Robot36, 320, 240, 100);
        assert!(!rebuilt_is_better(&scrap, None), "under half the rows");

        let rebuilt = with_quality(full.clone(), 0.95, 0);
        let pick = |saved| {
            Choice {
                live: Some(live.clone()),
                rebuilt: Some(rebuilt.clone()),
            }
            .pick(saved)
            .source()
        };
        assert_eq!(pick(true), "packets");
        assert_eq!(pick(false), "live", "a failed write keeps the live picture");
    }

    /// The rebuilt picture beats a poor live one: the PNG is the rebuilt
    /// picture, the packets sit beside it, and the row says so.
    #[test]
    fn packets_are_saved_beside_the_png_and_the_better_picture_kept() {
        let dir = tmp("packets-win");
        let (slot, fin) = finisher(&dir);
        let live = with_quality(picture(Mode::Robot36, 320, 240, 240), 0.2, 30);
        fin.finish(&pending(1), Some(live), Some(&captured(Some("ulaw"))));

        let pngs = files(&dir, "png");
        let pcaps = files(&dir, "pcap");
        assert_eq!(pngs.len(), 1);
        assert_eq!(
            pcaps,
            vec![pngs[0].with_extension("pcap")],
            "beside the PNG"
        );
        let back = read_pcap(&std::fs::read(&pcaps[0]).unwrap()).unwrap();
        assert_eq!(back, captured(None).records, "every packet, as captured");

        let indexed = &index_lines(&dir)[0];
        assert_eq!(indexed["source"], "packets");
        assert_eq!(indexed["codec"], "ulaw");
        assert_eq!(
            indexed["packets"],
            pcaps[0].file_name().unwrap().to_str().unwrap()
        );
        assert_eq!(indexed["partial"], false);

        let state = slot.state();
        assert_eq!(state.error, None);
        let last = state.last.expect("last");
        assert!(!last.lost && !last.partial);
        let frame = slot.frame().expect("frame");
        assert_eq!((frame.width, frame.height, frame.lines), (320, 240, 240));
        // The fixture's picture, not the synthetic live one.
        assert_ne!(frame.rgba[..12], [0, 1, 2, 255, 3, 4, 5, 255, 6, 7, 8, 255]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_better_live_picture_is_kept_and_its_packets_saved_all_the_same() {
        let dir = tmp("live-win");
        let (slot, fin) = finisher(&dir);
        let live = with_quality(picture(Mode::Robot36, 320, 240, 240), 1.0, 0);
        fin.finish(&pending(1), Some(live), Some(&captured(Some("ulaw"))));
        assert_eq!(files(&dir, "pcap").len(), 1);
        assert_eq!(index_lines(&dir)[0]["source"], "live");
        let frame = slot.frame().expect("frame");
        assert_eq!(
            frame.rgba[..8],
            [0, 1, 2, 255, 3, 4, 5, 255],
            "the live pixels"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Mini frames alone do not name the codec, so with none given there is
    /// nothing to rebuild: the live picture stays and says why.
    #[test]
    fn a_rebuild_failure_keeps_the_live_picture_and_says_so() {
        let dir = tmp("rebuild-fails");
        let (slot, fin) = finisher(&dir);
        let live = with_quality(picture(Mode::Robot36, 320, 240, 240), 0.2, 30);
        fin.finish(&pending(1), Some(live), Some(&captured(None)));
        assert_eq!(files(&dir, "png").len(), 1);
        assert_eq!(files(&dir, "pcap").len(), 1, "kept to decode again later");
        let indexed = &index_lines(&dir)[0];
        assert_eq!(indexed["source"], "live");
        assert!(indexed.get("codec").is_none());
        let state = slot.state();
        let error = state.error.expect("an error");
        assert!(error.contains("rebuild"), "{error}");
        assert!(!state.last.expect("last").lost);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_packet_the_pcap_cannot_hold_is_left_out_and_a_refused_file_reported() {
        let dir = tmp("pcap-fails");
        let (slot, fin) = finisher(&dir);
        // A packet the pcap cannot hold: the address families differ.
        let mut cap = captured(Some("ulaw"));
        cap.records[0].dst = "[::1]:4569".parse().unwrap();
        // ...is left out, not a failure. A folder that refuses the file is.
        let blocked = tmp("pcap-fails-blocked");
        let (slot2, fin2) = finisher(&blocked);
        std::fs::remove_dir_all(&blocked).unwrap();
        let live = with_quality(picture(Mode::Robot36, 320, 240, 240), 0.2, 30);
        fin.finish(&pending(1), Some(live.clone()), Some(&cap));
        assert_eq!(index_lines(&dir)[0]["source"], "packets");
        assert_eq!(slot.state().error, None);

        fin2.finish(&pending(1), Some(live), Some(&captured(Some("ulaw"))));
        let st = slot2.state();
        let error = st.error.expect("an error");
        assert!(error.contains("packets"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_picture_with_no_packets_indexes_exactly_as_before() {
        let dir = tmp("no-packets");
        let (_slot, fin) = finisher(&dir);
        fin.finish(
            &pending(1),
            Some(picture(Mode::Robot36, 320, 240, 240)),
            None,
        );
        let line = index_lines(&dir)[0].as_object().unwrap().clone();
        let mut keys: Vec<&str> = line.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "file", "mode", "network", "partial", "talker", "target", "time_ms"
            ]
        );
        assert_eq!(files(&dir, "pcap"), Vec::<PathBuf>::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lost_picture_with_packets_leaves_no_files() {
        let dir = tmp("lost-packets");
        let (slot, fin) = finisher(&dir);
        let cap = Captured {
            records: robot36_packets(400, 900)[..50].to_vec(),
            codec_name: Some("ulaw"),
            local: Some(local()),
        };
        fin.finish(&pending(1), None, Some(&cap));
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
        assert!(slot.state().last.expect("last").lost);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The sender gives up a quarter of the way in: the packets rebuild a
    /// picture, but one with under half its rows. It is no more worth
    /// keeping than the live one was, so nothing is saved and the row
    /// stays lost.
    #[test]
    fn a_rebuilt_picture_under_half_its_rows_is_not_saved() {
        let dir = tmp("rebuilt-scrap");
        let (slot, fin) = finisher(&dir);
        let records = robot36_packets(400, 900);
        let cap = Captured {
            records: records[..records.len() / 4].to_vec(),
            codec_name: Some("ulaw"),
            local: Some(local()),
        };
        let rebuilt = rebuild(&cap, Mode::Robot36)
            .expect("rebuilds")
            .expect("an abandoned picture to refuse");
        assert!(
            rebuilt.lines > 0 && !rebuilt.worth_keeping(),
            "{}",
            rebuilt.lines
        );

        let live = picture(Mode::Robot36, 320, 240, 20);
        fin.finish(&pending(1), Some(live), Some(&cap));
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
        assert!(slot.state().last.expect("last").lost);
        assert!(slot.frame().is_none());

        fin.finish(&pending(2), None, Some(&cap));
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
        assert!(slot.state().last.expect("last").lost);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A live picture lost to a dropout is still rescued by packets that
    /// rebuild the whole of it.
    #[test]
    fn a_rebuilt_picture_worth_keeping_rescues_a_lost_live_one() {
        let dir = tmp("rebuilt-rescue");
        let (slot, fin) = finisher(&dir);
        let live = picture(Mode::Robot36, 320, 240, 20);
        fin.finish(&pending(1), Some(live), Some(&captured(Some("ulaw"))));
        assert_eq!(files(&dir, "png").len(), 1);
        assert_eq!(files(&dir, "pcap").len(), 1);
        assert_eq!(index_lines(&dir)[0]["source"], "packets");
        let last = slot.state().last.expect("last");
        assert!(!last.lost && !last.partial);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A sink with a running keeper, and an `AllStar` call offering it
    /// packets.
    fn rig_with_packets(dir: &Path) -> (SstvSlot, Sink, astar_record::packet_tap::CallPacketTap) {
        use astar_record::packet_tap::{CallMeta, CallPacketTap, PacketSlot};
        let (slot, mut sink) = rig(dir);
        let (tap, rx) = packet_tap_channel(SSTV_PACKET_EVENTS, SSTV_PACKET_BUFFERS);
        let packet_slot = PacketSlot::default();
        packet_slot.set(Some(tap));
        sink.keeper = Some(Keeper::spawn(rx).unwrap());
        let call = CallPacketTap::new(
            PacketSlot::default(),
            |_| true,
            CallMeta {
                network: PacketNetwork::AllStar,
                codec: Some("ulaw"),
                label: "test".into(),
                local: local(),
                peer: peer(),
            },
        )
        .with_sstv(packet_slot);
        (slot, sink, call)
    }

    /// An M17 picture heard while an `AllStar` call carries traffic is
    /// saved at once with no packets, exactly as before; an `AllStar` one
    /// keeps them.
    #[test]
    fn only_an_allstar_picture_keeps_packets() {
        for (network, keeps) in [("M17", false), ("AllStar", true)] {
            let dir = tmp(&format!("gate-{network}"));
            let (slot, mut sink, mut call) = rig_with_packets(&dir);
            slot.set_context(SstvContext {
                network: network.to_owned(),
                ..ctx(Some("N0CALL"))
            });
            let t0 = Instant::now();
            sink.handle(started(1, Mode::Robot36, 320, 240), at(START_SECS), t0);
            for i in 0..10u32 {
                call.capture_at(Instant::now(), peer(), &i.to_be_bytes());
            }
            sleep(Duration::from_millis(100));
            sink.handle(
                Outcome::Finished {
                    seq: 1,
                    picture: picture(Mode::Robot36, 320, 240, 240),
                    rate: 8_000,
                },
                at(START_SECS + 40),
                Instant::now(),
            );
            if keeps {
                assert!(index_lines(&dir).is_empty(), "waits for late packets");
                wait_for("the packets", 10, || !index_lines(&dir).is_empty());
                assert_eq!(files(&dir, "pcap").len(), 1);
            } else {
                let lines = index_lines(&dir);
                assert_eq!(lines.len(), 1, "{network}: saved at once");
                assert!(lines[0].get("packets").is_none(), "{network}");
                assert!(lines[0].get("source").is_none(), "{network}");
                drop(sink);
                assert_eq!(files(&dir, "pcap"), Vec::<PathBuf>::new(), "{network}");
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Rebuilt pictures finish on threads of their own, so a later picture
    /// can finish first. The older one joins `recent` in its place, and
    /// `last`, `error` and the frame stay with the newer one.
    #[test]
    fn an_older_picture_finishing_late_keeps_the_newer_one_last() {
        let dir = tmp("out-of-order");
        let (slot, fin) = finisher(&dir);
        fin.finish(
            &pending(1),
            Some(picture(Mode::Robot36, 320, 240, 240)),
            None,
        );
        fin.finish(
            &pending(3),
            Some(picture(Mode::Robot36, 320, 240, 240)),
            None,
        );
        // Picture 2 finishes last, and its rebuild fails (no codec).
        let live = with_quality(picture(Mode::Robot36, 320, 240, 240), 0.2, 30);
        fin.finish(&pending(2), Some(live), Some(&captured(None)));

        let st = slot.state();
        let seqs: Vec<u64> = st.recent.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, [3, 2, 1], "newest first, by picture");
        assert_eq!(st.last.expect("last").seq, 3);
        // The newest picture had no error, so picture 2's is not lost.
        let error = st.error.expect("picture 2's error kept");
        assert!(error.starts_with("picture 2: "), "{error}");
        assert!(error.contains("rebuild"), "{error}");
        assert_eq!(slot.frame().expect("frame").seq, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An older picture's late error never covers the newest one's.
    #[test]
    fn an_older_picture_finishing_late_leaves_the_newest_error() {
        let dir = tmp("out-of-order-errors");
        let (slot, fin) = finisher(&dir);
        let live = || with_quality(picture(Mode::Robot36, 320, 240, 240), 0.2, 30);
        fin.finish(&pending(3), Some(live()), Some(&captured(None)));
        let newest = slot.state().error.expect("picture 3's error");
        fin.finish(&pending(2), Some(live()), Some(&captured(None)));
        assert_eq!(slot.state().error, Some(newest));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The packets are saved on the first name, the PNG finds that name
    /// taken, and on the next name the packets can't be saved: the live
    /// picture is kept, and the index says so, with no packets named.
    #[test]
    fn packets_lost_on_a_second_name_keep_the_live_picture() {
        let dir = tmp("second-name");
        let live = with_quality(picture(Mode::Robot36, 320, 240, 240), 0.2, 30);
        let rebuilt = with_quality(picture(Mode::Robot36, 320, 240, 240), 0.9, 0);
        let mut pcap_turn = 0;
        let mut png_turn = 0;
        let mut wrote = Vec::new();
        let saved = save_with(
            &dir,
            MST,
            &pending(1),
            Choice {
                live: Some(live),
                rebuilt: Some(rebuilt),
            },
            Some(&captured(Some("ulaw"))),
            &mut |path, c| {
                pcap_turn += 1;
                if pcap_turn == 1 {
                    write_packets(path, c)
                } else {
                    Err(io::Error::from(io::ErrorKind::PermissionDenied))
                }
            },
            &mut |p, path| {
                png_turn += 1;
                if png_turn == 1 {
                    Err(io::Error::from(io::ErrorKind::AlreadyExists))
                } else {
                    wrote.push(p.quality);
                    p.write_png(path)
                }
            },
        );
        assert!(saved.file.is_some());
        assert_eq!(wrote, [0.2], "the live picture went to the PNG");
        assert!((saved.picture.expect("kept").quality - 0.2).abs() < f32::EPSILON);
        assert!(saved.error.expect("an error").contains("packets"));
        assert_eq!(files(&dir, "pcap"), Vec::<PathBuf>::new());
        let indexed = &index_lines(&dir)[0];
        assert!(indexed.get("packets").is_none());
        assert_eq!(indexed["source"], "live");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sstv_on_puts_a_packet_tap_on_allstar_calls_and_off_takes_it() {
        let dir = tmp("packet-tap");
        let st = station();
        let has = Station::sstv_packet_tap_set;
        assert!(!has(&st), "off: nothing kept");
        st.sstv_configure(Some(SstvConfig {
            dir: dir.clone(),
            utc_offset_secs: MST,
        }))
        .unwrap();
        assert!(has(&st));
        st.sstv_configure(None).unwrap();
        st.sstv_wait_stopped();
        assert!(!has(&st));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
