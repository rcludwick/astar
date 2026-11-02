// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The run report: a table on stdout and `report.json` in the run directory
//! (`target/nodebench/<run-id>/` by default), with enough environment to
//! reproduce the run. The process exits non-zero when any check failed.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::rig::ContainerImage;
use crate::scenario::{Check, Outcome, Status};

/// Where and on what the run happened.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct EnvInfo {
    /// `git rev-parse HEAD`, with `-dirty` if the tree has changes.
    pub git_sha: Option<String>,
    pub podman_version: Option<String>,
    /// `CONTAINER_CONNECTION` in effect.
    pub podman_connection: Option<String>,
    pub images: Vec<ContainerImage>,
    pub host_os: String,
    pub host_arch: String,
    pub topology: String,
}

impl EnvInfo {
    /// Read the git state of `repo` (podman fields are filled by the caller).
    #[must_use]
    pub fn local(repo: &Path, topology: &str) -> Self {
        Self {
            git_sha: git_sha(repo),
            host_os: std::env::consts::OS.to_string(),
            host_arch: std::env::consts::ARCH.to_string(),
            topology: topology.to_string(),
            ..Self::default()
        }
    }
}

/// `git rev-parse HEAD` plus a `-dirty` suffix, or `None` outside a repo.
#[must_use]
pub fn git_sha(repo: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let dirty = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .is_ok_and(|o| !o.stdout.is_empty());
    Some(if dirty { format!("{sha}-dirty") } else { sha })
}

/// One scenario's result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScenarioResult {
    pub id: String,
    pub describe: String,
    pub duration_secs: f64,
    #[serde(flatten)]
    pub outcome: Outcome,
}

impl ScenarioResult {
    /// Worst status across the checks: any fail is a fail; all skipped (or
    /// none) is a skip; otherwise a pass.
    #[must_use]
    pub fn verdict(&self) -> Status {
        if self.outcome.failed() {
            Status::Fail
        } else if self.outcome.checks.is_empty() {
            Status::Skip("no checks".into())
        } else if self
            .outcome
            .checks
            .iter()
            .all(|c| matches!(c.status, Status::Skip(_)))
        {
            Status::Skip("all checks skipped".into())
        } else {
            Status::Pass
        }
    }
}

/// Counts across the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct Summary {
    pub scenarios: usize,
    pub checks: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
}

/// The whole run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub run_id: String,
    /// RFC 3339 UTC.
    pub started: String,
    pub finished: String,
    pub env: EnvInfo,
    pub scenarios: Vec<ScenarioResult>,
    /// Rig-level problems (a container that would not come up, a log that
    /// could not be copied). A rig error that stops the run also fails it.
    pub rig_notes: Vec<String>,
    /// Set when the rig itself failed, so no scenario could run.
    pub rig_error: Option<String>,
    pub summary: Summary,
}

impl Report {
    #[must_use]
    pub fn new(run_id: String, started: String, env: EnvInfo) -> Self {
        Self {
            run_id,
            finished: started.clone(),
            started,
            env,
            scenarios: Vec::new(),
            rig_notes: Vec::new(),
            rig_error: None,
            summary: Summary::default(),
        }
    }

    pub fn push(&mut self, r: ScenarioResult) {
        self.scenarios.push(r);
        self.summary = self.summarize();
    }

    fn summarize(&self) -> Summary {
        let checks: Vec<&Check> = self
            .scenarios
            .iter()
            .flat_map(|s| s.outcome.checks.iter())
            .collect();
        Summary {
            scenarios: self.scenarios.len(),
            checks: checks.len(),
            passed: checks.iter().filter(|c| c.status == Status::Pass).count(),
            failed: checks.iter().filter(|c| c.status == Status::Fail).count(),
            skipped: checks
                .iter()
                .filter(|c| matches!(c.status, Status::Skip(_)))
                .count(),
        }
    }

