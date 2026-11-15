// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Speech through the link and back: the spoken test phrase is echoed by
//! astar-server's parrot, and the echo is compared with what was sent
//! ([`crate::speech`]).
//!
//! `parrot_echoes_speech`: astar-server runs with `[bridge] mode =
//! "parrot"`. The node links to it with `ilink 3`; the bench keys the phrase
//! into the node's radio (USRP); the node carries it over the link;
//! astar-server records the over and plays it back; the node transmits the
//! echo on USRP, where the bench records it. It runs against the normal node
//! and the ulaw-only node.
//!
//! The design's second scenario, with `app_rpt`'s own parrot as the echo
//! server, cannot be built: `app_rpt` plays its parrot recording into the
//! node's transmit-only conference, so the echo goes out the node's radio
//! and never back over a link (RIG.md, "Speech echo").

use std::time::{Duration, Instant};

use super::common::{
    self, NodeOverlay, ServerLeg, clean_start, node_dials_server, server_dials_node,
};
use super::{Check, Ctx, Outcome, Scenario};
use crate::audio;
use crate::node::Ilink;
use crate::pcap::{self, IaxFrame, WireVoice};
use crate::probe::RxFrame;
use crate::probe::usrp::{SAMPLE_RATE, UsrpProbe, VOICE_SAMPLES};
use crate::server::LinkAction;
use crate::speech::{self, SpeechComparison, SpeechThresholds};

/// astar-server's parrot playback delay (`[parrot] playback_delay_ms`, the
/// shipped default). Long enough that the node's own hang time and
/// courtesy tone are over before the echo starts.
pub const PARROT_DELAY_MS: u64 = 3000;
/// The parrot's VOX silence gap (`[parrot] silence_gap_ms`, the default).
/// `app_rpt` keys a link with voice alone and never sends `RADIO_KEY`, so
/// the parrot's PTT gate never latches and the take ends on VOX: this much
/// quiet after the node's last voice frame, then the playback delay.
pub const PARROT_GAP_MS: u64 = 800;
/// The echo may start this much earlier than gap + delay says...
pub const PARROT_EARLY_MS: f64 = 250.0;
/// ...and this much later (the bench's unkey crossing the node, the link,
/// the bridge tick, and the echo crossing back to the bench).
pub const PARROT_LATE_MS: f64 = 500.0;
/// How long after the echo the link must still be up on both sides.
pub const AFTER_ECHO_HOLD: Duration = Duration::from_secs(5);
/// Lost USRP frames are filled with silence when the sequence number skips
/// at most this many frames; a longer skip is a new transmission, not loss.
const MAX_FILL_FRAMES: u32 = 5;

/// What astar-server's echo on the link must match, against the over it
/// received: it should give back exactly what it took in. In the measured
/// runs (RIG.md, "Speech echo") every echo was 100 % present, edges
/// included, correlation 1.000, level −0.3 dB, with no gaps, repeats or
/// stalls; these leave headroom for a lost frame without passing a lost
/// syllable.
#[must_use]
pub fn parrot_thresholds() -> SpeechThresholds {
    SpeechThresholds {
        min_correlation: 0.95,
        max_level_diff_db: 1.0,
        min_present_fraction: 0.99,
        min_edge_fraction: 0.8,
        max_gaps: 0,
        max_repeats: 0,
        max_stalls: 0,
    }
}

/// What the echo at the node's radio must match, against the phrase. This
/// copy has crossed `app_rpt` twice and the host relay twice, and those hops
/// stall, repeat or trim a frame or two of their own (the over on the link
/// shows them before astar-server touches it). So 90 % of the speech must
/// arrive, within 3 dB, and an edge passes with four of its six speech frames: `app_rpt`
/// trims one as it keys for the over and sometimes one more as it keys for
/// the echo.
#[must_use]
pub fn radio_thresholds() -> SpeechThresholds {
    SpeechThresholds {
        max_level_diff_db: 3.0,
        min_present_fraction: 0.9,
        min_edge_fraction: 0.6,
        ..parrot_thresholds()
    }
}

/// A recording laid out frame by frame: the samples, and when each 20 ms
/// frame arrived (`None` for a lost frame filled with silence).
#[derive(Debug, Default)]
struct Timeline {
    samples: Vec<i16>,
    arrived: Vec<Option<Instant>>,
    filled: usize,
}

