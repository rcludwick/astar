// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! SSTV through the node: a headless astar station sends a picture to the
//! ASL3 node through the jitter relay, and the bench decodes what the node
//! puts on the air.
//!
//! `sstv_station_to_node`: for each relay profile (`clean`, `net-1118`,
//! `harsh`, seed 1118), a [`HeadlessStation`] (no sound device) dials the
//! node through a [`Relay`] on 127.0.0.1 and calls `send_picture`. The node
//! carries the call's audio to its radio, which is the bench's USRP probe.
//! The recording is decoded three ways ([`judge`]): by the outside decoder,
//! which shares no code with astar, by `astar-sstv`, the decoder the app
//! uses, and by the `sstv` fork's own decoder with its de-jitter in front.
//! Robot 36 always; Scottie 2 and PD120 with `--long`.
//!
//! **How the station gets onto the node.** The station dials the way the
//! app dials a node: the Web Transceiver call shape, IAX2 username
//! `allstar-public`, `CALLING_NUMBER` = the node it wants and
//! `CALLING_NAME` = who it is. The bench node's `[allstar-public]` user and
//! `nodebench-station` context (`images/asl3/conf/`) stand in for the stock
//! ASL3 ones: in place of the portal lookup they accept only the name 1997,
//! then run `Rpt(${NODENUM},X)` as the stock context does (RIG.md, "SSTV through
//! the node").
//!
//! `sstv_station_via_server` ([`SstvStationViaServer`]) is the two-hop
//! version: the station dials astar-server's bridge through the relay, and
//! astar-server's link carries the picture to the node.
//!
//! Nothing here leaves 127.0.0.1: the relay refuses any other target, and
//! the station is told the relay's address, never a node number to look up.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use astar_station::{
    CallStatus, ConsoleConfig, ConsoleSession, PictureSend, SendEnd, SstvPicture, SstvTxMode,
    Station, StationConfig, sstv_tx_modes,
};

use super::common::{self, TEST_SECRET, clean_start};
use super::{Check, Ctx, Outcome, Scenario, wait_until};
use crate::node::parse::RptLinkMode;
use crate::outside::{self, OutsideDecode};
use crate::picture::{fitted_error, mean_error, test_picture, top_rows};
use crate::probe::RxFrame;
use crate::probe::usrp::{SAMPLE_RATE, UsrpProbe, VOICE_SAMPLES};
use crate::relay::{Profile, Relay, RelayReport};
use crate::server::LinkAction;

/// The relay's seed: the same seed gives the same schedule, so a failure
/// replays exactly. 1118 is the date of the recordings the profiles copy.
pub const SEED: u64 = 1118;
/// Who the station says it is (a bench test number; never registered).
pub const STATION_NUMBER: &str = "1997";

/// The limits on the mean error per colour channel (0–255) for each decoder:
/// twice the worst `clean` figure with the 4 s lead, among the cells where
/// the line-up shows the node carried the audio whole (RIG.md, "SSTV
/// through the node", "The limits"). A `clean` cell where the node slipped
/// is judged on the same limit and fails if it is over; its detail says what
/// the line-up measured. The outside decoder is judged on its fitted error
/// ([`fitted_error`]); `astar-sstv` on the plain one ([`mean_error`]).
pub const SLOWRX_LIMIT: Limits = Limits {
    // 7 slip-free clean cells of 12: worst 5.77.
    robot36: 11.5,
    // Provisional, 1 slip-free clean cell of 2: 8.43.
    scottie2: 16.9,
    // Provisional, 2 slip-free clean cells of 2: 6.46.
    pd120: 12.9,
};
/// See [`SLOWRX_LIMIT`].
pub const OUTSIDE_LIMIT: Limits = Limits {
    // 7 slip-free clean cells of 12: worst 21.54 fitted (29.42 raw).
    robot36: 43.1,
    // Provisional, 1 slip-free clean cell of 2: 16.05 fitted (28.16 raw).
    scottie2: 32.1,
    // Provisional, 2 slip-free clean cells of 2: 2.33 fitted, read at
    // 640x496 both times.
    pd120: 4.7,
};

/// See [`SLOWRX_LIMIT`]. The `sstv` fork's decoder, de-jittered, on the
/// plain mean error, counting the slip-free `clean` cells of both SSTV
/// scenarios. At 8 kHz its demodulator is noisier than slowrx (RIG.md, "The
/// fork's own decoder"), so its Robot 36 and PD120 limits sit above slowrx's.
pub const FORK_LIMIT: Limits = Limits {
    // Provisional, 4 slip-free clean cells of 8: worst 12.47.
    robot36: 24.9,
    // Provisional, 2 slip-free clean cells of 2: 6.81.
    scottie2: 13.6,
    // Provisional, 2 slip-free clean cells of 2: 11.06.
    pd120: 22.1,
};

/// One limit per picture mode the scenario sends.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub robot36: f64,
    pub scottie2: f64,
    pub pd120: f64,
}

impl Limits {
    fn of(&self, mode: astar_sstv::Mode) -> f64 {
        match mode {
            astar_sstv::Mode::Scottie2 => self.scottie2,
            astar_sstv::Mode::Pd120 => self.pd120,
            _ => self.robot36,
        }
    }
}

/// A lost USRP frame is filled with silence when the node's sequence number
/// skips at most this many frames (half a second: more than the `harsh`
/// profile's 400 ms dropout). A longer skip is a new transmission.
const MAX_FILL_FRAMES: u32 = 25;
/// The node has unkeyed once it sends no voice for this long.
const QUIET: Duration = Duration::from_millis(1500);
/// How long the station's call gets to be answered.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
/// The transmit time-out the PD120 station runs with: PD120 is 126 s plus
/// lead and tail, past the 120 s default.
const PD120_TX_TIMEOUT: Duration = Duration::from_secs(180);

pub struct SstvStationToNode;

impl Scenario for SstvStationToNode {
    fn id(&self) -> &'static str {
        "sstv_station_to_node"
    }

    fn describe(&self) -> &'static str {
        "a headless station sends an SSTV picture to the node through the jitter relay; the node's radio audio is decoded by an outside decoder and by astar-sstv"
    }

    fn run(&self, ctx: &mut Ctx) -> Outcome {
        let mut o = Outcome::new();
        if let Err(e) = clean_start(ctx) {
            o.check(Check::fail("clean start", e));
            return o;
        }
        let mut modes = vec![sstv_tx_modes::ROBOT_36];
        if ctx.tuning.long {
            modes.extend([sstv_tx_modes::SCOTTIE_2, sstv_tx_modes::PD_120]);
        }
        o.note(format!(
            "relay seed {SEED}; modes {:?}; the station is {STATION_NUMBER}, dialling {} through the relay",
            modes,
            ctx.cfg().node_iax_host
        ));
        for profile in Profile::all() {
            for &mode in &modes {
                let tag = format!("{} {}", profile.id(), sstv_name(mode).name());
                send_cell(ctx, &mut o, &tag, (profile, mode), &TO_NODE);
                if let Err(e) = clean_start(ctx) {
                    o.check(Check::fail(format!("{tag}: clean afterwards"), e));
                    return o;
                }
            }
        }
        o
    }
}

/// Where a cell's station goes and how it gets there: the parts that differ
/// between `sstv_station_to_node` and `sstv_station_via_server`.
struct Route {
    /// The address the relay forwards to (published on the host).
    target: fn(&Ctx) -> SocketAddr,
    /// Dial through the relay and wait until the station is on; `None`, with
    /// a failed check, when it is not.
    get_on: fn(&Ctx, &mut Outcome, &str, &Relay, &UsrpProbe) -> Option<HeadlessStation>,
    /// Measure what crossed since the pod mark: the worst lateness at the
    /// node (ms) and any artifacts to keep if the cell fails.
    arrivals: fn(&Ctx, &mut Outcome, &str, Option<f64>, &SstvPicture, SstvTxMode) -> Arrivals,
    /// Whether a failed cell also keeps the pod's full frames.
    keep_frames: bool,
}

/// What a [`Route`]'s arrival measurement returns.
type Arrivals = (Option<f64>, Vec<std::path::PathBuf>);

/// `sstv_station_to_node`: straight to the node.
const TO_NODE: Route = Route {
    target: node_host,
    get_on: on_the_node,
    arrivals: node_arrival,
    keep_frames: false,
};

fn node_host(ctx: &Ctx) -> SocketAddr {
    ctx.cfg().node_iax_host
}