    /// Any check failed, or the rig did.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.rig_error.is_some() || self.summary.failed > 0
    }

    /// 0 = everything passed or skipped; 1 = a check failed; 2 = the rig
    /// failed before scenarios could run.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        if self.rig_error.is_some() {
            2
        } else {
            u8::from(self.summary.failed > 0)
        }
    }

    /// Write `report.json` into `dir`.
    pub fn write_json(&self, dir: &Path) -> io::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let p = dir.join("report.json");
        let body = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        std::fs::write(&p, body)?;
        Ok(p)
    }

    /// The stdout table. `color` adds ANSI colour to the verdicts.
    #[must_use]
    pub fn table(&self, color: bool) -> String {
        let paint = |s: &Status| -> String {
            let l = s.label();
            if !color {
                return l.to_string();
            }
            let code = match s {
                Status::Pass => "32",
                Status::Fail => "31;1",
                Status::Skip(_) => "33",
            };
            format!("\x1b[{code}m{l}\x1b[0m")
        };
        let name_w = self
            .scenarios
            .iter()
            .flat_map(|s| {
                std::iter::once(s.id.len()).chain(s.outcome.checks.iter().map(|c| c.name.len() + 2))
            })
            .max()
            .unwrap_or(8)
            .clamp(8, 48);
        let mut out = String::new();
        let _ = writeln!(
            out,
            "nodebench run {}  ({}, {})",
            self.run_id,
            self.env.topology,
            self.env.git_sha.as_deref().unwrap_or("no git sha")
        );
        if let Some(e) = &self.rig_error {
            let _ = writeln!(out, "{}  rig: {e}", paint(&Status::Fail));
        }
        for s in &self.scenarios {
            let _ = writeln!(
                out,
                "{}  {:<name_w$}  {:>6.1}s  {}",
                paint(&s.verdict()),
                s.id,
                s.duration_secs,
                s.describe
            );
            for c in &s.outcome.checks {
                let measured = match (c.measured, &c.unit) {
                    (Some(v), Some(u)) => format!("{v:.2} {u}"),
                    (Some(v), None) => format!("{v:.2}"),
                    _ => String::new(),
                };
                let _ = writeln!(
                    out,
                    "  {}  {:<w$}  {:>12}  {}",
                    paint(&c.status),
                    c.name,
                    measured,
                    c.detail,
                    w = name_w - 2
                );
            }
            for n in &s.outcome.notes {
                let _ = writeln!(out, "        note: {n}");
            }
        }
        for n in &self.rig_notes {
            let _ = writeln!(out, "rig note: {n}");
        }
        let m = &self.summary;
        let _ = writeln!(
            out,
            "{} scenarios, {} checks: {} passed, {} failed, {} skipped",
            m.scenarios, m.checks, m.passed, m.failed, m.skipped
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(id: &str, checks: Vec<Check>) -> ScenarioResult {
        ScenarioResult {
            id: id.into(),
            describe: format!("{id} does a thing"),
            duration_secs: 1.5,
            outcome: Outcome {
                checks,
                notes: vec!["a note".into()],
                artifacts: vec![],
            },
        }
    }

    fn report() -> Report {
        let mut env = EnvInfo::local(Path::new("/nonexistent"), "loopback");
        env.images.push(ContainerImage {
            container: "nodebench-asl3".into(),
            image: "localhost/nb-asl3:latest".into(),
            image_id: "abc123".into(),
        });
        Report::new(
            "20261011T201503Z".into(),
            "2026-10-11T20:15:03Z".into(),
            env,
        )
    }

    #[test]
    fn all_pass_exits_zero() {
        let mut r = report();
        r.push(result(
            "a",
            vec![Check::pass("x", "ok"), Check::skip("y", "no probe")],
        ));
        assert_eq!(r.summary.passed, 1);
        assert_eq!(r.summary.skipped, 1);
        assert!(!r.failed());
        assert_eq!(r.exit_code(), 0);
        assert_eq!(r.scenarios[0].verdict(), Status::Pass);
    }

    #[test]
    fn any_fail_exits_one() {
        let mut r = report();
        r.push(result("a", vec![Check::pass("x", "ok")]));
        r.push(result(
            "b",
            vec![Check::fail("holds 60 s", "broke at 1.8 s").measured(1.8, "s")],
        ));
        assert_eq!(r.summary.failed, 1);
        assert_eq!(r.exit_code(), 1);
        assert_eq!(r.scenarios[1].verdict(), Status::Fail);
    }

    #[test]
    fn rig_error_exits_two() {
        let mut r = report();
        r.rig_error = Some("pod would not start".into());
        assert_eq!(r.exit_code(), 2);
        assert!(r.table(false).contains("rig: pod would not start"));
    }

    #[test]
    fn all_skipped_scenario_is_skip() {
        let r = result("a", vec![Check::skip("x", "needs probe")]);
        assert!(matches!(r.verdict(), Status::Skip(_)));
        assert!(matches!(result("b", vec![]).verdict(), Status::Skip(_)));
    }

    #[test]
    fn table_shows_checks_measures_and_summary() {
        let mut r = report();
        r.push(result(
            "node_links_to_server",
            vec![Check::fail("link holds 60 s", "broke at 1.8 s").measured(1.8, "s")],
        ));
        let t = r.table(false);
        assert!(t.contains("FAIL  node_links_to_server"), "{t}");
        assert!(t.contains("link holds 60 s"), "{t}");
        assert!(t.contains("1.80 s"), "{t}");
        assert!(t.contains("note: a note"), "{t}");
        assert!(t.contains("1 scenarios, 1 checks: 0 passed, 1 failed, 0 skipped"));
        assert!(!t.contains('\x1b'));
        assert!(r.table(true).contains("\x1b[31;1mFAIL"));
    }

    #[test]
    fn json_round_trip_shape() {
        let mut r = report();
        r.push(result(
            "a",
            vec![
                Check::pass("x", "ok").measured(1000.0, "Hz"),
                Check::skip("y", "no probe"),
            ],
        ));
        let dir =
            std::env::temp_dir().join(format!("nodebench-report-test-{}", std::process::id()));
        let p = r.write_json(&dir).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(v["run_id"], "20261011T201503Z");
        assert_eq!(v["env"]["topology"], "loopback");
        assert_eq!(v["env"]["images"][0]["image_id"], "abc123");
        assert_eq!(v["summary"]["passed"], 1);
        let checks = &v["scenarios"][0]["checks"];
        assert_eq!(checks[0]["status"]["status"], "pass");
        assert_eq!(checks[0]["measured"], 1000.0);
        assert_eq!(checks[0]["unit"], "Hz");
        assert_eq!(checks[1]["status"]["status"], "skip");
        assert_eq!(checks[1]["status"]["reason"], "no probe");
        assert_eq!(v["scenarios"][0]["notes"][0], "a note");
    }

    #[test]
    fn git_sha_outside_repo_is_none() {
        assert_eq!(git_sha(Path::new("/nonexistent-nodebench-dir")), None);
    }
}
