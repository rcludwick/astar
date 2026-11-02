// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! What the quick-tier scenarios share: link setup from either side, the
//! "both sides agree" probes, the config overlays, the probes onto the
//! node's radio and astar-server's bridge, and the tone checks.
//!
//! # Which side is which in `/status`
//!
//! A link astar-server dialled is in `links[]` under the node's number. A
//! link the ASL3 node dialled *in* is only an `Active` entry in `calls[]`,
//! with `node: ""` (RIG.md, "a quirk for scenario authors"). So a node-dialled
//! link is tracked by the call id that appeared when the node dialled
//! ([`ServerLeg::Call`]), never by guessing which anonymous call it is.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use super::{Check, Ctx, Outcome, Probe, wait_until};
use crate::audio::{self, ToneReport};
use crate::node::parse::{LinkConnState, RptLinkMode};
use crate::node::{Ilink, NodeError};
use crate::pcap::{self, IaxFrame, iax};
use crate::probe::iax::{IaxProbe, IaxProbeConfig};
use crate::probe::usrp::UsrpProbe;
use crate::probe::{RxFrame, TxReport};
use crate::server::{LinkAction, ServerEvent, Status};

/// Poll interval while waiting for a link edge.
pub const POLL: Duration = Duration::from_millis(250);
/// How long a clean start may take (ASL3 keeps a server-dropped link for
/// about 8 s; RIG.md finding 3).
pub const CLEAN_TIMEOUT: Duration = Duration::from_secs(30);
/// Both sides must agree a link is gone within this long after either side
/// drops it. The node-side `ilink 1` clears both in under 3 s.
pub const DISCONNECT_DEADLINE: Duration = Duration::from_secs(5);
/// How long a refused link gets to (wrongly) come up.
pub const REFUSE_WINDOW: Duration = Duration::from_secs(8);

/// The bench's loopback-only test secret (iax.conf `[1998]`, pod.yaml).
pub const TEST_SECRET: &str = "nodebench-loopback-only";

// ---- astar-server's side ----------------------------------------------------

/// The astar-server end of a node link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerLeg {
    /// astar-server dialled it: `links[]` under this node number.
    Link(String),
    /// The node dialled in: this `calls[]` id.
    Call(u64),
}

impl std::fmt::Display for ServerLeg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Link(n) => write!(f, "link {n}"),
            Self::Call(id) => write!(f, "inbound call {id}"),
        }
    }
}

/// Ids of the `Active` calls in a snapshot.
#[must_use]
pub fn active_call_ids(st: &Status) -> Vec<u64> {
    st.calls
        .iter()
        .filter(|c| c.state == "Active")
        .filter_map(|c| c.id.as_u64())
        .collect()
}

/// The `calls[]` entry with this id.
#[must_use]
pub fn call_by_id(st: &Status, id: u64) -> Option<&crate::server::CallInfo> {
    st.calls.iter().find(|c| c.id.as_u64() == Some(id))
}

/// `Ok` when the leg is up in `st`.
pub fn server_leg_up(st: &Status, leg: &ServerLeg) -> Probe {
    match leg {
        ServerLeg::Link(n) => match st.link(n) {
            Some(l) if l.is_up() => Ok(()),
            Some(l) => Err(format!("server: link {n} is {}", l.state)),
            None => Err(format!("server: no link {n}")),
        },
        ServerLeg::Call(id) => match call_by_id(st, *id) {
            Some(c) if c.state == "Active" => Ok(()),
            Some(c) => Err(format!("server: call {id} is {}", c.state)),
            None => Err(format!("server: call {id} is gone")),
        },
    }
}

/// The negotiated codec on the server's end of `leg`, normalised
/// (`ulaw`, `alaw`, `slin`, `slin16`).
#[must_use]
pub fn server_leg_codec(st: &Status, leg: &ServerLeg) -> Option<String> {
    match leg {
        ServerLeg::Call(id) => call_by_id(st, *id)
            .and_then(crate::server::CallInfo::format)
            .map(|f| norm_codec(&f)),
        ServerLeg::Link(n) => {
            // A dialled link's call carries the node number.
            st.calls
                .iter()
                .find(|c| c.node == *n)
                .and_then(crate::server::CallInfo::format)
                .map(|f| norm_codec(&f))
        }
    }
}

/// `G711U` / `Slin16` / `ulaw` / `slin` → one spelling.
#[must_use]
pub fn norm_codec(s: &str) -> String {
    match s.trim().trim_matches('"').to_ascii_lowercase().as_str() {
        "g711u" | "ulaw" | "mulaw" => "ulaw".into(),
        "g711a" | "alaw" => "alaw".into(),
        "slin" | "slinear" => "slin".into(),
        "slin16" | "slinear16" => "slin16".into(),
        other => other.into(),
    }
}

// ---- the node's side --------------------------------------------------------

