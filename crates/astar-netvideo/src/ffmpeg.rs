// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Finding `ffmpeg` and checking it can make the videos.
//!
//! The recorder never links a video encoder: it pipes frames into an
//! installed `ffmpeg`, run as a separate program. [`find`] locates that
//! program and asks it two questions before anything relies on it: its
//! version (`-version`), and whether it has the H.264 encoder `libx264` and
//! the AAC encoder `aac` (`-encoders`). A build of `ffmpeg` without
//! `libx264` exists (some distributions ship one), and it would fail only
//! once a render was under way.
//!
//! Every child process here is reaped: a probe that hangs is killed after
//! [`PROBE_TIMEOUT`], and [`Child`] kills and waits for its process when it
//! is dropped. Every one starts through [`Child::spawn`], which strips the
//! recorder's `ASTAR_NETREC_*` variables (the stream keys among them) from
//! the child's environment.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{self, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Where a bare program name is looked for after `PATH`: Homebrew on Apple
/// silicon, Homebrew on Intel (and most hand installs), and the system.
pub const FALLBACK_DIRS: [&str; 3] = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"];

/// How long `-version` or `-encoders` may take before the probe gives up.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// An `ffmpeg` that answered, with the encoders the videos need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ffmpeg {
    /// The program, as found.
    pub path: PathBuf,
    /// What follows `ffmpeg version` on its first line, such as `7.1.1`.
    pub version: String,
}

/// Why no usable `ffmpeg` was found. Each names the program it looked for
/// and how to put things right.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FfmpegError {
    /// Nothing runnable at the path, or nowhere on `PATH` and the fallbacks.
    Missing(PathBuf),
    /// It runs, but was built without the `libx264` encoder.
    NoLibx264(PathBuf),
    /// It would not answer, or answered something else. The text says what.
    Failed(String),
}

impl fmt::Display for FfmpegError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(p) => write!(
                f,
                "ffmpeg was not found at {}: install it (`apt install ffmpeg` on Debian or \
                 Ubuntu, `brew install ffmpeg` on a Mac), or set [video] ffmpeg to its full path",
                p.display()
            ),
            Self::NoLibx264(p) => write!(
                f,
                "{} has no libx264 encoder, which the videos need: install a full build \
                 (`apt install ffmpeg` on Debian or Ubuntu, `brew install ffmpeg` on a Mac), \
                 or set [video] ffmpeg to one that has it",
                p.display()
            ),
            Self::Failed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for FfmpegError {}

/// Find `ffmpeg` and check it. `configured` is `[video] ffmpeg`: a bare name
/// (`ffmpeg`) is looked for on `PATH` and then in [`FALLBACK_DIRS`]; a full
/// path is used as it is, and a relative one is refused.
///
/// # Errors
/// [`FfmpegError`]: not found, no `libx264`, or it would not answer.
pub fn find(configured: &Path) -> Result<Ffmpeg, FfmpegError> {
    let path_var = std::env::var_os("PATH");
    find_in(
        configured,
        path_var.as_deref(),
        &FALLBACK_DIRS.map(Path::new),
    )
}

/// [`find`] with `PATH` and the fallback directories given, so a test needs
/// no changes to the process's environment.
pub(crate) fn find_in(
    configured: &Path,
    path_var: Option<&OsStr>,
    fallbacks: &[&Path],
) -> Result<Ffmpeg, FfmpegError> {
    // A relative path would mean something different in every folder the
    // recorder is started from.
    if !is_bare(configured) && !configured.is_absolute() {
        return Err(FfmpegError::Failed(format!(
            "[video] ffmpeg is {}: it must be a program name or a full path",
            configured.display()
        )));
    }
    let path = locate(configured, path_var, fallbacks)
        .ok_or_else(|| FfmpegError::Missing(configured.to_path_buf()))?;
    probe(&path)
}

/// Whether `configured` is a bare program name rather than a path.
fn is_bare(configured: &Path) -> bool {
    let mut parts = configured.components();
    matches!(
        (parts.next(), parts.next()),
        (Some(std::path::Component::Normal(_)), None)
    )
}

