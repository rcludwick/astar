// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The soak tier: hold one link between the ASL3 node and astar-server for
//! hours, sample both sides every few seconds, and log every up/down edge
//! with who hung up.
//!
//! Output, in the soak's run directory (`target/nodebench/<run-id>/`):
//!
//! * `soak.jsonl` — one [`Event`] per line, appended as it happens, so a
//!   crash or a Ctrl-C loses nothing and `--resume <dir>` can carry on.
//! * `summary.json` — [`SoakSummary`], recomputed from `soak.jsonl` whenever
//!   the soak stops (so it is always a pure function of the log).
//! * `drops/NNN-frames.txt` — the IAX2 full frames around each drop, from
//!   the capture sidecar's ring buffer.
//! * `logs/` — container logs at the end, as `nodebench run` collects them.
//!
//! # Topologies
//!
//! `loopback` is the quick tier's pod (`pod.yaml`). `nat` is `nat.yaml`:
//! the node behind a MASQUERADE router on two podman `--internal` networks,
//! with conntrack's UDP timeouts set from `--udp-timeout`. [`nat_up`] plays
//! it pod by pod, because `podman kube play` has no per-pod network field.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::node::Ilink;
use crate::node::parse::LinkConnState;
use crate::pcap::{self, IaxFrame, iax};
use crate::rig::{Rig, RigConfig, RigError, Topology};
use crate::scenario::common::{self, ServerLeg};
use crate::scenario::{Ctx, Tuning};
use crate::server::LinkAction;
use crate::time::rfc3339_utc;

// ---- the nat topology -------------------------------------------------------

/// The two `--internal` networks `nat.yaml` names: (name, subnet).
pub const NAT_NETWORKS: [(&str, &str); 2] = [
    ("nodebench-nat-in", "10.89.71.0/24"),
    ("nodebench-nat-out", "10.89.72.0/24"),
];
/// The router's address on the outside network: everything astar-server
/// sees from the node comes from here.
pub const NAT_OUTSIDE_ADDR: &str = "10.89.72.254";
/// astar-server's address in the nat topology.
pub const NAT_SERVER_ADDR: &str = "10.89.72.3";
/// The annotation carrying a pod's `podman kube play --network` arguments.
pub const NETWORKS_ANNOTATION: &str = "nodebench.astar/networks";
/// The router container (iptables, conntrack).
pub const NAT_ROUTER_CONTAINER: &str = "nodebench-nat-router-nat";
/// Linux's long-standing defaults, used when `--udp-timeout` is not given.
pub const DEFAULT_UDP_TIMEOUT: u64 = 30;

/// `cfg` re-pointed at the nat topology's pods, containers and ports.
#[must_use]
pub fn nat_rig_config(mut cfg: RigConfig, udp_timeout: Option<u64>) -> RigConfig {
    cfg.topology = Topology::Nat;
    cfg.pod = "nodebench-nat-node".into();
    cfg.node_container = "nodebench-nat-node-asl3".into();
    cfg.server_container = "nodebench-nat-server-astar-server".into();
    cfg.capture_container = Some("nodebench-nat-router-capture".into());
    cfg.ami = SocketAddr::from(([127, 0, 0, 1], 35038));
    cfg.server_url = "http://127.0.0.1:38730".into();
    cfg.server_iax = SocketAddr::from(([10, 89, 72, 3], 4570));
    cfg.kube_vars.insert(
        "UDP_TIMEOUT".into(),
        udp_timeout.unwrap_or(DEFAULT_UDP_TIMEOUT).to_string(),
    );
    cfg
}

/// One Pod document of a multi-document kube file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KubePod {
    pub name: String,
    /// `--network` values, in order (the first is the pod's `eth0`).
    pub networks: Vec<String>,
    pub yaml: String,
}

/// Split a kube file on `---` document separators, dropping empty documents.
#[must_use]
pub fn split_docs(yaml: &str) -> Vec<String> {
    let mut docs = vec![String::new()];
    for line in yaml.lines() {
        if line.trim_end() == "---" {
            docs.push(String::new());
        } else if let Some(d) = docs.last_mut() {
            d.push_str(line);
            d.push('\n');
        }
    }
    docs.into_iter()
        .filter(|d| {
            d.lines()
                .any(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        })
        .collect()
}

/// The pod name and network annotation of one document. Reads the plain
/// line shapes `nat.yaml` uses (no YAML parser in the bench).
pub fn parse_pod(doc: &str) -> Result<KubePod, String> {
    let mut name = None;
    let mut networks = None;
    let mut in_metadata = false;
    for line in doc.lines() {
        if line.starts_with("metadata:") {
            in_metadata = true;
            continue;
        }
        if !line.starts_with(' ') && !line.trim().is_empty() {
            in_metadata = false;
        }
        let t = line.trim();
        if in_metadata && name.is_none() && line.starts_with("  name:") {
            name = Some(unquote(t.trim_start_matches("name:")));
        }
        if let Some(v) = t.strip_prefix(&format!("{NETWORKS_ANNOTATION}:")) {
            networks = Some(
                unquote(v)
                    .split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<_>>(),
            );
        }
    }
    let name = name.ok_or("a document has no metadata.name")?;
    let networks = networks
        .filter(|n| !n.is_empty())
        .ok_or_else(|| format!("pod {name} has no {NETWORKS_ANNOTATION} annotation"))?;
    for n in &networks {
        let net = n.split(':').next().unwrap_or_default();
        if !NAT_NETWORKS.iter().any(|(k, _)| *k == net) {
            return Err(format!(
                "pod {name} names network {net:?}, which is not one of the nat rig's"
            ));
        }
    }
    Ok(KubePod {
        name,
        networks,
        yaml: doc.to_string(),
    })
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').to_string()
}

/// The rendered nat.yaml, pod by pod.
pub fn nat_pods(rig: &Rig) -> Result<Vec<KubePod>, RigError> {
    let yaml = rig.rendered()?;
    split_docs(&yaml)
        .iter()
        .map(|d| parse_pod(d))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|last| RigError::NotReady {
            what: "nat.yaml".into(),
            last,
        })
}

