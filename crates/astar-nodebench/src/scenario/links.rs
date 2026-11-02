// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Link scenarios: who dials, whether it holds, how it ends, and the
//! ulaw-only (`ClearNode`-like) node. `node_links_to_server` is the shape of
//! the `ClearNode` report (`iax-cnlink`): a node's link to astar-server
//! dropping after a second or two.

use std::time::{Duration, Instant};

use super::common::{
    self, NodeOverlay, ServerLeg, clean_start, explain_teardown, frames_since_start, hold_link,
    leg_down, node_dials_server, note_server_events, server_dials_node,
};
use super::{Check, Ctx, Outcome, Scenario, wait_until};
use crate::audio;
use crate::node::Ilink;
use crate::node::parse::RptLinkMode;
use crate::pcap::{self, iax};
use crate::server::LinkAction;

/// Start clean, or fail the scenario saying why.
fn start(ctx: &Ctx, o: &mut Outcome) -> bool {
    if let Err(e) = clean_start(ctx) {
        return o.check(Check::fail("starts unlinked", e));
    }
    if let Err(e) = ctx.node.enable_debug() {
        o.note(format!("could not enable node debug: {e}"));
    }
    true
}

/// Note who tore the call down, from the capture since `since` (pod clock;
/// `None` = scenario start).
fn note_teardown(ctx: &Ctx, o: &mut Outcome, since: Option<f64>) {
    let frames = match since {
        Some(t) => pcap::full_frames(&ctx.rig, Some(t)).map_err(|e| format!("pcap: {e}")),
        None => frames_since_start(ctx),
    };
    match frames {
        Ok(frames) => {
            if let Some(why) = explain_teardown(ctx, &frames) {
                o.note(format!("teardown on the wire: {why}"));
            }
        }
        Err(e) => o.note(e),
    }
}

/// ASL3 `rpt cmd <node> ilink 3 <server>`; both sides report the link, and it
/// holds for the hold window with no drop.
pub struct NodeLinksToServer;

impl Scenario for NodeLinksToServer {
    fn id(&self) -> &'static str {
        "node_links_to_server"
    }

    fn describe(&self) -> &'static str {
        "ASL3 ilink 3 to astar-server; both sides report the link and it holds"
    }

    #[allow(clippy::too_many_lines)]
    fn run(&self, ctx: &mut Ctx) -> Outcome {
        let mut o = Outcome::new();
        if !start(ctx, &mut o) {
            return o;
        }
        let events = ctx.server.events().ok();
        let mark = pcap::pod_now(&ctx.rig).ok();
        match node_dials_server(ctx, Ilink::Transceive, Some(RptLinkMode::Transceive)) {
            Ok((leg, t)) => {
                o.check(
                    Check::pass(
                        "link establishes",
                        format!("both sides up after {t:.1?} ({leg})"),
                    )
                    .measured(t.as_secs_f64(), "s"),
                );
                if !hold_link(ctx, &mut o, &leg, None) {
                    note_teardown(ctx, &mut o, mark);
                }
                newkey_check(ctx, &mut o, mark, Some(ctx.cfg().server_iax.port()));
            }
            Err(e) => {
                ctx.mark_drop();
                o.check(Check::fail("link establishes", e));
                note_teardown(ctx, &mut o, mark);
            }
        }
        note_server_events(events.as_ref(), &mut o);
        let _ = ctx.node.ilink(Ilink::Disconnect, &ctx.cfg().server_number);
        o
    }
}

/// astar-server `POST /link connect <node>` at the node's explicit address;
/// both sides report the link, and it holds.
pub struct ServerLinksToNode;