impl Timeline {
    /// Keyed voice frames, with a skip of up to [`MAX_FILL_FRAMES`] in the
    /// sender's sequence numbers filled with silence, so a lost frame reads
    /// as 20 ms missing rather than as the rest of the phrase arriving early.
    fn of(frames: &[RxFrame]) -> Self {
        let mut t = Self::default();
        let mut last: Option<u32> = None;
        for f in frames.iter().filter(|f| f.samples.len() == VOICE_SAMPLES) {
            if let (Some(prev), Some(seq)) = (last, f.seq) {
                let skipped = seq.wrapping_sub(prev).wrapping_sub(1);
                if skipped > 0 && skipped <= MAX_FILL_FRAMES {
                    for _ in 0..skipped {
                        t.samples.extend(std::iter::repeat_n(0, VOICE_SAMPLES));
                        t.arrived.push(None);
                        t.filled += 1;
                    }
                }
            }
            last = f.seq.or(last);
            t.samples.extend_from_slice(&f.samples);
            t.arrived.push(Some(f.at));
        }
        t
    }

    /// When the frame holding sample `at` arrived (the next received frame
    /// if that one was filled).
    fn arrival_of(&self, at: i64) -> Option<Instant> {
        let first = usize::try_from(at.max(0)).ok()? / VOICE_SAMPLES;
        self.arrived.get(first..)?.iter().flatten().next().copied()
    }
}

/// The link is still up on both sides for [`AFTER_ECHO_HOLD`].
fn link_up_after(ctx: &Ctx, o: &mut Outcome, tag: &str, leg: &ServerLeg) {
    let h = super::hold_for(AFTER_ECHO_HOLD, Duration::from_millis(500), || {
        common::both_up(ctx, leg, None)
    });
    o.check(h.to_check(&format!(
        "{tag}: link still up on both sides {} s after the echo",
        AFTER_ECHO_HOLD.as_secs()
    )));
}

/// Turn a comparison into the design's checks, prefixed with `tag`. With
/// `whole` false only presence and level are checks; correlation and
/// gaps/repeats/stalls are noted (the node's own transmit hop, see
/// [`parrot_cell`]).
fn speech_checks(
    o: &mut Outcome,
    tag: &str,
    c: &SpeechComparison,
    t: &SpeechThresholds,
    whole: bool,
) {
    o.check(
        Check::expect(
            format!("{tag}: whole phrase came back"),
            c.present_fraction >= t.min_present_fraction,
            format!(
                "{:.1}% of {} speech frames (min {:.1}%)",
                c.present_fraction * 100.0,
                c.speech_frames,
                t.min_present_fraction * 100.0
            ),
        )
        .measured(c.present_fraction * 100.0, "%"),
    );
    for (edge, fraction) in [("first", c.head_fraction), ("last", c.tail_fraction)] {
        o.check(
            Check::expect(
                format!("{tag}: {edge} {} ms present", speech::EDGE_MS),
                fraction >= t.min_edge_fraction,
                format!(
                    "{:.0}% of its speech frames (min {:.0}%)",
                    fraction * 100.0,
                    t.min_edge_fraction * 100.0
                ),
            )
            .measured(fraction * 100.0, "%"),
        );
    }
    o.check(
        Check::expect(
            format!("{tag}: level"),
            c.level_diff_db.is_finite() && c.level_diff_db.abs() <= t.max_level_diff_db,
            format!(
                "{:+.1} dB against the phrase sent (±{:.1} dB)",
                c.level_diff_db, t.max_level_diff_db
            ),
        )
        .measured(c.level_diff_db, "dB"),
    );
    if !whole {
        o.note(format!("{tag}: {}", c.summary()));
        return;
    }
    o.check(
        Check::expect(
            format!("{tag}: same speech"),
            c.correlation >= t.min_correlation,
            format!(
                "loudness-curve correlation {:.3} (min {:.3})",
                c.correlation, t.min_correlation
            ),
        )
        .measured(c.correlation, ""),
    );
    let defects = c.gaps.len() > t.max_gaps
        || c.repeats.len() > t.max_repeats
        || c.stalls.len() > t.max_stalls;
    let detail = format!(
        "{} gap(s), {} repeat(s), {} stall(s)",
        c.gaps.len(),
        c.repeats.len(),
        c.stalls.len()
    );
    o.check(Check::expect(
        format!("{tag}: no gaps, repeats or stalls"),
        !defects,
        if defects {
            format!("{detail}: {}", c.summary())
        } else {
            detail
        },
    ));
}