/// Whether any nat pod exists.
pub fn nat_exists(rig: &Rig) -> Result<bool, RigError> {
    for p in nat_pods(rig)? {
        if rig.podman().succeeds(&["pod", "exists", &p.name])? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Create the internal networks (if missing) and play each pod with its own
/// `--network` arguments. Refuses when a nat pod already exists unless
/// `replace`, which removes them first.
pub fn nat_up(rig: &Rig, replace: bool) -> Result<(), RigError> {
    let pods = nat_pods(rig)?;
    if nat_exists(rig)? {
        if replace {
            nat_down(rig)?;
        } else {
            return Err(RigError::NotReady {
                what: "nat rig".into(),
                last: "already up; pass --reuse to use it or --replace to recreate it".into(),
            });
        }
    }
    let pm = rig.podman();
    for (name, subnet) in NAT_NETWORKS {
        if !pm.succeeds(&["network", "exists", name])? {
            pm.run(&["network", "create", "--internal", "--subnet", subnet, name])?;
        }
        // An existing network must still be internal: a routed one would
        // give the node a way out of the VM.
        let internal = pm.run(&["network", "inspect", name, "--format", "{{.Internal}}"])?;
        if internal.trim() != "true" {
            return Err(RigError::NotReady {
                what: format!("network {name}"),
                last: "exists but is not --internal; remove it (podman network rm)".into(),
            });
        }
    }
    if let Some(c) = rig.cfg.kube_vars.get("CAPTURES") {
        fs::create_dir_all(c)?;
    }
    for p in &pods {
        let mut args: Vec<String> = vec!["kube".into(), "play".into()];
        for n in &p.networks {
            args.push("--network".into());
            args.push(n.clone());
        }
        args.push("-".into());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        pm.run_stdin(&refs, &p.yaml)?;
    }
    Ok(())
}

/// Remove every nat pod, then the networks (when nothing else uses them).
pub fn nat_down(rig: &Rig) -> Result<(), RigError> {
    let pm = rig.podman();
    for p in nat_pods(rig)?.iter().rev() {
        if pm.succeeds(&["pod", "exists", &p.name])? {
            pm.run(&["pod", "rm", "-f", "--time", "2", &p.name])?;
        }
    }
    for (name, _) in NAT_NETWORKS {
        if pm.succeeds(&["network", "exists", name])? {
            let _ = pm.run(&["network", "rm", name]);
        }
    }
    Ok(())
}

/// The router is configured and the node has its route across it.
pub fn nat_ready(rig: &Rig) -> Result<(), String> {
    let pm = rig.podman();
    let rules = pm
        .exec(NAT_ROUTER_CONTAINER, &["iptables", "-t", "nat", "-S"])
        .map_err(|e| format!("router: {e}"))?;
    if !rules.contains("MASQUERADE") {
        return Err("router: no MASQUERADE rule yet".into());
    }
    let route = pm
        .exec("nodebench-nat-node-route", &["ip", "route"])
        .map_err(|e| format!("node route: {e}"))?;
    if !route.contains("via 10.89.71.254") {
        return Err("node: no route across the router yet".into());
    }
    Ok(())
}

/// The router's conntrack UDP timeouts, as the kernel reports them.
pub fn nat_timeouts(rig: &Rig) -> Result<String, RigError> {
    rig.podman().exec(
        NAT_ROUTER_CONTAINER,
        &[
            "sysctl",
            "net.netfilter.nf_conntrack_udp_timeout",
            "net.netfilter.nf_conntrack_udp_timeout_stream",
        ],
    )
}

/// `conntrack -L -p udp` on the router.
pub fn nat_conntrack(rig: &Rig) -> Result<String, RigError> {
    rig.podman()
        .exec(NAT_ROUTER_CONTAINER, &["conntrack", "-L", "-p", "udp"])
}

// ---- the event log ----------------------------------------------------------

/// Which side dials the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dialer {
    /// `rpt cmd 1999 ilink 3 1998` (a `ClearNode` linking to astar-server).
    Node,
    /// astar-server `POST /link connect 1999` at the node's address.
    Server,
}

impl std::str::FromStr for Dialer {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "node" => Ok(Self::Node),
            "server" => Ok(Self::Server),
            _ => Err(format!("unknown dialer {s:?} (node | server)")),
        }
    }
}

/// Which side the bench saw lose the link, or who sent the HANGUP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The ASL3 node.
    Node,
    /// astar-server.
    Server,
    /// Both at once (within one sample).
    Both,
    /// Nobody sent a HANGUP: the call just went away (a timeout, an INVAL).
    None,
    /// The capture could not be read.
    Unknown,
}

impl Side {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Server => "server",
            Self::Both => "both",
            Self::None => "none",
            Self::Unknown => "unknown",
        }
    }
}