/// `Ok` when the node has an established link to `peer` (optionally in `mode`).
pub fn node_link_up(ctx: &Ctx, peer: &str, mode: Option<RptLinkMode>) -> Probe {
    let l = ctx
        .node
        .link_to(peer)
        .map_err(|e| format!("node: {e}"))?
        .ok_or_else(|| format!("node: no link to {peer}"))?;
    if l.state != LinkConnState::Established {
        return Err(format!("node: link to {peer} is {:?}", l.state));
    }
    if let Some(want) = mode {
        let nodes = ctx.node.rpt_nodes().map_err(|e| format!("node: {e}"))?;
        match nodes.get(peer) {
            Some(n) if n.mode == want => {}
            Some(n) => {
                return Err(format!(
                    "node: link to {peer} is {:?}, want {want:?}",
                    n.mode
                ));
            }
            None => return Err(format!("node: rpt nodes does not list {peer}")),
        }
    }
    Ok(())
}

/// Both ends up.
pub fn both_up(ctx: &Ctx, leg: &ServerLeg, mode: Option<RptLinkMode>) -> Probe {
    node_link_up(ctx, &ctx.cfg().server_number, mode)?;
    let st = ctx.server.status().map_err(|e| format!("server: {e}"))?;
    server_leg_up(&st, leg)
}

/// Neither side has any link between the two, and astar-server has no
/// active call at all (a leftover probe call would skew the next scenario).
pub fn all_clear(ctx: &Ctx) -> Probe {
    let cfg = ctx.cfg();
    if let Some(l) = ctx
        .node
        .link_to(&cfg.server_number)
        .map_err(|e| format!("node: {e}"))?
    {
        return Err(format!("node still lists {} ({:?})", l.node, l.state));
    }
    let st = ctx.server.status().map_err(|e| format!("server: {e}"))?;
    if let Some(l) = st.links.first() {
        return Err(format!("server still has link {} ({})", l.node, l.state));
    }
    let active = active_call_ids(&st);
    if !active.is_empty() {
        return Err(format!("server still has active calls {active:?}"));
    }
    // Asterisk keeps a torn-down call (`(None)` in the list) until its
    // HANGUP is acknowledged, and retransmits that HANGUP meanwhile. A new
    // link started before then can be hit by the retransmission (see
    // `relink_after_drop`), so a clean start waits for the list to empty.
    let ch = ctx.node.iax2_channels().map_err(|e| format!("node: {e}"))?;
    if !ch.channels.is_empty() {
        return Err(format!(
            "node still has {} IAX channel(s): {:?}",
            ch.channels.len(),
            ch.channels
                .iter()
                .map(|c| (c.channel.clone(), c.local_call, c.remote_call))
                .collect::<Vec<_>>()
        ));
    }
    Ok(())
}

/// The link is gone from both sides (other calls may remain).
pub fn leg_down(ctx: &Ctx, leg: &ServerLeg) -> Probe {
    let cfg = ctx.cfg();
    if let Some(l) = ctx
        .node
        .link_to(&cfg.server_number)
        .map_err(|e| format!("node: {e}"))?
    {
        return Err(format!("node still lists {} ({:?})", l.node, l.state));
    }
    let st = ctx.server.status().map_err(|e| format!("server: {e}"))?;
    match server_leg_up(&st, leg) {
        Ok(()) => Err(format!("server still has {leg}")),
        Err(_) => Ok(()),
    }
}

/// Set the node's run-time caller-ID workaround (extensions.conf; off unless
/// a scenario turns it on, and every clean start turns it off again).
pub fn set_fill_callerid(ctx: &Ctx, on: bool) -> Result<(), NodeError> {
    ctx.node
        .rx(&format!(
            "dialplan set global NODEBENCH_FILL_CALLERID {}",
            u8::from(on)
        ))
        .map(|_| ())
}

/// Drop everything either side holds and wait until both agree.
pub fn clean_start(ctx: &Ctx) -> Result<(), String> {
    let _ = set_fill_callerid(ctx, false);
    let _ = ctx.node.ilink(Ilink::DisconnectAll, "");
    if let Ok(st) = ctx.server.status() {
        for l in &st.links {
            let _ = ctx.server.link(LinkAction::Disconnect, &l.node, None);
        }
    }
    wait_until(CLEAN_TIMEOUT, POLL, || all_clear(ctx))
        .map(|_| ())
        .map_err(|e| format!("not clean after {:.1?}: {}", e.waited, e.last))
}

