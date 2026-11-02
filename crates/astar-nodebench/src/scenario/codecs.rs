// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Negotiation matrices: codecs and auth.
//!
//! Each cell reconfigures one side through an overlay, which restarts only
//! that side's container, so the cells are independent of each other.

use std::time::Duration;

use super::common::{
    self, NodeOverlay, ServerLeg, TEST_SECRET, clean_start, node_dials_server, server_dials_node,
    set_key,
};
use super::{Check, Ctx, Outcome, Scenario, wait_until};
use crate::node::Ilink;
use crate::pcap::{self, iax};
use crate::server::LinkAction;

/// The node's codec sets (offer order), and astar-server's policies.
const NODE_CODECS: &[&str] = &["slin16,slin,ulaw", "slin,ulaw", "ulaw", "gsm"];
const SERVER_POLICIES: &[&str] = &["ulaw_only", "allow_slin", "prefer_slin", "prefer_slin16"];

/// Which way a cell's call goes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// The node dials astar-server: astar answers.
    NodeDials,
    /// astar-server dials the node: astar places the call.
    ServerDials,
}

/// What a policy can carry in `direction`. A call astar answers accepts
/// GSM (`CodecPolicy::preference_order`); a call it places never offers it
/// (`CodecPolicy::offer_order`), so a GSM-only node dialled by the server
/// has nothing in common with it and must be refused.
fn policy_codecs(p: &str, direction: Direction) -> &'static [&'static str] {
    let all: &'static [&'static str] = match p {
        "ulaw_only" => &["ulaw", "alaw", "gsm"],
        "allow_slin" => &["ulaw", "alaw", "slin", "gsm"],
        "prefer_slin" => &["slin", "ulaw", "alaw", "gsm"],
        _ => &["slin16", "slin", "ulaw", "alaw", "gsm"],
    };
    match direction {
        Direction::NodeDials => all,
        // GSM is always last: everything before it.
        Direction::ServerDials => &all[..all.len() - 1],
    }
}

/// Both sides' codec once they have settled: right after "up", Asterisk's
/// channel list and astar-server's snapshot can still show the pre-ACCEPT
/// format (or none). Polls up to 3 s for both to name one and agree, and
/// returns the last reading either way.
pub(super) fn settled_codecs(ctx: &Ctx, leg: &ServerLeg) -> (Option<String>, Option<String>) {
    let mut last = (None, None);
    let _ = wait_until(Duration::from_secs(3), common::POLL, || {
        last = codecs_on(ctx, leg);
        match &last {
            (Some(n), Some(s)) if n == s || n.starts_with("unknow") => Ok(()),
            other => Err(format!("{other:?}")),
        }
    });
    last
}

/// The node's and the server's view of the codec on the current link.
fn codecs_on(ctx: &Ctx, leg: &ServerLeg) -> (Option<String>, Option<String>) {
    let node = ctx.node.iax2_channels().ok().and_then(|c| {
        // A torn-down call lingers as a `(None)` channel for a while; only a
        // live channel says what this link negotiated.
        let live: Vec<_> = c
            .channels
            .iter()
            .filter(|ch| ch.channel.is_some())
            .collect();
        match live.as_slice() {
            [one] => Some(common::norm_codec(&one.format)),
            _ => None,
        }
    });
    let server = ctx
        .server
        .status()
        .ok()
        .and_then(|s| common::server_leg_codec(&s, leg));
    (node, server)
}

/// One cell's check: both sides agree, and the codec is one both allow.
/// The FORMAT the answerer put in the last ACCEPT since `since`: what the
/// wire says was negotiated.
pub(super) fn accepted_format(ctx: &Ctx, since: Option<f64>) -> Option<String> {
    pcap::full_frames(&ctx.rig, since)
        .ok()?
        .iter()
        .rev()
        .find(|f| f.is_iax(iax::ACCEPT))
        .and_then(pcap::IaxFrame::format)
}