/// One line of `soak.jsonl`. Times are host Unix seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// The soak began.
    Start {
        t: f64,
        run_id: String,
        topology: String,
        dialer: Dialer,
        planned_secs: f64,
        sample_secs: f64,
        udp_timeout: Option<u64>,
        git_sha: Option<String>,
        /// What the router's kernel says the timeouts are (nat only).
        conntrack: Option<String>,
    },
    /// `--resume` picked the soak up again.
    Resume { t: f64 },
    /// Both sides report the link.
    LinkUp {
        t: f64,
        setup_secs: f64,
        /// `inbound call 3` / `link 1999`.
        leg: String,
    },
    /// The link would not come up.
    SetupFailed { t: f64, error: String },
    /// One periodic look at both sides.
    Sample {
        t: f64,
        node: bool,
        server: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// The link went away.
    LinkDown {
        t: f64,
        /// The last sample with both sides up.
        last_good: f64,
        /// How long the link had been up at `last_good`.
        held_secs: f64,
        /// Which side the samples saw lose it first.
        first_side: Side,
        /// Who sent a HANGUP (from the capture).
        hangup: Side,
        /// What the samples and the capture said.
        detail: String,
        /// `drops/NNN-frames.txt`, relative to the run directory.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<String>,
    },
    /// A note (a rig hiccup, a skipped analysis).
    Note { t: f64, text: String },
    /// The soak stopped.
    Stop {
        t: f64,
        /// `duration` / `interrupted` / `error` / `first_drop`.
        reason: String,
    },
}

impl Event {
    #[must_use]
    pub fn t(&self) -> f64 {
        match self {
            Self::Start { t, .. }
            | Self::Resume { t }
            | Self::LinkUp { t, .. }
            | Self::SetupFailed { t, .. }
            | Self::Sample { t, .. }
            | Self::LinkDown { t, .. }
            | Self::Note { t, .. }
            | Self::Stop { t, .. } => *t,
        }
    }
}

/// Read every event from a `soak.jsonl`, skipping lines that do not parse
/// (a torn last line after a crash).
pub fn read_events(path: &Path) -> io::Result<Vec<Event>> {
    let f = File::open(path)?;
    Ok(BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect())
}

/// Appends events to `soak.jsonl`, one flushed line each.
#[derive(Debug)]
pub struct EventLog {
    file: File,
    pub path: PathBuf,
}

impl EventLog {
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(d) = path.parent() {
            fs::create_dir_all(d)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    pub fn push(&mut self, e: &Event) -> io::Result<()> {
        let mut line = serde_json::to_string(e).map_err(io::Error::other)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.flush()
    }
}

// ---- the summary ------------------------------------------------------------

/// One stretch of link-up time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkSession {
    pub up_at: f64,
    /// When it was seen gone (or the soak stopped).
    pub down_at: Option<f64>,
    /// Up time (to the last good sample, for a drop).
    pub secs: f64,
    /// `drop` / `stopped`.
    pub ended: String,
    pub first_side: Option<Side>,
    pub hangup: Option<Side>,
}

/// `summary.json`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SoakSummary {
    pub run_id: String,
    pub topology: String,
    pub dialer: Option<Dialer>,
    pub udp_timeout: Option<u64>,
    pub started: String,
    pub finished: String,
    pub planned_secs: f64,
    /// Wall time the soak was actually running (across resumes).
    pub covered_secs: f64,
    /// How the last segment ended (`running` if it has not).
    pub stop_reason: String,
    pub segments: u32,
    pub samples: u64,
    pub drops: u32,
    pub setup_failures: u32,
    /// Time with the link up, and as a share of `covered_secs`.
    pub uptime_secs: f64,
    pub uptime_pct: f64,
    pub longest_secs: f64,
    pub shortest_secs: Option<f64>,
    pub median_secs: Option<f64>,
    /// Drops by who sent the HANGUP.
    pub hangup_by: BTreeMap<String, u32>,
    /// Drops by which side the samples saw lose the link first.
    pub first_side_by: BTreeMap<String, u32>,
    pub links: Vec<LinkSession>,
}

impl SoakSummary {
    /// Is a soak with this summary finished with no drop?
    #[must_use]
    pub fn clean(&self) -> bool {
        self.drops == 0 && self.setup_failures == 0 && !self.links.is_empty()
    }
}

/// Fold the event log into a summary. Pure, so a resumed or crashed soak
/// summarises the same way as a clean one.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn summarize(events: &[Event]) -> SoakSummary {
    let mut s = SoakSummary {
        stop_reason: "running".into(),
        ..SoakSummary::default()
    };
    let mut seg_start: Option<f64> = None;
    let mut last_t: Option<f64> = None;
    let mut open: Option<LinkSession> = None;
    let mut last_good: Option<f64> = None;

    let close_segment = |s: &mut SoakSummary,
                         seg: Option<f64>,
                         last: Option<f64>,
                         open: &mut Option<LinkSession>| {
        if let (Some(a), Some(b)) = (seg, last) {
            s.covered_secs += (b - a).max(0.0);
        }
        if let Some(mut l) = open.take() {
            let end = last.unwrap_or(l.up_at);
            l.secs = (end - l.up_at).max(0.0);
            l.down_at = Some(end);
            l.ended = "stopped".into();
            s.links.push(l);
        }
    };

    for e in events {
        match e {
            Event::Start {
                t,
                run_id,
                topology,
                dialer,
                planned_secs,
                udp_timeout,
                ..
            } => {
                s.run_id.clone_from(run_id);
                s.topology.clone_from(topology);
                s.dialer = Some(*dialer);
                s.udp_timeout = *udp_timeout;
                s.planned_secs = *planned_secs;
                s.started = rfc3339_utc(unix_secs(*t));
                s.segments = 1;
                seg_start = Some(*t);
            }
            Event::Resume { t } => {
                close_segment(&mut s, seg_start, last_t, &mut open);
                s.segments += 1;
                s.stop_reason = "running".into();
                seg_start = Some(*t);
            }
            Event::LinkUp { t, .. } => {
                open = Some(LinkSession {
                    up_at: *t,
                    down_at: None,
                    secs: 0.0,
                    ended: "up".into(),
                    first_side: None,
                    hangup: None,
                });
                last_good = Some(*t);
            }
            Event::SetupFailed { .. } => s.setup_failures += 1,
            Event::Sample {
                t, node, server, ..
            } => {
                s.samples += 1;
                if *node && *server {
                    last_good = Some(*t);
                }
            }
            Event::LinkDown {
                t,
                last_good: lg,
                first_side,
                hangup,
                ..
            } => {
                s.drops += 1;
                *s.hangup_by.entry(hangup.as_str().into()).or_default() += 1;
                *s.first_side_by
                    .entry(first_side.as_str().into())
                    .or_default() += 1;
                let up_at = open.take().map_or(*lg, |l| l.up_at);
                s.links.push(LinkSession {
                    up_at,
                    down_at: Some(*t),
                    secs: (lg.max(last_good.unwrap_or(*lg)) - up_at).max(0.0),
                    ended: "drop".into(),
                    first_side: Some(*first_side),
                    hangup: Some(*hangup),
                });
            }
            Event::Note { .. } => {}
            Event::Stop { reason, t } => {
                s.stop_reason.clone_from(reason);
                s.finished = rfc3339_utc(unix_secs(*t));
            }
        }
        last_t = Some(e.t());
    }
    close_segment(&mut s, seg_start, last_t, &mut open);
    if s.finished.is_empty()
        && let Some(t) = last_t
    {
        s.finished = rfc3339_utc(unix_secs(t));
    }

    let mut secs: Vec<f64> = s.links.iter().map(|l| l.secs).collect();
    secs.sort_by(f64::total_cmp);
    s.uptime_secs = secs.iter().sum();
    s.uptime_pct = if s.covered_secs > 0.0 {
        (100.0 * s.uptime_secs / s.covered_secs).min(100.0)
    } else {
        0.0
    };
    s.longest_secs = secs.last().copied().unwrap_or(0.0);
    s.shortest_secs = secs.first().copied();
    s.median_secs = (!secs.is_empty()).then(|| secs[secs.len() / 2]);
    s
}