/// `rpt cmd ilink <op> <server>` and wait until both sides are up. Returns
/// the server's end and how long it took.
pub fn node_dials_server(
    ctx: &Ctx,
    op: Ilink,
    mode: Option<RptLinkMode>,
) -> Result<(ServerLeg, Duration), String> {
    let server_no = ctx.cfg().server_number.clone();
    let before = ctx
        .server
        .status()
        .map(|s| active_call_ids(&s))
        .map_err(|e| format!("server: {e}"))?;
    let t0 = Instant::now();
    let mark = pcap::pod_now(&ctx.rig).ok();
    ctx.node
        .ilink(op, &server_no)
        .map_err(|e| format!("ilink {} refused: {e}", op.code()))?;
    let mut leg = None;
    let r = wait_until(ctx.tuning.setup_timeout, POLL, || {
        node_link_up(ctx, &server_no, mode)?;
        let st = ctx.server.status().map_err(|e| format!("server: {e}"))?;
        let new: Vec<u64> = active_call_ids(&st)
            .into_iter()
            .filter(|id| !before.contains(id))
            .collect();
        match new.as_slice() {
            [id] => {
                leg = Some(ServerLeg::Call(*id));
                Ok(())
            }
            [] => Err("server: no new active call".into()),
            many => Err(format!("server: several new calls {many:?}")),
        }
    });
    match (r, leg) {
        (Ok(_), Some(l)) => Ok((l, t0.elapsed())),
        (Err(e), _) => {
            let why = pcap::full_frames(&ctx.rig, mark)
                .ok()
                .and_then(|f| explain_token_reject(&f))
                .map(|w| format!(" [{w}]"))
                .unwrap_or_default();
            Err(format!(
                "not up within {:?}: {}{why}",
                ctx.tuning.setup_timeout, e.last
            ))
        }
        (Ok(_), None) => Err("internal: up without a leg".into()),
    }
}

/// `POST /link` at the node's explicit address (never `AllStar` DNS) and wait
/// until both sides are up.
pub fn server_dials_node(
    ctx: &Ctx,
    action: LinkAction,
    mode: Option<RptLinkMode>,
) -> Result<(ServerLeg, Duration), String> {
    let cfg = ctx.cfg();
    let node_no = cfg.node_number.clone();
    let addr = cfg.node_iax.to_string();
    let t0 = Instant::now();
    ctx.server
        .link(action, &node_no, Some(&addr))
        .map_err(|e| format!("POST /link refused: {e}"))?;
    let leg = ServerLeg::Link(node_no);
    wait_until(ctx.tuning.setup_timeout, POLL, || both_up(ctx, &leg, mode))
        .map(|_| (leg.clone(), t0.elapsed()))
        .map_err(|e| format!("not up within {:?}: {}", ctx.tuning.setup_timeout, e.last))
}

/// Hold `leg` for the hold window; on a break, mark the drop for the
/// failure artifacts.
pub fn hold_link(
    ctx: &mut Ctx,
    o: &mut Outcome,
    leg: &ServerLeg,
    mode: Option<RptLinkMode>,
) -> bool {
    let (hold, poll) = (ctx.tuning.hold, ctx.tuning.hold_poll);
    let h = super::hold_for(hold, poll, || both_up(ctx, leg, mode));
    if !h.held_all() {
        ctx.mark_drop();
    }
    o.check(h.to_check(&format!("link holds {} s", hold.as_secs())))
}

// ---- what the wire says -----------------------------------------------------

/// Name the rig's ports in frame tables.
#[must_use]
pub fn port_name(ctx: &Ctx, port: u16) -> String {
    let cfg = ctx.cfg();
    if port == cfg.node_iax.port() {
        format!("asl3:{port}")
    } else if port == cfg.server_iax.port() {
        format!("astar:{port}")
    } else {
        format!(":{port}")
    }
}

/// Explain the first HANGUP or REJECT since `since` (pod clock): who sent
/// it, its cause, and how long after the call's NEW / ACCEPT it came.
#[must_use]
pub fn explain_teardown(ctx: &Ctx, frames: &[IaxFrame]) -> Option<String> {
    let end = frames
        .iter()
        .find(|f| f.is_iax(iax::HANGUP) || f.is_iax(iax::REJECT))?;
    let after = |sub: u32| {
        frames
            .iter()
            .rev()
            .find(|f| f.t <= end.t && f.is_iax(sub))
            .map(|f| {
                format!(
                    "{:.1} ms after {}",
                    (end.t - f.t) * 1000.0,
                    pcap::iax_name(sub).unwrap_or("?")
                )
            })
    };
    let mut s = format!(
        "{} sent {} (cause {:?}) to {}",
        port_name(ctx, end.src_port),
        end.label(),
        end.text,
        port_name(ctx, end.dst_port)
    );
    for t in [after(iax::ACCEPT), after(iax::NEW)].into_iter().flatten() {
        s.push_str("; ");
        s.push_str(&t);
    }
    Some(s)
}