/// The node's UNKEY toward astar-server that ended the over (pod clock).
fn node_unkey(ctx: &Ctx, frames: &[IaxFrame]) -> Option<f64> {
    const RADIO_UNKEY: u32 = 13;
    let np = ctx.cfg().node_iax.port();
    frames
        .iter()
        .find(|f| f.src_port == np && f.ftype == pcap::ftype::CONTROL && f.sub == RADIO_UNKEY)
        .map(|f| f.t)
}

/// One direction of the link's voice, concatenated: its rate, its samples
/// and its first frame's time (pod clock).
fn wire_audio(frames: &[WireVoice]) -> Option<(u32, Vec<i16>, f64)> {
    let first = frames.first()?;
    let samples = frames
        .iter()
        .filter(|f| f.rate == first.rate)
        .flat_map(|f| f.samples.iter().copied())
        .collect();
    Some((first.rate, samples, first.t))
}

/// "The echo began after the over ended" and "after the parrot's delay",
/// from `after_ms`: the echo's first frame relative to the over's end.
fn timing_checks(o: &mut Outcome, tag: &str, after_ms: f64, how: &str) {
    o.check(
        Check::expect(
            format!("{tag}: echo began after the over ended"),
            after_ms > 0.0,
            format!("{after_ms:.0} ms ({how})"),
        )
        .measured(after_ms, "ms"),
    );
    #[allow(clippy::cast_precision_loss)]
    let want = (PARROT_GAP_MS + PARROT_DELAY_MS) as f64;
    o.check(
        Check::expect(
            format!("{tag}: echo delay matches the parrot's"),
            after_ms >= want - PARROT_EARLY_MS && after_ms <= want + PARROT_LATE_MS,
            format!(
                "{after_ms:.0} ms (silence_gap_ms {PARROT_GAP_MS} + playback_delay_ms {PARROT_DELAY_MS}, window -{PARROT_EARLY_MS:.0}/+{PARROT_LATE_MS:.0} ms)"
            ),
        )
        .measured(after_ms, "ms"),
    );
}

/// The echo as astar-server put it on the link, and the over as the node
/// sent it, both out of the capture. These are the strict checks: this is
/// astar-server's own output.
fn wire_checks(ctx: &Ctx, o: &mut Outcome, tag: &str, mark: Option<f64>) -> bool {
    let (np, sp) = (ctx.cfg().node_iax.port(), ctx.cfg().server_iax.port());
    let phrase = speech::link_test_phrase();
    let full = pcap::full_frames(&ctx.rig, mark);
    let over = pcap::voice_frames(&ctx.rig, Some(np), sp, mark);
    let echo = pcap::voice_frames(&ctx.rig, Some(sp), np, mark);
    let (Ok(full), Ok(over), Ok(echo)) = (full, over, echo) else {
        o.check(Check::fail(
            format!("{tag}: echo on the link"),
            "could not read the capture",
        ));
        return false;
    };
    let Some((over_rate, over_pcm, _)) = wire_audio(&over) else {
        o.check(Check::fail(
            format!("{tag}: echo on the link"),
            "the node sent astar-server no voice during the over",
        ));
        return false;
    };
    // What the parrot heard: the node's over against the phrase. app_rpt's
    // own input path shapes it (a trimmed first and last frame, now and
    // then a 20 ms stall), so it is a note.
    if let Some(c) = speech::compare(&phrase.samples, phrase.rate, &over_pcm, over_rate) {
        o.note(format!(
            "{tag}: the over on the link, node -> astar-server ({} frames), against the phrase: {}",
            over.len(),
            c.summary()
        ));
    }
    let Some((rate, samples, first)) = wire_audio(&echo) else {
        o.check(Check::fail(
            format!("{tag}: echo on the link"),
            "astar-server sent the node no voice after the over",
        ));
        return false;
    };
    let path = ctx.dir.join(format!("{}-echo-on-link.wav", slug(tag)));
    if audio::write_wav(&path, rate, &samples).is_ok() {
        o.artifacts.push(path);
    }
    if let Some(c) = speech::compare(&phrase.samples, phrase.rate, &samples, rate) {
        o.note(format!(
            "{tag}: echo on the link ({} frames at {rate} Hz), against the phrase: {}",
            echo.len(),
            c.summary()
        ));
    }
    // The strict checks: astar-server's echo against the over it received.
    // Whatever astar-server adds or loses shows here; what the node did to
    // the phrase on its way in does not.
    let Some(c) = speech::compare(&over_pcm, over_rate, &samples, rate) else {
        o.check(Check::fail(
            format!("{tag}: echo on the link"),
            "the over carried no speech",
        ));
        return false;
    };
    let label = format!("{tag}: echo on the link");
    o.note(format!("{label}, against the over: {}", c.summary()));
    speech_checks(o, &label, &c, &parrot_thresholds(), true);
    match node_unkey(ctx, &full) {
        Some(end) => timing_checks(
            o,
            tag,
            (first - end) * 1000.0,
            "astar-server's first echo frame after the node's UNKEY, on the wire",
        ),
        None => {
            o.check(Check::fail(
                format!("{tag}: echo began after the over ended"),
                "no UNKEY from the node in the capture",
            ));
        }
    }
    true
}