impl Scenario for ServerLinksToNode {
    fn id(&self) -> &'static str {
        "server_links_to_node"
    }

    fn describe(&self) -> &'static str {
        "astar-server POST /link connect 1999 at 127.0.0.1:4569; both sides up and it holds; NEWKEY answered"
    }

    #[allow(clippy::too_many_lines)]
    fn run(&self, ctx: &mut Ctx) -> Outcome {
        let mut o = Outcome::new();
        if !start(ctx, &mut o) {
            return o;
        }
        let events = ctx.server.events().ok();
        let mark = pcap::pod_now(&ctx.rig).ok();
        match server_dials_node(ctx, LinkAction::Connect, Some(RptLinkMode::Transceive)) {
            Ok((leg, t)) => {
                o.check(
                    Check::pass("link establishes", format!("both sides up after {t:.1?}"))
                        .measured(t.as_secs_f64(), "s"),
                );
                if !hold_link(ctx, &mut o, &leg, None) {
                    note_teardown(ctx, &mut o, mark);
                }
                // app_rpt opens a link it accepted with !NEWKEY1! too.
                newkey_check(ctx, &mut o, mark, None);
            }
            Err(e) => {
                ctx.mark_drop();
                o.check(Check::fail("link establishes", e));
                note_teardown(ctx, &mut o, mark);
                note_node_refusal(ctx, &mut o);
            }
        }
        note_server_events(events.as_ref(), &mut o);
        let node_no = ctx.cfg().node_number.clone();
        let _ = ctx.server.link(LinkAction::Disconnect, &node_no, None);
        o
    }
}

/// `app_rpt` opens every link with a `!NEWKEY1!` TEXT frame and expects the far
/// end to send one back within 2 s (`NEWKEYTIME`); without it, `app_rpt` stops
/// treating the far end's voice as keyed and waits for `RADIO_KEY` control
/// frames (`app_rpt.c`, `newkeytimer`; it logs a WARNING when that happens).
///
/// `server_port` is astar-server's end of the link: its listener for a link
/// the node dialled, or `None` for one astar-server dialled (an ephemeral
/// port: anything that is not the node).
fn newkey_check(ctx: &Ctx, o: &mut Outcome, since: Option<f64>, server_port: Option<u16>) {
    let np = ctx.cfg().node_iax.port();
    let is_server = |p: u16| server_port.map_or(p != np, |sp| p == sp);
    let frames = match pcap::full_frames(&ctx.rig, since) {
        Ok(f) if !f.is_empty() => f,
        // An empty capture would otherwise read as "the node sent no
        // !NEWKEY1!" and pass.
        _ => {
            o.check(Check::skip(
                "app_rpt NEWKEY handshake answered",
                "no capture",
            ));
            return;
        }
    };
    let sent = frames
        .iter()
        .find(|f| f.src_port == np && is_server(f.dst_port) && f.text.contains("!NEWKEY1!"));
    let back = frames.iter().find(|f| {
        is_server(f.src_port)
            && f.dst_port == np
            && f.ftype == crate::pcap::ftype::TEXT
            && f.text.contains("NEWKEY")
    });
    let detail = match (sent, back) {
        (Some(s), Some(b)) => format!("node sent !NEWKEY1!; astar-server answered {:?} after {:.0} ms", b.text, (b.t - s.t) * 1000.0),
        (Some(_), None) => "node sent !NEWKEY1!; astar-server never sent NEWKEY back, so after 2 s app_rpt needs RADIO_KEY frames to hear it".into(),
        (None, _) => "the node sent no !NEWKEY1! (nothing to answer)".into(),
    };
    let ok = sent.is_none() || back.is_some_and(|b| sent.is_some_and(|s| b.t - s.t <= 2.0));
    o.check(Check::expect(
        "app_rpt NEWKEY handshake answered",
        ok,
        detail,
    ));
}

/// `app_rpt`'s own reason for refusing a link, from the Asterisk log.
fn note_node_refusal(ctx: &Ctx, o: &mut Outcome) {
    let node = ctx.cfg().node_container.clone();
    if let Ok(log) = ctx.rig.file_tail(&node, "/var/log/asterisk/full", 400) {
        for l in log.lines().filter(|l| l.contains("invalid for connection")) {
            o.note(format!("node log: {}", l.trim()));
        }
    }
}

/// ilink 2 (monitor): the link is up as a monitor, audio from astar-server's
/// bridge reaches the node's transmitter, and the node's receive audio does
/// NOT reach the bridge.
pub struct MonitorLink;

