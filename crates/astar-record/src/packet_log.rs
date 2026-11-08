// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The packet log's writer thread: a `.pcap` and a `.jitter.jsonl` companion
//! per call, written from the events the packet taps send. A long call
//! splits into parts, and the folder stays under its size limit by losing
//! its oldest closed logs.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::RecordError;
use crate::clock::rfc3339_utc;
use crate::disk::{check_free_with, free_bytes};
use crate::layout::with_suffix;
use crate::naming::{app_base_name, packet_log_label, unique_stem};
use crate::packet_tap::{CallMeta, JitterSample, PACKET_TAP_CAPACITY, PacketCallId, PacketEvent};
use crate::pcap::{GLOBAL_HEADER_LEN, PcapWriter, canonical};

/// The smallest size limit the edges (FFI, config validation) accept.
pub const MIN_PACKET_LOG_BYTES: u64 = 100_000_000;
/// The size limit when none is set.
pub const DEFAULT_PACKET_LOG_BYTES: u64 = 1_000_000_000;
/// What follows the shared stem on a companion file's name.
pub const COMPANION_SUFFIX: &str = ".jitter.jsonl";

const PCAP_SUFFIX: &str = ".pcap";
/// One tick of the writer's clock.
const TICK: Duration = Duration::from_millis(20);
/// How often buffered bytes reach the files, which is also how much log a
/// crash can lose.
const FLUSH_EVERY: Duration = Duration::from_secs(1);
/// How often the free-space guard looks while calls are open.
const FREE_CHECK_EVERY: Duration = Duration::from_secs(5);
/// How many taken names to step past before giving up on a call.
const OPEN_ATTEMPTS: usize = 100;
/// A pcap this fraction of the limit in size continues as a new part.
const PARTS_PER_LIMIT: u64 = 16;
/// What a companion file starts with, and so what makes a log ours.
const MARKER: &[u8] = b"{\"astar_packet_log\":";
/// The most a pcap record adds to a payload: the record header and the IPv6
/// and UDP headers.
const RECORD_OVERHEAD: u64 = 64;
/// Room held back for each open file's end line, so ending a file can never
/// take the folder past its limit.
const END_LINE_ROOM: u64 = 128;

const END_HANGUP: &str = "hangup";
const END_STOPPED: &str = "stopped";
const END_CONTINUED: &str = "continued";
const END_LIMIT: &str = "limit";
const END_DISK_LOW: &str = "disk_low";
const END_WRITE_ERROR: &str = "write_error";

/// What [`PacketLogger`] tells its thread: keep running, or stop and end each
/// open file with the reason named.
const RUN: u8 = 0;
const STOP_AS_STOPPED: u8 = 1;
const STOP_AS_HANGUP: u8 = 2;

/// Where the packet log goes. Public fields only, so a caller builds it as a
/// struct literal.
#[derive(Clone)]
pub struct PacketLogConfig {
    pub dir: PathBuf,
    pub max_bytes: u64,
    pub utc_offset_secs: i32,
    pub version: String,
}

/// Why a stop ends the files still open, as their end lines say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketLogEnd {
    /// Anything but the call ending: the button, new settings, astar quitting.
    Stopped,
    /// The call ended.
    Hangup,
}

/// What the packet log is doing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PacketLogState {
    pub capturing: bool,
    pub open_calls: u32,
    /// Bytes our logs take up in the folder.
    pub used_bytes: u64,
    /// Packets that never reached a file, for any reason.
    pub dropped: u64,
    /// The first thing that went wrong.
    pub error: Option<String>,
}

/// One log, as a listing shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PacketLogRow {
    /// The pcap's file name.
    pub name: String,
    /// The companion's file name.
    pub companion: String,
    /// Both files together.
    pub bytes: u64,
    /// RFC 3339, UTC.
    pub modified: String,
    /// A call is still writing to it.
    pub open: bool,
}

/// Why [`delete_log`] said no.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteRefused {
    NoSuchLog,
    Open,
    Io(String),
}

/// The companion that belongs beside `pcap`, if `pcap` is named like one.
fn companion_of(pcap: &Path) -> Option<PathBuf> {
    let stem = pcap.file_name()?.to_str()?.strip_suffix(PCAP_SUFFIX)?;
    Some(pcap.with_file_name(format!("{stem}{COMPANION_SUFFIX}")))
}

/// Whether `pcap` is one of ours: a `.pcap` with a sibling companion whose
/// first line opens with the marker. The folder may hold other people's
/// captures, and this is what keeps retention and deletion off them.
#[must_use]
pub fn is_our_log(pcap: &Path) -> bool {
    let Some(companion) = companion_of(pcap) else {
        return false;
    };
    let mut start = [0u8; MARKER.len()];
    pcap.is_file()
        && companion.is_file()
        && File::open(&companion)
            .and_then(|mut f| f.read_exact(&mut start))
            .is_ok()
        && start == MARKER
}

/// A log of ours in the folder.
struct LogPair {
    pcap: PathBuf,
    companion: PathBuf,
    bytes: u64,
    /// The later of the two files' times.
    modified: SystemTime,
}

impl LogPair {
    fn at(pcap: PathBuf) -> Option<Self> {
        if !is_our_log(&pcap) {
            return None;
        }
        let companion = companion_of(&pcap)?;
        let (p, c) = (
            std::fs::metadata(&pcap).ok()?,
            std::fs::metadata(&companion).ok()?,
        );
        Some(Self {
            bytes: p.len() + c.len(),
            modified: p.modified().ok()?.max(c.modified().ok()?),
            pcap,
            companion,
        })
    }

    /// The pcap goes first. If the second removal fails, a lone companion is
    /// left behind; the other way round would leave a pcap that looks foreign
    /// and that nothing would ever delete.
    fn remove(&self) -> io::Result<()> {
        for path in [&self.pcap, &self.companion] {
            match std::fs::remove_file(path) {
                Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }
}

/// Our logs in `dir`, oldest first. A folder that can't be read has none.
fn scan(dir: &Path) -> Vec<LogPair> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut logs: Vec<LogPair> = entries
        .flatten()
        .filter_map(|e| LogPair::at(e.path()))
        .collect();
    logs.sort_by(|a, b| (a.modified, &a.pcap).cmp(&(b.modified, &b.pcap)));
    logs
}

/// The logs in `dir`, newest first, skipping anything that isn't ours.
/// `open` is [`PacketLogger::open_paths`].
#[must_use]
pub fn list_logs(dir: &Path, open: &[PathBuf]) -> Vec<PacketLogRow> {
    let name = |p: &Path| {
        p.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };
    scan(dir)
        .iter()
        .rev()
        .map(|log| PacketLogRow {
            name: name(&log.pcap),
            companion: name(&log.companion),
            bytes: log.bytes,
            modified: rfc3339_utc(log.modified),
            open: open.contains(&log.pcap),
        })
        .collect()
}

/// Deletes the log whose pcap is called `name`, directly inside `dir`. The
/// name may have come over HTTP, so anything that isn't a plain file name of
/// a log of ours is a [`DeleteRefused::NoSuchLog`].
pub fn delete_log(dir: &Path, name: &str, open: &[PathBuf]) -> Result<(), DeleteRefused> {
    if Path::new(name).file_name() != Some(name.as_ref()) {
        return Err(DeleteRefused::NoSuchLog);
    }
    let pcap = dir.join(name);
    if open.contains(&pcap) {
        return Err(DeleteRefused::Open);
    }
    LogPair::at(pcap)
        .ok_or(DeleteRefused::NoSuchLog)?
        .remove()
        .map_err(|e| DeleteRefused::Io(e.to_string()))
}

/// The first line of a companion file.
#[derive(Serialize)]
struct HeaderLine<'a> {
    astar_packet_log: u32,
    network: &'a str,
    codec: Option<&'a str>,
    label: &'a str,
    local: SocketAddr,
    peer: SocketAddr,
    started_at: String,
    version: &'a str,
    part: u32,
}

/// A sampled row. `packets` and `dropped` count from the top of the file.
#[derive(Serialize)]
struct RowLine {
    ts_ns: u64,
    packets: u64,
    dropped: u64,
    depth_ms: Option<u32>,
    jitter_ms: Option<u32>,
    lost: Option<u64>,
    late: Option<u64>,
    ooo: Option<u64>,
    underruns: Option<u64>,
}

/// The last line of a companion file. A file without one means a crash.
#[derive(Serialize)]
struct EndLine {
    end: &'static str,
    ts_ns: u64,
    packets: u64,
    dropped: u64,
}

/// The local address to put in a packet, in the peer's family. A real
/// address stays. A wildcard has no IP worth logging, and a v6 socket can
/// serve a v4 peer, so those ask the routing table instead.
fn local_for(local: SocketAddr, peer: SocketAddr) -> SocketAddr {
    let (local, peer) = (canonical(local), canonical(peer));
    if !local.ip().is_unspecified() && local.is_ipv4() == peer.is_ipv4() {
        return local;
    }
    let wildcard = if peer.is_ipv4() {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    };
    SocketAddr::new(route_to(wildcard, peer).unwrap_or(wildcard), local.port())
}

/// The source IP the system would use to reach `peer`. Connecting a UDP
/// socket picks a route and sends nothing.
fn route_to(wildcard: IpAddr, peer: SocketAddr) -> io::Result<IpAddr> {
    let socket = UdpSocket::bind((wildcard, 0))?;
    socket.connect(peer)?;
    Ok(socket.local_addr()?.ip())
}

