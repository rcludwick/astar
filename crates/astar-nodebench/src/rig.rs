// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The rig: bring a topology up and down with `podman kube play`, wait for it
//! to be ready, and collect its logs and captures into the run directory.
//!
//! podman is driven through `std::process::Command`. The connection is
//! whatever `CONTAINER_CONNECTION` names (on the Mac,
//! `podman-machine-default`); [`RigConfig::connection`] can pin it, and it is
//! then passed to every podman invocation through that same variable.
//!
//! Every name, port and path the bench depends on lives in [`RigConfig`], so
//! the harness files can change shape without touching the scenarios.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde::Serialize;

/// Which pod layout to play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    /// One pod, one network namespace, everything on 127.0.0.1.
    Loopback,
    /// node ⇄ NAT ⇄ server over a `--internal` network (soak tier only).
    Nat,
}

impl Topology {
    #[must_use]
    pub fn kube_file(self) -> &'static str {
        match self {
            Self::Loopback => "pod.yaml",
            Self::Nat => "nat.yaml",
        }
    }
}

impl std::str::FromStr for Topology {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "loopback" => Ok(Self::Loopback),
            "nat" => Ok(Self::Nat),
            _ => Err(format!("unknown topology {s:?} (loopback | nat)")),
        }
    }
}

impl fmt::Display for Topology {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Loopback => "loopback",
            Self::Nat => "nat",
        })
    }
}

/// Everything the bench needs to know about the rig. Defaults match the
/// design (`docs: nodebench design`); the harness owner can change any of
/// them in one place.
#[derive(Debug, Clone, Serialize)]
pub struct RigConfig {
    /// Repository root (for `git rev-parse` and the default paths).
    pub repo_root: PathBuf,
    /// `harness/nodebench` — holds `pod.yaml` / `nat.yaml`.
    pub rig_dir: PathBuf,
    pub topology: Topology,
    /// podman executable.
    pub podman: String,
    /// `CONTAINER_CONNECTION` to pin, or `None` to inherit the environment.
    pub connection: Option<String>,
    /// Extra args for `podman kube play` (e.g. `--build`).
    pub kube_play_args: Vec<String>,
    /// `@KEY@` placeholders in the kube file and their values. The harness's
    /// `pod.yaml` is a template (`rig.sh` renders it the same way); a
    /// placeholder with no value here is an error, so template drift is loud.
    /// Scenario variants (ulaw-only node, register config) change these and
    /// replay the pod.
    pub kube_vars: BTreeMap<String, String>,
    /// Recreate the pod if it already exists (`kube play --replace`).
    pub replace: bool,

    /// Pod name from the kube file.
    pub pod: String,
    /// Container names as podman reports them (`<pod>-<container>`).
    pub node_container: String,
    pub server_container: String,
    /// The tshark sidecar, if the topology has one.
    pub capture_container: Option<String>,
    /// Where the sidecar writes pcaps, inside its container.
    pub capture_dir: String,
    /// Asterisk log files to copy out of the node container.
    pub node_log_files: Vec<String>,

    /// ASL3 node number (private test number; never registered).
    pub node_number: String,
    /// astar-server's node number (private test number; never registered).
    pub server_number: String,
    /// Where the ASL3 node's IAX2 listens (inside the pod).
    pub node_iax: SocketAddr,
    /// Where astar-server's IAX2 listener is (inside the pod).
    pub server_iax: SocketAddr,
    /// astar-server's IAX2 listener as published on the host (the IAX probe
    /// dials this). Host ports are offset by 20000 so stray local tests that
    /// dial 127.0.0.1:4569/4570/8730 never land in the rig.
    pub server_iax_host: SocketAddr,

    /// AMI, published on 127.0.0.1.
    pub ami: SocketAddr,
    pub ami_user: String,
    /// Test-only AMI secret. Skipped in serialization; redacted in `Debug`.
    #[serde(skip)]
    pub ami_secret: Secret,
    /// astar-server control channel, published on 127.0.0.1.
    pub server_url: String,

