// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Scenarios: the trait, the registry, and the timing helpers.
//!
//! A scenario runs against a live rig through [`Ctx`] and returns an
//! [`Outcome`] of named [`Check`]s. Each check passes, fails, or is skipped
//! with a reason; a scenario that cannot even start returns a failed check
//! saying why, never an empty outcome.
//!
//! [`wait_until`] waits for a condition; [`hold_for`] requires one to stay
//! true for a whole window and reports the first moment it broke (the
//! "link holds 60 s" checks).

mod audio_paths;
mod codecs;
pub mod common;
mod dtmf;
mod handshake;
mod keepalive;
mod links;
mod register;
mod smoke;
mod speech_echo;

use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

use crate::node::NodeDriver;
use crate::rig::{Rig, RigConfig};
use crate::server::ServerClient;
use crate::time::{Clock, SystemClock};

// ---- checks and outcomes ----------------------------------------------------

/// A check's verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "reason", rename_all = "snake_case")]
pub enum Status {
    Pass,
    Fail,
    /// Not run, and why.
    Skip(String),
}

impl Status {
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Skip(_) => "SKIP",
        }
    }
}

/// One named check inside a scenario.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    /// What was seen — enough to act on without opening the logs.
    pub detail: String,
    /// A number, when the check measured one (seconds held, Hz, ms...).
    pub measured: Option<f64>,
    /// The unit of `measured`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

impl Check {
    fn new(name: impl Into<String>, status: Status, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status,
            detail: detail.into(),
            measured: None,
            unit: None,
        }
    }

    #[must_use]
    pub fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Pass, detail)
    }

    #[must_use]
    pub fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Fail, detail)
    }

    #[must_use]
    pub fn skip(name: impl Into<String>, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self::new(name, Status::Skip(reason.clone()), reason)
    }

    /// Pass if `ok`, else fail, with the same detail.
    #[must_use]
    pub fn expect(name: impl Into<String>, ok: bool, detail: impl Into<String>) -> Self {
        Self::new(name, if ok { Status::Pass } else { Status::Fail }, detail)
    }

    #[must_use]
    pub fn measured(mut self, value: f64, unit: &str) -> Self {
        self.measured = Some(value);
        self.unit = Some(unit.to_string());
        self
    }

    #[must_use]
    pub fn is_fail(&self) -> bool {
        self.status == Status::Fail
    }
}

/// What a scenario produced.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Outcome {
    pub checks: Vec<Check>,
    /// Free-form observations worth keeping (timings, what the far side said).
    pub notes: Vec<String>,
    /// Files the scenario wrote (under its directory in the run dir).
    pub artifacts: Vec<PathBuf>,
}

impl Outcome {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a check and return whether it passed (handy for early exits).
    pub fn check(&mut self, c: Check) -> bool {
        let ok = c.status != Status::Fail;
        self.checks.push(c);
        ok
    }

    pub fn note(&mut self, n: impl Into<String>) {
        self.notes.push(n.into());
    }

    #[must_use]
    pub fn failed(&self) -> bool {
        self.checks.iter().any(Check::is_fail)
    }
}

// ---- context ----------------------------------------------------------------

/// Bench-wide knobs scenarios read.
#[derive(Debug, Clone, Serialize)]
pub struct Tuning {
    /// How long "holds" checks hold (the design says 60 s).
    pub hold: Duration,
    /// Poll interval during a hold.
    pub hold_poll: Duration,
    /// How long a link gets to come up.
    pub setup_timeout: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            hold: Duration::from_secs(60),
            hold_poll: Duration::from_secs(1),
            setup_timeout: Duration::from_secs(20),
        }
    }
}

/// Everything a scenario can drive.
#[derive(Debug)]
pub struct Ctx {
    pub rig: Rig,
    pub node: NodeDriver,
    pub server: ServerClient,
    pub tuning: Tuning,
    /// This scenario's artifact directory (`<run dir>/<scenario id>`).
    pub dir: PathBuf,
    /// Pod clock when the scenario started (pcap slicing).
    pub started_pod: Option<f64>,
    /// Pod clock when a hold first saw the link gone.
    pub drop_at: Option<f64>,
}