/// One cell's check: the ACCEPT on the wire, astar-server's snapshot and (if
/// it knows yet) the node agree, and the codec is one both sides allow.
///
/// Asterisk's `iax2 show channels` Format column is the format of the last
/// voice frame received, so it reads `unknown` until the far end has sent
/// audio; then it is not evidence either way and is only reported.
fn codec_check(
    name: String,
    both: &[&str],
    wire: Option<&str>,
    got: (Option<String>, Option<String>),
) -> Check {
    let (n, s) = got;
    let node_known = n.as_deref().filter(|c| !c.starts_with("unknow"));
    let seen = format!("wire ACCEPT {wire:?}, server {s:?}, node channel {n:?}");
    match (wire, &s) {
        (Some(w), Some(sv)) if w == sv && node_known.is_none_or(|nk| nk == w) => {
            if both.contains(&w) {
                Check::pass(name, format!("{w} (common set {both:?}); {seen}"))
            } else {
                Check::fail(
                    name,
                    format!("{w} is outside the common set {both:?}; {seen}"),
                )
            }
        }
        _ => Check::fail(name, format!("sides disagree: {seen}")),
    }
}

/// slin16 / slin / ulaw nodes against each `codec_policy`, both dial
/// directions. Checks the negotiated codec on both sides.
pub struct CodecMatrix;

impl Scenario for CodecMatrix {
    fn id(&self) -> &'static str {
        "codec_matrix"
    }

    fn describe(&self) -> &'static str {
        "node slin16/slin/ulaw x server codec_policy, both dial directions: negotiated codec agreed and allowed"
    }

    fn run(&self, ctx: &mut Ctx) -> Outcome {
        let mut o = Outcome::new();
        for node in NODE_CODECS {
            let ov = NodeOverlay {
                codecs: Some((*node).to_string()),
                ..NodeOverlay::default()
            };
            common::with_node_overlay(ctx, &mut o, &ov, |ctx, o| {
                for policy in SERVER_POLICIES {
                    let p = toml::Value::String((*policy).to_string());
                    common::with_server_overlay(
                        ctx,
                        o,
                        |t| {
                            t.insert("codec_policy".into(), p);
                        },
                        |ctx, o| cell(ctx, o, node, policy),
                    );
                }
            });
        }
        o
    }
}

/// The codecs both the node and astar-server can carry on a call going
/// `direction`.
fn common_set<'a>(node: &'a str, policy: &str, direction: Direction) -> Vec<&'a str> {
    node.split(',')
        .filter(|c| policy_codecs(policy, direction).contains(c))
        .collect()
}

fn cell(ctx: &mut Ctx, o: &mut Outcome, node: &str, policy: &str) {
    let tag = format!("node {node} / server {policy}");
    let mark = pcap::pod_now(&ctx.rig).ok();
    let failed_before = o.checks.iter().filter(|c| c.is_fail()).count();
    if let Err(e) = clean_start(ctx) {
        o.check(Check::fail(format!("{tag}: starts unlinked"), e));
        return;
    }
    // Node dials.
    let dial_mark = pcap::pod_now(&ctx.rig).ok();
    match node_dials_server(ctx, Ilink::Transceive, None) {
        Ok((leg, _)) => {
            let got = settled_codecs(ctx, &leg);
            let wire = accepted_format(ctx, dial_mark);
            o.check(codec_check(
                format!("{tag}, node dials: codec"),
                &common_set(node, policy, Direction::NodeDials),
                wire.as_deref(),
                got,
            ));
        }
        Err(e) => {
            o.check(Check::fail(format!("{tag}, node dials: codec"), e));
        }
    }
    if let Err(e) = clean_start(ctx) {
        o.check(Check::fail(format!("{tag}: clean between directions"), e));
        return;
    }
    // Server dials (with CALLING_NUMBER = 1998 since iax-cnlink; no node-side
    // caller-ID workaround).
    let dial_mark = pcap::pod_now(&ctx.rig).ok();
    let both = common_set(node, policy, Direction::ServerDials);
    match server_dials_node(ctx, LinkAction::Connect, None) {
        Ok((leg, _)) if both.is_empty() => {
            let got = settled_codecs(ctx, &leg);
            o.check(Check::fail(
                format!("{tag}, server dials: refused"),
                format!("linked with no codec in common (astar never offers gsm): {got:?}"),
            ));
        }
        Ok((leg, _)) => {
            let got = settled_codecs(ctx, &leg);
            let wire = accepted_format(ctx, dial_mark);
            o.check(codec_check(
                format!("{tag}, server dials: codec"),
                &both,
                wire.as_deref(),
                got,
            ));
        }
        Err(e) if both.is_empty() => {
            o.check(Check::pass(
                format!("{tag}, server dials: refused"),
                format!("no codec in common, as expected (astar never offers gsm): {e}"),
            ));
        }
        Err(e) => {
            o.check(Check::fail(format!("{tag}, server dials: codec"), e));
        }
    }
    let node_no = ctx.cfg().node_number.clone();
    let _ = ctx.server.link(LinkAction::Disconnect, &node_no, None);
    if o.checks.iter().filter(|c| c.is_fail()).count() > failed_before {
        common::save_cell_frames(ctx, o, &tag, mark);
    }
}

