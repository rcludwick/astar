// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! DTMF link commands from a bridge member, with `[dtmf] enabled = true`.
//!
//! # Why only `*1` (disconnect)
//!
//! `*3<node>` and `*2<node>` dial when the node is not already linked, and a
//! DTMF dial has no explicit address: astar-server would resolve the node
//! through `AllStar` DNS, which the bench must never query. `*1` only tears
//! down, so it is the one command the bench sends. The link it drops is one
//! astar-server dialled at an explicit address first.

use std::time::Duration;

use serde_json::Value;

use super::common::{self, clean_start, leg_down, server_dials_node, set_key};
use super::{Check, Ctx, Outcome, Scenario, wait_until};
use crate::server::{LinkAction, ServerEvent};

pub struct DtmfLinkCommand;

impl Scenario for DtmfLinkCommand {
    fn id(&self) -> &'static str {
        "dtmf_link_command"
    }

    fn describe(&self) -> &'static str {
        "[dtmf] enabled: the IAX probe sends *11999# into astar-server; the server drops its link to 1999"
    }

    fn run(&self, ctx: &mut Ctx) -> Outcome {
        let mut o = Outcome::new();
        common::with_server_overlay(
            ctx,
            &mut o,
            |t| set_key(t, "dtmf", "enabled", toml::Value::Boolean(true)),
            run_inner,
        );
        o
    }
}

fn run_inner(ctx: &mut Ctx, o: &mut Outcome) {
    if let Err(e) = clean_start(ctx) {
        o.check(Check::fail("starts unlinked", e));
        return;
    }
    let node_no = ctx.cfg().node_number.clone();
    // The link to drop: astar-server dials the node at its explicit address.
    let leg = match server_dials_node(ctx, LinkAction::Connect, None) {
        Ok((leg, _)) => {
            o.check(Check::pass(
                "server link to 1999 up",
                "dialled at 127.0.0.1:4569",
            ));
            leg
        }
        Err(e) => {
            o.check(Check::fail("server link to 1999 up", e));
            return;
        }
    };
    let probe = match common::iax_probe(ctx) {
        Ok(p) => p,
        Err(e) => {
            o.check(Check::fail("probe joins the bridge", e));
            return;
        }
    };
    o.check(Check::pass("probe joins the bridge", "answered"));
    let events = ctx.server.events().ok();
    std::thread::sleep(Duration::from_millis(500));
    // Only ever a disconnect (module doc), and only while the link is up.
    if common::both_up(ctx, &leg, None).is_err() {
        o.check(Check::fail(
            "link up before the command",
            "the link dropped first",
        ));
        return;
    }
    let digits = format!("*1{node_no}#");
    for d in digits.chars() {
        if let Err(e) = probe.send_dtmf(d) {
            o.check(Check::fail("digits sent", format!("{d}: {e}")));
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    o.check(Check::pass("digits sent", digits.clone()));

    let gone = wait_until(Duration::from_secs(10), common::POLL, || {
        leg_down(ctx, &leg)
    });
    // What astar-server said about the digits on SSE. The link can drop
    // before the last event reaches the stream (it drops within ~100 ms of
    // the command since RIG.md finding 3), so read on until the command
    // shows up or 2 s pass.
    let mut seen = String::new();
    let mut command = None;
    if let Some(es) = &events {
        let until = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            for st in es.drain() {
                if let ServerEvent::Other(v) = &st.event
                    && v.get("event").and_then(Value::as_str) == Some("dtmf")
                {
                    seen.push_str(v.get("digit").and_then(Value::as_str).unwrap_or("?"));
                    if let Some(c) = v.get("command").and_then(Value::as_str) {
                        command = Some(c.to_string());
                    }
                }
            }
            if command.is_some() || std::time::Instant::now() >= until {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    o.check(Check::expect(
        "server received the digits",
        seen == digits,
        format!("SSE dtmf digits {seen:?} (sent {digits:?})"),
    ));
    o.check(Check::expect(
        "server parsed the command",
        command.as_deref() == Some(&format!("disconnect {node_no}")),
        format!("SSE dtmf command {command:?}"),
    ));
    match gone {
        Ok(t) => o.check(
            Check::pass(
                "link dropped by the command",
                format!("both sides clear after {t:.1?}"),
            )
            .measured(t.as_secs_f64(), "s"),
        ),
        Err(e) => o.check(Check::fail(
            "link dropped by the command",
            format!("after 10 s: {}", e.last),
        )),
    };
    let _ = probe.hangup();
    let _ = ctx.server.link(LinkAction::Disconnect, &node_no, None);
}