impl Scenario for MonitorLink {
    fn id(&self) -> &'static str {
        "monitor_link"
    }

    fn describe(&self) -> &'static str {
        "ASL3 ilink 2 (monitor): link up; audio flows server -> node only"
    }

    #[allow(clippy::too_many_lines)]
    fn run(&self, ctx: &mut Ctx) -> Outcome {
        let mut o = Outcome::new();
        if !start(ctx, &mut o) {
            return o;
        }
        let leg = match node_dials_server(ctx, Ilink::Monitor, Some(RptLinkMode::Monitor)) {
            Ok((leg, t)) => {
                o.check(
                    Check::pass(
                        "monitor link up",
                        format!("node lists 1998 as R (monitor) after {t:.1?}"),
                    )
                    .measured(t.as_secs_f64(), "s"),
                );
                leg
            }
            Err(e) => {
                o.check(Check::fail("monitor link up", e));
                return o;
            }
        };
        let (usrp, probe) = match (common::usrp_probe(), common::iax_probe(ctx)) {
            (Ok(u), Ok(p)) => (u, p),
            (Err(e), _) | (_, Err(e)) => {
                o.check(Check::fail("probes connect", e));
                return o;
            }
        };
        o.check(Check::pass(
            "probes connect",
            format!("IAX probe answered, {:?}", probe.format()),
        ));
        // Wait out the node's own "connected" telemetry before measuring.
        match common::wait_node_quiet(&usrp, Duration::from_secs(60)) {
            Ok(t) => o.note(format!("node transmitter quiet after {t:.1?}")),
            Err(e) => {
                o.check(Check::fail("node transmitter quiet before the test", e));
                return o;
            }
        }
        let mark = pcap::pod_now(&ctx.rig).ok();

        // server -> node: the probe talks, the node's transmitter carries it.
        let rec = usrp.record_any_in_background(Duration::from_secs(TONE_WINDOW));
        let tone = audio::tone(
            700.0,
            common::TONE_DBFS,
            Duration::from_secs(common::TONE_SECS),
            8000,
        );
        let tx = probe.transmit(&tone);
        let heard = rec.ok().and_then(|h| h.join().ok()).and_then(Result::ok);
        match (tx, heard) {
            (Ok(_), Some(frames)) => {
                let lvl = common::tone_present(&frames, 700.0, 8000);
                if lvl.is_none() {
                    common::note_newkey(ctx, &mut o, mark);
                }
                o.check(Check::expect(
                    "server -> node audio passes",
                    lvl.is_some(),
                    lvl.map_or_else(
                        || format!("no 700 Hz in {} USRP frames from the node", frames.len()),
                        |l| format!("700 Hz at the node's transmitter, peak frame {l:.1} dBFS"),
                    ),
                ));
            }
            (Err(e), _) => {
                o.check(Check::fail(
                    "server -> node audio passes",
                    format!("probe transmit: {e}"),
                ));
            }
            (_, None) => {
                o.check(Check::fail(
                    "server -> node audio passes",
                    "USRP recorder failed",
                ));
            }
        }

        // node -> server: the node's radio talks; a monitor link must not
        // carry it.
        std::thread::sleep(Duration::from_millis(500));
        let mark = probe.mark();
        let tone = audio::tone(
            1000.0,
            common::TONE_DBFS,
            Duration::from_secs(common::TONE_SECS),
            8000,
        );
        let tx = usrp.transmit(&tone);
        std::thread::sleep(Duration::from_secs(1));
        let frames = probe.frames_since(mark);
        match tx {
            Ok(_) => {
                let lvl = common::tone_present(&frames, 1000.0, probe.sample_rate());
                o.check(Check::expect(
                    "node -> server audio blocked",
                    lvl.is_none(),
                    lvl.map_or_else(
                        || {
                            format!(
                                "no 1 kHz in {} frames at the bridge, as a monitor link should",
                                frames.len()
                            )
                        },
                        |l| format!("1 kHz reached the bridge at {l:.1} dBFS over a monitor link"),
                    ),
                ));
            }
            Err(e) => {
                o.check(Check::fail(
                    "node -> server audio blocked",
                    format!("USRP transmit: {e}"),
                ));
            }
        }
        let _ = probe.hangup();
        o.check(
            super::common::both_up(ctx, &leg, Some(RptLinkMode::Monitor)).map_or_else(
                |e| Check::fail("monitor link still up", e),
                |()| Check::pass("monitor link still up", "after both audio tests"),
            ),
        );
        let _ = ctx.node.ilink(Ilink::Disconnect, &ctx.cfg().server_number);
        o
    }
}

/// How long a recorder listens around one tone.
const TONE_WINDOW: u64 = common::TONE_SECS + 3;