fn locate(configured: &Path, path_var: Option<&OsStr>, fallbacks: &[&Path]) -> Option<PathBuf> {
    if configured.as_os_str().is_empty() {
        return None;
    }
    if !is_bare(configured) {
        return is_runnable(configured).then(|| configured.to_path_buf());
    }
    let on_path = path_var
        .map(|v| std::env::split_paths(v).collect::<Vec<_>>())
        .unwrap_or_default();
    on_path
        .iter()
        // An empty PATH entry means the current directory; never search it.
        .filter(|d| d.is_absolute())
        .map(PathBuf::as_path)
        .chain(fallbacks.iter().copied())
        .map(|d| d.join(configured))
        .find(|p| is_runnable(p))
}

#[cfg(unix)]
fn is_runnable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_runnable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file())
}

/// Ask `path` its version and its encoders.
fn probe(path: &Path) -> Result<Ffmpeg, FfmpegError> {
    let failed = |what: &str| {
        FfmpegError::Failed(format!(
            "{} did not answer `{what}` as ffmpeg does: check that [video] ffmpeg names ffmpeg itself",
            path.display()
        ))
    };
    let (status, out) = run_for_output(path, &["-hide_banner", "-version"], PROBE_TIMEOUT)
        .map_err(|e| FfmpegError::Failed(format!("{} could not be run: {e}", path.display())))?;
    if !status.success() {
        return Err(failed("-version"));
    }
    let version = out
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("ffmpeg version "))
        .and_then(|rest| rest.split_whitespace().next())
        .ok_or_else(|| failed("-version"))?
        .to_owned();
    let (status, out) = run_for_output(path, &["-hide_banner", "-encoders"], PROBE_TIMEOUT)
        .map_err(|e| FfmpegError::Failed(format!("{} could not be run: {e}", path.display())))?;
    if !status.success() {
        return Err(failed("-encoders"));
    }
    if !has_encoder(&out, "libx264") {
        return Err(FfmpegError::NoLibx264(path.to_path_buf()));
    }
    if !has_encoder(&out, "aac") {
        return Err(FfmpegError::Failed(format!(
            "{} has no aac encoder, which the videos need: install a full build \
             (`apt install ffmpeg` on Debian or Ubuntu, `brew install ffmpeg` on a Mac)",
            path.display()
        )));
    }
    Ok(Ffmpeg {
        path: path.to_path_buf(),
        version,
    })
}

/// `-encoders` lists one per line as ` V....D libx264   description`: the
/// flags, then the name.
fn has_encoder(listing: &str, name: &str) -> bool {
    listing
        .lines()
        .any(|l| l.split_whitespace().nth(1) == Some(name))
}

/// Run `path args`, with nothing on stdin and stderr dropped, and return its
/// status and stdout. Killed and reaped if it outlasts `limit`.
fn run_for_output(
    path: &Path,
    args: &[&str],
    limit: Duration,
) -> std::io::Result<(ExitStatus, String)> {
    let mut child = Child::spawn(
        process::Command::new(path)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
    )?;
    let mut stdout = child.take_stdout();
    let reader = thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(s) = stdout.as_mut() {
            // Capped: a probe's answer is a few kilobytes.
            let _ = s.take(1 << 20).read_to_end(&mut out);
        }
        out
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            // Dropping the child kills and reaps it; the reader then sees EOF.
            drop(child);
            let _ = reader.join();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("no answer within {} s", limit.as_secs()),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    let out = reader.join().unwrap_or_default();
    Ok((status, String::from_utf8_lossy(&out).into_owned()))
}

/// The prefix of the recorder's own environment variables. The stream keys
/// can arrive as `ASTAR_NETREC_YOUTUBE_STREAM_KEY` and
/// `ASTAR_NETREC_TWITCH_STREAM_KEY`, so no child is given any of them: a
/// child's environment is readable by its user (`/proc/<pid>/environ`,
/// `ps eww`).
const SCRUBBED_PREFIX: &[u8] = b"ASTAR_NETREC_";

/// Remove from `cmd` every `ASTAR_NETREC_*` variable, whether it would be
/// inherited (named in `inherited`) or was set on `cmd` itself.
fn scrub_env(cmd: &mut process::Command, inherited: impl IntoIterator<Item = OsString>) {
    let mut names: Vec<OsString> = inherited.into_iter().collect();
    names.extend(cmd.get_envs().map(|(k, _)| k.to_owned()));
    for name in names {
        if name.as_encoded_bytes().starts_with(SCRUBBED_PREFIX) {
            cmd.env_remove(&name);
        }
    }
}