impl Ctx {
    #[must_use]
    pub fn new(rig: Rig, tuning: Tuning, dir: PathBuf) -> Self {
        let node = NodeDriver::new(&rig.cfg);
        let server = ServerClient::new(&rig.cfg.server_url);
        Self {
            rig,
            node,
            server,
            tuning,
            dir,
            started_pod: None,
            drop_at: None,
        }
    }

    /// Note "the link dropped now" (pod clock), once.
    pub fn mark_drop(&mut self) {
        if self.drop_at.is_none() {
            self.drop_at = crate::pcap::pod_now(&self.rig).ok();
        }
    }

    /// Keep what a failure needs: the last 50 IAX2 full frames before the
    /// drop (or the end of the scenario) from the tshark ring, and the tails
    /// of the Asterisk full log and the astar-server log.
    pub fn failure_artifacts(&self, o: &mut Outcome) {
        let _ = std::fs::create_dir_all(&self.dir);
        match crate::pcap::full_frames(&self.rig, self.started_pod) {
            Ok(frames) => {
                let until = self.drop_at.map_or(f64::INFINITY, |t| t + 1.0);
                let before: Vec<_> = frames.into_iter().filter(|f| f.t <= until).collect();
                let tail = &before[before.len().saturating_sub(50)..];
                let t0 = self
                    .started_pod
                    .unwrap_or_else(|| tail.first().map_or(0.0, |f| f.t));
                let text = format!(
                    "# last {} IAX2 full frames{} (t relative to scenario start, pod clock)\n{}",
                    tail.len(),
                    if self.drop_at.is_some() {
                        " up to 1 s after the drop"
                    } else {
                        ""
                    },
                    crate::pcap::render(tail, t0, |p| common::port_name(self, p))
                );
                let path = self.dir.join("last-frames.txt");
                if std::fs::write(&path, text).is_ok() {
                    o.artifacts.push(path);
                }
            }
            Err(e) => o.note(format!("no pcap frames: {e}")),
        }
        let node = self.cfg().node_container.clone();
        let server = self.cfg().server_container.clone();
        let wants = [
            (
                "asterisk-full.tail.log",
                self.rig.file_tail(&node, "/var/log/asterisk/full", 800),
            ),
            ("astar-server.tail.log", self.rig.log_tail(&server, 400)),
        ];
        for (name, got) in wants {
            match got {
                Ok(text) => {
                    let path = self.dir.join(name);
                    if std::fs::write(&path, text).is_ok() {
                        o.artifacts.push(path);
                    }
                }
                Err(e) => o.note(format!("{name}: {e}")),
            }
        }
    }

    #[must_use]
    pub fn cfg(&self) -> &RigConfig {
        &self.rig.cfg
    }
}

// ---- the trait and the registry ---------------------------------------------

/// One bench scenario.
pub trait Scenario {
    /// Stable id, used on the command line and in reports.
    fn id(&self) -> &'static str;
    /// One line for `nodebench list` and the report.
    fn describe(&self) -> &'static str;
    /// Run against the live rig.
    fn run(&self, ctx: &mut Ctx) -> Outcome;
}

/// Every scenario, in run order.
#[must_use]
pub fn registry() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(smoke::RigReady),
        Box::new(links::NodeLinksToServer),
        Box::new(links::ServerLinksToNode),
        Box::new(links::MonitorLink),
        Box::new(links::DisconnectFromEachSide),
        Box::new(links::UlawOnlyNode),
        Box::new(links::RelinkAfterDrop),
        Box::new(codecs::CodecMatrix),
        Box::new(codecs::AuthVariants),
        Box::new(audio_paths::AudioNodeToServer),
        Box::new(audio_paths::AudioServerToNode),
        Box::new(speech_echo::ParrotEchoesSpeech),
        Box::new(speech_echo::SpeechServerToNode),
        Box::new(keepalive::KeepaliveExchange),
        Box::new(dtmf::DtmfLinkCommand),
        Box::new(handshake::CalltokenRetransmit),
        // Last: it can exhaust the node's call numbers (RIG.md finding 2),
        // and it restarts the node afterwards to clear them.
        Box::new(register::RegisterWithNode),
    ]
}