/// Disconnect from each side; both sides agree the link is gone, and the
/// capture says who hung up.
pub struct DisconnectFromEachSide;

/// astar-server's own disconnect must clear the node about as fast as the
/// node's `ilink 1` does. Before RIG.md finding 3 was fixed it took 10 s: the
/// node held the link for `app_rpt`'s `DISC_TIME`.
const SERVER_DISCONNECT_DEADLINE: Duration = Duration::from_secs(2);

impl DisconnectFromEachSide {
    /// Drop `leg` with `drop`, then time until both sides agree and name the
    /// first HANGUP on the wire after the drop. With `astar_hangs_up`, also
    /// check astar-server's teardown on the wire: `!!DISCONNECT!!` first, as
    /// `app_rpt`'s own disconnect does, and a HANGUP on the call's clock.
    fn drop_and_time(
        ctx: &Ctx,
        o: &mut Outcome,
        side: &str,
        leg: &ServerLeg,
        deadline: Duration,
        astar_hangs_up: bool,
        drop: impl FnOnce() -> Result<(), String>,
    ) {
        let mark = pcap::pod_now(&ctx.rig).ok();
        let t0 = Instant::now();
        if let Err(e) = drop() {
            o.check(Check::fail(format!("{side}: disconnect accepted"), e));
            return;
        }
        // Who still held it at the deadline, then keep timing to the end.
        let at_deadline = wait_until(deadline, common::POLL, || leg_down(ctx, leg))
            .err()
            .map(|e| e.last);
        let gone = wait_until(Duration::from_secs(20), common::POLL, || leg_down(ctx, leg));
        let took = t0.elapsed();
        match gone {
            Ok(_) => {
                o.check(
                    Check::expect(
                        format!("{side}: both sides clear"),
                        at_deadline.is_none(),
                        format!(
                            "both sides agree after {took:.1?} (deadline {deadline:?}){}",
                            at_deadline
                                .map(|w| format!("; at the deadline: {w}"))
                                .unwrap_or_default()
                        ),
                    )
                    .measured(took.as_secs_f64(), "s"),
                );
            }
            Err(e) => {
                o.check(
                    Check::fail(
                        format!("{side}: both sides clear"),
                        format!("after 20 s: {}", e.last),
                    )
                    .measured(took.as_secs_f64(), "s"),
                );
            }
        }
        let frames = pcap::full_frames(&ctx.rig, mark);
        if astar_hangs_up && let Ok(frames) = &frames {
            Self::astar_teardown_checks(ctx, o, side, frames);
        }
        match frames {
            Ok(frames) => match frames.iter().find(|f| f.is_iax(iax::HANGUP)) {
                Some(h) => {
                    let first = frames.first().map_or(h.t, |f| f.t);
                    o.check(Check::pass(
                        format!("{side}: hangup recorded"),
                        format!(
                            "{} sent HANGUP (ts {}, cause {:?}) {:.0} ms after the command",
                            common::port_name(ctx, h.src_port),
                            h.ts,
                            h.text,
                            (h.t - mark.unwrap_or(first)) * 1000.0
                        ),
                    ));
                }
                None => {
                    o.check(Check::fail(
                        format!("{side}: hangup recorded"),
                        format!(
                            "no HANGUP in {} full frames after the command",
                            frames.len()
                        ),
                    ));
                }
            },
            Err(e) => {
                o.check(Check::skip(
                    format!("{side}: hangup recorded"),
                    format!("pcap: {e}"),
                ));
            }
        }
    }
}