/// A child process that is killed and reaped when dropped, unless it has
/// already been waited for.
#[derive(Debug)]
pub(crate) struct Child {
    inner: process::Child,
    reaped: bool,
}

impl Child {
    /// Start `cmd`, without any `ASTAR_NETREC_*` variable in its
    /// environment. Every child process in this crate starts here.
    pub(crate) fn spawn(cmd: &mut process::Command) -> std::io::Result<Self> {
        scrub_env(cmd, std::env::vars_os().map(|(k, _)| k));
        Ok(Self {
            inner: cmd.spawn()?,
            reaped: false,
        })
    }

    pub(crate) fn id(&self) -> u32 {
        self.inner.id()
    }

    pub(crate) fn take_stdin(&mut self) -> Option<process::ChildStdin> {
        self.inner.stdin.take()
    }

    pub(crate) fn take_stdout(&mut self) -> Option<process::ChildStdout> {
        self.inner.stdout.take()
    }

    pub(crate) fn take_stderr(&mut self) -> Option<process::ChildStderr> {
        self.inner.stderr.take()
    }

    /// Kill the process without reaping it; [`Child::try_wait`] or the drop
    /// reaps it.
    pub(crate) fn kill(&mut self) {
        if !self.reaped {
            let _ = self.inner.kill();
        }
    }

    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        let status = self.inner.try_wait()?;
        if status.is_some() {
            self.reaped = true;
        }
        Ok(status)
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.inner.kill();
            let _ = self.inner.wait();
        }
    }
}

/// The `ffmpeg` the tests render with, or `None` (with a note on stdout)
/// when this machine has none with `libx264`, so the test can skip.
#[cfg(any(test, feature = "testing"))]
#[must_use]
pub fn ffmpeg_for_tests() -> Option<Ffmpeg> {
    let found = find(Path::new("ffmpeg")).ok();
    if found.is_none() {
        println!("skipped: no ffmpeg with libx264");
    }
    found
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fresh, empty directory under the system's temp dir, removed on drop.
    pub(crate) struct Scratch(pub PathBuf);

    impl Scratch {
        pub(crate) fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "netvideo-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("make a scratch dir");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Write an executable shell script `name` into `dir`.
    pub(crate) fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        // Written whole and closed before it is run, so exec never meets a
        // file still open for writing.
        std::fs::write(&p, format!("#!/bin/sh\n{body}")).expect("write a script");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        p
    }

    const GOOD: &str = r#"case "$2" in
  -version) echo "ffmpeg version 9.9-test Copyright (c) the test"; echo "built with nothing" ;;
  -encoders) echo "Encoders:"; echo " ------"; echo " V....D libx264               libx264 H.264"; echo " A....D aac                  AAC (Advanced Audio Coding)" ;;
esac
"#;

    const NO_X264: &str = r#"case "$2" in
  -version) echo "ffmpeg version 9.9-test Copyright (c) the test" ;;
  -encoders) echo "Encoders:"; echo " ------"; echo " V....D libopenh264           OpenH264"; echo " A....D aac                  AAC (Advanced Audio Coding)" ;;
