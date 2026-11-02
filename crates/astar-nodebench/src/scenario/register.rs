// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! astar-server's `[register]` against the ASL3 node as its registrar.
//!
//! RIG.md finding 2: this storms and can exhaust the node's 2048 call
//! numbers for the pod's address. The scenario bounds its own damage: it
//! measures the call-number use as a check, restores astar-server's config,
//! and restarts the node to give the numbers back.

use std::time::{Duration, Instant};

use super::common::{self, clean_start, set_key};
use super::{Check, Ctx, Outcome, Scenario, wait_until};
use crate::pcap::{self, iax};

/// How long astar-server gets to register.
const REGISTER_WITHIN: Duration = Duration::from_secs(20);
/// Window to see a refresh (astar-server asks for REFRESH 60).
const REFRESH_WINDOW: Duration = Duration::from_secs(75);
/// A registering client needs a handful of call numbers, not hundreds.
const MAX_CALLNOS: u32 = 16;

/// `iax2 show callnumber usage` → the highest count for any address.
#[must_use]
pub fn max_callno_usage(text: &str) -> Option<u32> {
    // Rows look like `127.0.0.1    2048 / 2048` (address, used / limit).
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let addr = it.next()?;
            if !addr.contains('.') && !addr.contains(':') {
                return None;
            }
            it.next()?.parse::<u32>().ok()
        })
        .max()
}

pub struct RegisterWithNode;

impl Scenario for RegisterWithNode {
    fn id(&self) -> &'static str {
        "register_with_node"
    }

    fn describe(&self) -> &'static str {
        "astar-server [register] against the node as registrar: registered, refreshes, call numbers bounded"
    }

    fn run(&self, ctx: &mut Ctx) -> Outcome {
        let mut o = Outcome::new();
        if let Err(e) = clean_start(ctx) {
            o.check(Check::fail("starts unlinked", e));
            return o;
        }
        let peer = toml::Value::String(ctx.cfg().node_iax.to_string());
        // Mark the capture before the restart: astar-server registers within
        // a millisecond of starting, so a mark taken after the overlay misses
        // the first REGACK and the refresh check sees only one.
        let mark = pcap::pod_now(&ctx.rig).ok();
        common::with_server_overlay(
            ctx,
            &mut o,
            |t| set_key(t, "register", "peer", peer),
            |ctx, o| {
                let t0 = Instant::now();
                let reg = wait_until(REGISTER_WITHIN, Duration::from_millis(500), || {
                    match ctx.server.status() {
                        Ok(s) if s.registered => Ok(()),
                        Ok(_) => Err("/status registered=false".into()),
                        Err(e) => Err(e.to_string()),
                    }
                });
                match &reg {
                    Ok(t) => {
                        o.check(
                            Check::pass("registers", format!("/status registered after {t:.1?}"))
                                .measured(t.as_secs_f64(), "s"),
                        );
                    }
                    Err(e) => {
                        o.check(Check::fail(
                            "registers",
                            format!("not registered within {REGISTER_WITHIN:?}: {}", e.last),
                        ));
                    }
                }
                let peer_ok = ctx.node.iax2_peers().ok().and_then(|p| {
                    p.get(&ctx.cfg().server_number)
                        .map(|p| (p.host.clone(), p.port))
                });
                o.check(Check::expect(
                    "node lists 1998 registered",
                    peer_ok
                        .as_ref()
                        .is_some_and(|(h, p)| h.parse::<std::net::IpAddr>().is_ok() && p.is_some()),
                    format!("iax2 show peers: 1998 at {peer_ok:?}"),
                ));
                if reg.is_ok() {
                    std::thread::sleep(REFRESH_WINDOW);
                    let acks = pcap::full_frames(&ctx.rig, mark)
                        .map_or(0, |f| f.iter().filter(|f| f.is_iax(iax::REGACK)).count());
                    o.check(
                        Check::expect(
                            "registration refreshes",
                            acks >= 2,
                            format!("{acks} REGACKs from the node over {REFRESH_WINDOW:?}"),
                        )
                        .measured(u32::try_from(acks).map_or(f64::MAX, f64::from), "REGACK"),
                    );
                } else {
                    o.check(Check::skip("registration refreshes", "never registered"));
                }
                // Sample the node's call numbers after the refresh window, so a
                // storm that builds over time is caught, not just the first
                // seconds of it.
                let usage = ctx
                    .node
                    .rx("iax2 show callnumber usage")
                    .unwrap_or_default();
                let _ = std::fs::write(ctx.dir.join("callnumber-usage.txt"), &usage);
                let used = max_callno_usage(&usage);
                o.check(
                    Check::expect(
                        "node call numbers bounded",
                        used.is_some_and(|u| u <= MAX_CALLNOS),
                        format!(
                            "highest per-address use {} (max {MAX_CALLNOS}), {:.0?} after the restart",
                            used.map_or_else(|| "unknown".into(), |u| u.to_string()),
                            t0.elapsed()
                        ),
                    )
                    .measured(f64::from(used.unwrap_or(0)), "callnos"),
                );
                if let Ok(frames) = pcap::full_frames(&ctx.rig, mark) {
                    let count = |s| frames.iter().filter(|f| f.is_iax(s)).count();
                    o.note(format!(
                        "on the wire since restart: REGREQ {}, REGACK {}, VNAK {}, CALLTOKEN {}, REJECT {}",
                        count(iax::REGREQ),
                        count(iax::REGACK),
                        count(iax::VNAK),
                        count(iax::CALLTOKEN),
                        count(iax::REJECT)
                    ));
                }
                ctx.mark_drop();
            },
        );
        // Give the node its call numbers back for whatever runs next.
        if let Err(e) = common::restart_node(ctx) {
            o.check(Check::fail("node restarted afterwards", e));
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callno_usage_rows() {
        let t = "IP Address                    Callno Usage   Callno Limit\n127.0.0.1                     2048           2048\n10.89.2.1                     3              2048\n";
        assert_eq!(max_callno_usage(t), Some(2048));
        assert_eq!(max_callno_usage("nothing here"), None);
    }
}