fn slug(tag: &str) -> String {
    tag.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// One over on an established link: the phrase keyed into the node, its
/// echo checked on the link and at the radio, and the link checked still up
/// afterwards. `mark` is set to the pod clock at the over's start. Returns
/// `false` when the over could not even be made.
#[allow(clippy::too_many_lines)]
fn one_over(
    ctx: &mut Ctx,
    o: &mut Outcome,
    tag: &str,
    usrp: &UsrpProbe,
    leg: &ServerLeg,
    mark: &mut Option<f64>,
) -> bool {
    // The node keys up for its own telemetry when a link connects.
    if let Err(e) = common::wait_node_quiet(usrp, Duration::from_secs(60)) {
        o.check(Check::fail(format!("{tag}: node quiet before the over"), e));
        return false;
    }
    if let Err(e) = common::both_up(ctx, leg, None) {
        o.check(Check::fail(format!("{tag}: link up before the over"), e));
        return false;
    }
    let phrase = speech::link_test_phrase();
    #[allow(clippy::cast_precision_loss)]
    let phrase_secs = phrase.samples.len() as f64 / f64::from(phrase.rate);
    let listen = Duration::from_secs_f64(
        2.0f64.mul_add(phrase_secs, 5.0)
            + Duration::from_millis(PARROT_GAP_MS + PARROT_DELAY_MS).as_secs_f64(),
    );
    *mark = pcap::pod_now(&ctx.rig).ok();
    let rec = usrp.record_any_in_background(listen);
    let tx = match usrp.transmit(&phrase.samples) {
        Ok(tx) => tx,
        Err(e) => {
            o.check(Check::fail(
                format!("{tag}: phrase sent"),
                format!("USRP: {e}"),
            ));
            return false;
        }
    };
    let Some(frames) = rec.ok().and_then(|h| h.join().ok()).and_then(Result::ok) else {
        o.check(Check::fail(
            format!("{tag}: echo recorded"),
            "USRP recorder failed",
        ));
        return false;
    };
    let path = ctx.dir.join(format!("{}-node-transmitted.wav", slug(tag)));
    if audio::write_wav(&path, SAMPLE_RATE, &crate::probe::concat(&frames)).is_ok() {
        o.artifacts.push(path);
    }
    wire_checks(ctx, o, tag, *mark);
    // The node is a full-duplex repeater: while the bench keys it, it
    // repeats the phrase on its own transmitter. Only what it sends after
    // the over ended can be the echo. This last hop (app_rpt's transmit
    // path, chan_usrp, the relay to the bench) now and then repeats or
    // drops a 20 ms frame of its own (RIG.md, "Speech echo"), so here the
    // echo must arrive whole and at the right level, and its timeline is
    // noted rather than checked.
    let after: Vec<RxFrame> = frames
        .iter()
        .filter(|f| f.at >= tx.finished)
        .cloned()
        .collect();
    let echo = Timeline::of(&after);
    let path = ctx.dir.join(format!("{}-echo-at-radio.wav", slug(tag)));
    if audio::write_wav(&path, SAMPLE_RATE, &echo.samples).is_ok() {
        o.artifacts.push(path);
    }
    o.note(format!(
        "{tag}: {} USRP frames in {listen:.1?}; {} voice frames after the over ({} lost, filled with silence)",
        frames.len(),
        echo.arrived.len(),
        echo.filled
    ));
    if let Some(c) = speech::compare(&phrase.samples, phrase.rate, &echo.samples, SAMPLE_RATE) {
        let label = format!("{tag}: echo at the node's radio");
        speech_checks(o, &label, &c, &radio_thresholds(), false);
        if let Some(at) = echo.arrival_of(c.offset_samples) {
            let ms = at.saturating_duration_since(tx.finished).as_secs_f64() * 1000.0;
            o.note(format!(
                "{label}: began {ms:.0} ms after the bench unkeyed (bench clock)"
            ));
        }
    }
    link_up_after(ctx, o, tag, leg);
    true
}

/// One over into the parrot and its echo back, on whatever node and server
/// config is in place.
#[allow(clippy::too_many_lines)]
fn parrot_cell(ctx: &mut Ctx, o: &mut Outcome, tag: &str) {
    if let Err(e) = clean_start(ctx) {
        o.check(Check::fail(format!("{tag}: starts unlinked"), e));
        return;
    }
    let _ = ctx.node.enable_debug();
    let events = ctx.server.events().ok();
    let leg = match node_dials_server(ctx, Ilink::Transceive, None) {
        Ok((leg, t)) => {
            o.check(Check::pass(
                format!("{tag}: node links to the parrot"),
                format!("{leg} up after {t:.1?}"),
            ));
            leg
        }
        Err(e) => {
            ctx.mark_drop();
            o.check(Check::fail(format!("{tag}: node links to the parrot"), e));
            return;
        }
    };
    let usrp = match common::usrp_probe() {
        Ok(u) => u,
        Err(e) => {
            o.check(Check::fail(format!("{tag}: USRP probe"), e));
            return;
        }
    };
    // Two overs on the same link: the second proves the node can talk
    // again after its first echo (the parrot keeps a node link up).
    let mut mark = None;
    for label in [tag.to_string(), format!("{tag}, second over")] {
        if !one_over(ctx, o, &label, &usrp, &leg, &mut mark) {
            break;
        }
    }
    if o.failed() {
        ctx.mark_drop();
        if let Some(why) = pcap::full_frames(&ctx.rig, mark)
            .ok()
            .and_then(|w| common::explain_teardown(ctx, &w))
        {
            o.note(format!("{tag}: teardown on the wire: {why}"));
        }
        common::save_cell_frames(ctx, o, tag, mark);
    }
    common::note_server_events(events.as_ref(), o);
    let _ = ctx.node.ilink(Ilink::Disconnect, &ctx.cfg().server_number);
}

/// The node's radio speaks the phrase; astar-server's parrot echoes it over
/// the link; the node transmits the echo.
pub struct ParrotEchoesSpeech;

impl Scenario for ParrotEchoesSpeech {
    fn id(&self) -> &'static str {
        "parrot_echoes_speech"
    }

    fn describe(&self) -> &'static str {
        "node keys the spoken phrase into astar-server's parrot twice on one link; each echo comes back whole and on time, and the link stays up (normal + ulaw-only node)"
    }

    fn run(&self, ctx: &mut Ctx) -> Outcome {
        let mut o = Outcome::new();
        let ms = |v: u64| toml::Value::Integer(i64::try_from(v).unwrap_or(i64::MAX));
        common::with_server_overlay(
            ctx,
            &mut o,
            |t| {
                common::set_key(t, "bridge", "mode", toml::Value::String("parrot".into()));
                common::set_key(t, "parrot", "playback_delay_ms", ms(PARROT_DELAY_MS));
                common::set_key(t, "parrot", "silence_gap_ms", ms(PARROT_GAP_MS));
            },
            |ctx, o| {
                parrot_cell(ctx, o, "default node");
                let ulaw = NodeOverlay {
                    codecs: Some("ulaw".into()),
                    ..NodeOverlay::default()
                };
                common::with_node_overlay(ctx, o, &ulaw, |ctx, o| {
                    parrot_cell(ctx, o, "ulaw node");
                });
            },
        );
        o
    }
}