// ---- auth -------------------------------------------------------------------

/// How the node presents itself when it dials astar-server.
#[derive(Clone, Copy)]
enum NodeCreds {
    /// Stock `app_rpt`: user `radio`, no secret.
    Anonymous,
    /// User 1999 with the secret astar-server holds for it.
    Right,
    /// User 1999 with a secret astar-server does not hold.
    Wrong,
}

impl NodeCreds {
    fn label(self) -> &'static str {
        match self {
            Self::Anonymous => "node radio/no secret",
            Self::Right => "node 1999/right secret",
            Self::Wrong => "node 1999/wrong secret",
        }
    }

    fn entry(self) -> Option<String> {
        let at = "@127.0.0.1:4570/1998,NONE";
        match self {
            Self::Anonymous => None,
            Self::Right => Some(format!("1999:{TEST_SECRET}{at}")),
            Self::Wrong => Some(format!("1999:not-the-secret{at}")),
        }
    }
}

/// `auth` on astar-server's listener against how the node dials in.
pub struct AuthVariants;

/// (node creds, server auth, should it link?)
const AUTH_CELLS: &[(u8, &str, bool)] = &[
    (0, "off", true),
    (0, "optional", true),
    (0, "required", false),
    (1, "optional", true),
    (1, "required", true),
    (2, "required", false),
];

fn creds(i: u8) -> NodeCreds {
    match i {
        0 => NodeCreds::Anonymous,
        1 => NodeCreds::Right,
        _ => NodeCreds::Wrong,
    }
}

impl Scenario for AuthVariants {
    fn id(&self) -> &'static str {
        "auth_variants"
    }

    fn describe(&self) -> &'static str {
        "astar-server auth off/optional/required against the node dialling anonymous, right and wrong secrets"
    }

    fn run(&self, ctx: &mut Ctx) -> Outcome {
        let mut o = Outcome::new();
        for c in 0..=2u8 {
            let nc = creds(c);
            let ov = NodeOverlay {
                server_node_entry: nc.entry(),
                ..NodeOverlay::default()
            };
            let cells: Vec<_> = AUTH_CELLS.iter().filter(|(i, _, _)| *i == c).collect();
            let run_cells = |ctx: &mut Ctx, o: &mut Outcome| {
                for (_, auth, expect) in &cells {
                    let a = toml::Value::String((*auth).to_string());
                    common::with_server_overlay(
                        ctx,
                        o,
                        |t| set_key(t, "listener", "auth", a),
                        |ctx, o| auth_cell(ctx, o, nc, auth, *expect),
                    );
                }
            };
            if nc.entry().is_some() {
                common::with_node_overlay(ctx, &mut o, &ov, run_cells);
            } else {
                run_cells(ctx, &mut o);
            }
        }
        o
    }
}