impl DisconnectFromEachSide {
    /// astar-server's teardown: `!!DISCONNECT!!` on the call clock, then
    /// HANGUP after it. `app_rpt` usually hangs up first once it has read the
    /// TEXT, so astar-server's own HANGUP may never be sent; when it is, it
    /// must follow the TEXT and carry the call clock too.
    fn astar_teardown_checks(ctx: &Ctx, o: &mut Outcome, side: &str, frames: &[pcap::IaxFrame]) {
        let node_port = ctx.cfg().node_iax.port();
        let ours = |f: &&pcap::IaxFrame| f.src_port != node_port;
        let disc = frames.iter().filter(ours).find(|f| {
            f.ftype == pcap::ftype::TEXT && f.text.trim_end_matches('\0') == "!!DISCONNECT!!"
        });
        let Some(disc) = disc else {
            o.check(Check::fail(
                format!("{side}: !!DISCONNECT!! sent"),
                "astar-server sent no !!DISCONNECT!!, so app_rpt holds the link for DISC_TIME (10 s)",
            ));
            return;
        };
        o.check(
            Check::expect(
                format!("{side}: !!DISCONNECT!! on the call clock"),
                disc.ts > 0,
                format!("astar-server's !!DISCONNECT!! carries ts {} ms", disc.ts),
            )
            .measured(f64::from(disc.ts), "ms"),
        );
        match frames.iter().filter(ours).find(|f| f.is_iax(iax::HANGUP)) {
            Some(h) => o.check(Check::expect(
                format!("{side}: astar-server HANGUP after the TEXT, on the call clock"),
                h.ts > 0 && h.oseq > disc.oseq,
                format!(
                    "HANGUP oseqno {} ts {} ms; !!DISCONNECT!! oseqno {}",
                    h.oseq, h.ts, disc.oseq
                ),
            )),
            None => o.check(Check::pass(
                format!("{side}: astar-server HANGUP after the TEXT, on the call clock"),
                "the node hung up first on reading !!DISCONNECT!!, as app_rpt does",
            )),
        };
    }
}

impl Scenario for DisconnectFromEachSide {
    fn id(&self) -> &'static str {
        "disconnect_from_each_side"
    }

    fn describe(&self) -> &'static str {
        "ilink 1 and POST /link disconnect: both sides agree the link is gone; who hung up"
    }

    #[allow(clippy::too_many_lines)]
    fn run(&self, ctx: &mut Ctx) -> Outcome {
        let mut o = Outcome::new();
        if !start(ctx, &mut o) {
            return o;
        }
        let server_no = ctx.cfg().server_number.clone();
        let node_no = ctx.cfg().node_number.clone();

        // The node drops a link it dialled.
        match node_dials_server(ctx, Ilink::Transceive, None) {
            Ok((leg, _)) => {
                std::thread::sleep(Duration::from_secs(3));
                Self::drop_and_time(
                    ctx,
                    &mut o,
                    "node ilink 1",
                    &leg,
                    common::DISCONNECT_DEADLINE,
                    false,
                    || {
                        ctx.node
                            .ilink(Ilink::Disconnect, &server_no)
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    },
                );
            }
            Err(e) => {
                o.check(Check::fail("node ilink 1: link establishes", e));
            }
        }

        // astar-server drops a link it dialled. Since iax-cnlink a server
        // dial carries CALLING_NUMBER = 1998, so this needs no caller-ID
        // workaround on the node (clean_start leaves it off).
        if let Err(e) = clean_start(ctx) {
            o.check(Check::fail("server /link disconnect: starts unlinked", e));
            return o;
        }
        match server_dials_node(ctx, LinkAction::Connect, None) {
            Ok((leg, _)) => {
                std::thread::sleep(Duration::from_secs(3));
                Self::drop_and_time(
                    ctx,
                    &mut o,
                    "server /link disconnect",
                    &leg,
                    SERVER_DISCONNECT_DEADLINE,
                    true,
                    || {
                        ctx.server
                            .link(LinkAction::Disconnect, &node_no, None)
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    },
                );
            }
            Err(e) => {
                o.check(Check::fail("server /link disconnect: link establishes", e));
            }
        }
        o
    }
}

/// The node offers only ulaw (`ClearNode`-like). It links, holds, both sides
/// say ulaw, and audio passes.
pub struct UlawOnlyNode;

