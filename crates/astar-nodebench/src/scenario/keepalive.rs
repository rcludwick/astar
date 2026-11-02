// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Keepalives over a held link, from the tshark sidecar's capture: every
//! PING gets a PONG, every LAGRQ a LAGRP, and every `app_rpt` TEXT frame an ACK,
//! in both directions, at a steady cadence.

use super::common::{self, clean_start, hold_link, node_dials_server};
use super::{Check, Ctx, Outcome, Scenario};
use crate::node::Ilink;
use crate::pcap::{self, IaxFrame, ftype, iax};

/// An answer must come within this long.
const ANSWER_WITHIN: f64 = 2.0;
/// Longest gap allowed between one side's keepalives (PING or LAGRQ).
/// astar-server sends both every 2 s; Asterisk sends LAGRQ every 10 s.
const MAX_KEEPALIVE_GAP: f64 = 25.0;

/// Requests of `req` from `from_port` and how many got `resp` back on the
/// same call within [`ANSWER_WITHIN`]. A request sent less than
/// [`ANSWER_WITHIN`] before `until` (the end of the capture window) is not
/// judged: its answer may legitimately fall outside the window.
#[must_use]
pub fn answered(
    frames: &[IaxFrame],
    from_port: u16,
    req: u32,
    resp: u32,
    until: f64,
) -> (usize, Vec<f64>) {
    let mut unanswered = Vec::new();
    let reqs: Vec<&IaxFrame> = frames
        .iter()
        .filter(|f| f.src_port == from_port && f.is_iax(req) && f.t + ANSWER_WITHIN <= until)
        .collect();
    for r in &reqs {
        let ok = frames.iter().any(|f| {
            f.dst_port == from_port
                && f.is_iax(resp)
                && f.dst_call == r.src_call
                && f.t >= r.t
                && f.t - r.t <= ANSWER_WITHIN
        });
        if !ok {
            unanswered.push(r.t);
        }
    }
    (reqs.len(), unanswered)
}

/// TEXT frames from `from_port` and which were not `ACK`ed (`ACK` echoes the
/// frame's timestamp on the same call). As in [`answered`], a frame sent
/// less than [`ANSWER_WITHIN`] before `until` is not judged.
#[must_use]
pub fn texts_acked(frames: &[IaxFrame], from_port: u16, until: f64) -> (usize, Vec<f64>) {
    let texts: Vec<&IaxFrame> = frames
        .iter()
        .filter(|f| {
            f.src_port == from_port && f.ftype == ftype::TEXT && f.t + ANSWER_WITHIN <= until
        })
        .collect();
    let mut missing = Vec::new();
    for t in &texts {
        let ok = frames.iter().any(|f| {
            f.dst_port == from_port
                && f.is_ack()
                && f.dst_call == t.src_call
                && f.ts == t.ts
                && f.t >= t.t
                && f.t - t.t <= ANSWER_WITHIN
        });
        if !ok {
            missing.push(t.t);
        }
    }
    (texts.len(), missing)
}

/// The longest gap between keepalives (PING or LAGRQ) sent from `port`.
#[must_use]
pub fn max_gap(frames: &[IaxFrame], port: u16, window: (f64, f64)) -> f64 {
    let mut ts: Vec<f64> = frames
        .iter()
        .filter(|f| f.src_port == port && (f.is_iax(iax::PING) || f.is_iax(iax::LAGRQ)))
        .map(|f| f.t)
        .collect();
    ts.insert(0, window.0);
    ts.push(window.1);
    ts.windows(2).map(|w| w[1] - w[0]).fold(0.0, f64::max)
}

/// A frame count as a check measurement.
fn count(n: usize) -> f64 {
    u32::try_from(n).map_or(f64::MAX, f64::from)
}

pub struct KeepaliveExchange;