/// If astar-server rejected an echoed call token, say why from the bytes:
/// the token it issued, where its NUL bytes are, and how much of it came
/// back. Asterisk treats a call token as a C string, so a token with a NUL
/// byte comes back truncated at it.
#[must_use]
pub fn explain_token_reject(frames: &[IaxFrame]) -> Option<String> {
    let rej = frames
        .iter()
        .find(|f| f.is_iax(iax::REJECT) && f.text.contains("Call Token"))?;
    let issued = frames.iter().rev().find(|f| {
        f.t <= rej.t && f.is_iax(40) && f.dst_call == rej.dst_call && f.src_port == rej.src_port
    })?;
    let token = issued.ie(pcap::ie::CALLTOKEN)?;
    let echoed = frames
        .iter()
        .find(|f| {
            f.t >= issued.t && f.t <= rej.t && f.is_iax(iax::NEW) && f.src_call == rej.dst_call
        })
        .and_then(|f| f.ie(pcap::ie::CALLTOKEN).map(<[u8]>::len));
    let nuls: Vec<usize> = token
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == 0)
        .map(|(i, _)| i)
        .collect();
    let hex: String = token.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    });
    Some(format!(
        "astar-server issued a {}-byte call token {hex} with NUL byte(s) at {nuls:?}; the node echoed {} byte(s); astar-server answered REJECT {:?}",
        token.len(),
        echoed.map_or_else(|| "no".into(), |n| n.to_string()),
        rej.text
    ))
}

/// Full frames since the scenario started, as a note-friendly result.
pub fn frames_since_start(ctx: &Ctx) -> Result<Vec<IaxFrame>, String> {
    pcap::full_frames(&ctx.rig, ctx.started_pod).map_err(|e| format!("pcap: {e}"))
}

/// Save every full frame since `since` for one matrix cell that failed, as
/// `<dir>/<slug>.frames.txt`.
pub fn save_cell_frames(ctx: &Ctx, o: &mut Outcome, tag: &str, since: Option<f64>) {
    let slug: String = tag
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let Ok(frames) = pcap::full_frames(&ctx.rig, since) else {
        return;
    };
    if let Some(why) = explain_token_reject(&frames) {
        o.note(format!("{tag}: {why}"));
    }
    let t0 = since.unwrap_or_else(|| frames.first().map_or(0.0, |f| f.t));
    let _ = std::fs::create_dir_all(&ctx.dir);
    let path = ctx.dir.join(format!("{slug}.frames.txt"));
    if std::fs::write(&path, pcap::render(&frames, t0, |p| port_name(ctx, p))).is_ok() {
        o.artifacts.push(path);
    }
}

// ---- overlays -------------------------------------------------------------------

/// A change to the ASL3 node's config, applied by restarting only the node
/// container.
#[derive(Debug, Clone, Default)]
pub struct NodeOverlay {
    /// Codec list, offer order (as `NODEBENCH_CODECS`): `"ulaw"`.
    pub codecs: Option<String>,
    /// A new rpt.conf `[nodes]` value for astar-server's node, e.g.
    /// `1999:<secret>@127.0.0.1:4570/1998,NONE` to dial in with credentials.
    pub server_node_entry: Option<String>,
}

const IAX_CONF_IN: &str = "/etc/asterisk/iax.conf.in";
const RPT_CONF: &str = "/etc/asterisk/rpt.conf";