/// One profile and one mode: dial through a fresh relay, send, record,
/// judge.
fn send_cell(
    ctx: &Ctx,
    o: &mut Outcome,
    tag: &str,
    (profile, mode): (Profile, SstvTxMode),
    route: &Route,
) {
    // Any failure in this cell, the send's own included, keeps its
    // artifacts.
    let fails = |o: &Outcome| o.checks.iter().filter(|c| c.is_fail()).count();
    let failed_before = fails(o);
    let cell_mark = route
        .keep_frames
        .then(|| crate::pcap::pod_now(&ctx.rig).ok())
        .flatten();
    let kept = send_and_judge(ctx, o, tag, (profile, mode), route);
    let failed = fails(o) > failed_before;
    if let Some((judged, rec, report, extra)) = kept {
        if failed {
            judged.save(ctx, o, tag, &rec, &report);
            o.artifacts.extend(extra);
        } else {
            judged.discard();
            for path in extra {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    if failed && route.keep_frames {
        common::save_cell_frames(ctx, o, tag, cell_mark);
    }
}

/// The body of [`send_cell`]: everything up to the verdict. `None` when the
/// cell stopped before a picture was recorded (its failed check says why).
fn send_and_judge(
    ctx: &Ctx,
    o: &mut Outcome,
    tag: &str,
    (profile, mode): (Profile, SstvTxMode),
    route: &Route,
) -> Option<(Judged, UsrpRecording, RelayReport, Vec<std::path::PathBuf>)> {
    let usrp = match common::usrp_probe() {
        Ok(u) => u,
        Err(e) => {
            o.check(Check::fail(format!("{tag}: USRP probe"), e));
            return None;
        }
    };
    let relay = match Relay::start((route.target)(ctx), profile, SEED) {
        Ok(r) => r,
        Err(e) => {
            o.check(Check::fail(format!("{tag}: relay"), e));
            return None;
        }
    };
    let Some(station) = (route.get_on)(ctx, o, tag, &relay, &usrp) else {
        let report = relay.stop();
        o.note(format!("{tag}: relay {}", relay_summary(&report)));
        return None;
    };

    if sstv_name(mode) == astar_sstv::Mode::Pd120
        && let Err(e) = station.station.set_tx_timeout(PD120_TX_TIMEOUT)
    {
        o.check(Check::fail(
            format!("{tag}: transmit time-out"),
            e.to_string(),
        ));
    }
    let (w, h) = mode.resolution();
    let original = test_picture(w, h);
    let recorder = match Recorder::start(&usrp) {
        Ok(r) => r,
        Err(e) => {
            o.check(Check::fail(format!("{tag}: USRP recorder"), e));
            station.hang_up();
            let _ = relay.stop();
            return None;
        }
    };
    let mark = crate::pcap::pod_now(&ctx.rig).ok();
    let started = Instant::now();
    let sent = station.send_and_wait(&original, mode);
    let quiet = recorder.wait_quiet(QUIET, Duration::from_secs(20));
    let frames = recorder.stop();
    station.hang_up();
    let report = relay.stop();
    let (late_max, extra) = (route.arrivals)(ctx, o, tag, mark, &original, mode);

    sent_check(o, tag, mode, &sent);
    if let Err(e) = quiet {
        o.note(format!("{tag}: {e}"));
    }
    let mut rec = UsrpRecording::of(&frames);
    rec.late_max_ms = late_max;
    o.note(format!(
        "{tag}: recorded {} frames from the node in {:.1} s, peak {} ({:.1} dBFS); relay {}",
        rec.frames,
        started.elapsed().as_secs_f64(),
        rec.peak,
        rec.peak_dbfs(),
        relay_summary(&report)
    ));
    let judged = judge(o, tag, &original, &rec, mode, &report, &ctx.dir);
    Some((judged, rec, report, extra))
}

/// The check that the picture went out in full.
fn sent_check(o: &mut Outcome, tag: &str, mode: SstvTxMode, sent: &Result<Duration, String>) {
    let name = format!("{tag}: the picture is sent");
    match sent {
        Ok(took) => o.check(
            Check::pass(
                name,
                format!(
                    "ended Sent after {:.1} s ({:.1} s expected)",
                    took.as_secs_f64(),
                    Station::picture_duration(mode).as_secs_f64()
                ),
            )
            .measured(took.as_secs_f64(), "s"),
        ),
        Err(e) => o.check(Check::fail(name, e.clone())),
    };
}

/// Dial the node through `relay` and wait until it lists the link and has
/// finished announcing it on the air. `None`, with a failed check, when the
/// call is not answered or the link never shows.
fn on_the_node(
    ctx: &Ctx,
    o: &mut Outcome,
    tag: &str,
    relay: &Relay,
    usrp: &UsrpProbe,
) -> Option<HeadlessStation> {
    let station =
        match HeadlessStation::dial(relay.local_addr(), &ctx.cfg().node_number, STATION_NUMBER) {
            Ok(s) => s,
            Err(e) => {
                o.check(Check::fail(
                    format!("{tag}: the station's call is answered"),
                    e,
                ));
                return None;
            }
        };
    let linked = wait_until(Duration::from_secs(5), common::POLL, || {
        match ctx.node.link_to(STATION_NUMBER) {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err("the node lists no link to the station".into()),
            Err(e) => Err(format!("node: {e}")),
        }
    });
    o.check(Check::expect(
        format!("{tag}: the node lists the station's link"),
        linked.is_ok(),
        match &linked {
            Ok(t) => format!(
                "{STATION_NUMBER} in rpt lstats after {:.1} s",
                t.as_secs_f64()
            ),
            Err(e) => e.last.clone(),
        },
    ));
    if linked.is_err() {
        station.hang_up();
        return None;
    }
    // The node announces a new link on the air; let that finish first.
    if let Err(e) = common::wait_node_quiet(usrp, Duration::from_secs(20)) {
        o.note(format!("{tag}: {e}"));
    }
    Some(station)
}

/// What reached the node's IAX2 port, from the pod's capture: how late
/// each voice frame arrived against its own timestamp (the jitter the
/// node's jitter buffer had to absorb), the timestamps never seen (frames
/// the relay dropped), and whether the audio itself arrived intact.
fn node_arrival(
    ctx: &Ctx,
    o: &mut Outcome,
    tag: &str,
    mark: Option<f64>,
    original: &SstvPicture,
    mode: SstvTxMode,
) -> Arrivals {
    let name = format!("{tag}: what reached the node");
    let late = arrival_at(
        ctx,
        o,
        &name,
        mark,
        (original, mode),
        ctx.cfg().node_iax.port(),
        None,
    );
    (late, Vec::new())
}

/// Voice frames as they reached the node.
#[derive(Debug, Clone, Default)]
struct Arrival {
    frames: usize,
    /// 20 ms steps of the timestamp with no frame.
    missing: u32,
    /// How late each frame arrived against the first, beyond what its
    /// timestamp says, less the earliest: ms.
    late_p50: f64,
    late_p99: f64,
    late_max: f64,
    /// The audio in timestamp order, missing frames filled with silence.
    audio: Vec<i16>,
}

impl Arrival {
    fn of(wire: &[crate::pcap::WireVoice]) -> Self {
        let mut a = Self::default();
        let Some(first) = wire.first() else {
            return a;
        };
        // Unwrap the 16-bit timestamps.
        let mut ts = i64::from(first.ts16);
        let mut prev = first.ts16;
        let mut late = Vec::with_capacity(wire.len());
        let mut last_ts: Option<i64> = None;
        for w in wire {
            ts += i64::from(w.ts16.wrapping_sub(prev).cast_signed());
            prev = w.ts16;
            #[allow(clippy::cast_precision_loss)]
            let expected = (ts - i64::from(first.ts16)) as f64;
            late.push((w.t - first.t) * 1000.0 - expected);
            if let Some(l) = last_ts {
                let steps = (ts - l + 10) / 20;
                if (2..=25).contains(&steps) {
                    let gap = u32::try_from(steps - 1).unwrap_or(0);
                    a.missing += gap;
                    a.audio
                        .extend(std::iter::repeat_n(0, VOICE_SAMPLES * gap as usize));
                }
            }
            last_ts = Some(ts);
            a.audio.extend_from_slice(&w.samples);
            a.frames += 1;
        }
        let min = late.iter().copied().fold(f64::INFINITY, f64::min);
        let mut late: Vec<f64> = late.into_iter().map(|l| l - min).collect();
        late.sort_by(f64::total_cmp);
        let pick = |q: f64| {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
            let i = ((late.len() - 1) as f64 * q).round() as usize;
            late[i]
        };
        a.late_p50 = pick(0.5);
        a.late_p99 = pick(0.99);
        a.late_max = pick(1.0);
        a
    }
}

/// The relay's report, in one line.
fn relay_summary(r: &RelayReport) -> String {
    format!(
        "{} seed {}: forwarded {}, dropped {}, delay p50 {:.1} ms, p99 {:.1} ms, max {:.1} ms",
        r.profile.id(),
        r.seed,
        r.forwarded,
        r.dropped,
        r.delay_ms_p50,
        r.delay_ms_p99,
        r.delay_ms_max
    )
}

/// `astar-sstv`'s name for one of the sender's modes.
fn sstv_name(mode: SstvTxMode) -> astar_sstv::Mode {
    // The VIS code is the mode's own name for itself on the air.
    let vis = mode.vis_code();
    if vis == sstv_tx_modes::ROBOT_36.vis_code() {
        astar_sstv::Mode::Robot36
    } else if vis == sstv_tx_modes::SCOTTIE_2.vis_code() {
        astar_sstv::Mode::Scottie2
    } else if vis == sstv_tx_modes::PD_120.vis_code() {
        astar_sstv::Mode::Pd120
    } else {
        astar_sstv::Mode::Other
    }
}

// ---- the station ------------------------------------------------------------

/// A station with no sound device on a call to a node. Its microphone is
/// [`astar_audio::NullBackend`]'s, which never produces a sample, so the only
/// audio the call carries is a picture's.
pub(super) struct HeadlessStation {
    pub(super) station: Station,
}

impl HeadlessStation {
    /// Dial `addr` (127.0.0.1 only) as `caller_number`, asking for node
    /// `dest`, and wait for the answer.
    ///
    /// The call has the Web Transceiver shape the app uses on a real node:
    /// username `allstar-public`, `CALLING_NUMBER` = `dest`, `CALLING_NAME`
    /// = `caller_number`. The secret is the rig's test-only one.
    pub(super) fn dial(addr: SocketAddr, dest: &str, caller_number: &str) -> Result<Self, String> {
        if !addr.ip().is_loopback() {
            return Err(format!("refusing to dial {addr}: not loopback"));
        }
        let session = Arc::new(Mutex::new(ConsoleSession::new()));
        let config = StationConfig::default();
        let codec_policy = config.codec_policy;
        let station = Station::with_shared_session(
            config,
            Arc::clone(&session),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        // The bench sends pictures on purpose, to its own loopback node; a
        // new station refuses until it is allowed.
        station.set_picture_sending_allowed(true);
        session
            .lock()
            .map_err(|_| "session poisoned".to_string())?
            .connect(
                Box::new(astar_audio::NullBackend::new()),
                addr,
                ConsoleConfig {
                    node: dest.to_string(),
                    calling_node: dest.to_string(),
                    secret: TEST_SECRET.to_string(),
                    name: caller_number.to_string(),
                    input_device: None,
                    output_device: None,
                    codec_policy,
                },
            )
            .map_err(|e| format!("dial {addr}: {e}"))?;
        let me = Self { station };
        let answered = wait_until(ANSWER_TIMEOUT, Duration::from_millis(20), || {
            match me.station.snapshot().status {
                CallStatus::Answered => Ok(()),
                s => Err(format!("{s:?}")),
            }
        });
        match answered {
            Ok(_) => Ok(me),
            Err(e) => {
                me.station.disconnect();
                Err(format!("not answered in {ANSWER_TIMEOUT:?}: {}", e.last))
            }
        }
    }

    /// Send `picture` and wait until it ends. `Ok` with how long it took
    /// when it ended `Sent`, else what ended it.
    pub(super) fn send_and_wait(
        &self,
        picture: &SstvPicture,
        mode: SstvTxMode,
    ) -> Result<Duration, String> {
        let t0 = Instant::now();
        let send: PictureSend = self
            .station
            .send_picture(picture, mode)
            .map_err(|e| format!("send_picture refused: {e}"))?;
        let limit = Station::picture_duration(mode) + Duration::from_secs(15);
        let ended = loop {
            if let Some(end) = send.ended() {
                break end;
            }
            if t0.elapsed() > limit {
                send.cancel();
                return Err(format!(
                    "still sending after {:.1} s ({:?} of {:?} sent)",
                    t0.elapsed().as_secs_f64(),
                    send.progress().0,
                    send.progress().1
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        match ended {
            SendEnd::Sent => Ok(t0.elapsed()),
            other => Err(format!(
                "ended {other:?} after {:.1} s",
                t0.elapsed().as_secs_f64()
            )),
        }
    }

    /// Hang up.
    pub(super) fn hang_up(self) {
        self.station.disconnect();
    }
}

// ---- recording the node's radio --------------------------------------------

/// Records every USRP packet the node sends, on a background thread, until
/// stopped.
struct Recorder {
    frames: Arc<Mutex<Vec<RxFrame>>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Recorder {
    fn start(usrp: &UsrpProbe) -> Result<Self, String> {
        let me = usrp.try_clone().map_err(|e| format!("USRP: {e}"))?;
        let frames = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let join = {
            let (frames, stop) = (Arc::clone(&frames), Arc::clone(&stop));
            std::thread::Builder::new()
                .name("sstv-usrp-rx".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match me.record_any(Duration::from_millis(200)) {
                            Ok(got) => frames.lock().expect("frames").extend(got),
                            Err(_) => std::thread::sleep(Duration::from_millis(20)),
                        }
                    }
                })
                .map_err(|e| format!("recorder thread: {e}"))?
        };
        Ok(Self {
            frames,
            stop,
            join: Some(join),
        })
    }

    /// Wait until the node has sent voice and then gone quiet for `quiet`.
    fn wait_quiet(&self, quiet: Duration, limit: Duration) -> Result<Duration, String> {
        let t0 = Instant::now();
        loop {
            let last = self
                .frames
                .lock()
                .expect("frames")
                .iter()
                .rev()
                .find(|f| f.keyed && !f.samples.is_empty())
                .map(|f| f.at);
            if let Some(at) = last
                && at.elapsed() >= quiet
            {
                return Ok(t0.elapsed());
            }
            if t0.elapsed() > limit {
                return Err(format!(
                    "the node did not unkey within {limit:?} of the picture's end"
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn stop(mut self) -> Vec<RxFrame> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        std::mem::take(&mut *self.frames.lock().expect("frames"))
    }
}

/// What the node put on the air, as one stretch of audio.
#[derive(Debug, Clone, Default)]
pub(super) struct UsrpRecording {
    /// 8 kHz audio of every keyed voice frame, lost frames filled with
    /// silence.
    pub audio: Vec<i16>,
    /// Keyed voice frames received.
    pub frames: usize,
    /// Frames the node's sequence numbers say were sent but never arrived.
    pub lost: u32,
    /// The largest sample, as a magnitude.
    pub peak: i32,
    /// How late the latest voice frame reached the node, ms, from the pod's
    /// capture (`arrival_at`), when it could be read.
    pub late_max_ms: Option<f64>,
}

impl UsrpRecording {
    pub(super) fn of(frames: &[RxFrame]) -> Self {
        let mut r = Self::default();
        let mut last: Option<u32> = None;
        for f in frames
            .iter()
            .filter(|f| f.keyed && f.samples.len() == VOICE_SAMPLES)
        {
            if let (Some(prev), Some(seq)) = (last, f.seq) {
                let skipped = seq.wrapping_sub(prev).wrapping_sub(1);
                if skipped > 0 && skipped <= MAX_FILL_FRAMES {
                    r.audio
                        .extend(std::iter::repeat_n(0, VOICE_SAMPLES * skipped as usize));
                    r.lost += skipped;
                }
            }
            last = f.seq.or(last);
            r.audio.extend_from_slice(&f.samples);
            r.frames += 1;
        }
        r.peak = r
            .audio
            .iter()
            .map(|s| i32::from(*s).abs())
            .max()
            .unwrap_or(0);
        r
    }

    /// The peak in dBFS (0 is full scale).
    pub(super) fn peak_dbfs(&self) -> f64 {
        if self.peak == 0 {
            return f64::NEG_INFINITY;
        }
        20.0 * (f64::from(self.peak) / 32767.0).log10()
    }
}

// ---- lining the node's audio up with what was sent -------------------------

/// Where the node's radio audio departs from what the station sent.
///
/// The sequence numbers of the node's USRP frames only show frames lost
/// between the node and the bench. The node's own jitter buffer can also
/// drop audio (the picture after it arrives early), make some up as
/// silence, or hold some back; none of that shows in the sequence numbers.
/// This lines the recording up against the station's exact audio, 20 ms at
/// a time, and measures each kind.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Timeline {
    /// Sent audio missing from the recording, ms.
    pub dropped_ms: f64,
    /// Audio in the recording that the station never sent, ms.
    pub inserted_ms: f64,
    /// 20 ms frames inside the picture that came out silent.
    pub silent: u32,
    /// Where the first silent frames fall, seconds into the sent audio.
    pub silent_at: Vec<f64>,
    /// 20 ms frames that matched no part of the sent audio nearby.
    pub unmatched: u32,
    /// Each slip: seconds into the sent audio, and ms (dropped > 0).
    pub slips: Vec<(f64, f64)>,
    /// The recording lined up against the sent audio half a sample late
    /// ([`line_up`]): what an 8 → 16 → 8 kHz round trip does to it.
    pub half_sample_late: bool,
}

impl Timeline {
    fn summary(&self) -> String {
        let list = |v: &[String]| {
            let mut s = v.iter().take(12).cloned().collect::<Vec<_>>().join(", ");
            if v.len() > 12 {
                let _ = write!(s, ", and {} more", v.len() - 12);
            }
            s
        };
        let slips: Vec<String> = self
            .slips
            .iter()
            .map(|(t, ms)| format!("{ms:+.1} ms at {t:.2} s"))
            .collect();
        let silent: Vec<String> = self.silent_at.iter().map(|t| format!("{t:.2}")).collect();
        format!(
            "{:.1} ms dropped, {:.1} ms inserted, {} frames silent{}, {} unmatched{}{}",
            self.dropped_ms,
            self.inserted_ms,
            self.silent,
            if silent.is_empty() {
                String::new()
            } else {
                format!(" (at {} s)", list(&silent))
            },
            self.unmatched,
            if slips.is_empty() {
                String::new()
            } else {
                format!(" (slips: {})", list(&slips))
            },
            if self.half_sample_late {
                "; half a sample late"
            } else {
                ""
            }
        )
    }
}

/// Normalised correlation of two equal-length blocks, −1 to 1.
fn ncorr(a: &[i16], b: &[i16]) -> f64 {
    let (mut ab, mut aa, mut bb) = (0.0, 0.0, 0.0);
    for (&x, &y) in a.iter().zip(b) {
        let (x, y) = (f64::from(x), f64::from(y));
        ab += x * y;
        aa += x * x;
        bb += y * y;
    }
    if aa == 0.0 || bb == 0.0 {
        0.0
    } else {
        ab / (aa * bb).sqrt()
    }
}

/// Samples per step of a tone track (10 ms).
const TRACK: usize = 80;
/// A sample this loud or quieter is silence.
const QUIET_LEVEL: u16 = 500;

/// Zero crossings per 10 ms, a rough tone frequency; −1 where quiet.
fn tone_track(x: &[i16]) -> Vec<f64> {
    x.as_chunks::<TRACK>()
        .0
        .iter()
        .map(|c| {
            if c.iter().all(|s| s.unsigned_abs() <= QUIET_LEVEL) {
                -1.0
            } else {
                let n = c.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
                #[allow(clippy::cast_precision_loss)]
                let n = n as f64;
                n
            }
        })
        .collect()
}

/// The step in `rec` (a tone track) within `lags` where `tpl` fits best:
/// the smallest summed difference, ties to the lag nearest `prefer`.
fn best_lag(
    tpl: &[f64],
    rec: &[f64],
    lags: std::ops::RangeInclusive<i64>,
    prefer: i64,
) -> Option<i64> {
    lags.filter_map(|lag| {
        let at = usize::try_from(lag).ok()?;
        let seg = rec.get(at..at + tpl.len())?;
        let d: f64 = tpl.iter().zip(seg).map(|(a, b)| (a - b).abs()).sum();
        #[allow(clippy::cast_precision_loss)]
        let score = d + 0.01 * (lag - prefer).abs() as f64;
        Some((score, lag))
    })
    .min_by(|a, b| a.0.total_cmp(&b.0))
    .map(|(_, lag)| lag)
}

/// A sample position at 8 kHz, in seconds.
fn secs_at(sample: usize) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let s = sample as f64;
    s / f64::from(SAMPLE_RATE)
}

/// Samples at 8 kHz, in ms.
fn ms_of(samples: i64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let s = samples as f64;
    s * 1000.0 / f64::from(SAMPLE_RATE)
}

/// Line `rec` (8 kHz, as recorded) up against `sent` (8 kHz, the station's
/// audio). `None` when either has no signal.
///
/// The walk keeps one offset (recorded position = sent position + shift)
/// and checks each 20 ms frame of the recording against the sent audio at
/// that offset. When a frame stops matching, it finds the new offset in
/// two steps: roughly, by sliding 400 ms of the tone track (10 ms
/// resolution) within ±60 ms, or within ±1 s once lost for a while; then to
/// the sample, by correlating the audio itself within ±10 ms of that.
pub(super) fn timeline(sent: &[i16], rec: &[i16]) -> Option<Timeline> {
    const B: usize = VOICE_SAMPLES;
    // A frame of clean μ-law audio at the right offset correlates at 0.99
    // and more.
    const GOOD: f64 = 0.9;
    // The new offset must hold over the next 400 ms, at this.
    const WINDOW: usize = 20;
    const WINDOW_GOOD: f64 = 0.98;
    // Tone-track steps (10 ms) searched near the old offset, and once lost
    // for LOST_BEFORE_WIDE frames, far from it.
    const NEAR: i64 = 6;
    const FAR: i64 = 100;
    const LOST_BEFORE_WIDE: u32 = 5;
    let to_i = |v: usize| i64::try_from(v).unwrap_or(i64::MAX);
    let loud = |x: &i16| x.unsigned_abs() > QUIET_LEVEL;
    let s0 = sent.iter().position(loud)?;
    let s_end = sent.iter().rposition(loud)?;
    let (ts, tr) = (tone_track(sent), tone_track(rec));
    let at = |shift: i64, r: usize, len: usize| -> Option<&[i16]> {
        let p = usize::try_from(to_i(r) - shift).ok()?;
        sent.get(p..p + len)
    };
    // The sample-exact offset near `rough`, over `len` samples from `r`.
    let fine = |rough: i64, r: usize, len: usize| -> Option<(f64, i64)> {
        let win = rec.get(r..r + len)?;
        (-80..=80)
            .filter_map(|d| Some((ncorr(win, at(rough + d, r, len)?), rough + d)))
            .max_by(|a, b| a.0.total_cmp(&b.0))
    };

    // Find the picture in the recording, which may hold other audio first
    // (the node's identifier, its courtesy tone): slide the first three
    // seconds of the sent tone track along the whole recorded one.
    let from = s0 / TRACK;
    let tpl = ts.get(from..(from + 300).min(ts.len()))?;
    let lag = best_lag(tpl, &tr, 0..=to_i(tr.len()), 0)?;
    let r0 = usize::try_from(lag).ok()? * TRACK;
    let rough = to_i(r0) - to_i(from * TRACK);
    let mut shift = fine(rough, r0, 1600.min(rec.len().saturating_sub(r0)))?.1;

    let mut tl = Timeline::default();
    // Walk the recording on its own 20 ms frame grid.
    let mut pos = r0 - r0 % B;
    let mut lost = 0_u32;
    while pos + B <= rec.len() {
        let blk = &rec[pos..pos + B];
        let Some(sent_pos) = usize::try_from(to_i(pos) - shift).ok() else {
            pos += B;
            continue;
        };
        if sent_pos + B > s_end {
            break;
        }
        if !blk.iter().any(loud) {
            tl.silent += 1;
            if tl.silent_at.len() < 50 {
                tl.silent_at.push(secs_at(sent_pos));
            }
            pos += B;
            continue;
        }
        if at(shift, pos, B).is_some_and(|s| ncorr(blk, s) >= GOOD) {
            lost = 0;
            pos += B;
            continue;
        }
        let wide = lost >= LOST_BEFORE_WIDE;
        if wide && !lost.is_multiple_of(LOST_BEFORE_WIDE) {
            // Far searches are dear: one every LOST_BEFORE_WIDE frames.
            tl.unmatched += 1;
            lost += 1;
            pos += B;
            continue;
        }
        let len = (WINDOW * B).min(rec.len() - pos);
        let reach = if wide { FAR } else { NEAR };
        // In tone-track steps: where the recording's window sits in the
        // sent track.
        let step = to_i(pos / TRACK);
        let here = step - shift.div_euclid(to_i(TRACK));
        let found = tr
            .get(pos / TRACK..(pos + len) / TRACK)
            .and_then(|win| best_lag(win, &ts, (here - reach).max(0)..=here + reach, here))
            // A fit at the edge of the search is no fit: look wider.
            .filter(|lag| (lag - here).abs() < reach)
            .and_then(|sent_step| fine(to_i(pos) - sent_step * to_i(TRACK), pos, len));
        match found {
            Some((corr, new_shift)) if corr >= WINDOW_GOOD => {
                let moved = shift - new_shift;
                if moved.abs() >= 8 {
                    tl.slips.push((secs_at(sent_pos), ms_of(moved)));
                    if moved > 0 {
                        tl.dropped_ms += ms_of(moved);
                    } else {
                        tl.inserted_ms -= ms_of(moved);
                    }
                }
                shift = new_shift;
                if at(shift, pos, B).is_some_and(|x| ncorr(blk, x) >= GOOD) {
                    lost = 0;
                } else {
                    tl.unmatched += 1;
                    lost += 1;
                }
            }
            _ => {
                tl.unmatched += 1;
                lost += 1;
            }
        }
        pos += B;
    }
    Some(tl)
}

/// [`timeline`], against the sent audio as it is and half a sample late,
/// keeping the reading with fewer unmatched frames (ties: as it is).
///
/// A picture that crosses a slin16 hop (astar-server's link to the node)
/// is resampled 8 → 16 → 8 kHz and comes back half a sample late with a
/// gentle roll-off. Each 20 ms frame then correlates at about 0.8 against
/// the sent audio, under the line-up's 0.9, and the walk loses track
/// though nothing was lost. Two-sample averaging of the sent audio
/// reproduces that delay and roll-off, and it correlates at 0.997 again
/// (RIG.md, "SSTV through astar-server's bridge").
pub(super) fn line_up(sent: &[i16], rec: &[i16]) -> Option<Timeline> {
    let plain = timeline(sent, rec);
    let late: Vec<i16> = std::iter::once(sent.first().copied().unwrap_or(0))
        .chain(sent.windows(2).map(|w| i16::midpoint(w[0], w[1])))
        .collect();
    let half = timeline(&late, rec).map(|t| Timeline {
        half_sample_late: true,
        ..t
    });
    match (plain, half) {
        (Some(p), Some(h)) if h.unmatched < p.unmatched => Some(h),
        (Some(p), _) => Some(p),
        (None, h) => h,
    }
}

// ---- judging ----------------------------------------------------------------

/// What [`judge`] decoded, kept for the artifacts of a failed cell.
pub(super) struct Judged {
    wav: Option<std::path::PathBuf>,
    slowrx: Option<astar_sstv::Picture>,
    outside: Option<OutsideDecode>,
    fork: Option<::sstv::DecodedImage>,
}

impl Judged {
    /// Keep the WAV, the decoded pictures and the relay's report in the
    /// scenario's directory.
    fn save(self, ctx: &Ctx, o: &mut Outcome, tag: &str, rec: &UsrpRecording, r: &RelayReport) {
        let stem = tag.replace(' ', "-");
        if let Some(wav) = self.wav {
            o.artifacts.push(wav);
        }
        if let Some(p) = &self.slowrx {
            let path = ctx.dir.join(format!("{stem}.slowrx.png"));
            if p.write_png(&path).is_ok() {
                o.artifacts.push(path);
            }
        }
        if let Some(p) = &self.outside {
            let path = ctx.dir.join(format!("{stem}.outside.png"));
            if write_rgb_png(&path, p.width, p.height, &p.rgb).is_ok() {
                o.artifacts.push(path);
            }
        }
        if let Some(p) = &self.fork {
            let path = ctx.dir.join(format!("{stem}.fork.png"));
            let (w, h) = p.mode().resolution();
            if write_rgb_png(&path, w, h, &rgb_of(p)).is_ok() {
                o.artifacts.push(path);
            }
        }
        let path = ctx.dir.join(format!("{stem}.relay.txt"));
        let text = format!(
            "{r:#?}\nUSRP: {} frames received, {} lost, peak {}\n",
            rec.frames, rec.lost, rec.peak
        );
        if std::fs::write(&path, text).is_ok() {
            o.artifacts.push(path);
        }
    }

    fn discard(self) {
        if let Some(wav) = self.wav {
            let _ = std::fs::remove_file(wav);
        }
    }
}

fn write_rgb_png(path: &Path, w: u32, h: u32, rgb: &[u8]) -> std::io::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(std::io::Error::other)?;
    writer.write_image_data(rgb).map_err(std::io::Error::other)
}

/// Decode `rec` with `astar-sstv` (the app's decoder, re-timing on).
fn slowrx_decode(rec: &UsrpRecording) -> Result<astar_sstv::Picture, String> {
    let mut decoder = astar_sstv::Decoder::new(SAMPLE_RATE).map_err(|e| format!("decoder: {e}"))?;
    let mut finished = None;
    let mut abandoned = None;
    let tail = vec![0_i16; SAMPLE_RATE as usize];
    let mut events = Vec::new();
    for chunk in rec
        .audio
        .chunks(VOICE_SAMPLES)
        .chain(tail.chunks(VOICE_SAMPLES))
    {
        events.extend(decoder.push(chunk));
    }
    events.extend(decoder.finish());
    for e in events {
        match e {
            astar_sstv::Event::Finished(p) if finished.is_none() => finished = Some(p),
            astar_sstv::Event::Abandoned(p) if abandoned.is_none() => abandoned = Some(p),
            _ => {}
        }
    }
    finished
        .or(abandoned)
        .ok_or_else(|| "no picture started".to_string())
}

/// The `sstv` fork's decoder with its de-jitter in front, on `rec` and a
/// second of silence after it.
fn fork_decode(rec: &UsrpRecording) -> Option<::sstv::DecodedImage> {
    let tail = std::iter::repeat_n(0_i16, SAMPLE_RATE as usize);
    ::sstv::Decoder::new(rec.audio.iter().copied().chain(tail), SAMPLE_RATE).decode_dejittered()
}

/// A decoded image as RGB bytes, row by row.
fn rgb_of(image: &::sstv::DecodedImage) -> Vec<u8> {
    image
        .pixels()
        .iter()
        .flat_map(|p| [p.red(), p.green(), p.blue()])
        .collect()
}

/// The `sstv` crate's mode for one astar names, `None` for the modes that
/// crate does not have.
fn fork_mode(mode: astar_sstv::Mode) -> Option<::sstv::Mode> {
    use ::sstv::modes;
    use astar_sstv::Mode as M;
    match mode {
        M::Robot36 => Some(modes::ROBOT_36),
        M::Robot72 => Some(modes::ROBOT_72),
        M::Scottie1 => Some(modes::SCOTTIE_1),
        M::Scottie2 => Some(modes::SCOTTIE_2),
        M::ScottieDx => Some(modes::SCOTTIE_DX),
        M::Martin1 => Some(modes::MARTIN_1),
        M::Martin2 => Some(modes::MARTIN_2),
        M::Pd120 => Some(modes::PD_120),
        M::Pd180 => Some(modes::PD_180),
        M::Pd240 => Some(modes::PD_240),
        M::Wraase180 => Some(modes::WRASSE_SC2_180),
        M::Robot24 | M::Robot24Bw | M::Robot12Bw | M::Robot8Bw | M::Wraase120 | M::Other => None,
    }
}

/// Decode the node's recording three ways and add one check per decoder.
///
/// - **slowrx** (`astar-sstv`, as the app decodes): must read the mode and
///   come back under [`SLOWRX_LIMIT`] on the plain mean error, on every
///   profile.
/// - **outside** (no astar code): must read the mode and come back under
///   [`OUTSIDE_LIMIT`] on the fitted error. Its plain error is in the detail.
///   On `clean`, and on any profile whose recording is whole, that is a pass
///   condition. On `net-1118` and `harsh` with audio lost it has no repair,
///   so its figure is recorded and the check reported as not required.
///   "Lost" counts both USRP frames that never reached the bench and audio
///   the node itself dropped or made up ([`timeline`]): with no USRP frame
///   lost, the node's jitter buffer can still cut 20 ms out of the picture.
///   Where it reads fewer rows than were sent (PD120: sometimes 494 of 496),
///   both pictures are compared over the rows it read, from the top.
/// - **fork** (the `sstv` fork's own decoder, de-jittered): must read the
///   mode and come back under [`FORK_LIMIT`] on the plain mean error, on
///   every profile.
///
/// Also adds a check carrying the relay's report, one carrying the USRP
/// frames lost, and one with where the node's audio departs from the
/// station's.
pub(super) fn judge(
    o: &mut Outcome,
    tag: &str,
    original: &SstvPicture,
    rec: &UsrpRecording,
    mode: SstvTxMode,
    relay: &RelayReport,
    dir: &Path,
) -> Judged {
    let want = sstv_name(mode);
    o.check(
        Check::pass(format!("{tag}: relay"), relay_summary(relay))
            .measured(relay.delay_ms_max, "ms max delay"),
    );
    o.check(
        Check::pass(
            format!("{tag}: USRP frames lost"),
            format!(
                "{} keyed frames received, {} lost (filled with silence), peak {} ({:.1} dBFS)",
                rec.frames,
                rec.lost,
                rec.peak,
                rec.peak_dbfs()
            ),
        )
        .measured(f64::from(rec.lost), "frames"),
    );
    let line_up = astar_station::encode_picture(original, mode, SAMPLE_RATE)
        .ok()
        .and_then(|sent| line_up(&sent, &rec.audio));
    let name = format!("{tag}: the node's audio against the sent audio");
    // What, if anything, says the recording is not whole.
    let mut lost = Vec::new();
    if rec.lost > 0 {
        lost.push(format!("USRP lost {} frames", rec.lost));
    }
    if let Some(tl) = &line_up {
        o.check(
            Check::pass(name, tl.summary()).measured(tl.dropped_ms + tl.inserted_ms, "ms slipped"),
        );
        if tl.dropped_ms > 0.0 {
            lost.push(format!("the node dropped {:.0} ms", tl.dropped_ms));
        }
        if tl.inserted_ms > 0.0 {
            lost.push(format!("the node made up {:.0} ms", tl.inserted_ms));
        }
        if tl.silent > 0 {
            lost.push(format!("{} frames silent", tl.silent));
        }
        if tl.unmatched > 0 {
            lost.push(format!("{} frames unmatched", tl.unmatched));
        }
    } else {
        o.check(Check::pass(name, "not lined up: no signal"));
        lost.push("not lined up".to_string());
    }

    let mut judged = Judged {
        wav: None,
        slowrx: None,
        outside: None,
        fork: None,
    };
    if rec.audio.is_empty() {
        for decoder in ["slowrx", "outside", "fork"] {
            o.check(Check::fail(
                format!("{tag}: {decoder}"),
                "the node put nothing on the air",
            ));
        }
        return judged;
    }
    // A recording that is not whole is judged on the same limits; its
    // details say what the line-up measured, so a red `clean` cell reads as
    // the node slipping, not as the sender.
    let slipped = (!lost.is_empty()).then(|| {
        let slips = line_up
            .as_ref()
            .map_or_else(String::new, |tl| format!(" [{}]", tl.summary()));
        let late = rec.late_max_ms.map_or_else(String::new, |ms| {
            format!("; frames reached the node up to {ms:.0} ms late")
        });
        format!(
            "the node did not carry the audio whole: {}{slips}{late}",
            lost.join(", ")
        )
    });
    judged.slowrx = judge_slowrx(o, tag, original, rec, want, slipped.as_deref());
    judged.fork = judge_fork(o, tag, original, rec, want, slipped.as_deref());

    let wav = dir.join(format!("{}.wav", tag.replace(' ', "-")));
    let written = std::fs::create_dir_all(dir)
        .and_then(|()| outside::write_wav(&wav, &rec.audio, SAMPLE_RATE));
    if let Err(e) = written {
        o.check(Check::fail(format!("{tag}: outside"), format!("WAV: {e}")));
        return judged;
    }
    let not_required = (relay.profile != Profile::Clean && !lost.is_empty())
        .then(|| format!("{}: {}", relay.profile.id(), lost.join(", ")));
    judged.outside = judge_outside(
        o,
        tag,
        original,
        &wav,
        want,
        not_required.as_deref(),
        slipped.as_deref(),
    );
    judged.wav = Some(wav);
    judged
}

/// The slowrx check: `astar-sstv`, as the app decodes.
fn judge_slowrx(
    o: &mut Outcome,
    tag: &str,
    original: &SstvPicture,
    rec: &UsrpRecording,
    want: astar_sstv::Mode,
    slipped: Option<&str>,
) -> Option<astar_sstv::Picture> {
    let note = slipped.map_or_else(String::new, |s| format!("; {s}"));
    let limit = SLOWRX_LIMIT.of(want);
    match slowrx_decode(rec) {
        Ok(p) => {
            let same = (p.width, p.height) == (original.width, original.height);
            let err = if same {
                mean_error(&original.rgb, &p.rgb)
            } else {
                f64::INFINITY
            };
            o.check(
                Check::expect(
                    format!("{tag}: slowrx"),
                    p.mode == want && same && !p.partial && err < limit,
                    format!(
                        "{} {}x{}, {} rows{}, {} repaired, quality {:.2}: mean error {err:.2} (limit {limit:.1}){note}",
                        p.mode.name(),
                        p.width,
                        p.height,
                        p.lines,
                        if p.partial { " (abandoned)" } else { "" },
                        p.repaired_rows,
                        p.quality
                    ),
                )
                .measured(err, "mean abs"),
            );
            Some(p)
        }
        Err(e) => {
            o.check(Check::fail(format!("{tag}: slowrx"), format!("{e}{note}")));
            None
        }
    }
}

/// The fork check: the `sstv` fork's own decoder with its de-jitter in
/// front, judged on the plain error.
fn judge_fork(
    o: &mut Outcome,
    tag: &str,
    original: &SstvPicture,
    rec: &UsrpRecording,
    want: astar_sstv::Mode,
    slipped: Option<&str>,
) -> Option<::sstv::DecodedImage> {
    let note = slipped.map_or_else(String::new, |s| format!("; {s}"));
    let limit = FORK_LIMIT.of(want);
    let Some(image) = fork_decode(rec) else {
        o.check(Check::fail(
            format!("{tag}: fork"),
            format!("no picture found{note}"),
        ));
        return None;
    };
    let (w, h) = image.mode().resolution();
    let same = (w, h) == (original.width, original.height);
    let err = if same {
        mean_error(&original.rgb, &rgb_of(&image))
    } else {
        f64::INFINITY
    };
    let right_mode = fork_mode(want) == Some(image.mode());
    let read = if right_mode {
        want.name()
    } else {
        "another mode"
    };
    o.check(
        Check::expect(
            format!("{tag}: fork"),
            right_mode && same && image.complete() && err < limit,
            format!(
                "{read} {w}x{h}{}, de-jittered: mean error {err:.2} (limit {limit:.1}){note}",
                if image.complete() {
                    ""
                } else {
                    " (incomplete)"
                },
            ),
        )
        .measured(err, "mean abs"),
    );
    Some(image)
}

/// The outside check: no astar code, judged on the fitted error.
fn judge_outside(
    o: &mut Outcome,
    tag: &str,
    original: &SstvPicture,
    wav: &Path,
    want: astar_sstv::Mode,
    // `Some(why)` when the recording is not whole off `clean`: what was
    // measured, so the check is a skip.
    not_required: Option<&str>,
    slipped: Option<&str>,
) -> Option<OutsideDecode> {
    let note = slipped.map_or_else(String::new, |s| format!("; {s}"));
    let limit = OUTSIDE_LIMIT.of(want);
    let mut decoded = None;
    let (ok, detail, err) = match outside::outside_decode(wav) {
        Ok(d) => {
            let rows = d.height.min(original.height);
            let comparable = d.width == original.width && rows > 0;
            let (raw, fitted) = if comparable {
                let a = top_rows(&original.rgb, original.width, rows);
                let b = top_rows(&d.rgb, d.width, rows);
                (mean_error(a, b), fitted_error(a, b))
            } else {
                (f64::INFINITY, f64::INFINITY)
            };
            let rows_note = if rows < original.height {
                format!(", compared over rows 0–{} of {}", rows - 1, original.height)
            } else {
                String::new()
            };
            let ok = d.mode == want.name() && comparable && fitted < limit;
            let detail = format!(
                "{} {}x{}{rows_note}: fitted error {fitted:.2} (limit {limit:.1}), raw {raw:.2}",
                d.mode, d.width, d.height
            );
            decoded = Some(d);
            (ok, detail, fitted)
        }
        Err(e) => (false, e, f64::INFINITY),
    };
    let name = format!("{tag}: outside");
    let check = match not_required {
        None => Check::expect(name, ok, format!("{detail}{note}")),
        Some(why) => Check::skip(
            name,
            format!("not required ({why}; this decoder has no repair): {detail}"),
        ),
    };
    o.check(check.measured(err, "mean abs"));
    decoded
}

// ---- two hops: the station, astar-server's bridge, the node ----------------

/// `sstv_station_via_server`: the two-hop version of
/// [`SstvStationToNode`]. astar-server links to the node (transceive); the
/// station dials astar-server's bridge through the [`Relay`] the way the
/// IAX probe does (`common::iax_probe`): as 1997, asking for astar-server's
/// node number. The picture crosses the bridge and the link and is
/// recorded at the node's radio, then judged by [`judge`] on the same limits.
///
/// If a profile passes `sstv_station_to_node` and fails here, the bridge is
/// the suspect: a failed cell keeps the pod's frames
/// (`common::save_cell_frames`), and both hops' arrivals are measured, what
/// reached astar-server from the relay and what astar-server sent the node.
pub struct SstvStationViaServer;

impl Scenario for SstvStationViaServer {
    fn id(&self) -> &'static str {
        "sstv_station_via_server"
    }

    fn describe(&self) -> &'static str {
        "a headless station sends an SSTV picture through the jitter relay to astar-server's bridge, linked to the node; the node's radio audio is decoded by an outside decoder and by astar-sstv"
    }

    fn run(&self, ctx: &mut Ctx) -> Outcome {
        let mut o = Outcome::new();
        if let Err(e) = clean_start(ctx) {
            o.check(Check::fail("clean start", e));
            return o;
        }
        let mut modes = vec![sstv_tx_modes::ROBOT_36];
        if ctx.tuning.long {
            modes.extend([sstv_tx_modes::SCOTTIE_2, sstv_tx_modes::PD_120]);
        }
        o.note(format!(
            "relay seed {SEED}; modes {:?}; the station is {STATION_NUMBER}, dialling astar-server ({}) at {} through the relay; astar-server links to the node",
            modes,
            ctx.cfg().server_number,
            ctx.cfg().server_iax_host
        ));
        for profile in Profile::all() {
            for &mode in &modes {
                let tag = format!("{} {}", profile.id(), sstv_name(mode).name());
                send_cell(ctx, &mut o, &tag, (profile, mode), &VIA_SERVER);
                if let Err(e) = clean_start(ctx) {
                    o.check(Check::fail(format!("{tag}: clean afterwards"), e));
                    return o;
                }
            }
        }
        o
    }
}

/// `sstv_station_via_server`: through astar-server's bridge, which links to
/// the node first. A failed cell keeps the pod's frames: what the bridge and
/// the node said to each other while the picture crossed.
const VIA_SERVER: Route = Route {
    target: server_host,
    get_on: link_then_on_the_bridge,
    arrivals: both_hops,
    keep_frames: true,
};

fn server_host(ctx: &Ctx) -> SocketAddr {
    ctx.cfg().server_iax_host
}

/// Link astar-server to the node, then dial its bridge ([`on_the_bridge`]).
fn link_then_on_the_bridge(
    ctx: &Ctx,
    o: &mut Outcome,
    tag: &str,
    relay: &Relay,
    usrp: &UsrpProbe,
) -> Option<HeadlessStation> {
    if link_server_to_node(ctx, o, tag) {
        on_the_bridge(ctx, o, tag, relay, usrp)
    } else {
        None
    }
}

/// Link astar-server to the node (transceive) and wait until both sides
/// list it; `false`, with a failed check, if it does not come up.
fn link_server_to_node(ctx: &Ctx, o: &mut Outcome, tag: &str) -> bool {
    match common::server_dials_node(ctx, LinkAction::Connect, Some(RptLinkMode::Transceive)) {
        Ok((leg, t)) => o.check(Check::pass(
            format!("{tag}: astar-server links to the node"),
            format!("{leg} up after {t:.1?}"),
        )),
        Err(e) => o.check(Check::fail(
            format!("{tag}: astar-server links to the node"),
            e,
        )),
    }
}

/// Dial astar-server's bridge through `relay` as [`STATION_NUMBER`], asking
/// for astar-server's node number, and wait until astar-server lists the
/// call as active and the node's transmitter is quiet. `None`, with a failed
/// check, when it does not happen.
fn on_the_bridge(
    ctx: &Ctx,
    o: &mut Outcome,
    tag: &str,
    relay: &Relay,
    usrp: &UsrpProbe,
) -> Option<HeadlessStation> {
    let before = ctx
        .server
        .status()
        .map(|s| common::active_call_ids(&s))
        .unwrap_or_default();
    let station =
        match HeadlessStation::dial(relay.local_addr(), &ctx.cfg().server_number, STATION_NUMBER) {
            Ok(s) => s,
            Err(e) => {
                o.check(Check::fail(
                    format!("{tag}: the station's call is answered"),
                    e,
                ));
                return None;
            }
        };
    let on = wait_until(Duration::from_secs(5), common::POLL, || {
        let st = ctx.server.status().map_err(|e| format!("server: {e}"))?;
        let new: Vec<u64> = common::active_call_ids(&st)
            .into_iter()
            .filter(|id| !before.contains(id))
            .collect();
        match new.as_slice() {
            [_] => Ok(()),
            [] => Err("astar-server lists no new active call".to_string()),
            many => Err(format!("astar-server lists several new calls {many:?}")),
        }
    });
    o.check(Check::expect(
        format!("{tag}: astar-server lists the station's call"),
        on.is_ok(),
        match &on {
            Ok(t) => format!("a new active call after {:.1} s", t.as_secs_f64()),
            Err(e) => e.last.clone(),
        },
    ));
    if on.is_err() {
        station.hang_up();
        return None;
    }
    // The node may announce on the air; let it finish first.
    if let Err(e) = common::wait_node_quiet(usrp, Duration::from_secs(20)) {
        o.note(format!("{tag}: {e}"));
    }
    Some(station)
}

/// Measure both hops from the pod's capture: the relay's output as it
/// reached astar-server, and what astar-server's bridge sent the node. The
/// worst lateness at the node, ms, and each hop's audio as a WAV (8 kHz).
fn both_hops(
    ctx: &Ctx,
    o: &mut Outcome,
    tag: &str,
    since: Option<f64>,
    original: &SstvPicture,
    mode: SstvTxMode,
) -> Arrivals {
    let stem = tag.replace(' ', "-");
    let (server_port, node_port) = (ctx.cfg().server_iax.port(), ctx.cfg().node_iax.port());
    let mut wavs = Vec::new();
    let to_server = ctx.dir.join(format!("{stem}.to-server.wav"));
    if arrival_at(
        ctx,
        o,
        &format!("{tag}: what reached astar-server"),
        since,
        (original, mode),
        server_port,
        Some(&to_server),
    )
    .is_some()
    {
        wavs.push(to_server);
    }
    let to_node = ctx.dir.join(format!("{stem}.to-node.wav"));
    let late = arrival_at(
        ctx,
        o,
        &format!("{tag}: what astar-server sent the node"),
        since,
        (original, mode),
        node_port,
        Some(&to_node),
    );
    if late.is_some() {
        wavs.push(to_node);
    }
    (late, wavs)
}

/// What crossed one hop, from the pod's capture: voice frames to port
/// `dst` from any port but the node's (astar-server's link leaves from its
/// own socket, not its listener's 4570), lateness against their
/// own timestamps, timestamps never seen, and a line-up against the sent
/// audio. 16 kHz audio (slin16 on the link) is taken to 8 kHz ([`at_8k`]) for the
/// line-up, and written to `wav` when given. The frame rates are named in
/// the detail when any is not 8 kHz. Returns the worst lateness, ms.
fn arrival_at(
    ctx: &Ctx,
    o: &mut Outcome,
    name: &str,
    mark: Option<f64>,
    (original, mode): (&SstvPicture, SstvTxMode),
    dst: u16,
    wav: Option<&Path>,
) -> Option<f64> {
    let wire = match crate::pcap::voice_frames(&ctx.rig, None, dst, mark) {
        Ok(w) if !w.is_empty() => w,
        Ok(_) => {
            o.check(Check::skip(name, "no voice frames in the capture"));
            return None;
        }
        Err(e) => {
            o.check(Check::skip(name, format!("capture: {e}")));
            return None;
        }
    };
    let rates = {
        let mut r: Vec<u32> = wire.iter().map(|w| w.rate).collect();
        r.dedup();
        if r.iter().all(|&rate| rate == SAMPLE_RATE) {
            String::new()
        } else {
            format!(" at {r:?} Hz")
        }
    };
    let wire: Vec<crate::pcap::WireVoice> = wire.into_iter().map(at_8k).collect();
    let a = Arrival::of(&wire);
    if let Some(wav) = wav {
        let _ = std::fs::create_dir_all(&ctx.dir)
            .and_then(|()| outside::write_wav(wav, &a.audio, SAMPLE_RATE));
    }
    let line_up = astar_station::encode_picture(original, mode, SAMPLE_RATE)
        .ok()
        .and_then(|sent| line_up(&sent, &a.audio))
        .map_or_else(|| "not lined up".to_string(), |t| t.summary());
    o.check(
        Check::pass(
            name,
            format!(
                "{} frames{rates}, {} timestamps missing; lateness against timestamp p50 {:.1} ms, p99 {:.1} ms, max {:.1} ms; audio against the sent audio: {line_up}",
                a.frames, a.missing, a.late_p50, a.late_p99, a.late_max
            ),
        )
        .measured(a.late_max, "ms late, max"),
    );
    Some(a.late_max)
}

/// A wire frame at 8 kHz: of a 16 kHz frame, every other sample. A slin16
/// frame on the bridge's link was resampled up from 8 kHz audio, so it holds
/// nothing above 4 kHz and needs no filter on the way down.
fn at_8k(mut w: crate::pcap::WireVoice) -> crate::pcap::WireVoice {
    if w.rate == 16_000 {
        w.samples = w.samples.iter().step_by(2).copied().collect();
        w.rate = 8_000;
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq: u32, v: i16) -> RxFrame {
        RxFrame {
            at: Instant::now(),
            keyed: true,
            seq: Some(seq),
            ts_ms: None,
            samples: vec![v; VOICE_SAMPLES],
        }
    }

    #[test]
    fn a_recording_fills_lost_frames_with_silence() {
        let frames = [frame(1, 5), frame(2, -7), frame(5, 3)];
        let r = UsrpRecording::of(&frames);
        assert_eq!(r.frames, 3);
        assert_eq!(r.lost, 2);
        assert_eq!(r.audio.len(), 5 * VOICE_SAMPLES);
        assert!(
            r.audio[2 * VOICE_SAMPLES..4 * VOICE_SAMPLES]
                .iter()
                .all(|&s| s == 0)
        );
        assert_eq!(r.peak, 7);
    }

    #[test]
    fn a_long_skip_is_not_loss() {
        let frames = [frame(1, 1), frame(100, 1)];
        let r = UsrpRecording::of(&frames);
        assert_eq!(r.lost, 0);
        assert_eq!(r.audio.len(), 2 * VOICE_SAMPLES);
    }

    #[test]
    fn modes_map_to_astar_sstv() {
        assert_eq!(
            sstv_name(sstv_tx_modes::ROBOT_36),
            astar_sstv::Mode::Robot36
        );
        assert_eq!(
            sstv_name(sstv_tx_modes::SCOTTIE_2),
            astar_sstv::Mode::Scottie2
        );
        assert_eq!(sstv_name(sstv_tx_modes::PD_120), astar_sstv::Mode::Pd120);
        assert_eq!(astar_sstv::Mode::Pd120.name(), "PD120");
    }

    #[test]
    fn the_station_refuses_a_non_loopback_address() {
        let e = HeadlessStation::dial("192.0.2.1:4569".parse().unwrap(), "1999", "1997")
            .err()
            .unwrap();
        assert!(e.contains("not loopback"), "{e}");
    }

    fn robot36_audio() -> Vec<i16> {
        let (w, h) = sstv_tx_modes::ROBOT_36.resolution();
        astar_station::encode_picture(&test_picture(w, h), sstv_tx_modes::ROBOT_36, SAMPLE_RATE)
            .unwrap()
    }

    #[test]
    fn a_clean_recording_lines_up() {
        for mode in [
            sstv_tx_modes::ROBOT_36,
            sstv_tx_modes::SCOTTIE_2,
            sstv_tx_modes::PD_120,
        ] {
            let (w, h) = mode.resolution();
            let sent =
                astar_station::encode_picture(&test_picture(w, h), mode, SAMPLE_RATE).unwrap();
            let mut rec = vec![0_i16; 1234];
            rec.extend_from_slice(&sent);
            let t = timeline(&sent, &rec).unwrap();
            assert_eq!(t, Timeline::default(), "{mode:?}: {}", t.summary());
        }
    }

    #[test]
    fn other_audio_before_the_picture_is_skipped() {
        // The node's identifier: two seconds of a 1 kHz tone, then a gap.
        let sent = robot36_audio();
        let mut rec: Vec<i16> = (0..16_000)
            .map(|i| {
                let ph = f64::from(i) * 2.0 * std::f64::consts::PI * 1000.0 / 8000.0;
                #[allow(clippy::cast_possible_truncation)]
                let v = (ph.sin() * 8000.0) as i16;
                v
            })
            .collect();
        rec.extend(std::iter::repeat_n(0, 4321));
        rec.extend_from_slice(&sent);
        let t = timeline(&sent, &rec).unwrap();
        assert_eq!(t, Timeline::default(), "{}", t.summary());
    }

    #[test]
    fn a_dropped_and_a_silenced_frame_are_found() {
        let sent = robot36_audio();
        let mut rec = vec![0_i16; 800];
        rec.extend_from_slice(&sent[..100_000]);
        // One frame made silent in place, one frame removed.
        let silenced = 800 + 60_000;
        rec[silenced..silenced + VOICE_SAMPLES].fill(0);
        rec.extend_from_slice(&sent[100_000 + VOICE_SAMPLES..]);
        let t = timeline(&sent, &rec).unwrap();
        assert!((t.dropped_ms - 20.0).abs() < 1.0, "{}", t.summary());
        assert!(t.inserted_ms.abs() < 1e-9, "{}", t.summary());
        assert!(t.silent >= 1, "{}", t.summary());
        assert_eq!(t.slips.len(), 1, "{}", t.summary());
        assert!((t.slips[0].0 - 12.5).abs() < 0.1, "{}", t.summary());
    }

    #[test]
    fn a_long_gap_is_found_again() {
        // Twelve frames (240 ms) missing: more than the near search reaches.
        let sent = robot36_audio();
        let mut rec = sent[..120_000].to_vec();
        rec.extend_from_slice(&sent[120_000 + 12 * VOICE_SAMPLES..]);
        let t = timeline(&sent, &rec).unwrap();
        assert!((t.dropped_ms - 240.0).abs() < 1.0, "{}", t.summary());
        assert!(t.inserted_ms.abs() < 1e-9, "{}", t.summary());
        assert!(t.unmatched <= 10, "{}", t.summary());
    }

    #[test]
    fn an_inserted_frame_is_found() {
        let sent = robot36_audio();
        let mut rec = sent[..150_000].to_vec();
        rec.extend_from_slice(&sent[150_000 - VOICE_SAMPLES..150_000]);
        rec.extend_from_slice(&sent[150_000..]);
        let t = timeline(&sent, &rec).unwrap();
        assert!(t.dropped_ms.abs() < 1e-9, "{}", t.summary());
        assert!((t.inserted_ms - 20.0).abs() < 1.0, "{}", t.summary());
    }

    /// The sent audio half a sample late, as a slin16 round trip leaves it:
    /// each sample the mean of itself and the one before.
    fn half_sample_late(x: &[i16]) -> Vec<i16> {
        let mut out = vec![x[0]];
        out.extend(x.windows(2).map(|w| i16::midpoint(w[0], w[1])));
        out
    }

    #[test]
    fn a_half_sample_late_recording_loses_the_plain_line_up() {
        let sent = robot36_audio();
        let mut rec = vec![0_i16; 777];
        rec.extend(half_sample_late(&sent));
        let t = timeline(&sent, &rec).unwrap();
        assert!(t.unmatched > 100, "{}", t.summary());
    }

    #[test]
    fn a_half_sample_late_recording_lines_up_whole() {
        let sent = robot36_audio();
        let mut rec = vec![0_i16; 777];
        rec.extend(half_sample_late(&sent));
        let t = line_up(&sent, &rec).unwrap();
        assert_eq!(
            t,
            Timeline {
                half_sample_late: true,
                ..Timeline::default()
            },
            "{}",
            t.summary()
        );
    }

    #[test]
    fn a_recording_on_time_lines_up_as_it_is() {
        let sent = robot36_audio();
        let mut rec = vec![0_i16; 777];
        rec.extend_from_slice(&sent);
        let t = line_up(&sent, &rec).unwrap();
        assert_eq!(t, Timeline::default(), "{}", t.summary());
    }

    /// The Robot 36 audio with one 20 ms frame dropped at sample 100 000,
    /// or with the frame before sample 150 000 played twice.
    fn with_a_slip(sent: &[i16], dropped: bool) -> Vec<i16> {
        let mut rec = vec![0_i16; 777];
        if dropped {
            rec.extend_from_slice(&sent[..100_000]);
            rec.extend_from_slice(&sent[100_000 + VOICE_SAMPLES..]);
        } else {
            rec.extend_from_slice(&sent[..150_000]);
            rec.extend_from_slice(&sent[150_000 - VOICE_SAMPLES..]);
        }
        rec
    }

    /// `line_up` on a recording with one slip, on time or half a sample
    /// late: the slip is still found, and the right alignment wins.
    fn check_slip_through_line_up(dropped: bool, late: bool) {
        let sent = robot36_audio();
        let mut rec = with_a_slip(&sent, dropped);
        if late {
            rec = half_sample_late(&rec);
        }
        let t = line_up(&sent, &rec).unwrap();
        let what = format!("dropped={dropped} late={late}: {}", t.summary());
        assert_eq!(t.half_sample_late, late, "{what}");
        assert_eq!(t.slips.len(), 1, "{what}");
        let (moved, other) = if dropped {
            (t.dropped_ms, t.inserted_ms)
        } else {
            (t.inserted_ms, t.dropped_ms)
        };
        assert!((moved - 20.0).abs() < 1.0, "{what}");
        assert!(other.abs() < 1e-9, "{what}");
        assert!(t.unmatched <= 2, "{what}");
    }

    #[test]
    fn line_up_finds_a_dropped_frame_on_time() {
        check_slip_through_line_up(true, false);
    }

    #[test]
    fn line_up_finds_a_dropped_frame_half_a_sample_late() {
        check_slip_through_line_up(true, true);
    }

    #[test]
    fn line_up_finds_an_inserted_frame_on_time() {
        check_slip_through_line_up(false, false);
    }

    #[test]
    fn line_up_finds_an_inserted_frame_half_a_sample_late() {
        check_slip_through_line_up(false, true);
    }

    #[test]
    fn a_slin16_frame_comes_down_to_8k() {
        let w = crate::pcap::WireVoice {
            t: 0.0,
            rate: 16_000,
            samples: (0..320).collect(),
            ts16: 0,
        };
        let w = at_8k(w);
        assert_eq!(w.rate, 8_000);
        assert_eq!(w.samples.len(), VOICE_SAMPLES);
        assert_eq!(&w.samples[..3], &[0, 2, 4]);
    }

    /// Line a saved recording up against the test picture's audio
    /// (`NODEBENCH_SSTV_MODE` = `scottie2` or `pd120`; Robot 36 otherwise):
    /// `NODEBENCH_SSTV_WAV=<cell>.wav cargo test -p astar-nodebench --lib
    /// saved_recording -- --ignored --nocapture`. The WAVs are 16-bit mono
    /// 8 kHz, as `outside::write_wav` writes them.
    #[test]
    #[ignore = "reads a recording named by NODEBENCH_SSTV_WAV"]
    fn saved_recording_lines_up() {
        let Ok(path) = std::env::var("NODEBENCH_SSTV_WAV") else {
            return;
        };
        let bytes = std::fs::read(&path).unwrap();
        let rec: Vec<i16> = bytes[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| i16::from_le_bytes(c))
            .collect();
        let mode = match std::env::var("NODEBENCH_SSTV_MODE").as_deref() {
            Ok("scottie2") => sstv_tx_modes::SCOTTIE_2,
            Ok("pd120") => sstv_tx_modes::PD_120,
            _ => sstv_tx_modes::ROBOT_36,
        };
        let (w, h) = mode.resolution();
        let sent = astar_station::encode_picture(&test_picture(w, h), mode, SAMPLE_RATE).unwrap();
        let t = line_up(&sent, &rec).unwrap();
        println!("{path}: {}", t.summary());
    }

    #[test]
    fn a_fully_lost_recording_peaks_at_minus_infinity() {
        assert!(UsrpRecording::default().peak_dbfs().is_infinite());
    }
}