/// Pick scenarios by id, in the order given; empty means all.
pub fn select(ids: &[String]) -> Result<Vec<Box<dyn Scenario>>, String> {
    let all = registry();
    if ids.is_empty() {
        return Ok(all);
    }
    let mut pool: Vec<Option<Box<dyn Scenario>>> = all.into_iter().map(Some).collect();
    let mut out = Vec::new();
    for id in ids {
        let slot = pool
            .iter_mut()
            .find(|s| s.as_ref().is_some_and(|s| s.id() == id))
            .ok_or_else(|| {
                let known: Vec<&str> = registry().iter().map(|s| s.id()).collect();
                format!("unknown scenario {id:?}; known: {}", known.join(", "))
            })?;
        out.extend(slot.take());
    }
    Ok(out)
}

// ---- timing helpers ---------------------------------------------------------

/// The result of a probe: `Ok` when the condition holds, `Err(why not)`.
pub type Probe = Result<(), String>;

/// `wait_until` ran out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitTimeout {
    pub waited: Duration,
    /// The last reason the probe gave.
    pub last: String,
}

/// Poll `probe` every `poll` until it returns `Ok` or `timeout` passes.
/// Returns how long it took.
pub fn wait_until_with(
    clock: &mut impl Clock,
    timeout: Duration,
    poll: Duration,
    mut probe: impl FnMut() -> Probe,
) -> Result<Duration, WaitTimeout> {
    let start = clock.elapsed();
    loop {
        let r = probe();
        let waited = clock.elapsed().saturating_sub(start);
        match r {
            Ok(()) => return Ok(waited),
            Err(last) if waited >= timeout => return Err(WaitTimeout { waited, last }),
            Err(_) => clock.sleep(
                poll.min(timeout.saturating_sub(waited))
                    .max(Duration::from_millis(1)),
            ),
        }
    }
}

/// [`wait_until_with`] on the real clock.
pub fn wait_until(
    timeout: Duration,
    poll: Duration,
    probe: impl FnMut() -> Probe,
) -> Result<Duration, WaitTimeout> {
    wait_until_with(&mut SystemClock::new(), timeout, poll, probe)
}

/// Where a hold broke.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Break {
    /// Time from the start of the hold to the failing sample.
    pub at: Duration,
    /// Time of the last good sample (the drop happened in between).
    pub last_good: Option<Duration>,
    pub reason: String,
}

/// The result of [`hold_for`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hold {
    pub required: Duration,
    /// How long it held: `required` on success, else the last good sample.
    pub held: Duration,
    pub samples: u32,
    pub broke: Option<Break>,
}

impl Hold {
    #[must_use]
    pub fn held_all(&self) -> bool {
        self.broke.is_none()
    }

    /// As a check, measured in seconds held.
    #[must_use]
    pub fn to_check(&self, name: &str) -> Check {
        let secs = self.held.as_secs_f64();
        match &self.broke {
            None => Check::pass(
                name,
                format!(
                    "held {:.1} s ({} samples)",
                    self.required.as_secs_f64(),
                    self.samples
                ),
            ),
            Some(b) => Check::fail(
                name,
                format!(
                    "broke at {:.1} s (last good {}): {}",
                    b.at.as_secs_f64(),
                    b.last_good.map_or_else(
                        || "never".to_string(),
                        |d| format!("{:.1} s", d.as_secs_f64())
                    ),
                    b.reason
                ),
            ),
        }
        .measured(secs, "s")
    }
}

/// Require `probe` to stay `Ok` for `duration`, sampling every `poll`
/// (the first sample is at t = 0, the last at or after `duration`). Stops at
/// the first failing sample and reports when it happened.
pub fn hold_for_with(
    clock: &mut impl Clock,
    duration: Duration,
    poll: Duration,
    mut probe: impl FnMut() -> Probe,
) -> Hold {
    let start = clock.elapsed();
    let poll = poll.max(Duration::from_millis(1));
    let mut samples = 0;
    let mut last_good: Option<Duration> = None;
    loop {
        let r = probe();
        samples += 1;
        let at = clock.elapsed().saturating_sub(start);
        match r {
            Ok(()) => {
                last_good = Some(at);
                if at >= duration {
                    return Hold {
                        required: duration,
                        held: duration,
                        samples,
                        broke: None,
                    };
                }
            }
            Err(reason) => {
                return Hold {
                    required: duration,
                    held: last_good.unwrap_or_default(),
                    samples,
                    broke: Some(Break {
                        at,
                        last_good,
                        reason,
                    }),
                };
            }
        }
        clock.sleep(
            poll.min(duration.saturating_sub(at))
                .max(Duration::from_millis(1)),
        );
    }
}