fn create_new(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// Both files of `stem`, or neither. Only a file this call made is removed.
fn create_pair(stem: &Path) -> io::Result<(PathBuf, File, PathBuf, File)> {
    let pcap_path = with_suffix(stem, PCAP_SUFFIX);
    let companion_path = with_suffix(stem, COMPANION_SUFFIX);
    let pcap = create_new(&pcap_path)?;
    match create_new(&companion_path) {
        Ok(companion) => Ok((pcap_path, pcap, companion_path, companion)),
        Err(e) => {
            let _ = std::fs::remove_file(&pcap_path);
            Err(e)
        }
    }
}

/// Files are opened with `create_new`, so a name taken between the look and
/// the open is an `AlreadyExists`, and the next free stem is tried.
fn create_unique(dir: &Path, base: &str) -> io::Result<(PathBuf, File, PathBuf, File)> {
    let mut last = None;
    for _ in 0..OPEN_ATTEMPTS {
        let stem = unique_stem(dir, base, &[PCAP_SUFFIX, COMPANION_SUFFIX]);
        match create_pair(&stem) {
            Err(e) if e.kind() == ErrorKind::AlreadyExists => last = Some(e),
            other => return other,
        }
    }
    Err(last.expect("the loop ran"))
}

/// JSON Lines: one value and its newline.
fn line_of(value: &impl Serialize) -> io::Result<Vec<u8>> {
    let mut json = serde_json::to_vec(value)?;
    json.push(b'\n');
    Ok(json)
}

/// What every part of a call shares.
#[derive(Clone)]
struct CallInfo {
    network: &'static str,
    codec: Option<&'static str>,
    label: String,
    /// The destination of every packet.
    local: SocketAddr,
    peer: SocketAddr,
    /// The name the first part's files have, or want, without a suffix.
    stem: String,
    /// The `Instant` half of the `Start` event's clock pair.
    start_at: Instant,
    /// The wall-clock half, in nanoseconds since the epoch.
    start_ns: u64,
}

impl CallInfo {
    fn new(cfg: &PacketLogConfig, meta: &CallMeta, at: Instant, wall: SystemTime) -> Self {
        let label = packet_log_label(&meta.label);
        Self {
            network: meta.network.as_str(),
            codec: meta.codec,
            stem: app_base_name(wall, cfg.utc_offset_secs, &label),
            label,
            local: local_for(meta.local, meta.peer),
            peer: canonical(meta.peer),
            start_at: at,
            start_ns: wall
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)),
        }
    }

    /// `at` on the wall clock of the `Start` event, so both files share one
    /// clock that cannot step backwards.
    fn ts_ns(&self, at: Instant) -> u64 {
        let since = at.saturating_duration_since(self.start_at).as_nanos();
        self.start_ns
            .saturating_add(u64::try_from(since).unwrap_or(u64::MAX))
    }

    /// The companion's first line for a part that starts at `at`.
    fn header(&self, version: &str, part: u32, at: Instant) -> io::Result<Vec<u8>> {
        line_of(&HeaderLine {
            astar_packet_log: 1,
            network: self.network,
            codec: self.codec,
            label: &self.label,
            local: self.local,
            peer: self.peer,
            started_at: rfc3339_utc(UNIX_EPOCH + Duration::from_nanos(self.ts_ns(at))),
            version,
            part,
        })
    }
}

/// One part of a call: a pair of files.
struct CallFiles {
    info: CallInfo,
    part: u32,
    pcap: PcapWriter<BufWriter<File>>,
    companion: BufWriter<File>,
    pcap_path: PathBuf,
    companion_path: PathBuf,
    /// The latest instant any line carries.
    last_at: Instant,
    /// Counted from the top of this file, like `dropped`.
    packets: u64,
    dropped: u64,
    /// Bytes handed to the pcap.
    pcap_bytes: u64,
    /// Bytes handed to the two files.
    bytes: u64,
}

impl CallFiles {
    /// Opens the files and writes the headers, flushed so a crash straight
    /// after `Start` still leaves a recognisable pair. Part 1 settles the
    /// name, and later parts add to whatever it turned out to be.
    fn create(
        dir: &Path,
        mut info: CallInfo,
        part: u32,
        header: &[u8],
        at: Instant,
    ) -> io::Result<Self> {
        let base = if part == 1 {
            info.stem.clone()
        } else {
            format!("{} part {part}", info.stem)
        };
        let (pcap_path, pcap, companion_path, companion) = create_unique(dir, &base)?;
        if part == 1 {
            let name = pcap_path.file_name().unwrap_or_default().to_string_lossy();
            name.strip_suffix(PCAP_SUFFIX)
                .unwrap_or(&name)
                .clone_into(&mut info.stem);
        }
        let mut files = Self {
            info,
            part,
            pcap: PcapWriter::new(BufWriter::new(pcap))?,
            companion: BufWriter::new(companion),
            pcap_path,
            companion_path,
            last_at: at,
            packets: 0,
            dropped: 0,
            pcap_bytes: GLOBAL_HEADER_LEN as u64,
            bytes: GLOBAL_HEADER_LEN as u64,
        };
        let written = files.write_line(header).and_then(|()| files.flush());
        if let Err(e) = written {
            let _ = std::fs::remove_file(&files.pcap_path);
            let _ = std::fs::remove_file(&files.companion_path);
            return Err(e);
        }
        Ok(files)
    }

    /// [`CallInfo::ts_ns`], remembering the latest time used.
    fn ts_ns(&mut self, at: Instant) -> u64 {
        self.last_at = self.last_at.max(at);
        self.info.ts_ns(at)
    }

    fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        self.companion.write_all(line)?;
        self.bytes += line.len() as u64;
        Ok(())
    }

    fn write_packet(&mut self, at: Instant, from: SocketAddr, bytes: &[u8]) -> io::Result<()> {
        let ts_ns = self.ts_ns(at);
        let n = self
            .pcap
            .write_packet(ts_ns, from, self.info.local, bytes)?;
        self.pcap_bytes += n;
        self.bytes += n;
        self.packets += 1;
        Ok(())
    }

    fn row_line(&mut self, at: Instant, sample: &JitterSample) -> io::Result<Vec<u8>> {
        line_of(&RowLine {
            ts_ns: self.ts_ns(at),
            packets: self.packets,
            dropped: self.dropped,
            depth_ms: sample.depth_ms,
            jitter_ms: sample.jitter_ms,
            lost: sample.lost,
            late: sample.late,
            ooo: sample.ooo,
            underruns: sample.underruns,
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.pcap.flush()?;
        self.companion.flush()
    }

    /// Flushes, after checking the files are still there: writing to a
    /// deleted file succeeds on Unix, so a removed folder is only noticed by
    /// looking.
    fn flush_if_present(&mut self) -> io::Result<()> {
        if !self.pcap_path.exists() || !self.companion_path.exists() {
            return Err(io::Error::new(ErrorKind::NotFound, "the file was removed"));
        }
        self.flush()
    }

    /// Writes the end line and flushes. The line's time is now, or the
    /// latest time already written if that is later.
    fn write_end(&mut self, end: &'static str) -> io::Result<()> {
        let ts_ns = self.ts_ns(self.last_at.max(Instant::now()));
        let line = line_of(&EndLine {
            end,
            ts_ns,
            packets: self.packets,
            dropped: self.dropped,
        })?;
        self.write_line(&line)?;
        self.flush()
    }
}

/// Creates and removes a uniquely named file in `dir`, so a folder that
/// can't be written is refused now and not at the first call. A name that
/// isn't a `.pcap` keeps retention off it.
fn probe_writable(dir: &Path) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let probe = dir.join(format!(".astar-probe-{}-{n}", std::process::id()));
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    std::fs::remove_file(&probe)
}

/// The running packet log. Dropping it stops it, as [`PacketLogger::stop`]
/// does.
pub struct PacketLogger {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicU8>,
    thread: Option<JoinHandle<()>>,
}

/// What the thread shows the outside world.
#[derive(Default)]
struct Shared {
    state: PacketLogState,
    /// The pcap of every open file.
    open: Vec<PathBuf>,
}