// ---- the other way: the bridge to the node ----------------------------------

/// What astar-server's bridge put on the link toward the node must match,
/// against the probe's audio as it reached astar-server. Set from measured
/// runs (RIG.md, "Speech echo").
#[must_use]
pub fn bridge_thresholds() -> SpeechThresholds {
    parrot_thresholds()
}

/// What the node transmits must match, against the phrase. Like the echo at
/// the radio, it has crossed `app_rpt` and the host relay.
#[must_use]
pub fn bridge_radio_thresholds() -> SpeechThresholds {
    radio_thresholds()
}

/// How the link is brought up in a `speech_server_to_node` cell.
#[derive(Debug, Clone, Copy)]
enum Dial {
    ServerDialsNode,
    NodeDialsServer,
}

/// The bridge's input from the probe and its output to the node, out of
/// the capture: the output must match the input (strict); each against the
/// phrase is a note.
fn bridge_wire_checks(ctx: &Ctx, o: &mut Outcome, tag: &str, mark: Option<f64>) {
    let (np, sp) = (ctx.cfg().node_iax.port(), ctx.cfg().server_iax.port());
    let phrase = speech::link_test_phrase();
    let label = format!("{tag}: on the link");
    let input = pcap::voice_frames(&ctx.rig, None, sp, mark);
    let output = pcap::voice_frames(&ctx.rig, None, np, mark);
    let (Ok(input), Ok(output)) = (input, output) else {
        o.check(Check::fail(label, "could not read the capture"));
        return;
    };
    let Some((irate, ipcm, _)) = wire_audio(&input) else {
        o.check(Check::fail(
            label,
            "no voice from the probe reached astar-server",
        ));
        return;
    };
    let Some((orate, opcm, _)) = wire_audio(&output) else {
        o.check(Check::fail(label, "astar-server sent the node no voice"));
        return;
    };
    let path = ctx.dir.join(format!("{}-on-link.wav", slug(tag)));
    if audio::write_wav(&path, orate, &opcm).is_ok() {
        o.artifacts.push(path);
    }
    for (what, rate, pcm, n) in [
        ("probe -> astar-server", irate, &ipcm, input.len()),
        ("astar-server -> node", orate, &opcm, output.len()),
    ] {
        if let Some(c) = speech::compare(&phrase.samples, phrase.rate, pcm, rate) {
            o.note(format!(
                "{tag}: {what} ({n} frames at {rate} Hz), against the phrase: {}",
                c.summary()
            ));
        }
    }
    match speech::compare(&ipcm, irate, &opcm, orate) {
        Some(c) => {
            o.note(format!(
                "{label}, against the bridge's input: {}",
                c.summary()
            ));
            speech_checks(o, &label, &c, &bridge_thresholds(), true);
        }
        None => {
            o.check(Check::fail(label, "the probe's input carried no speech"));
        }
    }
}