/// The summary as a few lines for the terminal.
#[must_use]
pub fn summary_text(s: &SoakSummary) -> String {
    use std::fmt::Write as _;
    let mut o = String::new();
    let _ = writeln!(
        o,
        "soak {}  topology={}{}  dialer={}",
        s.run_id,
        s.topology,
        s.udp_timeout
            .map(|u| format!(" udp-timeout={u}s"))
            .unwrap_or_default(),
        s.dialer.map_or("?", |d| match d {
            Dialer::Node => "node",
            Dialer::Server => "server",
        })
    );
    let _ = writeln!(
        o,
        "  ran {} of {} planned ({} segment{}), stopped: {}",
        fmt_secs(s.covered_secs),
        fmt_secs(s.planned_secs),
        s.segments,
        if s.segments == 1 { "" } else { "s" },
        s.stop_reason
    );
    let _ = writeln!(
        o,
        "  link up {} ({:.1} %), {} sample{}, {} drop{}, {} failed setup{}",
        fmt_secs(s.uptime_secs),
        s.uptime_pct,
        s.samples,
        if s.samples == 1 { "" } else { "s" },
        s.drops,
        if s.drops == 1 { "" } else { "s" },
        s.setup_failures,
        if s.setup_failures == 1 { "" } else { "s" },
    );
    if !s.links.is_empty() {
        let _ = writeln!(
            o,
            "  link durations: longest {}, median {}, shortest {}",
            fmt_secs(s.longest_secs),
            s.median_secs.map_or_else(|| "-".into(), fmt_secs),
            s.shortest_secs.map_or_else(|| "-".into(), fmt_secs),
        );
    }
    if s.drops > 0 {
        let by = |m: &BTreeMap<String, u32>| {
            m.iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let _ = writeln!(o, "  hangup sent by: {}", by(&s.hangup_by));
        let _ = writeln!(o, "  first seen gone on: {}", by(&s.first_side_by));
    }
    o
}

/// `75 s`, `12 m 05 s`, `3 h 04 m`.
#[must_use]
pub fn fmt_secs(secs: f64) -> String {
    let s = unix_secs(secs);
    if s < 120 {
        format!("{secs:.1} s")
    } else if s < 3600 {
        format!("{} m {:02} s", s / 60, s % 60)
    } else {
        format!("{} h {:02} m", s / 3600, (s % 3600) / 60)
    }
}

/// Whole non-negative seconds of an f64.
fn unix_secs(t: f64) -> u64 {
    if t.is_finite() && t > 0.0 {
        // Saturating float-to-int: fine for timestamps and durations.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let s = t as u64;
        s
    } else {
        0
    }
}

/// Host Unix time, fractional seconds.
#[must_use]
pub fn now_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

// ---- who hung up -------------------------------------------------------------

/// Who sent the HANGUP among `frames`, judged by source port (the node is
/// Asterisk's 4569; NAT preserves it), with a one-line account.
#[must_use]
pub fn hangup_side(frames: &[IaxFrame], node_port: u16, server_port: u16) -> (Side, String) {
    let first = |port: u16| {
        frames
            .iter()
            .find(|f| f.src_port == port && (f.is_iax(iax::HANGUP) || f.is_iax(iax::REJECT)))
    };
    let (n, s) = (first(node_port), first(server_port));
    let describe = |who: &str, f: &IaxFrame| {
        let cause = if f.text.is_empty() {
            String::new()
        } else {
            format!(" cause {:?}", f.text)
        };
        format!(
            "{who} sent {} on call {}>{}{cause}",
            f.label(),
            f.src_call,
            f.dst_call
        )
    };
    match (n, s) {
        (Some(a), Some(b)) if a.t <= b.t => (
            Side::Node,
            format!("{} first; {}", describe("node", a), describe("server", b)),
        ),
        (Some(a), Some(b)) => (
            Side::Server,
            format!("{} first; {}", describe("server", b), describe("node", a)),
        ),
        (Some(a), None) => (Side::Node, describe("node", a)),
        (None, Some(b)) => (Side::Server, describe("server", b)),
        (None, None) => {
            let inval = frames.iter().filter(|f| f.is_iax(iax::INVAL)).count();
            let why = if inval > 0 {
                format!("no HANGUP in the capture; {inval} INVAL frame(s)")
            } else {
                "no HANGUP in the capture".into()
            };
            (Side::None, why)
        }
    }
}

/// From two probe results, which side lost the link.
#[must_use]
pub fn first_side(node_up: bool, server_up: bool) -> Side {
    match (node_up, server_up) {
        (true, false) => Side::Server,
        (false, true) => Side::Node,
        _ => Side::Both,
    }
}

// ---- the soak loop ------------------------------------------------------------

/// What `nodebench soak` was asked to do.
#[derive(Debug, Clone)]
pub struct SoakOptions {
    pub duration: Duration,
    pub sample: Duration,
    pub dialer: Dialer,
    pub udp_timeout: Option<u64>,
    /// Re-link after a drop (default) rather than stop at the first.
    pub relink: bool,
    /// The run directory (`soak.jsonl` lives here).
    pub dir: PathBuf,
    pub run_id: String,
    /// Carry on an existing `soak.jsonl` rather than start one.
    pub resume: bool,
}

/// Write evidence for at most this many drops in full, then every
/// [`EVIDENCE_EVERY`]th: a pathological rig that drops every few seconds
/// should not spend the soak reading captures.
pub const EVIDENCE_FIRST: u32 = 20;
pub const EVIDENCE_EVERY: u32 = 25;

/// Consecutive samples on which the node or astar-server cannot be asked
/// (AMI or HTTP failing) before the soak treats the link as lost.
pub const BLIND_SAMPLES_LIMIT: u32 = 6;

/// The newest ring-buffer files, symlinked into a scratch dir in the
/// capture container, so a drop's analysis reads minutes, not the ring.
fn recent_frames(rig: &Rig, since: f64) -> Result<Vec<IaxFrame>, RigError> {
    let Some(cap) = &rig.cfg.capture_container else {
        return Ok(Vec::new());
    };
    let dir = rig.cfg.capture_dir.trim_end_matches('/');
    let link = format!(
        "rm -rf /tmp/nb-recent && mkdir /tmp/nb-recent && \
         for f in $(ls -t {dir}/*.pcapng 2>/dev/null | head -3); do ln -s \"$f\" /tmp/nb-recent/; done; \
         {}",
        pcap::tshark_script("/tmp/nb-recent", Some(since))
    );
    let out = rig.podman().exec(cap, &["sh", "-c", &link])?;
    let mut v: Vec<IaxFrame> = out.lines().filter_map(pcap::parse_line).collect();
    v.sort_by(|a, b| a.t.total_cmp(&b.t));
    Ok(v)
}

/// Keep only the frames astar-server actually exchanged: in the nat
/// topology the capture sees each packet on both router interfaces.
fn server_view(frames: Vec<IaxFrame>, topology: Topology) -> Vec<IaxFrame> {
    match topology {
        Topology::Loopback => frames,
        Topology::Nat => frames
            .into_iter()
            .filter(|f| f.src == NAT_OUTSIDE_ADDR || f.dst == NAT_OUTSIDE_ADDR)
            .collect(),
    }
}

/// A live soak.
struct Soaker<'a> {
    ctx: Ctx,
    opts: &'a SoakOptions,
    log: EventLog,
    drops: u32,
    stop: &'a AtomicBool,
}