/// [`hold_for_with`] on the real clock.
pub fn hold_for(duration: Duration, poll: Duration, probe: impl FnMut() -> Probe) -> Hold {
    hold_for_with(&mut SystemClock::new(), duration, poll, probe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::fake::FakeClock;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn hold_succeeds_when_always_true() {
        let mut c = FakeClock::default();
        let h = hold_for_with(&mut c, 60 * S, S, || Ok(()));
        assert!(h.held_all());
        assert_eq!(h.held, 60 * S);
        assert_eq!(h.samples, 61, "t=0..=60");
        assert_eq!(c.now, 60 * S);
        assert_eq!(h.to_check("holds").status, Status::Pass);
        assert_eq!(h.to_check("holds").measured, Some(60.0));
    }

    #[test]
    fn hold_reports_first_failure() {
        let mut c = FakeClock::default();
        let mut n = 0;
        let h = hold_for_with(&mut c, 60 * S, S, || {
            n += 1;
            if n <= 3 {
                Ok(())
            } else {
                Err(format!("link gone on sample {n}"))
            }
        });
        let b = h.broke.clone().unwrap();
        assert_eq!(b.at, 3 * S);
        assert_eq!(b.last_good, Some(2 * S));
        assert_eq!(b.reason, "link gone on sample 4");
        assert_eq!(h.held, 2 * S);
        assert_eq!(h.samples, 4);
        let ch = h.to_check("holds");
        assert!(ch.is_fail());
        assert!(ch.detail.contains("broke at 3.0 s"), "{}", ch.detail);
        assert_eq!(ch.measured, Some(2.0));
    }

    #[test]
    fn hold_failing_at_once_has_no_last_good() {
        let mut c = FakeClock::default();
        let h = hold_for_with(&mut c, 10 * S, S, || Err("never up".into()));
        let b = h.broke.unwrap();
        assert_eq!(b.at, Duration::ZERO);
        assert_eq!(b.last_good, None);
        assert_eq!(h.held, Duration::ZERO);
    }

    #[test]
    fn hold_with_uneven_poll_ends_exactly() {
        let mut c = FakeClock::default();
        let h = hold_for_with(&mut c, Duration::from_millis(2500), S, || Ok(()));
        assert!(h.held_all());
        assert_eq!(c.now, Duration::from_millis(2500));
        assert_eq!(h.samples, 4, "0, 1, 2, 2.5");
    }

    #[test]
    fn wait_until_returns_elapsed() {
        let mut c = FakeClock::default();
        let mut n = 0;
        let r = wait_until_with(&mut c, 10 * S, S, || {
            n += 1;
            if n == 4 {
                Ok(())
            } else {
                Err("not yet".into())
            }
        });
        assert_eq!(r, Ok(3 * S));
    }

    #[test]
    fn wait_until_times_out_with_last_reason() {
        let mut c = FakeClock::default();
        let mut n = 0;
        let r = wait_until_with(&mut c, Duration::from_millis(2500), S, || {
            n += 1;
            Err(format!("try {n}"))
        });
        let e = r.unwrap_err();
        assert_eq!(e.waited, Duration::from_millis(2500));
        assert_eq!(e.last, "try 4");
    }

    #[test]
    fn real_clock_helpers_work_quickly() {
        let h = hold_for(Duration::from_millis(20), Duration::from_millis(5), || {
            Ok(())
        });
        assert!(h.held_all());
        assert!(
            wait_until(Duration::from_millis(20), Duration::from_millis(5), || Ok(
                ()
            ))
            .is_ok()
        );
    }

    #[test]
    fn registry_ids_unique_and_selectable() {
        let ids: Vec<&str> = registry().iter().map(|s| s.id()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "duplicate ids: {ids:?}");
        assert!(ids.iter().all(|i| !i.is_empty()));
        assert_eq!(select(&[]).unwrap().len(), ids.len());
        let one = select(&["node_links_to_server".to_string()]).unwrap();
        assert_eq!(one.len(), 1);
        assert!(select(&["nope".to_string()]).is_err());
    }

    #[test]
    fn outcome_failed_tracks_checks() {
        let mut o = Outcome::new();
        assert!(o.check(Check::pass("a", "")));
        assert!(o.check(Check::skip("b", "no probe")));
        assert!(!o.failed());
        assert!(!o.check(Check::fail("c", "x")));
        assert!(o.failed());
    }
}