impl Scenario for UlawOnlyNode {
    fn id(&self) -> &'static str {
        "ulaw_only_node"
    }

    fn describe(&self) -> &'static str {
        "ulaw-only node (ClearNode-like) links to astar-server, holds, ulaw both sides, audio passes"
    }

    #[allow(clippy::too_many_lines)]
    fn run(&self, ctx: &mut Ctx) -> Outcome {
        let mut o = Outcome::new();
        let ov = NodeOverlay {
            codecs: Some("ulaw".into()),
            ..NodeOverlay::default()
        };
        common::with_node_overlay(ctx, &mut o, &ov, |ctx, o| {
            if !start(ctx, o) {
                return;
            }
            let events = ctx.server.events().ok();
            let mark = pcap::pod_now(&ctx.rig).ok();
            let leg = match node_dials_server(ctx, Ilink::Transceive, None) {
                Ok((leg, t)) => {
                    o.check(
                        Check::pass("link establishes", format!("both sides up after {t:.1?}"))
                            .measured(t.as_secs_f64(), "s"),
                    );
                    leg
                }
                Err(e) => {
                    ctx.mark_drop();
                    o.check(Check::fail("link establishes", e));
                    note_teardown(ctx, o, mark);
                    return;
                }
            };
            let (node_fmt, server_fmt) = super::codecs::settled_codecs(ctx, &leg);
            // The node's Format column is the last voice frame it received,
            // so it reads "unknown" until astar-server sends audio, and an
            // idle bridge sends none (RIG.md finding 6). Then the ACCEPT on
            // the wire is the evidence, as in `codec_matrix`.
            let wire = super::codecs::accepted_format(ctx, mark);
            let node_ulaw = node_fmt.as_deref() == Some("ulaw")
                || (node_fmt.as_deref().is_some_and(|n| n.starts_with("unknow"))
                    && wire.as_deref() == Some("ulaw"));
            o.check(Check::expect(
                "both sides ulaw",
                node_ulaw && server_fmt.as_deref() == Some("ulaw"),
                format!("node {node_fmt:?}, server {server_fmt:?}, ACCEPT on the wire {wire:?}"),
            ));
            if !hold_link(ctx, o, &leg, None) {
                note_teardown(ctx, o, mark);
                note_server_events(events.as_ref(), o);
                return;
            }
            // Audio both ways over the ulaw link.
            match (common::usrp_probe(), common::iax_probe(ctx)) {
                (Ok(usrp), Ok(probe)) => {
                    std::thread::sleep(Duration::from_millis(500));
                    let mark = probe.mark();
                    let tone = audio::tone(
                        1000.0,
                        common::TONE_DBFS,
                        Duration::from_secs(common::TONE_SECS),
                        8000,
                    );
                    match usrp.transmit(&tone) {
                        Ok(tx) => {
                            std::thread::sleep(Duration::from_secs(1));
                            let frames = probe.frames_since(mark);
                            let r = common::analyse_tone(
                                &frames,
                                &tx,
                                1000.0,
                                probe.sample_rate(),
                                common::iax_loss,
                            );
                            common::tone_checks(o, "node -> server", &r, 1000.0);
                        }
                        Err(e) => {
                            o.check(Check::fail(
                                "node -> server: tone heard",
                                format!("USRP transmit: {e}"),
                            ));
                        }
                    }
                    let _ = probe.hangup();
                }
                (Err(e), _) | (_, Err(e)) => {
                    o.check(Check::fail("node -> server: tone heard", e));
                }
            }
            note_server_events(events.as_ref(), o);
            let _ = ctx.node.ilink(Ilink::Disconnect, &ctx.cfg().server_number);
        });
        o
    }
}

/// The node drops its link and dials again at once (what an `AllStar` node's
/// reconnect does). The new link must hold, and the old call's HANGUP must
/// be acknowledged the first time, so nothing stale is left on the wire.
pub struct RelinkAfterDrop;

/// How long the second link must hold.
const RELINK_HOLD: Duration = Duration::from_secs(10);

