// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Cross-validate the Rust `JitterBuf` port against the original
//! `jitterbuf.c` reference implementation.
//!
//! Each trace under `harness/jitter_parity/traces/*.in` is driven through
//! the Rust `JitterBuf` here, the result formatted to match the C harness
//! line-for-line, and diffed against the committed `${trace}.out` golden.
//! See `harness/jitter_parity/README.md` for trace format details and the
//! one known divergence (the shrink-with-frame voice branch — see
//! ticket `iax-9c62`).

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use astar_codec::jitter::{Frame, FrameType, GetResult, JitterBuf, JitterConfig, JitterError};

/// Locate the parity trace directory by walking up from `CARGO_MANIFEST_DIR`
/// until we find the `harness/jitter_parity/traces` sibling. Keeps the test
/// robust against future workspace reshuffles.
fn traces_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .expect("canonical manifest");
    let mut cur: &Path = manifest.as_path();
    loop {
        let candidate = cur.join("harness").join("jitter_parity").join("traces");
        if candidate.is_dir() {
            return candidate;
        }
        cur = cur
            .parent()
            .expect("ran off filesystem root looking for traces");
    }
}

/// Mirror C's `payload_new`: the parser turns a hex token into a `Vec<u8>`.
/// An empty hex token (rendered as `-` in the golden) yields an empty vec.
fn parse_hex(s: &str) -> Option<Vec<u8>> {
    if s == "-" {
        return Some(Vec::new());
    }
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().chunks(2) {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out.push(u8::try_from((hi << 4) | lo).ok()?);
    }
    Some(out)
}

fn parse_ftype(s: &str) -> Option<FrameType> {
    Some(match s {
        "control" => FrameType::Control,
        "voice" => FrameType::Voice,
        "video" => FrameType::Video,
        "silence" => FrameType::Silence,
        _ => return None,
    })
}

fn ftype_name(t: FrameType) -> &'static str {
    match t {
        FrameType::Control => "control",
        FrameType::Voice => "voice",
        FrameType::Video => "video",
        FrameType::Silence => "silence",
        FrameType::Dtmf => "dtmf",
    }
}

fn render_payload(buf: &mut String, bytes: &[u8]) {
    if bytes.is_empty() {
        buf.push('-');
        return;
    }
    for b in bytes {
        let _ = write!(buf, "{b:02x}");
    }
}

fn render_frame(buf: &mut String, f: &Frame<Vec<u8>>) {
    let _ = write!(
        buf,
        "ts={} ms={} ftype={} payload=",
        f.ts,
        f.ms,
        ftype_name(f.frame_type)
    );
    render_payload(buf, &f.data);
}

/// State threaded through the trace driver: the buffer plus a shadow of
/// queued frame timestamps, used to translate the Rust `Result` from `put`
/// into the C harness's three-way `ok | sched | drop` output (the Rust API
/// doesn't expose "did the new frame become the queue head"). For the traces
/// we ship (resync never trips) the smallest queued ts is the head.
struct Driver {
    jb: JitterBuf<Vec<u8>>,
    shadow_ts: Vec<i64>,
}

impl Driver {
    fn new() -> Self {
        Self {
            jb: JitterBuf::new(JitterConfig::default()),
            shadow_ts: Vec::new(),
        }
    }

    fn handle_config(&mut self, toks: &mut std::str::SplitAsciiWhitespace<'_>, out: &mut String) {
        let (Some(a), Some(b), Some(c), Some(d)) =
            (toks.next(), toks.next(), toks.next(), toks.next())
        else {
            out.push_str("config error\n");
            return;
        };
        let cfg = JitterConfig {
            max_jitterbuf: a.parse().unwrap_or(0),
            resync_threshold: b.parse().unwrap_or(0),
            max_contig_interp: c.parse().unwrap_or(0),
            target_extra: d.parse().unwrap_or(0),
        };
        self.jb.set_config(cfg);
        out.push_str("config ok\n");
    }

    fn handle_put(&mut self, toks: &mut std::str::SplitAsciiWhitespace<'_>, out: &mut String) {
        let (Some(now_s), Some(ts_s), Some(ms_s), Some(typ_s)) =
            (toks.next(), toks.next(), toks.next(), toks.next())
        else {
            out.push_str("put error\n");
            return;
        };
        let hex_s = toks.next().unwrap_or("");
        let (Ok(now), Ok(ts), Ok(ms)) = (
            now_s.parse::<i64>(),
            ts_s.parse::<i64>(),
            ms_s.parse::<i64>(),
        ) else {
            out.push_str("put error\n");
            return;
        };
        let (Some(ftype), Some(data)) = (parse_ftype(typ_s), parse_hex(hex_s)) else {
            out.push_str("put error\n");
            return;
        };
        let became_head = self.shadow_ts.iter().min().is_none_or(|&min| ts <= min);
        match self.jb.put(
            Frame {
                data,
                ts,
                ms,
                frame_type: ftype,
            },
            now,
        ) {
            Ok(()) => {
                self.shadow_ts.push(ts);
                out.push_str(if became_head {
                    "put sched\n"
                } else {
                    "put ok\n"
                });
            }
            Err(JitterError::DiscontinuityDrop) => out.push_str("put drop\n"),
        }
    }