impl Scenario for KeepaliveExchange {
    fn id(&self) -> &'static str {
        "keepalive_exchange"
    }

    fn describe(&self) -> &'static str {
        "over a held link, PING/PONG, LAGRQ/LAGRP and app_rpt TEXT keepalives are each answered (pcap)"
    }

    #[allow(clippy::too_many_lines)]
    fn run(&self, ctx: &mut Ctx) -> Outcome {
        let mut o = Outcome::new();
        if let Err(e) = clean_start(ctx) {
            o.check(Check::fail("starts unlinked", e));
            return o;
        }
        let leg = match node_dials_server(ctx, Ilink::Transceive, None) {
            Ok((leg, _)) => leg,
            Err(e) => {
                o.check(Check::fail("link establishes", e));
                return o;
            }
        };
        let Ok(t_start) = pcap::pod_now(&ctx.rig) else {
            o.check(Check::fail("pod clock", "cannot read the pod's clock"));
            return o;
        };
        let held = hold_link(ctx, &mut o, &leg, None);
        let t_end = pcap::pod_now(&ctx.rig).unwrap_or(t_start);
        let _ = ctx.node.ilink(Ilink::Disconnect, &ctx.cfg().server_number);
        if !held {
            return o;
        }
        let frames = match pcap::full_frames(&ctx.rig, Some(t_start)) {
            Ok(f) => f,
            Err(e) => {
                o.check(Check::fail("capture read", e.to_string()));
                return o;
            }
        };
        let (np, sp) = (ctx.cfg().node_iax.port(), ctx.cfg().server_iax.port());
        // Only the node <-> server link (both directions, any call numbers).
        let link: Vec<IaxFrame> = frames
            .into_iter()
            .filter(|f| f.t <= t_end)
            .filter(|f| {
                (f.src_port == np && f.dst_port == sp) || (f.src_port == sp && f.dst_port == np)
            })
            .collect();
        let _ = std::fs::write(
            ctx.dir.join("link-frames.txt"),
            pcap::render(&link, t_start, |p| common::port_name(ctx, p)),
        );
        o.note(format!(
            "{} full frames on the link over the hold",
            link.len()
        ));

        for (who, port) in [("astar-server", sp), ("ASL3", np)] {
            for (req, resp) in [(iax::PING, iax::PONG), (iax::LAGRQ, iax::LAGRP)] {
                let (n, missing) = answered(&link, port, req, resp, t_end);
                let rq = pcap::iax_name(req).unwrap_or("?");
                let rp = pcap::iax_name(resp).unwrap_or("?");
                let name = format!("{who} {rq}s answered with {rp}");
                if n == 0 {
                    o.check(Check::skip(
                        name,
                        format!("{who} sent no {rq} during the hold"),
                    ));
                } else {
                    o.check(
                        Check::expect(
                            name,
                            missing.is_empty(),
                            format!(
                                "{} of {n} answered within {ANSWER_WITHIN} s{}",
                                n - missing.len(),
                                if missing.is_empty() {
                                    String::new()
                                } else {
                                    format!(
                                        "; unanswered at t={:?}",
                                        missing
                                            .iter()
                                            .map(|t| format!("{:.1}", t - t_start))
                                            .collect::<Vec<_>>()
                                    )
                                }
                            ),
                        )
                        .measured(count(n), "frames"),
                    );
                }
            }
            let gap = max_gap(&link, port, (t_start, t_end));
            o.check(
                Check::expect(
                    format!("{who} keepalive cadence"),
                    gap <= MAX_KEEPALIVE_GAP,
                    format!(
                        "longest gap between its PING/LAGRQ {gap:.1} s (max {MAX_KEEPALIVE_GAP:.0})"
                    ),
                )
                .measured(gap, "s"),
            );
        }
        let (n, missing) = texts_acked(&link, np, t_end);
        if n == 0 {
            o.check(Check::skip(
                "app_rpt TEXT frames ACKed",
                "the node sent no TEXT frame during the hold (it sends one about every 30 s)",
            ));
        } else {
            o.check(
                Check::expect(
                    "app_rpt TEXT frames ACKed",
                    missing.is_empty(),
                    format!("{} of {n} ACKed by astar-server", n - missing.len()),
                )
                .measured(count(n), "frames"),
            );
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn f(t: f64, src: u16, dst: u16, sc: u32, dc: u32, ft: u8, sub: u32, ts: u32) -> IaxFrame {
        IaxFrame {
            t,
            src: String::new(),
            dst: String::new(),
            src_port: src,
            dst_port: dst,
            src_call: sc,
            dst_call: dc,
            ts,
            oseq: 0,
            iseq: 0,
            ftype: ft,
            sub,
            text: String::new(),
            ies: Vec::new(),
        }
    }

    #[test]
    fn pings_matched_to_pongs_on_the_same_call() {
        let fr = vec![
            f(1.0, 4570, 4569, 1, 9, ftype::IAX, iax::PING, 100),
            f(1.01, 4569, 4570, 9, 1, ftype::IAX, iax::PONG, 100),
            f(3.0, 4570, 4569, 1, 9, ftype::IAX, iax::PING, 2100),
            // a PONG for some other call does not count
            f(3.01, 4569, 4570, 9, 7, ftype::IAX, iax::PONG, 2100),
        ];
        let (n, missing) = answered(&fr, 4570, iax::PING, iax::PONG, f64::INFINITY);
        assert_eq!(n, 2);
        assert_eq!(missing, vec![3.0]);
    }

    #[test]
    fn a_request_at_the_window_edge_is_not_judged() {
        // The PING at 59.5 s may be answered after the window closes at 60 s.
        let fr = vec![
            f(1.0, 4570, 4569, 1, 9, ftype::IAX, iax::PING, 100),
            f(1.01, 4569, 4570, 9, 1, ftype::IAX, iax::PONG, 100),
            f(59.5, 4570, 4569, 1, 9, ftype::IAX, iax::PING, 58600),
        ];
        let (n, missing) = answered(&fr, 4570, iax::PING, iax::PONG, 60.0);
        assert_eq!(n, 1);
        assert_eq!(missing, [] as [f64; 0]);
    }

    #[test]
    fn text_acks_match_timestamp() {
        let fr = vec![
            f(1.0, 4569, 4570, 9, 1, ftype::TEXT, 0, 500),
            f(1.001, 4570, 4569, 1, 9, ftype::IAX, iax::ACK, 500),
            f(5.0, 4569, 4570, 9, 1, ftype::TEXT, 0, 4500),
            f(5.001, 4570, 4569, 1, 9, ftype::IAX, iax::ACK, 4499),
        ];
        let (n, missing) = texts_acked(&fr, 4569, f64::INFINITY);
        assert_eq!(n, 2);
        assert_eq!(missing, vec![5.0]);
    }

    #[test]
    fn gap_includes_window_edges() {
        let fr = vec![f(12.0, 4569, 4570, 9, 1, ftype::IAX, iax::LAGRQ, 0)];
        assert!((max_gap(&fr, 4569, (0.0, 60.0)) - 48.0).abs() < 1e-9);
    }
}