impl Scenario for RelinkAfterDrop {
    fn id(&self) -> &'static str {
        "relink_after_drop"
    }

    fn describe(&self) -> &'static str {
        "node ilink 1 then ilink 3 again at once: the new link holds; the old HANGUP is ACKed first time"
    }

    #[allow(clippy::too_many_lines)]
    fn run(&self, ctx: &mut Ctx) -> Outcome {
        let mut o = Outcome::new();
        if !start(ctx, &mut o) {
            return o;
        }
        let server_no = ctx.cfg().server_number.clone();
        let first = match node_dials_server(ctx, Ilink::Transceive, None) {
            Ok((leg, _)) => leg,
            Err(e) => {
                o.check(Check::fail("first link establishes", e));
                return o;
            }
        };
        o.check(Check::pass("first link establishes", format!("{first}")));
        std::thread::sleep(Duration::from_secs(3));
        let mark = pcap::pod_now(&ctx.rig).ok();
        if let Err(e) = ctx.node.ilink(Ilink::Disconnect, &server_no) {
            o.check(Check::fail("first link dropped", e.to_string()));
            return o;
        }
        if let Err(e) = wait_until(common::DISCONNECT_DEADLINE, common::POLL, || {
            leg_down(ctx, &first)
        }) {
            o.check(Check::fail("first link dropped", e.last));
            return o;
        }
        o.check(Check::pass("first link dropped", "both sides clear"));
        let relinked = Instant::now();
        let second = match node_dials_server(ctx, Ilink::Transceive, None) {
            Ok((leg, t)) => {
                o.check(Check::pass(
                    "second link establishes",
                    format!("{leg} up after {t:.1?}"),
                ));
                leg
            }
            Err(e) => {
                o.check(Check::fail("second link establishes", e));
                return o;
            }
        };
        let hold_start = relinked.elapsed();
        let h = super::hold_for(RELINK_HOLD, ctx.tuning.hold_poll, || {
            common::both_up(ctx, &second, None)
        });
        if !h.held_all() {
            ctx.mark_drop();
        }
        let mut c = h.to_check(&format!("second link holds {} s", RELINK_HOLD.as_secs()));
        if let Some(b) = &h.broke {
            c.detail = format!(
                "{} (gone by {:.1} s after the re-dial)",
                c.detail,
                (hold_start + b.at).as_secs_f64()
            );
        }
        o.check(c);

        // What the wire says about the first call's HANGUP.
        match pcap::full_frames(&ctx.rig, mark) {
            Ok(frames) => {
                let np = ctx.cfg().node_iax.port();
                let hangups: Vec<_> = frames
                    .iter()
                    .filter(|f| f.src_port == np && f.is_iax(iax::HANGUP))
                    .collect();
                if let Some(h0) = hangups.first() {
                    let same: Vec<_> = hangups
                        .iter()
                        .filter(|h| h.src_call == h0.src_call)
                        .collect();
                    let ack = frames.iter().find(|f| {
                        f.dst_port == np
                            && f.is_ack()
                            && f.dst_call == h0.src_call
                            && f.ts == h0.ts
                            && f.t >= h0.t
                    });
                    // Sent once AND acknowledged with iseqno = its oseqno + 1:
                    // an ACK one behind is the iax-cnlink defect, and it only
                    // shows as a retransmit ~2 s later, which a short capture
                    // window could miss.
                    let want_iseq = (h0.oseq + 1) % 256;
                    o.check(Check::expect(
                        "old HANGUP acknowledged first time",
                        same.len() == 1 && ack.is_some_and(|a| a.iseq == want_iseq),
                        format!(
                            "node HANGUP on call {} (oseqno {}) sent {} time(s); astar-server's ACK carried iseqno {} (the HANGUP is acknowledged by iseqno {})",
                            h0.src_call,
                            h0.oseq,
                            same.len(),
                            ack.map_or_else(|| "none".into(), |a| a.iseq.to_string()),
                            want_iseq
                        ),
                    ));
                    for r in same.iter().skip(1) {
                        let hit = frames.iter().find(|f| {
                            f.t >= r.t && f.dst_port == np && f.is_ack() && f.t - r.t < 0.05
                        });
                        o.note(format!(
                            "retransmitted HANGUP from old call {} to astar call {} at +{:.2} s; astar-server answered {}",
                            r.src_call,
                            r.dst_call,
                            r.t - h0.t,
                            hit.map_or_else(
                                || "nothing".into(),
                                |a| format!("ACK addressed to call {} (the NEW link's call)", a.dst_call)
                            )
                        ));
                    }
                } else {
                    o.check(Check::fail(
                        "old HANGUP acknowledged first time",
                        "no HANGUP from the node in the capture",
                    ));
                }
                let invals = frames
                    .iter()
                    .filter(|f| f.dst_port == np && f.is_iax(iax::INVAL))
                    .count();
                if invals > 0 {
                    o.note(format!(
                        "astar-server sent {invals} INVAL to the node after the re-dial"
                    ));
                }
            }
            Err(e) => o.note(format!("pcap: {e}")),
        }
        let _ = ctx.node.ilink(Ilink::Disconnect, &server_no);
        o
    }
}