    /// How long `up` waits for both sides to be ready.
    pub ready_timeout: Duration,
    pub ready_poll: Duration,
}

/// A credential that never shows up in `Debug` output.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// The value, for the one place that sends it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Default test-only AMI secret. Overridden by `NODEBENCH_AMI_SECRET`.
pub const DEFAULT_AMI_SECRET: &str = "nodebench-loopback-only";

impl RigConfig {
    /// Defaults rooted at `repo_root`.
    #[must_use]
    pub fn new(repo_root: PathBuf) -> Self {
        let rig_dir = repo_root.join("harness").join("nodebench");
        Self {
            repo_root,
            rig_dir,
            topology: Topology::Loopback,
            podman: "podman".into(),
            connection: std::env::var("CONTAINER_CONNECTION")
                .ok()
                .filter(|s| !s.is_empty()),
            kube_play_args: Vec::new(),
            kube_vars: default_kube_vars(),
            replace: false,
            pod: "nodebench".into(),
            node_container: "nodebench-asl3".into(),
            server_container: "nodebench-astar-server".into(),
            capture_container: Some("nodebench-capture".into()),
            capture_dir: "/captures".into(),
            node_log_files: vec!["/var/log/asterisk/full".into()],
            node_number: "1999".into(),
            server_number: "1998".into(),
            node_iax: SocketAddr::from(([127, 0, 0, 1], 4569)),
            server_iax: SocketAddr::from(([127, 0, 0, 1], 4570)),
            server_iax_host: SocketAddr::from(([127, 0, 0, 1], 24570)),
            ami: SocketAddr::from(([127, 0, 0, 1], 25038)),
            ami_user: "nodebench".into(),
            ami_secret: Secret::new(
                std::env::var("NODEBENCH_AMI_SECRET").unwrap_or_else(|_| DEFAULT_AMI_SECRET.into()),
            ),
            server_url: "http://127.0.0.1:28730".into(),
            ready_timeout: Duration::from_secs(120),
            ready_poll: Duration::from_millis(500),
        }
    }

    /// The repo root this binary was built from (`CARGO_MANIFEST_DIR/../..`).
    #[must_use]
    pub fn default_repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from("."))
    }

    #[must_use]
    pub fn kube_file(&self) -> PathBuf {
        self.rig_dir.join(self.topology.kube_file())
    }

    #[must_use]
    pub fn podman(&self) -> Podman {
        Podman {
            bin: self.podman.clone(),
            connection: self.connection.clone(),
        }
    }

    /// Every container the topology runs, for logs and environment info.
    #[must_use]
    pub fn containers(&self) -> Vec<&str> {
        let mut v = vec![self.node_container.as_str(), self.server_container.as_str()];
        if let Some(c) = &self.capture_container {
            v.push(c.as_str());
        }
        v
    }
}

