// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Audio through the link, tone in and analysis out, both directions.
//!
//! The node dials astar-server (`ilink 3`), and an IAX2 probe (node 1997)
//! dials into astar-server's bridge beside it. The node's "radio" is the
//! USRP probe. So:
//!
//! * node -> server: USRP keys and sends 1 kHz into the node; the node sends
//!   it over the link; the bridge mixes it to the probe, which records it.
//! * server -> node: the probe keys and sends 700 Hz into the bridge; the
//!   bridge sends it over the link; the node transmits it on USRP, where the
//!   bench records it.

use std::time::Duration;

use super::common::{self, clean_start, node_dials_server};
use super::{Check, Ctx, Outcome, Scenario};
use crate::audio;
use crate::node::Ilink;
use crate::probe::iax::ProbeEvent;
use crate::probe::usrp::UsrpProbe;

const NODE_TONE_HZ: f64 = 1000.0;
const SERVER_TONE_HZ: f64 = 700.0;

/// Link the node, bind the USRP probe, dial the IAX probe; fail `o` and
/// return `None` if any of it does not happen.
fn setup(ctx: &Ctx, o: &mut Outcome) -> Option<(UsrpProbe, crate::probe::iax::IaxProbe)> {
    if let Err(e) = clean_start(ctx) {
        o.check(Check::fail("starts unlinked", e));
        return None;
    }
    let _ = ctx.node.enable_debug();
    let leg = match node_dials_server(ctx, Ilink::Transceive, None) {
        Ok((leg, t)) => {
            o.check(Check::pass(
                "node links to server",
                format!("{leg} up after {t:.1?}"),
            ));
            leg
        }
        Err(e) => {
            o.check(Check::fail("node links to server", e));
            return None;
        }
    };
    let usrp = match common::usrp_probe() {
        Ok(u) => u,
        Err(e) => {
            o.check(Check::fail("probes connect", e));
            return None;
        }
    };
    match common::iax_probe(ctx) {
        Ok(p) => {
            o.check(Check::pass(
                "probes connect",
                format!("IAX probe answered by astar-server, {:?}", p.format()),
            ));
            // The node keys up for its own telemetry when a link connects
            // ("connected" messages, courtesy tones). Measure only once its
            // transmitter has been quiet, or that audio is in the recording.
            match common::wait_node_quiet(&usrp, Duration::from_secs(60)) {
                Ok(t) => o.note(format!("node transmitter quiet after {t:.1?}")),
                Err(e) => {
                    o.check(Check::fail("node transmitter quiet before the test", e));
                    return None;
                }
            }
            // Both the link and the probe's call must still be up.
            let link = common::both_up(ctx, &leg, None);
            if !o.check(Check::expect(
                "link and probe up before the tone",
                link.is_ok() && p.is_up(),
                format!("link: {link:?}; probe call up: {}", p.is_up()),
            )) {
                return None;
            }
            Some((usrp, p))
        }
        Err(e) => {
            o.check(Check::fail("probes connect", e));
            None
        }
    }
}

/// USRP injects a 1 kHz tone with PTT into the node; the IAX probe on
/// astar-server's bridge hears it.
pub struct AudioNodeToServer;

impl Scenario for AudioNodeToServer {
    fn id(&self) -> &'static str {
        "audio_node_to_server"
    }

    fn describe(&self) -> &'static str {
        "USRP 1 kHz into the node -> link -> astar-server bridge -> IAX probe: freq, level, onset, loss"
    }

    fn run(&self, ctx: &mut Ctx) -> Outcome {
        let mut o = Outcome::new();
        let Some((usrp, probe)) = setup(ctx, &mut o) else {
            return o;
        };
        let mark = probe.mark();
        let tone = audio::tone(
            NODE_TONE_HZ,
            common::TONE_DBFS,
            Duration::from_secs(common::TONE_SECS),
            8000,
        );
        match usrp.transmit(&tone) {
            Ok(tx) => {
                std::thread::sleep(Duration::from_secs(1));
                let frames = probe.frames_since(mark);
                let rate = probe.sample_rate();
                let _ = audio::write_wav(
                    &ctx.dir.join("probe-heard.wav"),
                    rate,
                    &crate::probe::concat(&frames),
                );
                let r = common::analyse_tone(&frames, &tx, NODE_TONE_HZ, rate, common::iax_loss);
                common::tone_checks(&mut o, "node -> server", &r, NODE_TONE_HZ);
                let keyed = probe
                    .events_since(mark)
                    .iter()
                    .any(|e| e.event == ProbeEvent::RemotePtt(true));
                o.note(format!(
                    "probe saw astar-server key toward it: {keyed}; {} frames received",
                    frames.len()
                ));
            }
            Err(e) => {
                o.check(Check::fail(
                    "node -> server: tone sent",
                    format!("USRP: {e}"),
                ));
            }
        }
        let _ = probe.hangup();
        let _ = ctx.node.ilink(Ilink::Disconnect, &ctx.cfg().server_number);
        o
    }
}

/// The IAX probe keys and sends 700 Hz into astar-server's bridge; the node
/// transmits it on USRP.
pub struct AudioServerToNode;

impl Scenario for AudioServerToNode {
    fn id(&self) -> &'static str {
        "audio_server_to_node"
    }

    fn describe(&self) -> &'static str {
        "IAX probe 700 Hz into astar-server bridge -> link -> node -> USRP: freq, level, onset, loss"
    }

    fn run(&self, ctx: &mut Ctx) -> Outcome {
        let mut o = Outcome::new();
        let Some((usrp, probe)) = setup(ctx, &mut o) else {
            return o;
        };
        let rate = probe.sample_rate();
        let mark = crate::pcap::pod_now(&ctx.rig).ok();
        let rec = usrp.record_any_in_background(Duration::from_secs(common::TONE_SECS + 3));
        let tone = audio::tone(
            SERVER_TONE_HZ,
            common::TONE_DBFS,
            Duration::from_secs(common::TONE_SECS),
            rate,
        );
        let tx = probe.transmit(&tone);
        let heard = rec.ok().and_then(|h| h.join().ok()).and_then(Result::ok);
        match (tx, heard) {
            (Ok(tx), Some(frames)) => {
                let _ = audio::write_wav(
                    &ctx.dir.join("node-transmitted.wav"),
                    8000,
                    &crate::probe::concat(&frames),
                );
                let keyed = frames.iter().filter(|f| f.keyed).count();
                o.note(format!(
                    "{} USRP frames from the node, {keyed} keyed",
                    frames.len()
                ));
                let r = common::analyse_tone(&frames, &tx, SERVER_TONE_HZ, 8000, common::usrp_loss);
                if !common::tone_checks(&mut o, "server -> node", &r, SERVER_TONE_HZ) {
                    common::note_newkey(ctx, &mut o, mark);
                }
            }
            (Err(e), _) => {
                o.check(Check::fail(
                    "server -> node: tone sent",
                    format!("probe: {e}"),
                ));
            }
            (_, None) => {
                o.check(Check::fail(
                    "server -> node: tone heard",
                    "USRP recorder failed",
                ));
            }
        }
        let _ = probe.hangup();
        let _ = ctx.node.ilink(Ilink::Disconnect, &ctx.cfg().server_number);
        o
    }
}