impl Soaker<'_> {
    fn emit(&mut self, e: &Event) {
        if let Err(err) = self.log.push(e) {
            eprintln!(
                "nodebench soak: cannot append to {}: {err}",
                self.log.path.display()
            );
        }
        match e {
            Event::LinkUp {
                setup_secs, leg, ..
            } => {
                eprintln!("{}  link up ({leg}, {setup_secs:.1} s to set up)", stamp());
            }
            Event::SetupFailed { error, .. } => {
                eprintln!("{}  link setup failed: {error}", stamp());
            }
            Event::LinkDown {
                held_secs,
                first_side,
                hangup,
                detail,
                ..
            } => eprintln!(
                "{}  LINK DOWN after {} (gone first on {}, hangup by {}): {detail}",
                stamp(),
                fmt_secs(*held_secs),
                first_side.as_str(),
                hangup.as_str()
            ),
            Event::Note { text, .. } => eprintln!("{}  note: {text}", stamp()),
            _ => {}
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Sleep up to `d`, waking early on Ctrl-C.
    fn nap(&self, d: Duration) {
        let end = Instant::now() + d;
        while !self.stopped() {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(Duration::from_millis(200)));
        }
    }

    fn dial(&mut self) -> Option<ServerLeg> {
        if let Err(e) = common::clean_start(&self.ctx) {
            self.emit(&Event::SetupFailed {
                t: now_f64(),
                error: format!("clean start: {e}"),
            });
            return None;
        }
        let r = match self.opts.dialer {
            Dialer::Node => common::node_dials_server(&self.ctx, Ilink::Transceive, None),
            Dialer::Server => common::server_dials_node(&self.ctx, LinkAction::Connect, None),
        };
        match r {
            Ok((leg, took)) => {
                self.emit(&Event::LinkUp {
                    t: now_f64(),
                    setup_secs: took.as_secs_f64(),
                    leg: leg.to_string(),
                });
                Some(leg)
            }
            Err(error) => {
                self.emit(&Event::SetupFailed {
                    t: now_f64(),
                    error,
                });
                None
            }
        }
    }

    /// One look at both sides. `Err` means a side could not be asked (an
    /// AMI or HTTP failure), which says nothing about the link; `Ok` is what
    /// both sides reported: (node up, server up, why not).
    fn look(&self, leg: &ServerLeg) -> Result<(bool, bool, Option<String>), String> {
        let server_no = self.ctx.cfg().server_number.clone();
        let node = match self.ctx.node.link_to(&server_no) {
            Err(e) => return Err(format!("node query failed: {e}")),
            Ok(None) => Err(format!("node: no link to {server_no}")),
            Ok(Some(l)) if l.state == LinkConnState::Established => Ok(()),
            Ok(Some(l)) => Err(format!("node: link to {server_no} is {:?}", l.state)),
        };
        let server = match self.ctx.server.status() {
            Err(e) => return Err(format!("server query failed: {e}")),
            Ok(st) => common::server_leg_up(&st, leg),
        };
        let detail = match (&node, &server) {
            (Ok(()), Ok(())) => None,
            (Err(a), Ok(())) => Some(a.clone()),
            (Ok(()), Err(b)) => Some(b.clone()),
            (Err(a), Err(b)) => Some(format!("{a}; {b}")),
        };
        Ok((node.is_ok(), server.is_ok(), detail))
    }

    /// [`Self::look`], asking again (after 500 ms) whenever the first answer
    /// is not "both up": a podman exec or an AMI login can fail on its own,
    /// and one bad sample must not end a link that is still there.
    fn look_twice(&mut self, leg: &ServerLeg) -> Result<(bool, bool, Option<String>), String> {
        let first = self.look(leg);
        if matches!(first, Ok((true, true, _))) {
            return first;
        }
        std::thread::sleep(Duration::from_millis(500));
        let again = self.look(leg);
        if matches!(again, Ok((true, true, _))) {
            let said = match &first {
                Ok((_, _, d)) => d.clone().unwrap_or_default(),
                Err(e) => e.clone(),
            };
            self.emit(&Event::Note {
                t: now_f64(),
                text: format!("one sample said down, the next up: {said}"),
            });
        }
        again
    }

    /// The link went away: attribute it and keep the frames.
    fn on_drop(&mut self, last_good: f64, up_at: f64, node: bool, server: bool, why: &str) {
        self.drops += 1;
        let n = self.drops;
        let cfg = self.ctx.cfg().clone();
        let analyse = n <= EVIDENCE_FIRST || n.is_multiple_of(EVIDENCE_EVERY);
        let (hangup, mut detail, evidence) = if analyse {
            // Pod clock for the capture: approximate the last good sample by
            // its age on the host clock, with two seconds' margin.
            let age = now_f64() - last_good;
            match pcap::pod_now(&self.ctx.rig)
                .and_then(|now| recent_frames(&self.ctx.rig, now - age - 2.0).map(|f| (now, f)))
            {
                Ok((pod_now, frames)) => {
                    let frames = server_view(frames, cfg.topology);
                    let (side, account) =
                        hangup_side(&frames, cfg.node_iax.port(), cfg.server_iax.port());
                    let rel = format!("drops/{n:03}-frames.txt");
                    let path = self.opts.dir.join(&rel);
                    let tail = &frames[frames.len().saturating_sub(80)..];
                    let t0 = pod_now - age;
                    let text = format!(
                        "# drop {n}: IAX2 full frames from 2 s before the last good sample \
                         (t = 0) to now, pod clock{}\n# {account}\n{}",
                        if cfg.topology == Topology::Nat {
                            "; outside leg only (router <-> astar-server)"
                        } else {
                            ""
                        },
                        pcap::render(tail, t0, |p| common::port_name(&self.ctx, p))
                    );
                    let saved = fs::create_dir_all(self.opts.dir.join("drops"))
                        .and_then(|()| fs::write(&path, text))
                        .is_ok();
                    (side, account, saved.then_some(rel))
                }
                Err(e) => (Side::Unknown, format!("capture unreadable: {e}"), None),
            }
        } else {
            (
                Side::Unknown,
                format!(
                    "not analysed (drop {n}; evidence is kept for the first {EVIDENCE_FIRST} and every {EVIDENCE_EVERY}th)"
                ),
                None,
            )
        };
        detail = format!("{why} | {detail}");
        if cfg.topology == Topology::Nat
            && let Ok(ct) = nat_conntrack(&self.ctx.rig)
        {
            use std::fmt::Write as _;
            let flows = ct.lines().filter(|l| l.starts_with("udp")).count();
            let _ = write!(detail, " | router conntrack: {flows} UDP flow(s)");
        }
        self.emit(&Event::LinkDown {
            t: now_f64(),
            last_good,
            held_secs: (last_good - up_at).max(0.0),
            first_side: first_side(node, server),
            hangup,
            detail,
            evidence,
        });
    }

    /// Hold, sample, and re-link until `deadline` or Ctrl-C. Returns the
    /// stop reason.
    fn run(&mut self, deadline: Instant) -> String {
        let mut leg: Option<(ServerLeg, f64)> = None;
        let mut last_good = 0.0;
        // Consecutive samples on which a side could not be asked at all.
        let mut blind = 0u32;
        let mut next = Instant::now();
        loop {
            if self.stopped() {
                return "interrupted".into();
            }
            if Instant::now() >= deadline {
                return "duration".into();
            }
            if leg.is_none() {
                let Some(l) = self.dial() else {
                    self.nap(self.opts.sample);
                    continue;
                };
                last_good = now_f64();
                leg = Some((l, last_good));
                next = Instant::now() + self.opts.sample;
            }
            self.nap(next.saturating_duration_since(Instant::now()));
            if self.stopped() {
                return "interrupted".into();
            }
            next += self.opts.sample;
            let Some((l, up_at)) = leg.clone() else {
                continue;
            };
            let (node, server, detail) = match self.look_twice(&l) {
                Ok(seen) => {
                    blind = 0;
                    seen
                }
                // Neither side could be asked: not a drop, unless it lasts.
                Err(e) if blind < BLIND_SAMPLES_LIMIT => {
                    blind += 1;
                    self.emit(&Event::Note {
                        t: now_f64(),
                        text: format!("sample skipped ({blind}/{BLIND_SAMPLES_LIMIT}): {e}"),
                    });
                    continue;
                }
                Err(e) => {
                    blind = 0;
                    (
                        false,
                        false,
                        Some(format!("no answer for {BLIND_SAMPLES_LIMIT} samples: {e}")),
                    )
                }
            };
            let t = now_f64();
            self.emit(&Event::Sample {
                t,
                node,
                server,
                detail: detail.clone(),
            });
            if node && server {
                last_good = t;
                continue;
            }
            self.on_drop(last_good, up_at, node, server, &detail.unwrap_or_default());
            leg = None;
            if !self.opts.relink {
                return "first_drop".into();
            }
        }
    }
}