/// Defaults for the harness template's placeholders (`rig.sh up` with no
/// options). `CAPTURES` is set per run to the run directory's `pcap/`.
#[must_use]
pub fn default_kube_vars() -> BTreeMap<String, String> {
    [
        ("CODECS", "slin16,slin,ulaw"),
        ("DEBUG", "0"),
        ("RUST_LOG", "debug"),
        ("CONFIG", "node.toml"),
        ("CAPTURES", "/tmp/nodebench-captures"),
        ("FILLCID", "0"),
        ("USRP_HOST", "host.containers.internal"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// Substitute every `@KEY@` in `template`. Fails naming any placeholder
/// that has no value.
pub fn render_kube(template: &str, vars: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut missing: Vec<String> = Vec::new();
    let mut rest = template;
    while let Some(i) = rest.find('@') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let key_len = after
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(after.len());
        if key_len > 0 && after[key_len..].starts_with('@') {
            let key = &after[..key_len];
            match vars.get(key) {
                Some(v) => out.push_str(v),
                None => {
                    if !missing.iter().any(|m| m == key) {
                        missing.push(key.to_string());
                    }
                }
            }
            rest = &after[key_len + 1..];
        } else {
            out.push('@');
            rest = after;
        }
    }
    out.push_str(rest);
    if missing.is_empty() {
        Ok(out)
    } else {
        Err(format!(
            "kube file placeholders with no value: {}",
            missing.join(", ")
        ))
    }
}

/// A failed podman invocation.
#[derive(Debug)]
pub enum RigError {
    /// podman could not be started at all.
    Spawn {
        cmd: String,
        err: io::Error,
    },
    /// podman ran and exited non-zero.
    Failed {
        cmd: String,
        code: Option<i32>,
        stderr: String,
    },
    /// A readiness wait ran out.
    NotReady {
        what: String,
        last: String,
    },
    Io(io::Error),
}

impl fmt::Display for RigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { cmd, err } => write!(f, "could not run `{cmd}`: {err}"),
            Self::Failed { cmd, code, stderr } => {
                write!(f, "`{cmd}` exited {code:?}: {}", stderr.trim())
            }
            Self::NotReady { what, last } => write!(f, "{what} not ready: {last}"),
            Self::Io(e) => write!(f, "i/o: {e}"),
        }
    }
}

impl std::error::Error for RigError {}

impl From<io::Error> for RigError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// A podman executable plus the connection to use.
#[derive(Debug, Clone)]
pub struct Podman {
    pub bin: String,
    pub connection: Option<String>,
}

impl Podman {
    /// A `Command` for `podman <args>` with the connection applied.
    #[must_use]
    pub fn command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut c = Command::new(&self.bin);
        c.args(args);
        if let Some(conn) = &self.connection {
            c.env("CONTAINER_CONNECTION", conn);
        }
        c.stdin(Stdio::null());
        c
    }

    fn describe(args: &[&str]) -> String {
        format!("podman {}", args.join(" "))
    }

    /// Run and capture; `Ok` only on exit 0.
    pub fn output(&self, args: &[&str]) -> Result<Output, RigError> {
        let out = self.command(args).output().map_err(|err| RigError::Spawn {
            cmd: Self::describe(args),
            err,
        })?;
        if out.status.success() {
            Ok(out)
        } else {
            Err(RigError::Failed {
                cmd: Self::describe(args),
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            })
        }
    }

    /// Run with `input` on stdin; `Ok` (stdout) only on exit 0.
    pub fn run_stdin(&self, args: &[&str], input: &str) -> Result<String, RigError> {
        use std::io::Write as _;
        let spawn_err = |err| RigError::Spawn {
            cmd: Self::describe(args),
            err,
        };
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(spawn_err)?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(RigError::Failed {
                cmd: Self::describe(args),
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            })
        }
    }

    /// Run and return stdout as text.
    pub fn run(&self, args: &[&str]) -> Result<String, RigError> {
        Ok(String::from_utf8_lossy(&self.output(args)?.stdout).into_owned())
    }

    /// Run and report only whether it exited 0 (spawn failure is an error).
    pub fn succeeds(&self, args: &[&str]) -> Result<bool, RigError> {
        let st = self
            .command(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|err| RigError::Spawn {
                cmd: Self::describe(args),
                err,
            })?;
        Ok(st.success())
    }

    /// `podman exec <ctr> <cmd...>` → stdout.
    pub fn exec(&self, container: &str, cmd: &[&str]) -> Result<String, RigError> {
        let mut args = vec!["exec", container];
        args.extend_from_slice(cmd);
        self.run(&args)
    }

    /// `podman version` client and server versions.
    pub fn version(&self) -> Result<String, RigError> {
        let v = self.run(&[
            "version",
            "--format",
            "client={{.Client.Version}} server={{if .Server}}{{.Server.Version}}{{end}}",
        ])?;
        Ok(v.trim().to_string())
    }
}