/// The `iax.conf.in` text with every `@CODECS@` line rendered as the
/// entrypoint would render `codecs`.
#[must_use]
pub fn render_codecs(iax_conf_in: &str, codecs: &str) -> String {
    let mut block = format!("; codecs from the nodebench overlay: {codecs}\ndisallow = all");
    for c in codecs.split(',').map(str::trim).filter(|c| !c.is_empty()) {
        block.push_str("\nallow = ");
        block.push_str(c);
    }
    iax_conf_in
        .lines()
        .map(|l| {
            if l.trim() == "@CODECS@" {
                block.as_str()
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// rpt.conf with the `[nodes]` entry for `node` replaced by `value`.
#[must_use]
pub fn replace_node_entry(rpt_conf: &str, node: &str, value: &str) -> String {
    let prefix = format!("{node} =");
    let mut in_nodes = false;
    let mut out = Vec::new();
    for l in rpt_conf.lines() {
        let t = l.trim();
        if t.starts_with('[') {
            in_nodes = t == "[nodes]";
        }
        if in_nodes && (t.starts_with(&prefix) || t.starts_with(&format!("{node}="))) {
            out.push(format!("{node} = {value}"));
        } else {
            out.push(l.to_string());
        }
    }
    out.join("\n") + "\n"
}

fn wait_node_ready(ctx: &Ctx) -> Result<(), String> {
    wait_until(
        Duration::from_secs(60),
        Duration::from_millis(500),
        || match ctx.node.node_loaded() {
            Ok(true) => Ok(()),
            Ok(false) => Err("app_rpt has not loaded the node".into()),
            Err(e) => Err(e.to_string()),
        },
    )
    .map(|_| ())
    .map_err(|e| format!("node not ready after restart: {}", e.last))
}

fn wait_server_ready(ctx: &Ctx) -> Result<(), String> {
    wait_until(
        Duration::from_secs(60),
        Duration::from_millis(500),
        || match ctx.server.status() {
            Ok(s) if s.listening => Ok(()),
            Ok(_) => Err("not listening".into()),
            Err(e) => Err(e.to_string()),
        },
    )
    .map(|_| ())
    .map_err(|e| format!("astar-server not ready after restart: {}", e.last))
}

/// Restart the node container and wait for `app_rpt`.
pub fn restart_node(ctx: &Ctx) -> Result<(), String> {
    let c = ctx.cfg().node_container.clone();
    ctx.rig.restart_container(&c).map_err(|e| e.to_string())?;
    wait_node_ready(ctx)
}

/// Restart astar-server and wait for it to listen.
pub fn restart_server(ctx: &Ctx) -> Result<(), String> {
    let c = ctx.cfg().server_container.clone();
    ctx.rig.restart_container(&c).map_err(|e| e.to_string())?;
    wait_server_ready(ctx)
}

/// Apply `ov` to the node, run `f`, and put the node back as it was, always.
/// A failed restore is a failed check (the next scenario would be wrong).
pub fn with_node_overlay(
    ctx: &mut Ctx,
    o: &mut Outcome,
    ov: &NodeOverlay,
    f: impl FnOnce(&mut Ctx, &mut Outcome),
) {
    let node_ctr = ctx.cfg().node_container.clone();
    let server_no = ctx.cfg().server_number.clone();
    let scratch = ctx.dir.join("overlay");
    let orig_iax = ctx.rig.read_file(&node_ctr, IAX_CONF_IN);
    let orig_rpt = ctx.rig.read_file(&node_ctr, RPT_CONF);
    let (Ok(orig_iax), Ok(orig_rpt)) = (orig_iax, orig_rpt) else {
        o.check(Check::fail(
            "node overlay applied",
            "cannot read the node's config",
        ));
        return;
    };
    let mut applied = Ok(());
    if let Some(c) = &ov.codecs {
        applied = ctx
            .rig
            .write_file(
                &node_ctr,
                IAX_CONF_IN,
                &render_codecs(&orig_iax, c),
                &scratch,
            )
            .map_err(|e| e.to_string());
    }
    if let (Ok(()), Some(entry)) = (&applied, &ov.server_node_entry) {
        applied = ctx
            .rig
            .write_file(
                &node_ctr,
                RPT_CONF,
                &replace_node_entry(&orig_rpt, &server_no, entry),
                &scratch,
            )
            .map_err(|e| e.to_string());
    }
    let applied = applied.and_then(|()| restart_node(ctx));
    match applied {
        Ok(()) => {
            let _ = ctx.node.enable_debug();
            f(ctx, o);
        }
        Err(e) => {
            o.check(Check::fail("node overlay applied", e));
        }
    }
    let restored = ctx
        .rig
        .write_file(&node_ctr, IAX_CONF_IN, &orig_iax, &scratch)
        .and_then(|()| ctx.rig.write_file(&node_ctr, RPT_CONF, &orig_rpt, &scratch))
        .map_err(|e| e.to_string())
        .and_then(|()| restart_node(ctx));
    if let Err(e) = restored {
        o.check(Check::fail("node config restored", e));
    } else {
        let _ = ctx.node.enable_debug();
    }
}

/// astar-server's active config file inside its container.
fn server_config_path(ctx: &Ctx) -> String {
    let name = ctx
        .cfg()
        .kube_vars
        .get("CONFIG")
        .cloned()
        .unwrap_or_else(|| "node.toml".into());
    format!("/etc/nodebench/{name}")
}

/// Edit astar-server's config with `edit`, restart only its container, run
/// `f`, and restore the original (restarting again), always.
pub fn with_server_overlay(
    ctx: &mut Ctx,
    o: &mut Outcome,
    edit: impl FnOnce(&mut toml::Table),
    f: impl FnOnce(&mut Ctx, &mut Outcome),
) {
    let ctr = ctx.cfg().server_container.clone();
    let path = server_config_path(ctx);
    let scratch = ctx.dir.join("overlay");
    let orig = match ctx.rig.read_file(&ctr, &path) {
        Ok(s) => s,
        Err(e) => {
            o.check(Check::fail(
                "server overlay applied",
                format!("read {path}: {e}"),
            ));
            return;
        }
    };
    let mut table: toml::Table = match orig.parse() {
        Ok(t) => t,
        Err(e) => {
            o.check(Check::fail(
                "server overlay applied",
                format!("parse {path}: {e}"),
            ));
            return;
        }
    };
    edit(&mut table);
    let text = format!(
        "# nodebench overlay of {path}; restored after the scenario\n{}",
        toml::to_string(&table).unwrap_or_default()
    );
    let _ = std::fs::create_dir_all(&scratch);
    let _ = std::fs::write(scratch.join("server-overlay.toml"), &text);
    let applied = ctx
        .rig
        .write_file(&ctr, &path, &text, &scratch)
        .map_err(|e| e.to_string())
        .and_then(|()| restart_server(ctx));
    match applied {
        Ok(()) => f(ctx, o),
        Err(e) => {
            o.check(Check::fail("server overlay applied", e));
        }
    }
    let restored = ctx
        .rig
        .write_file(&ctr, &path, &orig, &scratch)
        .map_err(|e| e.to_string())
        .and_then(|()| restart_server(ctx));
    if let Err(e) = restored {
        o.check(Check::fail("server config restored", e));
    }
}

/// Set `[section] key = value` in a config table (creating the section).
pub fn set_key(t: &mut toml::Table, section: &str, key: &str, value: toml::Value) {
    let s = t
        .entry(section.to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let toml::Value::Table(s) = s {
        s.insert(key.to_string(), value);
    }
}

// ---- probes -----------------------------------------------------------------

/// Where the bench meets the node's USRP channel from the host: it receives
/// the node's transmit audio on 127.0.0.1:34001 and sends the radio's audio
/// to 127.0.0.1:32001 (RIG.md, "USRP and the host").
pub const USRP_LOCAL: ([u8; 4], u16) = ([127, 0, 0, 1], 34001);
pub const USRP_REMOTE: ([u8; 4], u16) = ([127, 0, 0, 1], 32001);

/// Bind the USRP probe.
pub fn usrp_probe() -> Result<UsrpProbe, String> {
    UsrpProbe::bind(SocketAddr::from(USRP_LOCAL), SocketAddr::from(USRP_REMOTE))
        .map_err(|e| format!("bind USRP probe on 127.0.0.1:34001: {e}"))
}

/// Wait until the node sends no voice on USRP for a whole 800 ms window.
pub fn wait_node_quiet(usrp: &UsrpProbe, limit: Duration) -> Result<Duration, String> {
    let t0 = std::time::Instant::now();
    while t0.elapsed() < limit {
        let frames = usrp
            .record_any(Duration::from_millis(800))
            .map_err(|e| format!("USRP: {e}"))?;
        if !frames.iter().any(|f| f.keyed && !f.samples.is_empty()) {
            return Ok(t0.elapsed());
        }
    }
    Err(format!("the node was still transmitting after {limit:?}"))
}

/// Why the node may have ignored astar-server's audio: `app_rpt`'s `NEWKEY`
/// handshake. `app_rpt` sends `!NEWKEY1!` on a new link and, if the far end
/// does not send it back within 2 s, stops treating voice frames as "keyed"
/// and waits for `RADIO_KEY` control frames instead (`app_rpt.c`, `newkeytimer`).
pub fn note_newkey(ctx: &Ctx, o: &mut Outcome, since: Option<f64>) {
    let (np, sp) = (ctx.cfg().node_iax.port(), ctx.cfg().server_iax.port());
    if let Ok(frames) = pcap::full_frames(&ctx.rig, ctx.started_pod) {
        let newkey_back = frames
            .iter()
            .filter(|f| f.src_port == sp && f.dst_port == np && f.ftype == pcap::ftype::TEXT)
            .any(|f| f.text.contains("NEWKEY"));
        let keys = frames
            .iter()
            .filter(|f| since.is_none_or(|t| f.t >= t))
            .filter(|f| {
                f.src_port == sp
                    && f.dst_port == np
                    && f.ftype == pcap::ftype::CONTROL
                    && f.sub == 12
            })
            .count();
        o.note(format!(
            "astar-server sent the node !NEWKEY1!: {newkey_back}; RADIO_KEY frames to the node during the tone: {keys}"
        ));
    }
    let node = ctx.cfg().node_container.clone();
    if let Ok(log) = ctx.rig.file_tail(&node, "/var/log/asterisk/full", 2000)
        && let Some(l) = log
            .lines()
            .rev()
            .find(|l| l.contains("newkeytimer expired"))
    {
        o.note(format!("node log: {}", l.trim()));
    }
}

/// Dial the IAX probe into astar-server's bridge as node 1997.
pub fn iax_probe(ctx: &Ctx) -> Result<IaxProbe, String> {
    let cfg = ctx.cfg();
    let pc = IaxProbeConfig::new(cfg.server_iax_host, cfg.server_number.clone());
    IaxProbe::dial(&pc).map_err(|e| format!("probe dial to {}: {e}", cfg.server_iax_host))
}

// ---- tone checks ------------------------------------------------------------

/// Test tone level, sine-referenced dBFS.
pub const TONE_DBFS: f64 = -10.0;
/// How long the bench keys and sends a tone.
pub const TONE_SECS: u64 = 2;
/// Frequency tolerance (design: ±10 Hz).
pub const FREQ_TOL_HZ: f64 = 10.0;
/// Received level within this many dB of the injected level. The gain chain
/// (`app_rpt`, the bridge mix) is not unity by contract, so this is a sanity
/// window, not a calibration.
pub const LEVEL_TOL_DB: f64 = 12.0;
/// Minimum tone-to-everything-else ratio.
pub const MIN_SNR_DB: f64 = 20.0;
/// Key-up to first tone frame at the far end.
pub const MAX_ONSET_MS: f64 = 500.0;
/// Frames missing inside the received tone.
pub const MAX_LOSS: f64 = 0.02;
/// A frame counts as "tone present" above this Goertzel level.
pub const PRESENT_DBFS: f64 = -40.0;
/// Frames (20 ms) skipped after the tone's onset before measuring level and
/// SNR: `app_rpt`'s key-up transient (see [`analyse_tone`]).
pub const HEAD_SKIP_FRAMES: usize = 12;
/// Frames skipped before the tone's last frame (the key-down edge).
pub const TAIL_SKIP_FRAMES: usize = 3;

/// What a tone check measured.
#[derive(Debug, Clone)]
pub struct ToneResult {
    /// `snr_db` here is the per-frame median ([`audio::frame_snr`]).
    pub report: Option<ToneReport>,
    /// Dropout frames inside the measured part of the tone.
    pub dropouts: usize,
    pub onset_ms: Option<f64>,
    pub loss: Option<audio::LossReport>,
    pub frames: usize,
}

/// Analyse `frames` (8 kHz, 20 ms each) received after `tx` for a tone at
/// `freq`; `loss_of` counts lost frames from each frame's sender numbering.
#[must_use]
pub fn analyse_tone(
    frames: &[RxFrame],
    tx: &TxReport,
    freq: f64,
    rate: u32,
    loss_of: impl Fn(&[RxFrame]) -> audio::LossReport,
) -> ToneResult {
    let len = audio::frame_len(rate);
    let voiced: Vec<RxFrame> = frames
        .iter()
        .filter(|f| f.samples.len() == len)
        .cloned()
        .collect();
    let samples = crate::probe::concat(&voiced);
    let Some(on) = audio::onset_of_tone(&samples, rate, freq, PRESENT_DBFS) else {
        return ToneResult {
            report: None,
            dropouts: 0,
            onset_ms: None,
            loss: None,
            frames: voiced.len(),
        };
    };
    let onset_ms = voiced
        .get(on.frame)
        .map(|f| f.at.saturating_duration_since(tx.started).as_secs_f64() * 1000.0);
    // The tone's frames: from the onset to the last frame still carrying it.
    let present = |f: &RxFrame| {
        audio::amplitude_to_dbfs(audio::goertzel_amplitude(&f.samples, rate, freq)) >= PRESENT_DBFS
    };
    let last = voiced.iter().rposition(present).unwrap_or(on.frame);
    let tone_frames = &voiced[on.frame..=last.max(on.frame)];
    let body = crate::probe::concat(tone_frames);
    // Measure the steady state only. At the head, skip the key-up transient:
    // stock `app_rpt` puts a silent frame into its own link audio somewhere
    // in the first ~200 ms after its radio keys (seen in the node's outbound
    // IAX frames while the USRP input was continuous). At the tail, skip the
    // key-down frame, which is only partly tone. Lost frames are the loss
    // check's job, and later dropouts are counted, not scored as noise.
    let head = (len * HEAD_SKIP_FRAMES).min(body.len() / 4);
    let tail = (len * TAIL_SKIP_FRAMES).min(body.len() / 4);
    let steady = &body[head..body.len() - tail];
    let mut report = audio::detect_tone(steady, rate, Some((freq - 200.0, freq + 200.0)));
    // SNR frame by frame: app_rpt's link audio has an occasional silent
    // frame of its own, and one of those would otherwise read as noise.
    let mut dropouts = 0;
    if let Some(rep) = report.as_mut()
        && let Some(fs) = audio::frame_snr(steady, rate, rep.freq_hz, len)
    {
        rep.snr_db = fs.median_db;
        dropouts = fs.dropouts;
    }
    ToneResult {
        report,
        dropouts,
        onset_ms,
        loss: Some(loss_of(tone_frames)),
        frames: voiced.len(),
    }
}

/// Turn a [`ToneResult`] into the design's checks, prefixed with `what`.
pub fn tone_checks(o: &mut Outcome, what: &str, r: &ToneResult, freq: f64) -> bool {
    let Some(rep) = r.report else {
        return o.check(Check::fail(
            format!("{what}: tone heard"),
            format!("no {freq:.0} Hz tone in {} received frames", r.frames),
        ));
    };
    let mut ok = true;
    ok &= o.check(
        Check::expect(
            format!("{what}: frequency"),
            (rep.freq_hz - freq).abs() <= FREQ_TOL_HZ,
            format!(
                "{:.1} Hz (sent {freq:.0} Hz, ±{FREQ_TOL_HZ:.0})",
                rep.freq_hz
            ),
        )
        .measured(rep.freq_hz, "Hz"),
    );
    ok &= o.check(
        Check::expect(
            format!("{what}: level"),
            (rep.level_dbfs - TONE_DBFS).abs() <= LEVEL_TOL_DB,
            format!(
                "{:.1} dBFS (sent {TONE_DBFS:.0}, ±{LEVEL_TOL_DB:.0} dB); SNR {:.1} dB",
                rep.level_dbfs, rep.snr_db
            ),
        )
        .measured(rep.level_dbfs, "dBFS"),
    );
    ok &= o.check(
        Check::expect(
            format!("{what}: SNR"),
            rep.snr_db >= MIN_SNR_DB,
            format!(
                "{:.1} dB median per 20 ms frame (min {MIN_SNR_DB:.0}); {} dropout frame(s)",
                rep.snr_db, r.dropouts
            ),
        )
        .measured(rep.snr_db, "dB"),
    );
    match r.onset_ms {
        Some(ms) => {
            ok &= o.check(
                Check::expect(
                    format!("{what}: onset latency"),
                    ms <= MAX_ONSET_MS,
                    format!("{ms:.0} ms from key-up to first tone frame (max {MAX_ONSET_MS:.0})"),
                )
                .measured(ms, "ms"),
            );
        }
        None => {
            ok &= o.check(Check::fail(
                format!("{what}: onset latency"),
                "no onset frame",
            ));
        }
    }
    if let Some(l) = r.loss {
        ok &= o.check(
            Check::expect(
                format!("{what}: frame loss"),
                l.loss_ratio() <= MAX_LOSS,
                format!(
                    "{} of {} lost ({:.1}%, max {:.0}%), {} gaps, longest {}, {} dup, {} reordered",
                    l.lost,
                    l.expected,
                    l.loss_ratio() * 100.0,
                    MAX_LOSS * 100.0,
                    l.gaps,
                    l.max_gap_frames,
                    l.duplicates,
                    l.reordered
                ),
            )
            .measured(l.loss_ratio() * 100.0, "%"),
        );
    }
    ok
}

/// Loss over USRP frames (the node's own sequence numbers).
#[must_use]
pub fn usrp_loss(frames: &[RxFrame]) -> audio::LossReport {
    let seqs: Vec<u32> = frames.iter().filter_map(|f| f.seq).collect();
    audio::loss_from_sequence(&seqs)
}

/// Loss over IAX frames (the sender's media timestamps, 20 ms apart).
#[must_use]
pub fn iax_loss(frames: &[RxFrame]) -> audio::LossReport {
    let ts: Vec<i64> = frames.iter().filter_map(|f| f.ts_ms).collect();
    audio::loss_from_timestamps(&ts)
}

/// Is a tone at `freq` present anywhere in `frames`?
#[must_use]
pub fn tone_present(frames: &[RxFrame], freq: f64, rate: u32) -> Option<f64> {
    frames
        .iter()
        .filter(|f| f.samples.len() == audio::frame_len(rate))
        .map(|f| audio::amplitude_to_dbfs(audio::goertzel_amplitude(&f.samples, rate, freq)))
        .filter(|l| *l >= PRESENT_DBFS)
        .reduce(f64::max)
}

/// Record astar-server's link and hangup events as notes.
pub fn note_server_events(es: Option<&crate::server::EventStream>, o: &mut Outcome) {
    let Some(es) = es else { return };
    for st in es.drain() {
        match st.event {
            ServerEvent::Link {
                kind, node, reason, ..
            } => o.note(format!(
                "server event at {:.1?}: link {kind} {node}{}",
                st.at,
                reason.map(|r| format!(" ({r})")).unwrap_or_default()
            )),
            ServerEvent::Hangup { reason } => {
                o.note(format!("server event at {:.1?}: hangup ({reason})", st.at));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codecs_render_like_the_entrypoint() {
        let t = "[general]\n@CODECS@\n[radio]\n  @CODECS@\n";
        let r = render_codecs(t, "ulaw");
        assert_eq!(r.matches("disallow = all").count(), 2);
        assert_eq!(r.matches("allow = ulaw").count(), 2);
        assert!(!r.contains("@CODECS@"));
    }

    #[test]
    fn node_entry_replaced_only_in_nodes() {
        let t = "[general]\n1998 = keep\n[nodes]\n1999 = radio@a/1999,NONE\n1998 = radio@b/1998,NONE\n[1999]\nx=1\n";
        let r = replace_node_entry(t, "1998", "1999:s@b/1998,NONE");
        assert!(r.contains("[general]\n1998 = keep"));
        assert!(r.contains("1998 = 1999:s@b/1998,NONE"));
        assert!(r.contains("1999 = radio@a/1999,NONE"));
    }

    #[test]
    fn codec_names_normalise() {
        assert_eq!(norm_codec("G711U"), "ulaw");
        assert_eq!(norm_codec("\"Slin16\""), "slin16");
        assert_eq!(norm_codec("slin"), "slin");
    }

    #[test]
    fn set_key_creates_sections() {
        let mut t: toml::Table = "[listener]\nauth = \"off\"\n".parse().unwrap();
        set_key(
            &mut t,
            "listener",
            "auth",
            toml::Value::String("required".into()),
        );
        set_key(&mut t, "dtmf", "enabled", toml::Value::Boolean(true));
        let s = toml::to_string(&t).unwrap();
        assert!(s.contains("auth = \"required\""));
        assert!(s.contains("[dtmf]"));
    }
}