fn auth_cell(ctx: &mut Ctx, o: &mut Outcome, nc: NodeCreds, auth: &str, expect: bool) {
    let name = format!(
        "{} / server auth={auth}: {}",
        nc.label(),
        if expect { "links" } else { "refused" }
    );
    if let Err(e) = clean_start(ctx) {
        o.check(Check::fail(name, format!("not clean: {e}")));
        return;
    }
    // astar-server's inbound credential for user 1999 (lost on each restart).
    if let Err(e) = ctx.server.provide_secret("1999", TEST_SECRET) {
        o.check(Check::fail(name, format!("POST /secrets: {e}")));
        return;
    }
    let mark = pcap::pod_now(&ctx.rig).ok();
    let failed_before = o.checks.iter().filter(|c| c.is_fail()).count();
    let server_no = ctx.cfg().server_number.clone();
    if expect {
        match node_dials_server(ctx, Ilink::Transceive, None) {
            Ok((leg, t)) => {
                // A link that comes up and is then torn down is not a pass.
                std::thread::sleep(Duration::from_secs(3));
                match common::both_up(ctx, &leg, None) {
                    Ok(()) => o.check(Check::pass(
                        name,
                        format!("up after {t:.1?}, still up 3 s later"),
                    )),
                    Err(e) => o.check(Check::fail(name, format!("up, then dropped: {e}"))),
                };
            }
            Err(e) => {
                o.check(Check::fail(name, e));
            }
        }
    } else {
        if let Err(e) = ctx.node.ilink(Ilink::Transceive, &server_no) {
            o.check(Check::fail(name, format!("ilink 3 refused locally: {e}")));
            return;
        }
        let came_up = wait_until(common::REFUSE_WINDOW, common::POLL, || {
            let st = ctx.server.status().map_err(|e| e.to_string())?;
            common::node_link_up(ctx, &server_no, None)?;
            if common::active_call_ids(&st).is_empty() {
                Err("no server call".into())
            } else {
                Ok(())
            }
        });
        // "Refused" needs evidence of a refusal: the node dialled (a NEW to
        // astar-server) and the call was turned away (REJECT or HANGUP).
        // Without that, a node that never dialled at all would pass.
        let frames = pcap::full_frames(&ctx.rig, mark).unwrap_or_default();
        let sp = ctx.cfg().server_iax.port();
        let dialled = frames
            .iter()
            .any(|f| f.dst_port == sp && f.is_iax(iax::NEW));
        let wire = frames
            .iter()
            .find(|f| f.is_iax(iax::REJECT) || f.is_iax(iax::HANGUP))
            .map(|f| {
                format!(
                    "{} sent {} ({:?})",
                    common::port_name(ctx, f.src_port),
                    f.label(),
                    f.text
                )
            });
        match (came_up, dialled, wire) {
            (Ok(_), _, _) => o.check(Check::fail(name, "the link came up")),
            (Err(_), true, Some(w)) => o.check(Check::pass(
                name,
                format!("not linked within {:?}; {w}", common::REFUSE_WINDOW),
            )),
            (Err(_), dialled, w) => o.check(Check::fail(
                name,
                format!(
                    "not linked, but no refusal to show for it: node NEW seen {dialled}, REJECT/HANGUP {}",
                    w.unwrap_or_else(|| "none".into())
                ),
            )),
        };
    }
    let _ = ctx.node.ilink(Ilink::Disconnect, &server_no);
    if o.checks.iter().filter(|c| c.is_fail()).count() > failed_before {
        common::save_cell_frames(ctx, o, &format!("{} auth {auth}", nc.label()), mark);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gsm_is_common_only_on_calls_astar_answers() {
        for policy in SERVER_POLICIES {
            assert_eq!(common_set("gsm", policy, Direction::NodeDials), vec!["gsm"]);
            assert_eq!(
                common_set("gsm", policy, Direction::ServerDials),
                [] as [&str; 0]
            );
            assert!(!policy_codecs(policy, Direction::ServerDials).contains(&"gsm"));
        }
        assert_eq!(
            common_set("slin,ulaw", "ulaw_only", Direction::ServerDials),
            vec!["ulaw"]
        );
    }
}
