// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The `astar-netrec` binary itself. Only commands that never touch the
//! network run here: `check`, `status`, a `now` that names no net, and a `run`
//! whose config has no nets, so there is nothing it could ever join.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use astar_netrec::daemon::{DaemonState, STATE_FILE};

/// How long a daemon gets to start or to exit. Far longer than either takes.
const PATIENCE: Duration = Duration::from_secs(20);

fn temp(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "netrec-cli-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Write a config with one daily net into `dir`, and return its path.
fn config(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("netrec.toml");
    let text = format!(
        r#"[recorder]
root = "{}"
callsign = "W1AW"

[[net]]
name = "Evening net"
network = "m17"
host = "127.0.0.1"
port = 17000
module = "C"
when = "daily 19:00"
duration = "1h"
tz = "America/Denver"
"#,
        dir.join("archive").display()
    );
    fs::write(&path, text).unwrap();
    path
}

/// Write a config with no nets into `dir`, and return its path.
fn config_without_nets(dir: &Path) -> PathBuf {
    let path = dir.join("netrec.toml");
    let root = dir.join("archive");
    let text = format!(
        "[recorder]\nroot = \"{}\"\ncallsign = \"W1AW\"\n",
        root.display()
    );
    fs::write(&path, text).unwrap();
    path
}

/// A running `astar-netrec run`, killed if the test ends first.
struct Daemon(Child);

impl Daemon {
    fn start(config: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_astar-netrec"))
            .arg("--config")
            .arg(config)
            .arg("run")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self(child)
    }

    /// Wait for `status` to say the daemon has come up.
    fn wait_until_idle(&mut self, config: &Path) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let line = stdout(&netrec(config, &["status"]));
            if line.trim() == "idle, nothing scheduled" {
                return;
            }
            let exited = self.0.try_wait().unwrap();
            assert!(exited.is_none(), "the daemon exited: {exited:?}");
            assert!(Instant::now() < deadline, "never came up: {line}");
            sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "the daemon did not exit");
            sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn netrec(config: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_astar-netrec"))
        .arg("--config")
        .arg(config)
        .args(args)
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn check_lists_next_windows() {
    let dir = temp("check");
    let out = netrec(&config(&dir), &["check"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert_eq!(text.lines().count(), 10, "{text}");
    assert!(text.lines().all(|l| l.contains("Evening net")), "{text}");
}

#[test]
fn check_prints_warnings_and_exits_1_on_a_bad_config() {
    let dir = temp("bad");
    let path = config(&dir);
    let text = fs::read_to_string(&path).unwrap();

    fs::write(&path, text.replace("W1AW", "N0CALL")).unwrap();
    let out = netrec(&path, &["check"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("N0CALL"), "{}", stderr(&out));

    fs::write(&path, text.replace("daily 19:00", "someday")).unwrap();
    let out = netrec(&path, &["check"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("someday"), "{}", stderr(&out));
}

#[test]
fn status_with_nothing_running() {
    let dir = temp("status");
    let out = netrec(&config(&dir), &["status"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "not running");
}

#[test]
fn status_without_a_config_fails_and_writes_nothing() {
    let dir = temp("noconfig");
    let path = dir.join("netrec.toml");
    let out = netrec(&path, &["status"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!path.exists());
}

#[test]
fn now_with_an_unknown_net_exits_1_and_lists_the_names() {
    let dir = temp("now");
    let out = netrec(&config(&dir), &["now", "Nobody's net"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("Evening net"), "{}", stderr(&out));
}

#[test]
fn now_refuses_a_bad_length() {
    let dir = temp("length");
    let out = netrec(&config(&dir), &["now", "Evening net", "--for", "soon"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("soon"), "{}", stderr(&out));
}

#[test]
fn lock_is_freed_when_the_holder_dies() {
    let dir = temp("lock");
    let path = config_without_nets(&dir);
    let state_file = dir.join("archive").join(STATE_FILE);

    let mut first = Daemon::start(&path);
    first.wait_until_idle(&path);

    // A second recorder is told why it cannot start.
    let mut second = Daemon::start(&path);
    assert_eq!(second.wait_for_exit().code(), Some(1));
    let mut refusal = String::new();
    std::io::Read::read_to_string(second.0.stderr.as_mut().unwrap(), &mut refusal).unwrap();
    assert!(refusal.contains("another recorder is running"), "{refusal}");

    // SIGKILL, so nothing gets to clean up: this is what a power cut leaves.
    first.0.kill().unwrap();
    first.0.wait().unwrap();
    assert!(state_file.exists(), "a killed daemon leaves its state file");

    // After the reboot, its pid belongs to some other live process.
    let mut state: DaemonState = serde_json::from_slice(&fs::read(&state_file).unwrap()).unwrap();
    state.pid = std::process::id();
    fs::write(&state_file, serde_json::to_vec(&state).unwrap()).unwrap();
    let line = stdout(&netrec(&path, &["status"]));
    assert!(line.starts_with("not running (last seen"), "{line}");

    let mut third = Daemon::start(&path);
    third.wait_until_idle(&path);

    // A clean stop, by the signal a service manager sends.
    let term = Command::new("kill")
        .arg(third.0.id().to_string())
        .status()
        .unwrap();
    assert!(term.success());
    assert!(third.wait_for_exit().success());
    assert_eq!(stdout(&netrec(&path, &["status"])).trim(), "not running");
}