fn stamp() -> String {
    rfc3339_utc(unix_secs(now_f64()))
}

/// Run a soak against a rig that is already up and ready. Appends to (or
/// starts) `soak.jsonl` and writes `summary.json` when it stops.
pub fn soak(
    cfg: &RigConfig,
    opts: &SoakOptions,
    stop: &AtomicBool,
    git_sha: Option<String>,
) -> io::Result<SoakSummary> {
    let jsonl = opts.dir.join("soak.jsonl");
    let prior = if opts.resume {
        read_events(&jsonl)?
    } else {
        Vec::new()
    };
    let mut log = EventLog::open(&jsonl)?;
    let rig = Rig::new(cfg.clone());
    let remaining = if opts.resume {
        let before = summarize(&prior);
        log.push(&Event::Resume { t: now_f64() })?;
        Duration::from_secs_f64((before.planned_secs - before.covered_secs).max(0.0))
    } else {
        let conntrack = (cfg.topology == Topology::Nat)
            .then(|| nat_timeouts(&rig).ok())
            .flatten()
            .map(|s| s.trim().to_string());
        log.push(&Event::Start {
            t: now_f64(),
            run_id: opts.run_id.clone(),
            topology: cfg.topology.to_string(),
            dialer: opts.dialer,
            planned_secs: opts.duration.as_secs_f64(),
            sample_secs: opts.sample.as_secs_f64(),
            udp_timeout: opts.udp_timeout,
            git_sha,
            conntrack,
        })?;
        opts.duration
    };
    let tuning = Tuning {
        hold: opts.duration,
        hold_poll: opts.sample,
        ..Tuning::default()
    };
    let mut ctx = Ctx::new(rig, tuning, opts.dir.clone());
    ctx.started_pod = pcap::pod_now(&ctx.rig).ok();
    let mut s = Soaker {
        ctx,
        opts,
        log,
        drops: summarize(&prior).drops,
        stop,
    };
    let reason = s.run(Instant::now() + remaining);
    // Leave the link as we found it: down.
    let _ = common::clean_start(&s.ctx);
    s.emit(&Event::Stop {
        t: now_f64(),
        reason,
    });
    write_summary(&opts.dir)
}