    fn handle_get(&mut self, toks: &mut std::str::SplitAsciiWhitespace<'_>, out: &mut String) {
        let (Some(now_s), Some(interpl_s)) = (toks.next(), toks.next()) else {
            out.push_str("get error\n");
            return;
        };
        let (Ok(now), Ok(interpl)) = (now_s.parse::<i64>(), interpl_s.parse::<i64>()) else {
            out.push_str("get error\n");
            return;
        };
        match self.jb.get(now, interpl) {
            GetResult::Ok(f) => {
                out.push_str("get ok ");
                render_frame(out, &f);
                out.push('\n');
                self.drop_shadow(f.ts);
            }
            GetResult::Drop(f) => {
                out.push_str("get drop ");
                render_frame(out, &f);
                out.push('\n');
                self.drop_shadow(f.ts);
            }
            GetResult::Interpolate => out.push_str("get interpolate\n"),
            GetResult::Empty => out.push_str("get empty\n"),
            GetResult::NoFrame => out.push_str("get noframe\n"),
        }
    }

    fn handle_next(&mut self, out: &mut String) {
        match self.jb.next() {
            None => out.push_str("next none\n"),
            Some(t) => {
                let _ = writeln!(out, "next at={t}");
            }
        }
    }

    fn handle_reset(&mut self, out: &mut String) {
        self.jb.reset();
        self.shadow_ts.clear();
        out.push_str("reset ok\n");
    }

    fn drop_shadow(&mut self, ts: i64) {
        if let Some(pos) = self.shadow_ts.iter().position(|&t| t == ts) {
            self.shadow_ts.swap_remove(pos);
        }
    }
}

/// Run one trace, return the formatted output.
fn run_trace(input: &str) -> String {
    let mut drv = Driver::new();
    let mut out = String::new();
    for raw in input.lines() {
        let stripped = raw.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        let mut toks = stripped.split_ascii_whitespace();
        let Some(op) = toks.next() else { continue };
        match op {
            "config" => drv.handle_config(&mut toks, &mut out),
            "put" => drv.handle_put(&mut toks, &mut out),
            "get" => drv.handle_get(&mut toks, &mut out),
            "next" => {
                // Consume (and ignore) the trace's `now` argument.
                let _ = toks.next();
                drv.handle_next(&mut out);
            }
            "reset" => drv.handle_reset(&mut out),
            other => {
                let _ = writeln!(out, "unknown op={other}");
            }
        }
    }
    out
}

/// Allowlist of `(trace stem, line numbers)` where Rust output may legitimately
/// differ from the C golden due to the documented shrink-with-frame-present
/// voice branch simplification. Empty for now; add entries with a comment
/// pointing to the analysis if a future trace exercises that branch.
fn allowed_diff_lines(_trace_stem: &str) -> &'static [usize] {
    &[]
}

fn diff_with_allowlist(trace_stem: &str, actual: &str, expected: &str) -> Result<(), String> {
    let actual_lines: Vec<&str> = actual.lines().collect();
    let expected_lines: Vec<&str> = expected.lines().collect();
    let allowed = allowed_diff_lines(trace_stem);

    let max = actual_lines.len().max(expected_lines.len());
    let mut diffs: Vec<String> = Vec::new();
    for i in 0..max {
        let a = actual_lines.get(i).copied().unwrap_or("<EOF>");
        let e = expected_lines.get(i).copied().unwrap_or("<EOF>");
        if a != e && !allowed.contains(&(i + 1)) {
            diffs.push(format!("  line {}: rust={a:?} c={e:?}", i + 1));
            if diffs.len() >= 5 {
                break;
            }
        }
    }
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "trace {trace_stem}: divergence:\n{}",
            diffs.join("\n")
        ))
    }
}

#[test]
fn jitter_buf_matches_c_reference() {
    let dir = traces_dir();
    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("read traces dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("in"))
        .collect();
    entries.sort();
    assert!(!entries.is_empty(), "no traces found in {}", dir.display());

    let mut failures: Vec<String> = Vec::new();
    for path in &entries {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("trace stem");
        let input = fs::read_to_string(path).expect("read input trace");
        let golden_path = path.with_extension("out");
        let expected = fs::read_to_string(&golden_path).expect("read golden output");
        let actual = run_trace(&input);
        if let Err(msg) = diff_with_allowlist(stem, &actual, &expected) {
            failures.push(msg);
        }
    }

    assert!(
        failures.is_empty(),
        "parity failures:\n{}",
        failures.join("\n\n")
    );
}