fn lock(m: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl PacketLogger {
    /// Creates the folder, checks that it can be written and has free space,
    /// and starts the thread, which
    /// prunes the folder to `cfg.max_bytes` first. Files appear as calls
    /// start. A limit of zero is refused; the floor users see is the edges'
    /// to enforce.
    pub fn start(
        cfg: PacketLogConfig,
        rx: Receiver<PacketEvent>,
    ) -> Result<PacketLogger, RecordError> {
        Self::start_with(cfg, rx, free_bytes, FREE_CHECK_EVERY)
    }

    /// [`PacketLogger::start`] with the free-space guard swapped out (a test
    /// seam).
    fn start_with(
        cfg: PacketLogConfig,
        rx: Receiver<PacketEvent>,
        free_space: fn(&Path) -> Option<u64>,
        free_every: Duration,
    ) -> Result<PacketLogger, RecordError> {
        if cfg.max_bytes == 0 {
            return Err(RecordError::Io(io::Error::new(
                ErrorKind::InvalidInput,
                "the packet log's size limit must be above zero",
            )));
        }
        std::fs::create_dir_all(&cfg.dir)
            .map_err(|e| RecordError::Folder(format!("{}: {e}", cfg.dir.display())))?;
        probe_writable(&cfg.dir)
            .map_err(|e| RecordError::Folder(format!("{}: {e}", cfg.dir.display())))?;
        check_free_with(free_space, &cfg.dir)?;
        let shared = Arc::new(Mutex::new(Shared {
            state: PacketLogState {
                capturing: true,
                ..PacketLogState::default()
            },
            open: Vec::new(),
        }));
        let stop = Arc::new(AtomicU8::new(RUN));
        let worker = Worker::new(
            cfg,
            rx,
            Arc::clone(&shared),
            Arc::clone(&stop),
            free_space,
            free_every,
        );
        let on_panic = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("astar-packet-log".to_owned())
            .spawn(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run()));
                if r.is_err() {
                    let mut s = lock(&on_panic);
                    s.open.clear();
                    let s = &mut s.state;
                    s.capturing = false;
                    s.open_calls = 0;
                    s.error
                        .get_or_insert_with(|| "the packet log stopped unexpectedly".to_owned());
                }
            })?;
        Ok(PacketLogger {
            shared,
            stop,
            thread: Some(thread),
        })
    }

    /// The pcap of every file a call is writing to, for [`list_logs`] and
    /// [`delete_log`].
    #[must_use]
    pub fn open_paths(&self) -> Vec<PathBuf> {
        lock(&self.shared).open.clone()
    }

    /// The live state, at most one tick old.
    #[must_use]
    pub fn state(&self) -> PacketLogState {
        lock(&self.shared).state.clone()
    }

    /// [`PacketLogger::stop_as`] with [`PacketLogEnd::Stopped`].
    #[must_use]
    pub fn stop(self) -> PacketLogState {
        self.stop_as(PacketLogEnd::Stopped)
    }

    /// Drains what's queued, ends each open file with `end`, joins the thread
    /// and returns the final state.
    #[must_use]
    pub fn stop_as(mut self, end: PacketLogEnd) -> PacketLogState {
        self.shutdown(end);
        self.state()
    }

    fn shutdown(&mut self, end: PacketLogEnd) {
        let stop = match end {
            PacketLogEnd::Stopped => STOP_AS_STOPPED,
            PacketLogEnd::Hangup => STOP_AS_HANGUP,
        };
        self.stop.store(stop, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for PacketLogger {
    fn drop(&mut self) {
        self.shutdown(PacketLogEnd::Stopped);
    }
}

struct Worker {
    cfg: PacketLogConfig,
    rx: Receiver<PacketEvent>,
    calls: HashMap<PacketCallId, CallFiles>,
    /// Bytes in the folder's closed logs: measured at each prune, and added
    /// to as files end.
    closed_bytes: u64,
    dropped: u64,
    error: Option<String>,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicU8>,
    free_space: fn(&Path) -> Option<u64>,
    free_every: Duration,
}

impl Worker {
    fn new(
        cfg: PacketLogConfig,
        rx: Receiver<PacketEvent>,
        shared: Arc<Mutex<Shared>>,
        stop: Arc<AtomicU8>,
        free_space: fn(&Path) -> Option<u64>,
        free_every: Duration,
    ) -> Self {
        Self {
            cfg,
            rx,
            calls: HashMap::new(),
            closed_bytes: 0,
            dropped: 0,
            error: None,
            shared,
            stop,
            free_space,
            free_every,
        }
    }

    /// At most one channel's worth of events, so a producer that outruns the
    /// writer can't keep it from its flush or from noticing a stop. That is
    /// also everything queued when the pass began.
    fn drain(&mut self) {
        for _ in 0..PACKET_TAP_CAPACITY {
            let Ok(event) = self.rx.try_recv() else {
                return;
            };
            self.handle(event);
        }
    }

    /// The thread loop.
    ///
    /// Invariant: the channel is only ever drained with `try_recv`, on this
    /// thread's own clock. A receiver parked in `recv` or `recv_timeout`
    /// would cost the sender a futex wake on a receive thread.
    fn run(mut self) {
        self.prune(0);
        let mut flushed = Instant::now();
        let mut free_checked = Instant::now();
        let stop = loop {
            // Read before draining, so everything sent before the stop is seen.
            let stop = self.stop.load(Ordering::Acquire);
            self.drain();
            if stop != RUN {
                break stop;
            }
            if flushed.elapsed() >= FLUSH_EVERY {
                self.flush_all();
                flushed = Instant::now();
            }
            if free_checked.elapsed() >= self.free_every {
                if !self.calls.is_empty() {
                    self.disk_is_low();
                }
                free_checked = Instant::now();
            }
            self.publish(true);
            std::thread::sleep(TICK);
        };
        self.close_all(if stop == STOP_AS_HANGUP {
            END_HANGUP
        } else {
            END_STOPPED
        });
        self.publish(false);
    }

    fn handle(&mut self, event: PacketEvent) {
        match event {
            PacketEvent::Start {
                call,
                meta,
                at,
                wall,
            } => self.open(call, &meta, at, wall),
            PacketEvent::Packet {
                call,
                at,
                from,
                dropped_before,
                bytes,
            } => self.packet(call, at, from, dropped_before, &bytes),
            PacketEvent::Jitter { call, at, sample } => self.row(call, at, &sample),
            PacketEvent::End {
                call,
                dropped_before,
            } => {
                self.count_dropped(call, dropped_before);
                self.close(call, END_HANGUP);
            }
        }
    }

    /// A `Start` always means a new pair of files. One for a call that is
    /// already open is a re-offer after the tap was switched. A call that
    /// gets no file has its packets counted as an unknown call's are.
    fn open(&mut self, call: PacketCallId, meta: &CallMeta, at: Instant, wall: SystemTime) {
        self.close(call, END_STOPPED);
        if self.disk_is_low() {
            return;
        }
        let info = CallInfo::new(&self.cfg, meta, at, wall);
        match self.open_part(info, 1, at, 0) {
            Ok(Some(files)) => {
                self.calls.insert(call, files);
                self.share_open();
            }
            Ok(None) => self.note_folder_full(),
            Err(e) => self.note_error(&self.cfg.dir.clone(), &e),
        }
    }

    /// Prunes, then opens `part` if it and `then` more bytes fit. `Ok(None)`
    /// when they don't.
    fn open_part(
        &mut self,
        info: CallInfo,
        part: u32,
        at: Instant,
        then: u64,
    ) -> io::Result<Option<CallFiles>> {
        let header = info.header(&self.cfg.version, part, at)?;
        let size = (GLOBAL_HEADER_LEN + header.len()) as u64;
        if !self.prune(size + END_LINE_ROOM + then) {
            return Ok(None);
        }
        CallFiles::create(&self.cfg.dir, info, part, &header, at).map(Some)
    }

    /// Moves a call on to its next part, with room for `then` more bytes.
    /// `false` when the folder has no room, which changes nothing. A part
    /// that can't be opened ends the call's file, as a write error does.
    fn roll(&mut self, call: PacketCallId, at: Instant, then: u64) -> bool {
        let Some(files) = self.calls.get(&call) else {
            return true;
        };
        let (info, part) = (files.info.clone(), files.part + 1);
        match self.open_part(info, part, at, then) {
            Ok(None) => return false,
            Ok(Some(next)) => {
                if let Some(previous) = self.calls.insert(call, next) {
                    self.end(previous, END_CONTINUED);
                }
                self.share_open();
            }
            Err(e) => self.abandon(call, &e),
        }
        true
    }

    /// A packet for a call with no open file is counted and goes no further:
    /// its `Start` was lost, its file failed, or it was never opened.
    fn packet(
        &mut self,
        call: PacketCallId,
        at: Instant,
        from: SocketAddr,
        dropped_before: u32,
        bytes: &[u8],
    ) {
        self.count_dropped(call, dropped_before);
        let Some(files) = self.calls.get(&call) else {
            self.dropped += 1;
            return;
        };
        // Rolling here rather than after a write means a call that ends right
        // at the size opens no empty part.
        let need = RECORD_OVERHEAD + bytes.len() as u64;
        let room = if files.pcap_bytes >= self.cfg.max_bytes / PARTS_PER_LIMIT {
            self.roll(call, at, need)
        } else {
            self.room_for(need)
        };
        let Some(files) = self.calls.get_mut(&call) else {
            self.dropped += 1;
            return;
        };
        let written = room.then(|| files.write_packet(at, from, bytes));
        if !matches!(written, Some(Ok(()))) {
            files.dropped += 1;
            self.dropped += 1;
        }
        match written {
            None => self.out_of_room(call),
            // Refused before a byte was written, so the file is still good.
            Some(Err(e)) if e.kind() != ErrorKind::InvalidInput => self.abandon(call, &e),
            Some(_) => {}
        }
    }

    fn row(&mut self, call: PacketCallId, at: Instant, sample: &JitterSample) {
        let Some(files) = self.calls.get_mut(&call) else {
            return;
        };
        let line = match files.row_line(at, sample) {
            Ok(line) => line,
            Err(e) => return self.abandon(call, &e),
        };
        if !self.room_for(line.len() as u64) {
            return self.out_of_room(call);
        }
        if let Some(files) = self.calls.get_mut(&call)
            && let Err(e) = files.write_line(&line)
        {
            self.abandon(call, &e);
        }
    }

    fn count_dropped(&mut self, call: PacketCallId, n: u32) {
        self.dropped += u64::from(n);
        if let Some(files) = self.calls.get_mut(&call) {
            files.dropped += u64::from(n);
        }
    }

    fn flush_all(&mut self) {
        let ids: Vec<PacketCallId> = self.calls.keys().copied().collect();
        for id in ids {
            if let Some(files) = self.calls.get_mut(&id)
                && let Err(e) = files.flush_if_present()
            {
                self.abandon(id, &e);
            }
        }
    }

    /// Any write error is fatal for that file: the pcap may hold a torn
    /// record, so nothing more goes after it. The call's later packets are
    /// counted as an unknown call's are, and the channel keeps draining.
    fn abandon(&mut self, call: PacketCallId, e: &io::Error) {
        if let Some(files) = self.calls.get(&call) {
            self.note_error(&files.pcap_path.clone(), e);
            self.close(call, END_WRITE_ERROR);
        }
    }

    /// The folder has no room left for this call, so its file ends here.
    fn out_of_room(&mut self, call: PacketCallId) {
        self.note_folder_full();
        self.close(call, END_LIMIT);
    }

    /// Ends the call's file, if it has one.
    fn close(&mut self, call: PacketCallId, reason: &'static str) {
        if let Some(files) = self.calls.remove(&call) {
            self.end(files, reason);
            self.share_open();
        }
    }

    fn close_all(&mut self, reason: &'static str) {
        let ids: Vec<PacketCallId> = self.calls.keys().copied().collect();
        for id in ids {
            self.close(id, reason);
        }
    }

    fn end(&mut self, mut files: CallFiles, reason: &'static str) {
        if let Err(e) = files.write_end(reason) {
            self.note_error(&files.pcap_path, &e);
        }
        self.closed_bytes += files.bytes;
    }

    /// The free-space guard. Below it, every open file ends.
    fn disk_is_low(&mut self) -> bool {
        let Err(e) = check_free_with(self.free_space, &self.cfg.dir) else {
            return false;
        };
        self.note(e.to_string());
        self.close_all(END_DISK_LOW);
        true
    }

    fn used_bytes(&self) -> u64 {
        let open: u64 = self.calls.values().map(|f| f.bytes).sum();
        self.closed_bytes + open
    }

    /// Whether `need` more bytes fit, leaving every open file its end line.
    fn fits(&self, need: u64) -> bool {
        let end_lines = END_LINE_ROOM * self.calls.len() as u64;
        self.used_bytes()
            .saturating_add(end_lines)
            .saturating_add(need)
            <= self.cfg.max_bytes
    }

    fn room_for(&mut self, need: u64) -> bool {
        self.fits(need) || self.prune(need)
    }

    /// Measures the folder, so a log removed behind our back stops counting,
    /// then deletes closed logs, oldest first, until `need` more bytes fit.
    /// Returns whether they do.
    fn prune(&mut self, need: u64) -> bool {
        let closed: Vec<LogPair> = scan(&self.cfg.dir)
            .into_iter()
            .filter(|log| !self.calls.values().any(|f| f.pcap_path == log.pcap))
            .collect();
        self.closed_bytes = closed.iter().map(|log| log.bytes).sum();
        for log in closed {
            if self.fits(need) {
                break;
            }
            match log.remove() {
                Ok(()) => self.closed_bytes -= log.bytes,
                Err(e) => self.note_error(&log.pcap, &e),
            }
        }
        self.fits(need)
    }

    fn note_folder_full(&mut self) {
        self.note("the packet log folder is full".to_owned());
    }

    fn note_error(&mut self, path: &Path, e: &io::Error) {
        self.note(format!("{}: {e}", path.display()));
    }

    fn note(&mut self, text: String) {
        tracing::warn!(error = %text, "packet log write failed");
        self.error.get_or_insert(text);
    }

    /// Runs as files open and close, not on the tick: [`delete_log`] trusts
    /// this list.
    fn share_open(&self) {
        let open = self.calls.values().map(|f| f.pcap_path.clone()).collect();
        lock(&self.shared).open = open;
    }

    fn publish(&self, capturing: bool) {
        let mut shared = lock(&self.shared);
        let s = &mut shared.state;
        s.capturing = capturing;
        s.open_calls = u32::try_from(self.calls.len()).unwrap_or(u32::MAX);
        s.used_bytes = self.used_bytes();
        s.dropped = self.dropped;
        s.error.clone_from(&self.error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::MIN_FREE_BYTES;
    use crate::packet_pool::PacketBuf;
    use crate::packet_tap::{PACKET_TAP_CAPACITY, PacketNetwork};
    use crate::pcap::{PcapRecord, read_pcap};
    use serde_json::Value;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};

    const LOCAL: &str = "192.0.2.1:5198";
    const PEER: &str = "203.0.113.7:5198";
    const LABEL: &str = "EchoLink WA7ABU-R";
    const OFFSET: i32 = -6 * 3600;
    const FOLDER_FULL: &str = "the packet log folder is full";

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn tdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("astar-plog-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn rig_with(name: &str, capacity: usize) -> (PathBuf, SyncSender<PacketEvent>, PacketLogger) {
        let dir = tdir(name);
        let (tx, rx) = sync_channel(capacity);
        let cfg = PacketLogConfig {
            dir: dir.clone(),
            max_bytes: DEFAULT_PACKET_LOG_BYTES,
            utc_offset_secs: OFFSET,
            version: "9.9.9-test".to_owned(),
        };
        (dir, tx, PacketLogger::start(cfg, rx).unwrap())
    }

    fn rig(name: &str) -> (PathBuf, SyncSender<PacketEvent>, PacketLogger) {
        rig_with(name, 4096)
    }

    fn small_cfg(dir: &Path, max_bytes: u64) -> PacketLogConfig {
        PacketLogConfig {
            dir: dir.to_path_buf(),
            max_bytes,
            utc_offset_secs: OFFSET,
            version: "9.9.9-test".to_owned(),
        }
    }

    /// A logger on `dir` as it stands, under a limit far below the floor the
    /// edges enforce.
    fn rig_in(dir: &Path, max_bytes: u64) -> (SyncSender<PacketEvent>, PacketLogger) {
        let (tx, rx) = sync_channel(4096);
        (
            tx,
            PacketLogger::start(small_cfg(dir, max_bytes), rx).unwrap(),
        )
    }

    fn rig_limit(name: &str, max_bytes: u64) -> (PathBuf, SyncSender<PacketEvent>, PacketLogger) {
        let dir = tdir(name);
        let (tx, log) = rig_in(&dir, max_bytes);
        (dir, tx, log)
    }

    fn folder(name: &str) -> PathBuf {
        let dir = tdir(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn set_modified(path: &Path, when: SystemTime) {
        let file = File::options().write(true).open(path).unwrap();
        file.set_modified(when).unwrap();
    }

    /// A closed log of ours, last touched `age_secs` ago. Returns its bytes.
    fn closed_log(dir: &Path, stem: &str, pcap_bytes: usize, age_secs: u64) -> u64 {
        let pcap = dir.join(format!("{stem}.pcap"));
        let companion = dir.join(format!("{stem}.jitter.jsonl"));
        let line = "{\"astar_packet_log\":1}\n";
        std::fs::write(&pcap, vec![0u8; pcap_bytes]).unwrap();
        std::fs::write(&companion, line).unwrap();
        let when = SystemTime::now() - Duration::from_secs(age_secs);
        set_modified(&pcap, when);
        set_modified(&companion, when);
        (pcap_bytes + line.len()) as u64
    }

    /// The folder exists and a start can see it, but nothing can be created
    /// in it, so the first call would fail long after the press.
    #[cfg(unix)]
    #[test]
    fn a_read_only_folder_is_refused_at_the_start() {
        use std::os::unix::fs::PermissionsExt;

        let dir = folder("read-only");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let (_tx, rx) = sync_channel(8);
        let refused = PacketLogger::start(small_cfg(&dir, DEFAULT_PACKET_LOG_BYTES), rx);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(matches!(refused, Err(RecordError::Folder(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_write_probe_leaves_nothing_behind() {
        let dir = folder("probe-clean");
        let (_tx, rx) = sync_channel(8);
        let log = PacketLogger::start(small_cfg(&dir, DEFAULT_PACKET_LOG_BYTES), rx).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = log.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn is_pcap(name: &str) -> bool {
        Path::new(name).extension().is_some_and(|e| e == "pcap")
    }

    fn file_len(path: &Path) -> u64 {
        std::fs::metadata(path).map_or(0, |m| m.len())
    }

    /// Everything in `dir` that retention counts, tolerating a file that goes
    /// between the listing and the look.
    fn log_bytes(dir: &Path) -> u64 {
        names(dir)
            .iter()
            .filter(|n| is_pcap(n) || n.ends_with(COMPANION_SUFFIX))
            .map(|n| file_len(&dir.join(n)))
            .sum()
    }

    fn companions(dir: &Path) -> Vec<Vec<Value>> {
        names(dir)
            .iter()
            .filter(|n| n.ends_with(COMPANION_SUFFIX))
            .map(|n| lines_of(&dir.join(n)))
            .collect()
    }

    fn wall() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_793_062_804)
    }

    fn start_at(
        call: PacketCallId,
        label: &str,
        local: &str,
        peer: &str,
        at: Instant,
        wall: SystemTime,
    ) -> PacketEvent {
        PacketEvent::Start {
            call,
            meta: Arc::new(CallMeta {
                network: PacketNetwork::EchoLink,
                codec: None,
                label: label.to_owned(),
                local: a(local),
                peer: a(peer),
            }),
            at,
            wall,
        }
    }

    fn start(call: PacketCallId, at: Instant) -> PacketEvent {
        start_at(call, LABEL, LOCAL, PEER, at, wall())
    }

    /// The `Start` of an `AllStar` call that agreed on ulaw.
    fn ulaw_start(call: PacketCallId, at: Instant) -> PacketEvent {
        PacketEvent::Start {
            call,
            meta: Arc::new(CallMeta {
                network: PacketNetwork::AllStar,
                codec: Some("ulaw"),
                label: LABEL.to_owned(),
                local: a(LOCAL),
                peer: a(PEER),
            }),
            at,
            wall: wall(),
        }
    }

    fn packet(call: PacketCallId, at: Instant, dropped_before: u32, bytes: &[u8]) -> PacketEvent {
        PacketEvent::Packet {
            call,
            at,
            from: a(PEER),
            dropped_before,
            bytes: PacketBuf::unpooled(bytes),
        }
    }

    fn end(call: PacketCallId, dropped_before: u32) -> PacketEvent {
        PacketEvent::End {
            call,
            dropped_before,
        }
    }

    fn jitter(call: PacketCallId, at: Instant) -> PacketEvent {
        PacketEvent::Jitter {
            call,
            at,
            sample: JitterSample::default(),
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// The path of the only file in `dir` with this suffix.
    fn the_file(dir: &Path, suffix: &str) -> PathBuf {
        let hits: Vec<String> = names(dir)
            .into_iter()
            .filter(|n| n.ends_with(suffix))
            .collect();
        assert_eq!(hits.len(), 1, "{hits:?}");
        dir.join(&hits[0])
    }

    fn pcap_in(dir: &Path) -> Vec<PcapRecord> {
        read_pcap(&std::fs::read(the_file(dir, ".pcap")).unwrap()).unwrap()
    }

    fn lines_of(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn companion_in(dir: &Path) -> Vec<Value> {
        lines_of(&the_file(dir, COMPANION_SUFFIX))
    }

    fn wait_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < limit {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        done()
    }

    fn wall_ns(t: SystemTime) -> u64 {
        u64::try_from(t.duration_since(UNIX_EPOCH).unwrap().as_nanos()).unwrap()
    }

    #[test]
    fn a_call_makes_a_pcap_and_a_companion_with_matching_names() {
        let (dir, tx, log) = rig("names");
        let call = PacketCallId::next();
        tx.send(start(call, Instant::now())).unwrap();
        tx.send(end(call, 0)).unwrap();
        let _ = log.stop();
        let base = app_base_name(wall(), OFFSET, LABEL);
        assert_eq!(
            names(&dir),
            vec![format!("{base}.jitter.jsonl"), format!("{base}.pcap")]
        );
        assert_eq!(COMPANION_SUFFIX, ".jitter.jsonl");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_pcap_round_trips() {
        let (dir, tx, log) = rig("round");
        let call = PacketCallId::next();
        let at0 = Instant::now();
        tx.send(start(call, at0)).unwrap();
        let payload = |i: u32| vec![(i % 251) as u8; 1 + (i % 200) as usize];
        for i in 0..500u32 {
            let at = at0 + Duration::from_millis(u64::from(i) * 20);
            tx.send(packet(call, at, 0, &payload(i))).unwrap();
        }
        tx.send(end(call, 0)).unwrap();
        let state = log.stop();
        assert_eq!(state.error, None);
        let got = pcap_in(&dir);
        assert_eq!(got.len(), 500);
        for (i, r) in (0u32..).zip(&got) {
            assert_eq!(r.payload, payload(i));
            assert_eq!(r.src, a(PEER));
            assert_eq!(r.dst, a(LOCAL));
        }
        assert!(got.windows(2).all(|w| w[0].ts_ns < w[1].ts_ns));
        let bytes = std::fs::metadata(the_file(&dir, ".pcap")).unwrap().len()
            + std::fs::metadata(the_file(&dir, COMPANION_SUFFIX))
                .unwrap()
                .len();
        assert_eq!(state.used_bytes, bytes);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn timestamps_come_from_the_instant_not_the_wall_clock() {
        let (dir, tx, log) = rig("clock");
        let call = PacketCallId::next();
        let at0 = Instant::now();
        let year_ago = SystemTime::now() - Duration::from_secs(365 * 86_400);
        tx.send(start_at(call, LABEL, LOCAL, PEER, at0, year_ago))
            .unwrap();
        tx.send(packet(call, at0 + Duration::from_secs(1), 0, b"a"))
            .unwrap();
        tx.send(packet(call, at0 + Duration::from_secs(2), 0, b"b"))
            .unwrap();
        let _ = log.stop();
        let got = pcap_in(&dir);
        assert_eq!(got[0].ts_ns, wall_ns(year_ago) + 1_000_000_000);
        assert_eq!(got[1].ts_ns, wall_ns(year_ago) + 2_000_000_000);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_companion_opens_with_the_marker_and_ends_with_a_reason() {
        let (dir, tx, log) = rig("marker");
        let call = PacketCallId::next();
        let at0 = Instant::now();
        tx.send(start(call, at0)).unwrap();
        // A packet stamped later than "now" must still not outrun the end line.
        tx.send(packet(call, at0 + Duration::from_secs(60), 0, b"x"))
            .unwrap();
        tx.send(end(call, 0)).unwrap();
        let _ = log.stop();
        let lines = companion_in(&dir);
        assert_eq!(lines.len(), 2);
        let first = &lines[0];
        assert_eq!(first["astar_packet_log"], 1);
        assert_eq!(first["network"], "echolink");
        assert_eq!(first["label"], LABEL);
        assert_eq!(first["local"], LOCAL);
        assert_eq!(first["peer"], PEER);
        assert_eq!(first["started_at"], rfc3339_utc(wall()));
        assert_eq!(first["version"], "9.9.9-test");
        assert_eq!(first["part"], 1);
        let last = &lines[1];
        assert_eq!(last["end"], "hangup");
        assert_eq!(last["packets"], 1);
        assert_eq!(last["dropped"], 0);
        assert!(last["ts_ns"].as_u64().unwrap() >= pcap_in(&dir)[0].ts_ns);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_jitter_row_shares_the_pcap_clock() {
        let (dir, tx, log) = rig("row");
        let call = PacketCallId::next();
        let at0 = Instant::now();
        let at = at0 + Duration::from_millis(1500);
        tx.send(start(call, at0)).unwrap();
        tx.send(packet(call, at, 0, b"x")).unwrap();
        tx.send(PacketEvent::Jitter {
            call,
            at,
            sample: JitterSample {
                depth_ms: Some(60),
                lost: Some(3),
                ..JitterSample::default()
            },
        })
        .unwrap();
        tx.send(end(call, 0)).unwrap();
        let _ = log.stop();
        let lines = companion_in(&dir);
        let row = &lines[1];
        assert_eq!(row["ts_ns"].as_u64().unwrap(), pcap_in(&dir)[0].ts_ns);
        assert_eq!(row["packets"], 1);
        assert_eq!(row["depth_ms"], 60);
        assert_eq!(row["lost"], 3);
        let fields = row.as_object().unwrap();
        for k in ["jitter_ms", "late", "ooo", "underruns"] {
            assert_eq!(fields.get(k), Some(&Value::Null), "{k}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dropped_before_adds_up_in_the_rows_and_the_end_line() {
        let (dir, tx, log) = rig("dropped");
        let call = PacketCallId::next();
        let at = Instant::now();
        tx.send(start(call, at)).unwrap();
        tx.send(packet(call, at, 3, b"a")).unwrap();
        tx.send(jitter(call, at)).unwrap();
        tx.send(packet(call, at, 2, b"b")).unwrap();
        tx.send(jitter(call, at)).unwrap();
        tx.send(end(call, 4)).unwrap();
        let state = log.stop();
        let lines = companion_in(&dir);
        assert_eq!(lines[1]["dropped"], 3);
        assert_eq!(lines[2]["dropped"], 5);
        assert_eq!(lines[3]["dropped"], 9);
        assert_eq!(state.dropped, 9);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_packet_for_an_unknown_call_is_counted_not_written() {
        let (dir, tx, log) = rig("unknown");
        tx.send(packet(PacketCallId::next(), Instant::now(), 0, b"x"))
            .unwrap();
        assert!(wait_for(Duration::from_secs(2), || log.state().dropped == 1));
        let state = log.stop();
        assert_eq!((state.dropped, state.used_bytes, state.error), (1, 0, None));
        assert_eq!(names(&dir).len(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_end_or_a_row_for_an_unknown_call_is_ignored() {
        let (dir, tx, log) = rig("unknown-end");
        tx.send(end(PacketCallId::next(), 0)).unwrap();
        tx.send(jitter(PacketCallId::next(), Instant::now()))
            .unwrap();
        let state = log.stop();
        assert_eq!((state.dropped, state.error), (0, None));
        assert_eq!(names(&dir).len(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_file_is_readable_within_two_seconds_without_a_stop() {
        let (dir, tx, log) = rig("live");
        let call = PacketCallId::next();
        let at = Instant::now();
        tx.send(start(call, at)).unwrap();
        for _ in 0..10 {
            tx.send(packet(call, at, 0, b"voice")).unwrap();
        }
        let flushed = wait_for(Duration::from_secs(2), || {
            // The file exists a moment before its header is flushed.
            names(&dir).len() == 2
                && std::fs::read(the_file(&dir, ".pcap"))
                    .is_ok_and(|b| read_pcap(&b).is_ok_and(|r| r.len() == 10))
        });
        assert!(flushed, "packets still buffered after two seconds");
        assert!(log.state().capturing);
        let _ = log.stop();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stop_ends_open_files_with_stopped() {
        let (dir, tx, log) = rig("stop");
        let call = PacketCallId::next();
        let at = Instant::now();
        tx.send(start(call, at)).unwrap();
        tx.send(packet(call, at, 0, b"x")).unwrap();
        assert!(wait_for(Duration::from_secs(2), || log.state().open_calls == 1));
        // Queued just before the stop: still written.
        tx.send(packet(call, at, 0, b"y")).unwrap();
        let state = log.stop();
        assert_eq!(
            state,
            PacketLogState {
                capturing: false,
                open_calls: 0,
                used_bytes: state.used_bytes,
                dropped: 0,
                error: None,
            }
        );
        let lines = companion_in(&dir);
        let last = lines.last().unwrap();
        assert_eq!(last["end"], "stopped");
        assert_eq!(last["packets"], 2);
        assert_eq!(pcap_in(&dir).len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn two_calls_in_one_second_get_two_files() {
        let (dir, tx, log) = rig("twins");
        let (one, two) = (PacketCallId::next(), PacketCallId::next());
        let at = Instant::now();
        tx.send(start(one, at)).unwrap();
        tx.send(start(two, at)).unwrap();
        tx.send(packet(one, at, 0, b"first")).unwrap();
        tx.send(packet(two, at, 0, b"second")).unwrap();
        tx.send(end(one, 0)).unwrap();
        tx.send(end(two, 0)).unwrap();
        let _ = log.stop();
        let base = app_base_name(wall(), OFFSET, LABEL);
        let read = |n: String| read_pcap(&std::fs::read(dir.join(n)).unwrap()).unwrap();
        assert_eq!(read(format!("{base}.pcap"))[0].payload, b"first");
        assert_eq!(read(format!("{base} 2.pcap"))[0].payload, b"second");
        assert!(dir.join(format!("{base} 2.jitter.jsonl")).exists());
        assert_eq!(names(&dir).len(), 4);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_already_in_the_folder_is_never_overwritten() {
        let (dir, tx, log) = rig("theirs");
        let base = app_base_name(wall(), OFFSET, LABEL);
        std::fs::write(dir.join(format!("{base}.jitter.jsonl")), b"theirs").unwrap();
        let call = PacketCallId::next();
        tx.send(start(call, Instant::now())).unwrap();
        tx.send(end(call, 0)).unwrap();
        let _ = log.stop();
        assert_eq!(
            std::fs::read(dir.join(format!("{base}.jitter.jsonl"))).unwrap(),
            b"theirs"
        );
        assert!(dir.join(format!("{base} 2.pcap")).exists());
        assert!(!dir.join(format!("{base}.pcap")).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_second_start_for_an_open_call_ends_the_first_file() {
        let (dir, tx, log) = rig("restart");
        let call = PacketCallId::next();
        let at = Instant::now();
        tx.send(start(call, at)).unwrap();
        tx.send(packet(call, at, 0, b"a")).unwrap();
        tx.send(start(call, at)).unwrap();
        tx.send(packet(call, at, 0, b"b")).unwrap();
        tx.send(end(call, 0)).unwrap();
        let _ = log.stop();
        let base = app_base_name(wall(), OFFSET, LABEL);
        let first = lines_of(&dir.join(format!("{base}.jitter.jsonl")));
        let second = lines_of(&dir.join(format!("{base} 2.jitter.jsonl")));
        assert_eq!(first.last().unwrap()["end"], "stopped");
        assert_eq!(second.last().unwrap()["end"], "hangup");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_hostile_label_cannot_leave_the_folder() {
        let (dir, tx, log) = rig("hostile");
        let call = PacketCallId::next();
        let label = format!("../../\"evil\u{202E}\n{}", "x".repeat(300));
        tx.send(start_at(call, &label, LOCAL, PEER, Instant::now(), wall()))
            .unwrap();
        tx.send(end(call, 0)).unwrap();
        let _ = log.stop();
        let found = names(&dir);
        assert_eq!(found.len(), 2, "{found:?}");
        for n in &found {
            assert!(n.len() <= 255, "{n}");
            assert!(!n.contains(['"', '/', '\\', '\n', '\u{202E}']), "{n}");
        }
        let lines = companion_in(&dir);
        let written = lines[0]["label"].as_str().unwrap();
        assert!(!written.contains(['"', '/', '\n', '\u{202E}']), "{written}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_wildcard_local_address_is_replaced_by_the_routes_address() {
        let (dir, tx, log) = rig("wild4");
        let call = PacketCallId::next();
        let at = Instant::now();
        tx.send(start_at(
            call,
            LABEL,
            "0.0.0.0:5198",
            "127.0.0.1:9",
            at,
            wall(),
        ))
        .unwrap();
        tx.send(PacketEvent::Packet {
            call,
            at,
            from: a("127.0.0.1:9"),
            dropped_before: 0,
            bytes: PacketBuf::unpooled(b"x"),
        })
        .unwrap();
        let state = log.stop();
        assert_eq!(state.error, None);
        assert_eq!(pcap_in(&dir)[0].dst, a("127.0.0.1:5198"));
        assert_eq!(companion_in(&dir)[0]["local"], "127.0.0.1:5198");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_v4_peer_on_a_v6_wildcard_socket_writes_ipv4() {
        let (dir, tx, log) = rig("wild6");
        let call = PacketCallId::next();
        let at = Instant::now();
        let peer = "[::ffff:127.0.0.1]:9";
        tx.send(start_at(call, LABEL, "[::]:5198", peer, at, wall()))
            .unwrap();
        tx.send(PacketEvent::Packet {
            call,
            at,
            from: a(peer),
            dropped_before: 0,
            bytes: PacketBuf::unpooled(b"x"),
        })
        .unwrap();
        let state = log.stop();
        assert_eq!(state.error, None);
        let got = pcap_in(&dir);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].src, a("127.0.0.1:9"));
        assert_eq!(got[0].dst, a("127.0.0.1:5198"));
        assert_eq!(companion_in(&dir)[0]["peer"], "127.0.0.1:9");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_local_address_always_ends_up_in_the_peers_family() {
        // A real address is kept without a lookup.
        assert_eq!(local_for(a(LOCAL), a(PEER)), a(LOCAL));
        assert_eq!(
            local_for(a("[::ffff:192.0.2.1]:5198"), a(PEER)),
            a(LOCAL),
            "a v4-mapped local is plain IPv4"
        );
        // Whether or not a route exists, the family follows the peer.
        assert!(local_for(a(LOCAL), a("[2001:db8::1]:9")).is_ipv6());
        assert!(local_for(a("[2001:db8::2]:5198"), a("127.0.0.1:9")).is_ipv4());
        // Port zero can't be connected to; the wildcard is the fallback.
        let l = local_for(a("0.0.0.0:5198"), a("127.0.0.1:0"));
        assert!(l.is_ipv4());
        assert_eq!(l.port(), 5198);
    }

    #[test]
    fn a_write_error_ends_the_file_and_later_packets_are_counted() {
        let (dir, tx, log) = rig("write-error");
        let call = PacketCallId::next();
        let at = Instant::now();
        tx.send(start(call, at)).unwrap();
        tx.send(packet(call, at, 0, b"good")).unwrap();
        assert!(wait_for(Duration::from_secs(2), || log.state().open_calls == 1));
        // The pcap going away is fatal for the file, and the next flush sees it.
        std::fs::remove_file(the_file(&dir, ".pcap")).unwrap();
        assert!(wait_for(Duration::from_secs(3), || log
            .state()
            .error
            .is_some()));
        assert_eq!(log.state().open_calls, 0);
        tx.send(packet(call, at, 0, b"late")).unwrap();
        tx.send(end(call, 0)).unwrap();
        let state = log.stop();
        assert_eq!((state.dropped, state.open_calls), (1, 0));
        let lines = companion_in(&dir);
        let last = lines.last().unwrap();
        assert_eq!(last["end"], "write_error");
        assert_eq!(last["packets"], 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_packet_the_pcap_cannot_hold_is_counted_and_the_file_stays_open() {
        let (dir, tx, log) = rig("invalid");
        let call = PacketCallId::next();
        let at = Instant::now();
        tx.send(start(call, at)).unwrap();
        tx.send(packet(call, at, 0, b"good")).unwrap();
        // A v6 source against a v4 local is refused by the pcap writer.
        tx.send(PacketEvent::Packet {
            call,
            at,
            from: a("[2001:db8::1]:9"),
            dropped_before: 0,
            bytes: PacketBuf::unpooled(b"wrong family"),
        })
        .unwrap();
        tx.send(packet(call, at, 0, &vec![0u8; 70_000])).unwrap();
        tx.send(packet(call, at, 0, b"late")).unwrap();
        tx.send(end(call, 0)).unwrap();
        let state = log.stop();
        assert_eq!((state.dropped, state.error), (2, None));
        let got = pcap_in(&dir);
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].payload, b"late");
        let lines = companion_in(&dir);
        let last = lines.last().unwrap();
        assert_eq!(last["end"], "hangup");
        assert_eq!(last["packets"], 2);
        assert_eq!(last["dropped"], 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_folder_removed_mid_call_reports_an_error_and_keeps_draining() {
        let (dir, tx, log) = rig_with("gone", 16);
        let call = PacketCallId::next();
        let at = Instant::now();
        tx.send(start(call, at)).unwrap();
        tx.send(packet(call, at, 0, b"before")).unwrap();
        assert!(wait_for(Duration::from_secs(2), || log.state().open_calls == 1));
        std::fs::remove_dir_all(&dir).unwrap();

        // Far more than the channel holds, so a writer that quit draining
        // would run this into the deadline.
        let deadline = Instant::now() + Duration::from_secs(10);
        let feed = |ev: PacketEvent| {
            let mut ev = ev;
            loop {
                match tx.try_send(ev) {
                    Ok(()) => return,
                    Err(TrySendError::Full(back)) => {
                        assert!(Instant::now() < deadline, "the channel backed up");
                        ev = back;
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(TrySendError::Disconnected(_)) => panic!("the writer died"),
                }
            }
        };
        for _ in 0..200 {
            feed(packet(call, at, 0, b"after"));
        }
        assert!(wait_for(Duration::from_secs(3), || log
            .state()
            .error
            .is_some()));
        assert_eq!(log.state().open_calls, 0);
        for _ in 0..200 {
            feed(packet(call, at, 0, b"after"));
        }

        // The next call cannot open a file either, and that is not fatal.
        let next = PacketCallId::next();
        feed(start(next, at));
        feed(packet(next, at, 0, b"after"));
        let state = log.stop();
        assert!(state.error.is_some());
        assert_eq!(state.open_calls, 0);
        assert!(state.dropped >= 201, "{}", state.dropped);
        assert!(!dir.exists(), "the folder must not be recreated");
    }

    #[test]
    fn start_refuses_a_folder_it_cannot_create() {
        let file = tdir("not-a-folder");
        std::fs::write(&file, b"x").unwrap();
        let (_tx, rx) = sync_channel(1);
        let cfg = PacketLogConfig {
            dir: file.join("sub"),
            max_bytes: MIN_PACKET_LOG_BYTES,
            utc_offset_secs: 0,
            version: String::new(),
        };
        assert!(matches!(
            PacketLogger::start(cfg, rx),
            Err(RecordError::Folder(_))
        ));
        std::fs::remove_file(&file).unwrap();
    }

    #[test]
    fn start_refuses_a_zero_limit() {
        let dir = tdir("zero");
        let (_tx, rx) = sync_channel(1);
        assert!(matches!(
            PacketLogger::start(small_cfg(&dir, 0), rx),
            Err(RecordError::Io(e)) if e.kind() == ErrorKind::InvalidInput
        ));
        assert!(!dir.exists(), "a refused start touches nothing");
    }

    #[test]
    fn one_drain_pass_stops_at_the_channel_capacity() {
        let dir = folder("drain");
        let (tx, rx) = sync_channel(PACKET_TAP_CAPACITY * 2);
        let call = PacketCallId::next();
        for _ in 0..PACKET_TAP_CAPACITY + 10 {
            tx.send(packet(call, Instant::now(), 0, b"x")).unwrap();
        }
        let mut worker = Worker::new(
            small_cfg(&dir, DEFAULT_PACKET_LOG_BYTES),
            rx,
            Arc::default(),
            Arc::default(),
            |_| None,
            FREE_CHECK_EVERY,
        );
        worker.drain();
        assert_eq!(worker.dropped, PACKET_TAP_CAPACITY as u64);
        worker.drain();
        assert_eq!(worker.dropped, PACKET_TAP_CAPACITY as u64 + 10);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stop_returns_while_a_producer_keeps_sending() {
        let (dir, tx, log) = rig("firehose");
        let call = PacketCallId::next();
        tx.send(start(call, Instant::now())).unwrap();
        let producer = std::thread::spawn(move || {
            let at = Instant::now();
            loop {
                match tx.try_send(packet(call, at, 0, b"x")) {
                    Err(TrySendError::Disconnected(_)) => return,
                    Ok(()) | Err(TrySendError::Full(_)) => {}
                }
            }
        });
        std::thread::sleep(Duration::from_millis(100));
        let _ = log.stop();
        producer.join().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_long_call_rolls_into_parts() {
        let (dir, tx, log) = rig_limit("parts", 1_600_000);
        let call = PacketCallId::next();
        let at0 = Instant::now();
        tx.send(start(call, at0)).unwrap();
        let total = 2500u32;
        for i in 0..total {
            let at = at0 + Duration::from_millis(u64::from(i) * 20);
            let mut bytes = vec![0u8; 200];
            bytes[..4].copy_from_slice(&i.to_be_bytes());
            tx.send(packet(call, at, 0, &bytes)).unwrap();
        }
        tx.send(end(call, 0)).unwrap();
        let state = log.stop();
        assert_eq!((state.dropped, state.error), (0, None));

        let base = app_base_name(wall(), OFFSET, LABEL);
        let stem = |part: u32| match part {
            1 => base.clone(),
            n => format!("{base} part {n}"),
        };
        let parts = u32::try_from(names(&dir).len() / 2).unwrap();
        assert!(parts >= 6, "{:?}", names(&dir));

        let (mut seen, mut counted) = (0u32, 0u64);
        for part in 1..=parts {
            let pcap = dir.join(format!("{}.pcap", stem(part)));
            let got = read_pcap(&std::fs::read(&pcap).unwrap()).unwrap();
            for r in &got {
                assert_eq!(r.payload[..4], seen.to_be_bytes(), "order across parts");
                seen += 1;
            }
            let lines = lines_of(&dir.join(format!("{}.jitter.jsonl", stem(part))));
            assert_eq!(lines[0]["astar_packet_log"], 1);
            assert_eq!(lines[0]["part"], part);
            assert_eq!(lines[0]["label"], LABEL);
            assert_eq!(lines[0]["peer"], PEER);
            let last = lines.last().unwrap();
            let reason = if part == parts { "hangup" } else { "continued" };
            assert_eq!(last["end"], reason);
            assert_eq!(last["packets"], got.len());
            counted += last["packets"].as_u64().unwrap();
            if part < parts {
                assert!(file_len(&pcap) >= 100_000);
                assert!(file_len(&pcap) < 100_000 + 300, "one packet past at most");
            }
        }
        assert_eq!(seen, total);
        assert_eq!(counted, u64::from(total));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_log_keeps_what_replay_needs() {
        const OFFSETS_NS: [u64; 3] = [0, 20_000_001, 40_000_003];

        let (dir, tx, log) = rig("replay");
        let call = PacketCallId::next();
        let at0 = Instant::now();
        tx.send(ulaw_start(call, at0)).unwrap();
        // Multiplying by an odd number reorders the bytes and loses none.
        let sent: Vec<Vec<u8>> = [1u8, 3, 5]
            .iter()
            .map(|step| (0..=255u8).map(|b| b.wrapping_mul(*step)).collect())
            .collect();
        for (offset, bytes) in OFFSETS_NS.iter().zip(&sent) {
            let at = at0 + Duration::from_nanos(*offset);
            tx.send(packet(call, at, 0, bytes)).unwrap();
        }
        tx.send(end(call, 0)).unwrap();
        let state = log.stop();
        assert_eq!((state.dropped, state.error), (0, None));

        let got = read_pcap(&std::fs::read(the_file(&dir, ".pcap")).unwrap()).unwrap();
        assert_eq!(got.len(), sent.len());
        for ((record, bytes), offset) in got.iter().zip(&sent).zip(OFFSETS_NS) {
            assert_eq!(&record.payload, bytes);
            assert_eq!(record.ts_ns - got[0].ts_ns, offset);
        }

        let header = &companion_in(&dir)[0];
        assert_eq!(header["network"], "allstar");
        assert_eq!(header["codec"], "ulaw");
        assert_eq!(header["local"], LOCAL);
        assert_eq!(header["peer"], PEER);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_codec_follows_the_network_in_the_header() {
        let (dir, tx, log) = rig("codec-key");
        let call = PacketCallId::next();
        let at = Instant::now();
        tx.send(start(call, at)).unwrap();
        tx.send(packet(call, at, 0, b"x")).unwrap();
        tx.send(end(call, 0)).unwrap();
        let _ = log.stop();

        let text = std::fs::read_to_string(the_file(&dir, COMPANION_SUFFIX)).unwrap();
        assert!(
            text.starts_with("{\"astar_packet_log\":1,\"network\":\"echolink\",\"codec\":null,"),
            "{text}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_second_part_keeps_the_codec() {
        let (dir, tx, log) = rig_limit("parts-codec", 16_000);
        let call = PacketCallId::next();
        let at = Instant::now();
        tx.send(ulaw_start(call, at)).unwrap();
        tx.send(packet(call, at, 0, &[0u8; 1200])).unwrap();
        tx.send(packet(call, at, 0, b"next part")).unwrap();
        tx.send(end(call, 0)).unwrap();
        let _ = log.stop();

        let parts = companions(&dir);
        assert_eq!(parts.len(), 2);
        for part in &parts {
            assert_eq!(part[0]["codec"], "ulaw");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_part_of_a_renamed_call_keeps_the_name_it_got() {
        let dir = folder("parts-twin");
        let base = app_base_name(wall(), OFFSET, LABEL);
        std::fs::write(dir.join(format!("{base}.pcap")), b"theirs").unwrap();
        let (tx, log) = rig_in(&dir, 16_000);
        let call = PacketCallId::next();
        let at = Instant::now();
        tx.send(start(call, at)).unwrap();
        tx.send(packet(call, at, 0, &[0u8; 1200])).unwrap();
        tx.send(packet(call, at, 0, b"next part")).unwrap();
        let _ = log.stop();
        assert!(dir.join(format!("{base} 2 part 2.pcap")).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_label_ending_in_pcap_keeps_it_in_later_parts() {
        let (dir, tx, log) = rig_limit("parts-label", 16_000);
        let call = PacketCallId::next();
        let at = Instant::now();
        tx.send(start_at(call, "x.pcap", LOCAL, PEER, at, wall()))
            .unwrap();
        tx.send(packet(call, at, 0, &[0u8; 1200])).unwrap();
        tx.send(packet(call, at, 0, b"next part")).unwrap();
        let _ = log.stop();
        let base = app_base_name(wall(), OFFSET, "x.pcap");
        assert!(dir.join(format!("{base} part 2.pcap")).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_call_that_stops_at_the_part_size_opens_no_empty_part() {
        let (dir, tx, log) = rig_limit("parts-exact", 16_000);
        let call = PacketCallId::next();
        let at = Instant::now();
        tx.send(start(call, at)).unwrap();
        tx.send(packet(call, at, 0, &[0u8; 1200])).unwrap();
        tx.send(end(call, 0)).unwrap();
        let _ = log.stop();
        assert_eq!(names(&dir).len(), 2, "{:?}", names(&dir));
        assert_eq!(companion_in(&dir).last().unwrap()["end"], "hangup");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_folder_never_passes_the_limit() {
        let max = 100_000;
        let (dir, tx, log) = rig_limit("limit", max);
        let call = PacketCallId::next();
        let at0 = Instant::now();
        tx.send(start(call, at0)).unwrap();
        for batch in 0..40u64 {
            for i in 0..50u64 {
                let at = at0 + Duration::from_millis((batch * 50 + i) * 20);
                tx.send(packet(call, at, 0, &[7u8; 200])).unwrap();
                if i % 10 == 0 {
                    tx.send(jitter(call, at)).unwrap();
                }
            }
            std::thread::sleep(Duration::from_millis(25));
            assert!(log_bytes(&dir) <= max, "batch {batch}");
            assert!(log.state().used_bytes <= max, "batch {batch}");
        }
        tx.send(end(call, 0)).unwrap();
        let state = log.stop();
        assert!(log_bytes(&dir) <= max);
        assert_eq!(state.used_bytes, log_bytes(&dir));
        assert_eq!((state.dropped, state.error), (0, None));
        let base = app_base_name(wall(), OFFSET, LABEL);
        assert!(
            !dir.join(format!("{base}.pcap")).exists(),
            "the first part was the oldest"
        );
        for lines in companions(&dir) {
            assert!(lines.last().unwrap().get("end").is_some());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_oldest_closed_logs_go_first() {
        let dir = folder("oldest");
        // The names run against the ages, so a sort by name would get this wrong.
        let used = closed_log(&dir, "c", 1000, 3 * 86_400)
            + closed_log(&dir, "a", 1000, 86_400)
            + closed_log(&dir, "b", 1000, 2 * 86_400);
        let (tx, log) = rig_in(&dir, used + 30);
        let call = PacketCallId::next();
        tx.send(start(call, Instant::now())).unwrap();
        tx.send(end(call, 0)).unwrap();
        let _ = log.stop();
        assert!(!dir.join("c.pcap").exists());
        assert!(!dir.join("c.jitter.jsonl").exists());
        assert!(dir.join("a.pcap").exists());
        assert!(dir.join("b.pcap").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_open_file_is_never_deleted() {
        let (dir, tx, log) = rig_limit("open", 1600);
        let (open, next) = (PacketCallId::next(), PacketCallId::next());
        let at = Instant::now();
        tx.send(start(open, at)).unwrap();
        assert!(wait_for(Duration::from_secs(2), || log.state().open_calls == 1));
        // Older than the closed log, so only being open can save it.
        let long_ago = SystemTime::now() - Duration::from_secs(10 * 86_400);
        set_modified(&the_file(&dir, ".pcap"), long_ago);
        set_modified(&the_file(&dir, COMPANION_SUFFIX), long_ago);
        closed_log(&dir, "closed", 1000, 86_400);

        tx.send(start_at(next, "Next", LOCAL, PEER, at, wall()))
            .unwrap();
        let state = log.stop();
        assert_eq!(state.error, None);
        assert!(!dir.join("closed.pcap").exists());
        let base = app_base_name(wall(), OFFSET, LABEL);
        assert!(dir.join(format!("{base}.pcap")).exists());
        let lines = lines_of(&dir.join(format!("{base}.jitter.jsonl")));
        assert_eq!(lines.last().unwrap()["end"], "stopped");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn when_open_files_fill_the_limit_packets_are_counted_and_the_file_ends_with_limit() {
        // A part holds a thousand bytes, so no packet here makes a file roll,
        // and twenty open files can't all fit.
        let max = 16_000;
        let (dir, tx, log) = rig_limit("full", max);
        let at = Instant::now();
        let calls: Vec<PacketCallId> = (0..20).map(|_| PacketCallId::next()).collect();
        for (i, call) in calls.iter().enumerate() {
            let label = format!("Call {i}");
            tx.send(start_at(*call, &label, LOCAL, PEER, at, wall()))
                .unwrap();
        }
        for call in &calls {
            tx.send(packet(*call, at, 0, &[1u8; 800])).unwrap();
        }
        let state = log.stop();
        assert!(state.dropped > 0);
        assert_eq!(state.error.as_deref(), Some(FOLDER_FULL));
        assert!(log_bytes(&dir) <= max);

        let reasons: Vec<String> = companions(&dir)
            .iter()
            .map(|lines| lines.last().unwrap()["end"].as_str().unwrap().to_owned())
            .collect();
        assert!(reasons.iter().any(|r| r == "limit"), "{reasons:?}");
        assert!(reasons.iter().any(|r| r == "stopped"), "{reasons:?}");

        // A file that ended here held no packets, so one pruned loses none.
        let written: u64 = names(&dir)
            .iter()
            .filter(|n| is_pcap(n))
            .map(|n| {
                read_pcap(&std::fs::read(dir.join(n)).unwrap())
                    .unwrap()
                    .len() as u64
            })
            .sum();
        assert_eq!(written + state.dropped, 20);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_limit_ended_file_counts_the_packet_that_ended_it() {
        let (dir, tx, log) = rig_limit("too-big", 1000);
        let call = PacketCallId::next();
        let at = Instant::now();
        tx.send(start(call, at)).unwrap();
        tx.send(packet(call, at, 0, &[0u8; 2000])).unwrap();
        tx.send(packet(call, at, 0, b"later")).unwrap();
        let state = log.stop();
        assert_eq!(state.dropped, 2);
        let lines = companion_in(&dir);
        let last = lines.last().unwrap();
        assert_eq!(last["end"], "limit");
        assert_eq!(last["dropped"], 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stop_as_hangup_writes_hangup() {
        let (dir, tx, log) = rig("stop-hangup");
        let call = PacketCallId::next();
        let at = Instant::now();
        tx.send(start(call, at)).unwrap();
        tx.send(packet(call, at, 0, b"x")).unwrap();
        assert!(wait_for(Duration::from_secs(2), || log.state().open_calls == 1));
        let state = log.stop_as(PacketLogEnd::Hangup);
        assert!(!state.capturing);
        assert_eq!(state.error, None);
        assert_eq!(companion_in(&dir).last().unwrap()["end"], "hangup");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dropping_the_logger_writes_stopped() {
        let (dir, tx, log) = rig("drop");
        let call = PacketCallId::next();
        tx.send(start(call, Instant::now())).unwrap();
        assert!(wait_for(Duration::from_secs(2), || log.state().open_calls == 1));
        drop(log);
        assert_eq!(companion_in(&dir).last().unwrap()["end"], "stopped");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reaching_the_limit_reports_an_error() {
        let (dir, tx, log) = rig_limit("full-error", 1000);
        let call = PacketCallId::next();
        let at = Instant::now();
        tx.send(start(call, at)).unwrap();
        tx.send(packet(call, at, 0, &[0u8; 2000])).unwrap();
        let state = log.stop();
        assert_eq!(companion_in(&dir).last().unwrap()["end"], "limit");
        assert_eq!(state.error.as_deref(), Some(FOLDER_FULL));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_jitter_row_with_no_room_reports_an_error() {
        // Room for the header and the end line, and not for one row.
        let (dir, tx, log) = rig_limit("row-full", 400);
        let call = PacketCallId::next();
        let at = Instant::now();
        tx.send(start(call, at)).unwrap();
        tx.send(jitter(call, at)).unwrap();
        let state = log.stop();
        assert_eq!(companion_in(&dir).last().unwrap()["end"], "limit");
        assert_eq!(state.error.as_deref(), Some(FOLDER_FULL));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_call_with_no_room_for_a_file_reports_an_error() {
        let (dir, tx, log) = rig_limit("open-full", 10);
        tx.send(start(PacketCallId::next(), Instant::now()))
            .unwrap();
        let state = log.stop();
        assert_eq!(names(&dir).len(), 0);
        assert_eq!(state.error.as_deref(), Some(FOLDER_FULL));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn retention_leaves_foreign_files_alone() {
        let dir = folder("foreign");
        std::fs::write(dir.join("alone.pcap"), vec![0u8; 5000]).unwrap();
        std::fs::write(dir.join("theirs.pcap"), vec![0u8; 5000]).unwrap();
        std::fs::write(dir.join("theirs.jitter.jsonl"), "{\"other\":1}\n").unwrap();
        std::fs::write(dir.join("notes.txt"), vec![0u8; 5000]).unwrap();
        closed_log(&dir, "ours", 1000, 86_400);
        let (tx, log) = rig_in(&dir, 10);
        tx.send(start(PacketCallId::next(), Instant::now()))
            .unwrap();
        let state = log.stop();
        assert_eq!(state.used_bytes, 0, "foreign files aren't ours to count");
        assert_eq!(
            names(&dir),
            vec![
                "alone.pcap",
                "notes.txt",
                "theirs.jitter.jsonl",
                "theirs.pcap"
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_lowered_limit_prunes_at_start() {
        let dir = folder("lowered");
        let each = closed_log(&dir, "day1", 1000, 86_400 * 4);
        closed_log(&dir, "day2", 1000, 86_400 * 3);
        closed_log(&dir, "day3", 1000, 86_400 * 2);
        closed_log(&dir, "day4", 1000, 86_400);
        let (_tx, log) = rig_in(&dir, each * 2 + 1);
        // No event at all: starting is what prunes.
        assert!(wait_for(Duration::from_secs(2), || !dir
            .join("day2.pcap")
            .exists()));
        let state = log.stop();
        assert_eq!(
            names(&dir),
            vec![
                "day3.jitter.jsonl",
                "day3.pcap",
                "day4.jitter.jsonl",
                "day4.pcap"
            ]
        );
        assert_eq!(state.used_bytes, each * 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn used_bytes_starts_from_what_the_folder_holds() {
        let dir = folder("measured");
        let each = closed_log(&dir, "old", 1000, 86_400);
        let (_tx, log) = rig_in(&dir, DEFAULT_PACKET_LOG_BYTES);
        assert!(wait_for(Duration::from_secs(2), || log.state().used_bytes == each));
        let _ = log.stop();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn used_bytes_forgets_files_that_were_removed() {
        let (dir, tx, log) = rig("vanished");
        let (one, two) = (PacketCallId::next(), PacketCallId::next());
        let at = Instant::now();
        tx.send(start(one, at)).unwrap();
        tx.send(packet(one, at, 0, &[0u8; 500])).unwrap();
        tx.send(end(one, 0)).unwrap();
        assert!(wait_for(Duration::from_secs(2), || log.state().used_bytes
            > 500
            && log.state().open_calls == 0));
        for n in names(&dir) {
            std::fs::remove_file(dir.join(n)).unwrap();
        }
        tx.send(start(two, at)).unwrap();
        tx.send(end(two, 0)).unwrap();
        let state = log.stop();
        assert_eq!(state.used_bytes, log_bytes(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    static LOW_MID_CALL: AtomicBool = AtomicBool::new(false);
    static LOW_AT_START: AtomicBool = AtomicBool::new(false);

    fn low_if(flag: &AtomicBool) -> Option<u64> {
        flag.load(Ordering::SeqCst).then_some(MIN_FREE_BYTES - 1)
    }

    fn rig_free(
        name: &str,
        free_space: fn(&Path) -> Option<u64>,
        free_every: Duration,
    ) -> (PathBuf, SyncSender<PacketEvent>, PacketLogger) {
        let dir = tdir(name);
        let (tx, rx) = sync_channel(4096);
        let cfg = small_cfg(&dir, DEFAULT_PACKET_LOG_BYTES);
        let log = PacketLogger::start_with(cfg, rx, free_space, free_every).unwrap();
        (dir, tx, log)
    }

    #[test]
    fn low_disk_ends_files_with_disk_low() {
        let (dir, tx, log) = rig_free(
            "disk-low",
            |_| low_if(&LOW_MID_CALL),
            Duration::from_millis(10),
        );
        let call = PacketCallId::next();
        let at = Instant::now();
        tx.send(start(call, at)).unwrap();
        tx.send(packet(call, at, 0, b"before")).unwrap();
        assert!(wait_for(Duration::from_secs(2), || log.state().open_calls == 1));
        LOW_MID_CALL.store(true, Ordering::SeqCst);
        assert!(wait_for(Duration::from_secs(2), || log.state().open_calls == 0));
        tx.send(packet(call, at, 0, b"after")).unwrap();
        let state = log.stop();
        assert_eq!(state.dropped, 1);
        assert!(state.error.unwrap().contains("disk has only 499 MB free"));
        let lines = companion_in(&dir);
        let last = lines.last().unwrap();
        assert_eq!(last["end"], "disk_low");
        assert_eq!(last["packets"], 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_start_on_a_low_disk_ends_open_files_and_opens_nothing() {
        // The periodic check never fires here, so only the `Start` can notice.
        let (dir, tx, log) = rig_free(
            "disk-start",
            |_| low_if(&LOW_AT_START),
            Duration::from_secs(3600),
        );
        let (first, second, third) = (
            PacketCallId::next(),
            PacketCallId::next(),
            PacketCallId::next(),
        );
        let at = Instant::now();
        tx.send(start(first, at)).unwrap();
        assert!(wait_for(Duration::from_secs(2), || log.state().open_calls == 1));
        LOW_AT_START.store(true, Ordering::SeqCst);
        tx.send(start(second, at)).unwrap();
        assert!(wait_for(Duration::from_secs(2), || log.state().open_calls == 0));
        assert_eq!(names(&dir).len(), 2);
        assert_eq!(companion_in(&dir).last().unwrap()["end"], "disk_low");

        // Space coming back lets the next call log.
        LOW_AT_START.store(false, Ordering::SeqCst);
        tx.send(start(third, at)).unwrap();
        let _ = log.stop();
        assert_eq!(names(&dir).len(), 4);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn start_refuses_a_low_disk() {
        let dir = tdir("disk-refused");
        let (_tx, rx) = sync_channel(1);
        let refused = PacketLogger::start_with(
            small_cfg(&dir, DEFAULT_PACKET_LOG_BYTES),
            rx,
            |_| Some(1),
            Duration::from_secs(5),
        );
        assert!(matches!(refused, Err(RecordError::DiskLow { .. })));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_paths_follow_the_call_through_its_parts() {
        let (dir, tx, log) = rig_limit("open-paths", 16_000);
        let call = PacketCallId::next();
        let at = Instant::now();
        let base = app_base_name(wall(), OFFSET, LABEL);
        tx.send(start(call, at)).unwrap();
        assert!(wait_for(Duration::from_secs(2), || log.open_paths()
            == [dir.join(format!("{base}.pcap"))]));
        tx.send(packet(call, at, 0, &[0u8; 1200])).unwrap();
        tx.send(packet(call, at, 0, b"next part")).unwrap();
        assert!(wait_for(Duration::from_secs(2), || log.open_paths()
            == [dir.join(format!("{base} part 2.pcap"))]));
        tx.send(end(call, 0)).unwrap();
        assert!(wait_for(Duration::from_secs(2), || log
            .open_paths()
            .is_empty()));
        let _ = log.stop();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn is_our_log_reads_the_siblings_first_line() {
        let dir = folder("ours");
        closed_log(&dir, "ours", 10, 0);
        std::fs::write(dir.join("alone.pcap"), b"x").unwrap();
        std::fs::write(dir.join("late.pcap"), b"x").unwrap();
        let second_line = "{\"other\":1}\n{\"astar_packet_log\":1}\n";
        std::fs::write(dir.join("late.jitter.jsonl"), second_line).unwrap();
        std::fs::write(dir.join("ours.txt"), b"x").unwrap();
        assert!(is_our_log(&dir.join("ours.pcap")));
        assert!(!is_our_log(&dir.join("alone.pcap")));
        assert!(!is_our_log(&dir.join("late.pcap")));
        assert!(!is_our_log(&dir.join("ours.txt")));
        assert!(!is_our_log(&dir.join("missing.pcap")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_logs_skips_foreign_files_and_marks_open_ones() {
        let dir = folder("list");
        let old = closed_log(&dir, "old", 100, 2 * 86_400);
        closed_log(&dir, "new", 200, 86_400);
        std::fs::write(dir.join("alone.pcap"), b"x").unwrap();
        std::fs::write(dir.join("theirs.pcap"), b"x").unwrap();
        std::fs::write(dir.join("theirs.jitter.jsonl"), "{}\n").unwrap();

        let rows = list_logs(&dir, &[dir.join("old.pcap")]);
        let got: Vec<(&str, bool)> = rows.iter().map(|r| (r.name.as_str(), r.open)).collect();
        assert_eq!(got, [("new.pcap", false), ("old.pcap", true)]);
        assert_eq!(rows[1].companion, "old.jitter.jsonl");
        assert_eq!(rows[1].bytes, old);
        let modified = std::fs::metadata(dir.join("old.pcap"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(rows[1].modified, rfc3339_utc(modified));
        assert_eq!(list_logs(&dir.join("missing"), &[]).len(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn delete_log_refuses_paths_foreign_files_and_open_logs() {
        let outer = folder("delete");
        let dir = outer.join("logs");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        closed_log(&outer, "x", 10, 0);
        closed_log(&dir.join("a"), "b", 10, 0);
        closed_log(&dir, "open", 10, 0);
        closed_log(&dir, "closed", 10, 0);
        std::fs::write(dir.join("alone.pcap"), b"x").unwrap();
        let open = [dir.join("open.pcap")];

        for name in [
            "../x.pcap",
            "a/b.pcap",
            "alone.pcap",
            "missing.pcap",
            "",
            "..",
        ] {
            assert_eq!(
                delete_log(&dir, name, &open),
                Err(DeleteRefused::NoSuchLog),
                "{name}"
            );
        }
        let absolute = outer.join("x.pcap");
        assert_eq!(
            delete_log(&dir, absolute.to_str().unwrap(), &open),
            Err(DeleteRefused::NoSuchLog)
        );
        assert_eq!(
            delete_log(&dir, "open.pcap", &open),
            Err(DeleteRefused::Open)
        );
        assert!(outer.join("x.pcap").exists());
        assert!(dir.join("a/b.pcap").exists());
        assert!(dir.join("alone.pcap").exists());
        assert!(dir.join("open.pcap").exists());

        assert_eq!(delete_log(&dir, "closed.pcap", &open), Ok(()));
        assert!(!dir.join("closed.pcap").exists());
        assert!(!dir.join("closed.jitter.jsonl").exists());
        std::fs::remove_dir_all(&outer).unwrap();
    }
}