/// One cell: link up, the probe speaks the phrase into the bridge, and the
/// node's transmitter and the wire are checked.
#[allow(clippy::too_many_lines)]
fn bridge_cell(ctx: &mut Ctx, o: &mut Outcome, tag: &str, dial: Dial) {
    if let Err(e) = clean_start(ctx) {
        o.check(Check::fail(format!("{tag}: starts unlinked"), e));
        return;
    }
    let _ = ctx.node.enable_debug();
    let events = ctx.server.events().ok();
    let up = match dial {
        Dial::ServerDialsNode => server_dials_node(ctx, LinkAction::Connect, None),
        Dial::NodeDialsServer => node_dials_server(ctx, Ilink::Transceive, None),
    };
    let leg = match up {
        Ok((leg, t)) => {
            o.check(Check::pass(
                format!("{tag}: link up"),
                format!("{leg} up after {t:.1?}"),
            ));
            leg
        }
        Err(e) => {
            ctx.mark_drop();
            o.check(Check::fail(format!("{tag}: link up"), e));
            return;
        }
    };
    let usrp = match common::usrp_probe() {
        Ok(u) => u,
        Err(e) => {
            o.check(Check::fail(format!("{tag}: probes connect"), e));
            return;
        }
    };
    let probe = match common::iax_probe(ctx) {
        Ok(p) => p,
        Err(e) => {
            o.check(Check::fail(format!("{tag}: probes connect"), e));
            return;
        }
    };
    if let Err(e) = common::wait_node_quiet(&usrp, Duration::from_secs(60)) {
        o.check(Check::fail(format!("{tag}: node quiet before the over"), e));
        return;
    }
    if let Err(e) = common::both_up(ctx, &leg, None) {
        o.check(Check::fail(format!("{tag}: link up before the over"), e));
        return;
    }
    let phrase = speech::link_test_phrase();
    let rate = probe.sample_rate();
    let sent = speech::resample(&phrase.samples, phrase.rate, rate);
    #[allow(clippy::cast_precision_loss)]
    let phrase_secs = phrase.samples.len() as f64 / f64::from(phrase.rate);
    let listen = Duration::from_secs_f64(phrase_secs + 6.0);
    let mark = pcap::pod_now(&ctx.rig).ok();
    let rec = usrp.record_any_in_background(listen);
    let tx = match probe.transmit(&sent) {
        Ok(tx) => tx,
        Err(e) => {
            o.check(Check::fail(
                format!("{tag}: phrase sent"),
                format!("probe: {e}"),
            ));
            return;
        }
    };
    let Some(frames) = rec.ok().and_then(|h| h.join().ok()).and_then(Result::ok) else {
        o.check(Check::fail(
            format!("{tag}: node recorded"),
            "USRP recorder failed",
        ));
        return;
    };
    let path = ctx.dir.join(format!("{}-node-transmitted.wav", slug(tag)));
    if audio::write_wav(&path, SAMPLE_RATE, &crate::probe::concat(&frames)).is_ok() {
        o.artifacts.push(path);
    }

    // On the wire: what reached astar-server from the probe, and what its
    // bridge sent the node. The probe's path in from the host (podman's port
    // forwarding) has jitter of its own, so the strict checks hold the
    // bridge's output to its input; both against the phrase are notes.
    bridge_wire_checks(ctx, o, tag, mark);

    // At the node's radio: it keys for the over, carries the phrase, and
    // unkeys after it.
    let keyed: Vec<&RxFrame> = frames
        .iter()
        .filter(|f| f.keyed && f.samples.len() == VOICE_SAMPLES)
        .collect();
    let first_key = keyed.first().map(|f| f.at);
    let last_key = keyed.last().map(|f| f.at);
    let unkeyed_after = last_key.is_some_and(|t| frames.iter().any(|f| f.at >= t && !f.keyed));
    o.check(
        Check::expect(
            format!("{tag}: node keys up for the over"),
            first_key.is_some(),
            first_key.map_or_else(
                || "the node never keyed its transmitter".to_string(),
                |t| {
                    format!(
                        "first keyed frame {:.0} ms after the probe keyed; {} keyed frames",
                        t.saturating_duration_since(tx.started).as_secs_f64() * 1000.0,
                        keyed.len()
                    )
                },
            ),
        )
        .measured(
            first_key.map_or(f64::NAN, |t| {
                t.saturating_duration_since(tx.started).as_secs_f64() * 1000.0
            }),
            "ms",
        ),
    );
    let unkey_ms = last_key.map(|t| {
        if t >= tx.finished {
            t.duration_since(tx.finished).as_secs_f64() * 1000.0
        } else {
            -(tx.finished.duration_since(t).as_secs_f64() * 1000.0)
        }
    });
    o.check(
        Check::expect(
            format!("{tag}: node unkeys after the over"),
            unkeyed_after,
            format!(
                "last keyed frame {} ms after the probe unkeyed (hang time and courtesy tone included); unkey packet seen: {unkeyed_after}",
                unkey_ms.map_or_else(|| "-".into(), |v| format!("{v:.0}"))
            ),
        )
        .measured(unkey_ms.unwrap_or(f64::NAN), "ms"),
    );
    let owned: Vec<RxFrame> = frames.clone();
    let radio = Timeline::of(&owned);
    if let Some(c) = speech::compare(&phrase.samples, phrase.rate, &radio.samples, SAMPLE_RATE) {
        let label = format!("{tag}: at the node's radio");
        o.note(format!(
            "{label} ({} frames, {} lost): {}",
            radio.arrived.len(),
            radio.filled,
            c.summary()
        ));
        speech_checks(o, &label, &c, &bridge_radio_thresholds(), false);
    }

    let h = super::hold_for(AFTER_ECHO_HOLD, Duration::from_millis(500), || {
        common::both_up(ctx, &leg, None)?;
        if probe.is_up() {
            Ok(())
        } else {
            Err("the probe's call is gone".into())
        }
    });
    o.check(h.to_check(&format!(
        "{tag}: link and probe still up {} s after the over",
        AFTER_ECHO_HOLD.as_secs()
    )));
    if o.failed() {
        ctx.mark_drop();
        common::save_cell_frames(ctx, o, tag, mark);
        common::note_newkey(ctx, o, mark);
    }
    common::note_server_events(events.as_ref(), o);
    let _ = probe.hangup();
    let _ = ctx.node.ilink(Ilink::Disconnect, &ctx.cfg().server_number);
    if let Ok(st) = ctx.server.status() {
        for l in &st.links {
            let _ = ctx.server.link(LinkAction::Disconnect, &l.node, None);
        }
    }
}