/// Recompute `summary.json` from `soak.jsonl` in `dir`.
pub fn write_summary(dir: &Path) -> io::Result<SoakSummary> {
    let events = read_events(&dir.join("soak.jsonl"))?;
    let s = summarize(&events);
    let body = serde_json::to_vec_pretty(&s).map_err(io::Error::other)?;
    fs::write(dir.join("summary.json"), body)?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(t: f64, src_port: u16, sub: u32, text: &str) -> IaxFrame {
        IaxFrame {
            t,
            src: "127.0.0.1".into(),
            dst: "127.0.0.1".into(),
            src_port,
            dst_port: if src_port == 4569 { 4570 } else { 4569 },
            src_call: 7,
            dst_call: 1,
            ts: 0,
            oseq: 0,
            iseq: 0,
            ftype: pcap::ftype::IAX,
            sub,
            text: text.into(),
            ies: Vec::new(),
        }
    }

    fn start(t: f64) -> Event {
        Event::Start {
            t,
            run_id: "R".into(),
            topology: "nat".into(),
            dialer: Dialer::Node,
            planned_secs: 100.0,
            sample_secs: 5.0,
            udp_timeout: Some(5),
            git_sha: None,
            conntrack: None,
        }
    }

    fn sample(t: f64, up: bool) -> Event {
        Event::Sample {
            t,
            node: up,
            server: up,
            detail: None,
        }
    }

    #[test]
    fn nat_yaml_pods_parse_and_stay_internal() {
        let yaml = include_str!("../../../harness/nodebench/nat.yaml");
        let pods: Vec<KubePod> = split_docs(yaml)
            .iter()
            .map(|d| parse_pod(d).unwrap())
            .collect();
        let names: Vec<&str> = pods.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "nodebench-nat-server",
                "nodebench-nat-router",
                "nodebench-nat-node"
            ]
        );
        // The router straddles both networks, inside first (its eth0).
        assert_eq!(
            pods[1].networks,
            [
                "nodebench-nat-in:ip=10.89.71.254",
                "nodebench-nat-out:ip=10.89.72.254"
            ]
        );
        assert!(pods[0].networks[0].ends_with(&format!("ip={NAT_SERVER_ADDR}")));
        // Every published port is on the host's loopback.
        for line in yaml.lines().filter(|l| l.contains("hostPort")) {
            assert!(line.contains("hostIP: 127.0.0.1"), "{line}");
        }
        // The node is pointed at astar-server across the NAT.
        assert!(yaml.contains(&format!("radio@{NAT_SERVER_ADDR}:4570/1998")));
    }

    #[test]
    fn nat_yaml_renders_with_nat_vars() {
        let cfg = nat_rig_config(RigConfig::new(PathBuf::from("/repo")), Some(7));
        let yaml = include_str!("../../../harness/nodebench/nat.yaml");
        let r = crate::rig::render_kube(yaml, &cfg.kube_vars).unwrap();
        assert!(r.contains("nf_conntrack_udp_timeout, value: \"7\""));
        assert!(r.contains("nf_conntrack_udp_timeout_stream, value: \"7\""));
        assert!(cfg.kube_file().ends_with("nat.yaml"));
        assert_eq!(cfg.server_url, "http://127.0.0.1:38730");
        let d = nat_rig_config(RigConfig::new(PathBuf::from("/repo")), None);
        assert_eq!(d.kube_vars["UDP_TIMEOUT"], "30");
    }

    #[test]
    fn foreign_network_is_refused() {
        let doc = "kind: Pod\nmetadata:\n  name: x\n  annotations:\n    nodebench.astar/networks: \"podman:ip=10.88.0.9\"\n";
        assert!(parse_pod(doc).unwrap_err().contains("not one of"));
        let none = "kind: Pod\nmetadata:\n  name: y\n";
        assert!(parse_pod(none).unwrap_err().contains("annotation"));
    }

    #[test]
    fn split_drops_comment_only_docs() {
        let d = split_docs("# header\n---\nkind: Pod\n---\n\n");
        assert_eq!(d, ["kind: Pod\n"]);
    }

    #[test]
    fn hangup_attribution() {
        let f = [
            frame(1.0, 4570, iax::ACK, ""),
            frame(2.0, 4569, iax::HANGUP, "Normal Clearing"),
            frame(2.1, 4570, iax::HANGUP, ""),
        ];
        let (side, why) = hangup_side(&f, 4569, 4570);
        assert_eq!(side, Side::Node);
        assert!(why.contains("Normal Clearing"), "{why}");
        let (side, _) = hangup_side(&f[2..], 4569, 4570);
        assert_eq!(side, Side::Server);
        let (side, why) = hangup_side(&[frame(1.0, 4570, 10, "")], 4569, 4570);
        assert_eq!(side, Side::None);
        assert!(why.contains("INVAL"));
    }

    #[test]
    fn first_side_from_samples() {
        assert_eq!(first_side(true, false), Side::Server);
        assert_eq!(first_side(false, true), Side::Node);
        assert_eq!(first_side(false, false), Side::Both);
    }

    #[test]
    fn summary_of_a_clean_soak() {
        let ev = vec![
            start(1000.0),
            Event::LinkUp {
                t: 1001.0,
                setup_secs: 1.0,
                leg: "inbound call 1".into(),
            },
            sample(1006.0, true),
            sample(1011.0, true),
            Event::Stop {
                t: 1012.0,
                reason: "duration".into(),
            },
        ];
        let s = summarize(&ev);
        assert_eq!(s.drops, 0);
        assert_eq!(s.samples, 2);
        assert!((s.covered_secs - 12.0).abs() < 1e-9);
        assert_eq!(s.links.len(), 1);
        assert_eq!(s.links[0].ended, "stopped");
        assert!((s.links[0].secs - 11.0).abs() < 1e-9);
        assert!(s.clean());
        assert_eq!(s.stop_reason, "duration");
    }

    #[test]
    fn summary_counts_drops_and_resumes() {
        let ev = vec![
            start(0.0),
            Event::LinkUp {
                t: 1.0,
                setup_secs: 1.0,
                leg: "x".into(),
            },
            sample(6.0, true),
            sample(11.0, false),
            Event::LinkDown {
                t: 11.5,
                last_good: 6.0,
                held_secs: 5.0,
                first_side: Side::Server,
                hangup: Side::None,
                detail: String::new(),
                evidence: None,
            },
            Event::SetupFailed {
                t: 13.0,
                error: "no".into(),
            },
            Event::Stop {
                t: 20.0,
                reason: "interrupted".into(),
            },
            Event::Resume { t: 100.0 },
            Event::LinkUp {
                t: 101.0,
                setup_secs: 1.0,
                leg: "y".into(),
            },
            sample(130.0, true),
        ];
        let s = summarize(&ev);
        assert_eq!(s.drops, 1);
        assert_eq!(s.setup_failures, 1);
        assert_eq!(s.segments, 2);
        // 20 s, then 30 s of the second (still running) segment.
        assert!((s.covered_secs - 50.0).abs() < 1e-9, "{}", s.covered_secs);
        assert_eq!(s.hangup_by["none"], 1);
        assert_eq!(s.first_side_by["server"], 1);
        assert_eq!(s.links.len(), 2);
        assert!((s.links[0].secs - 5.0).abs() < 1e-9);
        assert!((s.links[1].secs - 29.0).abs() < 1e-9);
        assert_eq!(s.stop_reason, "running");
        assert!(!s.clean());
    }

    #[test]
    fn events_round_trip_through_jsonl() {
        let dir = std::env::temp_dir().join(format!("nb-soak-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("soak.jsonl");
        let mut log = EventLog::open(&path).unwrap();
        log.push(&start(5.0)).unwrap();
        log.push(&sample(10.0, true)).unwrap();
        drop(log);
        // A torn last line (a crash mid-write) is skipped.
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"kind\":\"sam")
            .unwrap();
        let ev = read_events(&path).unwrap();
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[1], sample(10.0, true));
        let s = write_summary(&dir).unwrap();
        assert_eq!(s.samples, 1);
        assert!(dir.join("summary.json").is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn durations_read_well() {
        assert_eq!(fmt_secs(75.0), "75.0 s");
        assert_eq!(fmt_secs(725.0), "12 m 05 s");
        assert_eq!(fmt_secs(11_040.0), "3 h 04 m");
    }
}