/// What a container is running, for the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContainerImage {
    pub container: String,
    pub image: String,
    /// Local image id (a built image has no registry digest).
    pub image_id: String,
}

/// Parse `podman container inspect --format '{{.Name}}|{{.ImageName}}|{{.Image}}'`.
#[must_use]
pub fn parse_container_image(line: &str) -> Option<ContainerImage> {
    let mut p = line.trim().splitn(3, '|');
    let (Some(c), Some(i), Some(id)) = (p.next(), p.next(), p.next()) else {
        return None;
    };
    Some(ContainerImage {
        container: c.trim_start_matches('/').to_string(),
        image: i.to_string(),
        image_id: id.to_string(),
    })
}

/// A live (or about-to-be-live) rig.
#[derive(Debug)]
pub struct Rig {
    pub cfg: RigConfig,
    podman: Podman,
}

impl Rig {
    #[must_use]
    pub fn new(cfg: RigConfig) -> Self {
        let podman = cfg.podman();
        Self { cfg, podman }
    }

    #[must_use]
    pub fn podman(&self) -> &Podman {
        &self.podman
    }

    /// Check the kube file exists and podman answers.
    pub fn preflight(&self) -> Result<String, RigError> {
        let kube = self.cfg.kube_file();
        if !kube.is_file() {
            return Err(RigError::NotReady {
                what: "kube file".into(),
                last: format!("{} does not exist", kube.display()),
            });
        }
        self.podman.version()
    }

    /// Whether the pod exists already.
    pub fn is_up(&self) -> Result<bool, RigError> {
        self.podman.succeeds(&["pod", "exists", &self.cfg.pod])
    }

    /// The kube file rendered with [`RigConfig::kube_vars`].
    pub fn rendered(&self) -> Result<String, RigError> {
        let kube = self.cfg.kube_file();
        let template = fs::read_to_string(&kube)?;
        render_kube(&template, &self.cfg.kube_vars).map_err(|last| RigError::NotReady {
            what: format!("{}", kube.display()),
            last,
        })
    }

    /// Render the kube file and `podman kube play -` it.
    ///
    /// Refuses when the pod already exists (it may be someone else's run)
    /// unless [`RigConfig::replace`] is set, which adds `--replace`.
    pub fn up(&self) -> Result<(), RigError> {
        let yaml = self.rendered()?;
        if !self.cfg.replace && self.is_up()? {
            return Err(RigError::NotReady {
                what: format!("pod {}", self.cfg.pod),
                last: "already exists; pass --reuse to use it or --replace to recreate it".into(),
            });
        }
        if let Some(c) = self.cfg.kube_vars.get("CAPTURES") {
            fs::create_dir_all(c)?;
        }
        let mut args = vec!["kube", "play"];
        if self.cfg.replace {
            args.push("--replace");
        }
        args.extend(self.cfg.kube_play_args.iter().map(String::as_str));
        args.push("-");
        self.podman.run_stdin(&args, &yaml).map(|_| ())
    }

    /// `podman kube down -` with the rendered file, falling back to
    /// `podman pod rm -f` (as `rig.sh down` does).
    pub fn down(&self) -> Result<(), RigError> {
        if !self.is_up()? {
            return Ok(());
        }
        let kube_down = self
            .rendered()
            .and_then(|yaml| self.podman.run_stdin(&["kube", "down", "-"], &yaml));
        match kube_down {
            Ok(_) => Ok(()),
            Err(_) => self
                .podman
                .run(&["pod", "rm", "-f", &self.cfg.pod])
                .map(|_| ()),
        }
    }

    /// `podman restart` one container of the pod (the pod's network namespace
    /// and the other containers are untouched). Two seconds to stop cleanly.
    pub fn restart_container(&self, container: &str) -> Result<(), RigError> {
        self.podman
            .run(&["restart", "--time", "2", container])
            .map(|_| ())
    }