esac
"#;

    #[test]
    fn ffmpeg_found_off_path() {
        let s = Scratch::new("find");
        let fake = script(&s.0, "ffmpeg", GOOD);
        let empty = OsStr::new("");
        let ff = find_in(&fake, Some(empty), &[]).expect("a full path is used as given");
        assert_eq!(ff.path, fake);
        assert_eq!(ff.version, "9.9-test");

        // A bare name is found in a fallback directory when PATH is empty...
        let ff = find_in(Path::new("ffmpeg"), Some(empty), &[s.0.as_path()]).expect("fallback");
        assert_eq!(ff.path, fake);
        // ...and on PATH.
        let ff = find_in(Path::new("ffmpeg"), Some(s.0.as_os_str()), &[]).expect("on PATH");
        assert_eq!(ff.path, fake);

        // Nothing on PATH, no fallback that holds it: missing, naming it.
        let err = find_in(Path::new("ffmpeg"), Some(empty), &[]).unwrap_err();
        assert_eq!(err, FfmpegError::Missing(PathBuf::from("ffmpeg")));
        let text = err.to_string();
        assert!(text.contains("ffmpeg was not found at ffmpeg"), "{text}");
        assert!(text.contains("apt install ffmpeg") && text.contains("brew install ffmpeg"));

        // A full path that is not there is missing too.
        let gone = s.0.join("nope/ffmpeg");
        assert_eq!(
            find_in(&gone, Some(empty), &[]).unwrap_err(),
            FfmpegError::Missing(gone)
        );
    }

    #[test]
    fn a_relative_path_is_refused() {
        let s = Scratch::new("relative");
        let _ = script(&s.0, "ffmpeg", GOOD);
        for p in ["bin/ffmpeg", "./ffmpeg", "../x/ffmpeg"] {
            let err = find_in(Path::new(p), Some(s.0.as_os_str()), &[s.0.as_path()]).unwrap_err();
            let text = err.to_string();
            assert!(matches!(err, FfmpegError::Failed(_)), "{p}: {err:?}");
            assert!(text.contains("a program name or a full path"), "{text}");
        }
    }

    #[test]
    fn ffmpeg_without_libx264_is_refused() {
        let s = Scratch::new("nox264");
        let fake = script(&s.0, "ffmpeg", NO_X264);
        let err = find_in(&fake, None, &[]).unwrap_err();
        assert_eq!(err, FfmpegError::NoLibx264(fake.clone()));
        let text = err.to_string();
        assert!(text.contains(&fake.display().to_string()), "{text}");
        assert!(text.contains("apt install ffmpeg") && text.contains("brew install ffmpeg"));
    }

    #[test]
    fn something_that_is_not_ffmpeg_is_refused() {
        let s = Scratch::new("notff");
        let fake = script(&s.0, "ffmpeg", "echo hello\n");
        let err = find_in(&fake, None, &[]).unwrap_err();
        assert!(matches!(err, FfmpegError::Failed(_)), "{err:?}");
        let failing = script(&s.0, "ffmpeg2", "exit 3\n");
        assert!(matches!(
            find_in(&failing, None, &[]).unwrap_err(),
            FfmpegError::Failed(_)
        ));
    }

    #[test]
    fn a_hung_probe_is_killed() {
        let s = Scratch::new("hung");
        let fake = script(&s.0, "ffmpeg", "exec sleep 30\n");
        let t = Instant::now();
        let err = run_for_output(&fake, &["-version"], Duration::from_millis(300)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    /// A made-up key: the tests only ever check it is absent, and never
    /// print what the child saw.
    const SENTINEL_NAME: &str = "ASTAR_NETREC_SCRUBTEST_STREAM_KEY";
    const SENTINEL_VALUE: &str = "sentinel-not-a-real-key-7f3a";

    #[test]
    fn a_child_never_sees_the_recorders_variables() {
        let mut cmd = process::Command::new("/usr/bin/env");
        cmd.env(SENTINEL_NAME, SENTINEL_VALUE)
            .env("NETVIDEO_SCRUBTEST_KEPT", "kept")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = Child::spawn(&mut cmd).expect("spawn /usr/bin/env");
        let mut out = String::new();
        child
            .take_stdout()
            .expect("stdout")
            .read_to_string(&mut out)
            .expect("read env's output");
        let leaked = out.contains(SENTINEL_NAME) || out.contains(SENTINEL_VALUE);
        assert!(!leaked, "an ASTAR_NETREC_ variable reached the child");
        assert!(
            out.contains("NETVIDEO_SCRUBTEST_KEPT=kept"),
            "other variables still pass through"
        );
    }

    #[test]
    fn the_inherited_recorder_variables_are_removed() {
        let mut cmd = process::Command::new("/usr/bin/env");
        let inherited = [
            OsString::from("ASTAR_NETREC_YOUTUBE_STREAM_KEY"),
            OsString::from("ASTAR_NETREC_TWITCH_STREAM_KEY"),
            OsString::from("PATH"),
        ];
        scrub_env(&mut cmd, inherited);
        let removed: Vec<_> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_owned())
            .collect();
        assert_eq!(
            removed,
            [
                "ASTAR_NETREC_TWITCH_STREAM_KEY",
                "ASTAR_NETREC_YOUTUBE_STREAM_KEY"
            ]
        );
    }

    #[test]
    fn the_real_ffmpeg_if_any() {
        if let Some(ff) = ffmpeg_for_tests() {
            assert_ne!(ff.version, "");
        }
    }
}
