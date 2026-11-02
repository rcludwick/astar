// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `rig_ready`: both sides answer, before anything else is trusted.

use super::{Check, Ctx, Outcome, Scenario};

/// Both sides of the rig answer: `app_rpt` has loaded the node, AMI accepts the
/// test login, and astar-server's control channel reports it is listening.
pub struct RigReady;

impl Scenario for RigReady {
    fn id(&self) -> &'static str {
        "rig_ready"
    }

    fn describe(&self) -> &'static str {
        "app_rpt loaded the node, AMI logs in, astar-server is listening"
    }

    fn run(&self, ctx: &mut Ctx) -> Outcome {
        let mut o = Outcome::new();
        let node = ctx.cfg().node_number.clone();
        o.check(match ctx.node.node_loaded() {
            Ok(true) => Check::pass(
                "app_rpt node loaded",
                format!("rpt localnodes lists {node}"),
            ),
            Ok(false) => Check::fail(
                "app_rpt node loaded",
                format!("rpt localnodes does not list {node}"),
            ),
            Err(e) => Check::fail("app_rpt node loaded", e.to_string()),
        });
        o.check(
            match ctx.node.ami(false, std::time::Duration::from_secs(5)) {
                Ok(c) => {
                    let banner = c.banner.clone();
                    c.logoff();
                    Check::pass("AMI login", banner)
                }
                Err(e) => Check::fail("AMI login", e.to_string()),
            },
        );
        o.check(match ctx.server.status() {
            Ok(s) => Check::expect(
                "astar-server listening",
                s.listening,
                format!("node_id={:?} listening={}", s.node_id, s.listening),
            ),
            Err(e) => Check::fail("astar-server listening", e.to_string()),
        });
        o
    }
}