    /// The text of `path` inside `container`.
    pub fn read_file(&self, container: &str, path: &str) -> Result<String, RigError> {
        self.podman.exec(container, &["cat", path])
    }

    /// Replace `path` inside `container` with `contents`, staged through a
    /// file in `scratch` (`podman cp` works over a remote connection too).
    /// The staged copy is world-readable so a non-root service can read it.
    pub fn write_file(
        &self,
        container: &str,
        path: &str,
        contents: &str,
        scratch: &Path,
    ) -> Result<(), RigError> {
        fs::create_dir_all(scratch)?;
        let name = Path::new(path)
            .file_name()
            .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
        let local = scratch.join(format!("{container}-{name}"));
        fs::write(&local, contents)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&local, fs::Permissions::from_mode(0o644))?;
        }
        let dest = format!("{container}:{path}");
        self.podman
            .run(&["cp", &local.to_string_lossy(), &dest])
            .map(|_| ())
    }

    /// The last `lines` of a container's log (stdout and stderr together).
    pub fn log_tail(&self, container: &str, lines: usize) -> Result<String, RigError> {
        let n = lines.to_string();
        let out = self
            .podman
            .output(&["logs", "--timestamps", "--tail", &n, container])?;
        let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.stderr.is_empty() {
            s.push_str(&String::from_utf8_lossy(&out.stderr));
        }
        Ok(s)
    }

    /// The last `lines` of a file inside a container.
    pub fn file_tail(&self, container: &str, path: &str, lines: usize) -> Result<String, RigError> {
        let n = lines.to_string();
        self.podman.exec(container, &["tail", "-n", &n, path])
    }

    /// Image of each container, best effort (missing containers are skipped).
    #[must_use]
    pub fn images(&self) -> Vec<ContainerImage> {
        self.cfg
            .containers()
            .into_iter()
            .filter_map(|c| {
                self.podman
                    .run(&[
                        "container",
                        "inspect",
                        "--format",
                        "{{.Name}}|{{.ImageName}}|{{.Image}}",
                        c,
                    ])
                    .ok()
                    .and_then(|s| parse_container_image(&s))
            })
            .collect()
    }

    /// Copy logs and captures into `dir`. Returns what was written and a
    /// note for everything that could not be collected (never fails the run).
    #[must_use]
    pub fn collect(&self, dir: &Path) -> (Vec<PathBuf>, Vec<String>) {
        let mut files = Vec::new();
        let mut notes = Vec::new();
        let logs = dir.join("logs");
        if let Err(e) = fs::create_dir_all(&logs) {
            notes.push(format!("cannot create {}: {e}", logs.display()));
            return (files, notes);
        }
        for c in self.cfg.containers() {
            match self.podman.output(&["logs", "--timestamps", c]) {
                Ok(out) => {
                    let p = logs.join(format!("{c}.log"));
                    let mut body = out.stdout;
                    if !out.stderr.is_empty() {
                        body.extend_from_slice(b"\n--- stderr ---\n");
                        body.extend_from_slice(&out.stderr);
                    }
                    match fs::write(&p, body) {
                        Ok(()) => files.push(p),
                        Err(e) => notes.push(format!("write {}: {e}", p.display())),
                    }
                }
                Err(e) => notes.push(format!("logs for {c}: {e}")),
            }
        }
        for f in &self.cfg.node_log_files {
            let name = Path::new(f).file_name().map_or_else(
                || "asterisk.log".into(),
                |n| n.to_string_lossy().into_owned(),
            );
            let dest = logs.join(format!("asterisk-{name}"));
            let src = format!("{}:{f}", self.cfg.node_container);
            match self.podman.run(&["cp", &src, &dest.to_string_lossy()]) {
                Ok(_) => files.push(dest),
                Err(e) => notes.push(format!("copy {src}: {e}")),
            }
        }
        let captures_here = self
            .cfg
            .kube_vars
            .get("CAPTURES")
            .is_some_and(|c| Path::new(c).starts_with(dir));
        if captures_here {
            files.push(dir.join("pcap"));
        } else if let Some(cap) = &self.cfg.capture_container {
            let dest = dir.join("pcap");
            if let Err(e) = fs::create_dir_all(&dest) {
                notes.push(format!("cannot create {}: {e}", dest.display()));
            } else {
                let src = format!("{cap}:{}/.", self.cfg.capture_dir.trim_end_matches('/'));
                match self.podman.run(&["cp", &src, &dest.to_string_lossy()]) {
                    Ok(_) => files.push(dest),
                    Err(e) => notes.push(format!("copy captures {src}: {e}")),
                }
            }
        }
        (files, notes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_design() {
        let c = RigConfig::new(PathBuf::from("/repo"));
        assert_eq!(
            c.kube_file(),
            PathBuf::from("/repo/harness/nodebench/pod.yaml")
        );
        assert_eq!(c.ami.to_string(), "127.0.0.1:25038");
        assert_eq!(c.server_url, "http://127.0.0.1:28730");
        assert_eq!(c.node_number, "1999");
        assert_eq!(c.server_number, "1998");
        assert!(c.ami.ip().is_loopback() && c.node_iax.ip().is_loopback());
    }

    #[test]
    fn nat_topology_uses_nat_yaml() {
        let mut c = RigConfig::new(PathBuf::from("/repo"));
        c.topology = "nat".parse().unwrap();
        assert!(c.kube_file().ends_with("nat.yaml"));
        assert!("bogus".parse::<Topology>().is_err());
    }

    #[test]
    fn secret_not_serialized() {
        let mut c = RigConfig::new(PathBuf::from("/repo"));
        c.ami_secret = Secret::new("swordfish");
        let j = serde_json::to_string(&c).unwrap();
        assert!(!j.contains("swordfish"));
        assert!(!j.contains("ami_secret"));
        assert!(!format!("{c:?}").contains("swordfish"));
    }

    #[test]
    fn command_pins_connection() {
        let p = Podman {
            bin: "podman".into(),
            connection: Some("podman-machine-default".into()),
        };
        let c = p.command(["ps"]);
        let env: Vec<_> = c.get_envs().collect();
        assert!(env.iter().any(|(k, v)| *k == "CONTAINER_CONNECTION"
            && v.is_some_and(|v| v == "podman-machine-default")));
    }

    #[test]
    fn render_fills_placeholders() {
        let mut vars = default_kube_vars();
        vars.insert("CAPTURES".into(), "/run/pcap".into());
        let t = "value: \"@CODECS@\"\npath: \"@CAPTURES@\"\nmail: a@b.c @ x @lower@\n";
        let r = render_kube(t, &vars).unwrap();
        assert_eq!(
            r,
            "value: \"slin16,slin,ulaw\"\npath: \"/run/pcap\"\nmail: a@b.c @ x @lower@\n"
        );
    }

    #[test]
    fn render_names_missing_placeholders() {
        let e = render_kube("@NEWTHING@ @CODECS@ @NEWTHING@", &default_kube_vars()).unwrap_err();
        assert!(e.contains("NEWTHING"), "{e}");
        assert_eq!(e.matches("NEWTHING").count(), 1);
    }

    #[test]
    fn container_image_line() {
        let ci = parse_container_image("nodebench-asl3|localhost/nb-asl3:latest|3f2a9c\n").unwrap();
        assert_eq!(ci.container, "nodebench-asl3");
        assert_eq!(ci.image, "localhost/nb-asl3:latest");
        assert_eq!(ci.image_id, "3f2a9c");
        assert!(parse_container_image("garbage").is_none());
    }

    #[test]
    fn missing_podman_is_spawn_error() {
        let p = Podman {
            bin: "/nonexistent/podman-for-test".into(),
            connection: None,
        };
        assert!(matches!(p.run(&["version"]), Err(RigError::Spawn { .. })));
    }
}