/// The IAX probe speaks the phrase into astar-server's bridge; the node
/// transmits it.
pub struct SpeechServerToNode;

impl Scenario for SpeechServerToNode {
    fn id(&self) -> &'static str {
        "speech_server_to_node"
    }

    fn describe(&self) -> &'static str {
        "IAX probe speaks the phrase into astar-server's bridge -> link -> node -> USRP: whole, edges, level, keying, link stays up (both dial directions, normal + ulaw-only node)"
    }

    fn run(&self, ctx: &mut Ctx) -> Outcome {
        let mut o = Outcome::new();
        bridge_cell(
            ctx,
            &mut o,
            "default node, server dials",
            Dial::ServerDialsNode,
        );
        bridge_cell(
            ctx,
            &mut o,
            "default node, node dials",
            Dial::NodeDialsServer,
        );
        let ulaw = NodeOverlay {
            codecs: Some("ulaw".into()),
            ..NodeOverlay::default()
        };
        common::with_node_overlay(ctx, &mut o, &ulaw, |ctx, o| {
            bridge_cell(ctx, o, "ulaw node, server dials", Dial::ServerDialsNode);
            bridge_cell(ctx, o, "ulaw node, node dials", Dial::NodeDialsServer);
        });
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq: u32, at: Instant, level: i16) -> RxFrame {
        RxFrame {
            at,
            keyed: true,
            seq: Some(seq),
            ts_ms: None,
            samples: vec![level; VOICE_SAMPLES],
        }
    }

    #[test]
    fn a_lost_frame_is_filled_in_place() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let frames = [
            frame(10, ms(0), 1),
            frame(11, ms(20), 2),
            frame(13, ms(60), 4),
            // An unkey packet carries no samples and is skipped.
            RxFrame {
                samples: Vec::new(),
                ..frame(14, ms(70), 0)
            },
            // A long skip is a new transmission, not loss.
            frame(100, ms(5000), 5),
        ];
        let t = Timeline::of(&frames);
        assert_eq!(t.filled, 1);
        assert_eq!(t.arrived.len(), 5);
        let levels: Vec<i16> = t.samples.chunks(VOICE_SAMPLES).map(|c| c[0]).collect();
        assert_eq!(levels, [1, 2, 0, 4, 5]);
        // Sample 2.5 frames in is the filled frame: time the next one.
        let at = i64::try_from(2 * VOICE_SAMPLES + 80).unwrap();
        assert_eq!(t.arrival_of(at), Some(ms(60)));
        assert_eq!(t.arrival_of(-50), Some(ms(0)));
        assert_eq!(t.arrival_of(1_000_000), None);
    }
}
