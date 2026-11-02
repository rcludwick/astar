// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `nodebench serve`: a small page on 127.0.0.1 for browsing runs.
//!
//! It reads the run directories under `target/nodebench/` as they are on
//! disk (a soak in progress shows up live, because its summary is recomputed
//! from `soak.jsonl` on every request) and serves:
//!
//! | path | what |
//! |---|---|
//! | `/` | the page (vanilla HTML/JS, no CDN, light and dark) |
//! | `/api/runs` | every run, newest first, with its headline numbers |
//! | `/api/run/<id>` | one run: `report.json`, or a soak's summary and events, and its files |
//! | `/files/<id>/<path>` | a file inside a run directory (logs, frame dumps) |
//!
//! It runs on `astar-http`, like astar-server's own control channel, rather
//! than pulling an async stack into the bench. It binds loopback only (the CLI
//! refuses anything else) and never serves a path outside the run directory.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Serialize;
use serde_json::{Value, json};

use crate::soak;

/// The page.
pub const INDEX_HTML: &str = include_str!("serve.html");

/// A run directory younger than this with no report is "running".
const RUNNING_WINDOW: Duration = Duration::from_secs(15 * 60);
/// Files listed per run.
const MAX_FILES: usize = 400;

/// One line of `/api/runs`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunEntry {
    pub id: String,
    /// `run` / `soak` / `incomplete`.
    pub kind: String,
    /// The run is still being written.
    pub running: bool,
    pub started: Option<String>,
    pub topology: Option<String>,
    pub git_sha: Option<String>,
    /// For a run: passed/failed/skipped checks and scenarios; for a soak:
    /// drops, uptime, duration.
    pub headline: Value,
}

/// A run id is a plain directory name.
#[must_use]
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !id.starts_with('.')
}

fn modified_recently(p: &Path) -> bool {
    fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .is_some_and(|age| age < RUNNING_WINDOW)
}

/// The newest mtime in a directory (one level), for "is it still going".
fn dir_recent(dir: &Path) -> bool {
    modified_recently(dir)
        || fs::read_dir(dir).is_ok_and(|rd| {
            rd.filter_map(Result::ok)
                .any(|e| modified_recently(&e.path()))
        })
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(p).ok()?).ok()
}

/// A check's verdict from `report.json` (`{"status":{"status":"fail"}}`;
/// a bare string is accepted too).
fn check_status(c: &Value) -> &str {
    c["status"]["status"]
        .as_str()
        .or_else(|| c["status"].as_str())
        .unwrap_or_default()
}

/// Describe one run directory, or `None` if it is not one.
#[must_use]
pub fn describe_run(dir: &Path) -> Option<RunEntry> {
    let id = dir.file_name()?.to_str()?.to_string();
    if !valid_id(&id) || !dir.is_dir() {
        return None;
    }
    let jsonl = dir.join("soak.jsonl");
    if jsonl.is_file() {
        let events = soak::read_events(&jsonl).ok()?;
        let s = soak::summarize(&events);
        let git_sha = events.iter().find_map(|e| match e {
            soak::Event::Start { git_sha, .. } => git_sha.clone(),
            _ => None,
        });
        return Some(RunEntry {
            id,
            kind: "soak".into(),
            running: s.stop_reason == "running" && modified_recently(&jsonl),
            started: Some(s.started.clone()).filter(|x| !x.is_empty()),
            topology: Some(s.topology.clone()),
            git_sha,
            headline: json!({
                "drops": s.drops,
                "setup_failures": s.setup_failures,
                "uptime_pct": s.uptime_pct,
                "covered_secs": s.covered_secs,
                "planned_secs": s.planned_secs,
                "udp_timeout": s.udp_timeout,
                "stop_reason": s.stop_reason,
            }),
        });
    }
    if let Some(r) = read_json(&dir.join("report.json")) {
        let scen = r["scenarios"].as_array().cloned().unwrap_or_default();
        let failed_scen = scen
            .iter()
            .filter(|s| {
                s["checks"]
                    .as_array()
                    .is_some_and(|c| c.iter().any(|c| check_status(c) == "fail"))
            })
            .count();
        return Some(RunEntry {
            id,
            kind: "run".into(),
            running: false,
            started: r["started"].as_str().map(str::to_string),
            topology: r["env"]["topology"].as_str().map(str::to_string),
            git_sha: r["env"]["git_sha"].as_str().map(str::to_string),
            headline: json!({
                "scenarios": scen.len(),
                "failed_scenarios": failed_scen,
                "checks": r["summary"]["checks"],
                "passed": r["summary"]["passed"],
                "failed": r["summary"]["failed"],
                "skipped": r["summary"]["skipped"],
                "rig_error": r["rig_error"],
            }),
        });
    }
    // Scenario directories but no report yet: a run in progress (or one
    // that died).
    let scenarios: Vec<String> = fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n != "pcap" && n != "logs")
        .collect();
    Some(RunEntry {
        id,
        kind: "incomplete".into(),
        running: dir_recent(dir),
        started: None,
        topology: None,
        git_sha: None,
        headline: json!({ "scenarios_started": scenarios.len() }),
    })
}

/// Every run under `base`, newest first (run ids sort by time).
#[must_use]
pub fn list_runs(base: &Path) -> Vec<RunEntry> {
    let mut v: Vec<RunEntry> = fs::read_dir(base)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter_map(|e| describe_run(&e.path()))
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| b.id.cmp(&a.id));
    v
}

/// Files inside a run directory (relative paths), two levels deep plus the
/// scenario directories' contents, capped at [`MAX_FILES`].
#[must_use]
pub fn run_files(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, d: &Path, depth: u8, out: &mut Vec<String>) {
        let Ok(rd) = fs::read_dir(d) else { return };
        let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for e in entries {
            if out.len() >= MAX_FILES {
                return;
            }
            let p = e.path();
            if p.is_dir() {
                if depth < 2 {
                    walk(root, &p, depth + 1, out);
                }
            } else if let Ok(rel) = p.strip_prefix(root) {
                out.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, 0, &mut out);
    out
}

/// Thin a soak's events for the page: every edge, note and failure, plus
/// only the samples that saw something wrong (the rest are implied by the
/// link sessions).
#[must_use]
pub fn soak_events_for_page(events: &[soak::Event]) -> Vec<&soak::Event> {
    events
        .iter()
        .filter(|e| {
            !matches!(
                e,
                soak::Event::Sample {
                    node: true,
                    server: true,
                    ..
                }
            )
        })
        .collect()
}

/// `/api/run/<id>`.
#[must_use]
pub fn run_detail(base: &Path, id: &str) -> Option<Value> {
    if !valid_id(id) {
        return None;
    }
    let dir = base.join(id);
    let entry = describe_run(&dir)?;
    let files = run_files(&dir);
    let mut v = json!({ "entry": entry, "files": files });
    match entry.kind.as_str() {
        "soak" => {
            let events = soak::read_events(&dir.join("soak.jsonl")).ok()?;
            let summary = soak::summarize(&events);
            let start = events.iter().find_map(|e| match e {
                soak::Event::Start { .. } => Some(e.clone()),
                _ => None,
            });
            v["soak"] = json!({
                "summary": summary,
                "start": start,
                "events": soak_events_for_page(&events),
            });
        }
        "run" => v["report"] = read_json(&dir.join("report.json"))?,
        _ => {}
    }
    Some(v)
}

/// Resolve `rel` inside `base/id`, refusing anything that escapes it.
#[must_use]
pub fn resolve_file(base: &Path, id: &str, rel: &str) -> Option<PathBuf> {
    if !valid_id(id) || rel.is_empty() {
        return None;
    }
    let rel_path = Path::new(rel);
    if rel_path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return None;
    }
    let root = base.join(id).canonicalize().ok()?;
    let p = root.join(rel_path).canonicalize().ok()?;
    (p.starts_with(&root) && p.is_file()).then_some(p)
}

/// Content type by extension: text for logs and frame dumps (so the browser
/// shows them), JSON for JSON, a download for captures.
#[must_use]
pub fn content_type(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).unwrap_or_default() {
        "json" => "application/json; charset=utf-8",
        // HTML from a run directory is shown as text, never rendered.
        "jsonl" | "txt" | "log" | "toml" | "conf" | "yaml" | "html" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Percent-decode a URL path segment (`%2F` and friends).
#[must_use]
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A routed response: status, content type, body.
#[derive(Debug)]
pub struct Routed {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Routed {
    fn json(v: &impl Serialize) -> Self {
        Self {
            status: 200,
            content_type: "application/json; charset=utf-8",
            body: serde_json::to_vec(v).unwrap_or_default(),
        }
    }
    fn not_found(what: &str) -> Self {
        Self {
            status: 404,
            content_type: "text/plain; charset=utf-8",
            body: format!("not found: {what}\n").into_bytes(),
        }
    }
}

/// Route one GET.
#[must_use]
pub fn route(base: &Path, url: &str) -> Routed {
    let path = url.split(['?', '#']).next().unwrap_or("/");
    if path == "/" || path == "/index.html" {
        return Routed {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: INDEX_HTML.as_bytes().to_vec(),
        };
    }
    if path == "/favicon.ico" {
        return Routed {
            status: 204,
            content_type: "image/x-icon",
            body: Vec::new(),
        };
    }
    if path == "/api/runs" {
        return Routed::json(&list_runs(base));
    }
    if let Some(id) = path.strip_prefix("/api/run/") {
        let id = percent_decode(id);
        return run_detail(base, &id).map_or_else(|| Routed::not_found(&id), |v| Routed::json(&v));
    }
    if let Some(rest) = path.strip_prefix("/files/") {
        let rest = percent_decode(rest);
        let (id, rel) = rest.split_once('/').unwrap_or((&rest, ""));
        return match resolve_file(base, id, rel).and_then(|p| fs::read(&p).ok().map(|b| (p, b))) {
            Some((p, body)) => Routed {
                status: 200,
                content_type: content_type(&p),
                body,
            },
            None => Routed::not_found(&rest),
        };
    }
    Routed::not_found(path)
}

/// The page's Content-Security-Policy.
const CSP: &str = "default-src 'self'; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; img-src 'self' data:";

/// Bind the run browser and serve it on `astar-http`'s own threads.
///
/// # Errors
/// A non-loopback address (checked first), or an address that cannot be bound.
pub fn start(bind: std::net::SocketAddr, base: &Path) -> Result<astar_http::Server, String> {
    if !bind.ip().is_loopback() {
        return Err(format!("refusing non-loopback bind {bind}"));
    }
    let base = base.to_path_buf();
    astar_http::Server::bind_with(
        &bind.to_string(),
        astar_http::Limits::loopback(),
        astar_http::ServerOptions {
            name: "nodebench",
            csp: Some(CSP),
            ..astar_http::ServerOptions::default()
        },
        move |req| {
            if !astar_http::guards::is_loopback(req) {
                return astar_http::Response::text(403, "loopback only\n").into();
            }
            let r = if req.method() == "GET" {
                route(&base, req.target())
            } else {
                Routed {
                    status: 405,
                    content_type: "text/plain; charset=utf-8",
                    body: b"GET only\n".to_vec(),
                }
            };
            astar_http::Response::new(r.status)
                .with_body(r.content_type, r.body)
                .header("Cache-Control", "no-store")
                .into()
        },
    )
    .map_err(|e| format!("bind {bind}: {e}"))
}

/// Serve until the process is stopped.
///
/// # Errors
/// As [`start`].
pub fn serve(bind: std::net::SocketAddr, base: &Path) -> Result<(), String> {
    let server = start(bind, base)?;
    eprintln!(
        "nodebench serve: http://{}/  (runs from {})",
        server.local_addr(),
        base.display()
    );
    // The server runs on its own threads; park until the process ends.
    loop {
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nb-serve-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_run_browser_answers_and_refuses_what_it_should() {
        let base = tmp("serve-http");
        let server = start(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), &base).unwrap();
        let a = server.local_addr();
        let get = |raw: &str| {
            use std::io::{Read, Write};
            let mut s = std::net::TcpStream::connect(a).unwrap();
            s.write_all(raw.as_bytes()).unwrap();
            let mut o = String::new();
            let _ = s.read_to_string(&mut o);
            o
        };
        assert!(get("GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").starts_with("HTTP/1.1 200 "));
        assert!(
            get("POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n")
                .starts_with("HTTP/1.1 405 ")
        );
        assert!(
            get("GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Forwarded-For: 198.51.100.7\r\n\r\n")
                .starts_with("HTTP/1.1 403 ")
        );
        // Path traversal, raw and percent-encoded, is refused over the wire.
        for t in [
            "/../../etc/passwd",
            "/files/../../etc/passwd",
            "/files/R1/..%2f..%2fetc/passwd",
            "/files/R1/%2e%2e/%2e%2e/etc/passwd",
            "/files/%2e%2e/etc/passwd",
        ] {
            let r = get(&format!("GET {t} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"));
            assert!(!r.starts_with("HTTP/1.1 200 "), "{t} was served: {r}");
            assert!(!r.contains("root:"), "{t} leaked a file: {r}");
        }
        server.stop();
        assert!(
            serve(std::net::SocketAddr::from(([0, 0, 0, 0], 0)), &base)
                .unwrap_err()
                .contains("non-loopback")
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn ids_and_paths_stay_inside() {
        assert!(valid_id("20261011T212733Z"));
        assert!(!valid_id("../etc"));
        assert!(!valid_id(".hidden"));
        assert!(!valid_id("a/b"));
        let base = tmp("paths");
        fs::create_dir_all(base.join("R1/logs")).unwrap();
        fs::write(base.join("R1/logs/x.log"), "hi").unwrap();
        fs::write(base.join("secret.txt"), "no").unwrap();
        assert!(resolve_file(&base, "R1", "logs/x.log").is_some());
        assert!(resolve_file(&base, "R1", "../secret.txt").is_none());
        assert!(resolve_file(&base, "R1", "/etc/passwd").is_none());
        assert!(resolve_file(&base, "R1", "logs").is_none());
        let r = route(&base, "/files/R1/logs/x.log");
        assert_eq!((r.status, r.body.as_slice()), (200, b"hi".as_slice()));
        assert_eq!(route(&base, "/files/R1/..%2Fsecret.txt").status, 404);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn lists_runs_and_soaks_newest_first() {
        let base = tmp("list");
        fs::create_dir_all(base.join("20261011T100000Z")).unwrap();
        fs::write(
            base.join("20261011T100000Z/report.json"),
            r#"{"started":"2026-10-11T10:00:00Z","env":{"topology":"loopback","git_sha":"abc"},
               "scenarios":[{"id":"a","checks":[{"name":"x","status":{"status":"fail"}}]}],
               "summary":{"checks":1,"passed":0,"failed":1,"skipped":0},"rig_error":null}"#,
        )
        .unwrap();
        let soak_dir = base.join("20261011T110000Z");
        let mut log = soak::EventLog::open(&soak_dir.join("soak.jsonl")).unwrap();
        log.push(&soak::Event::Start {
            t: 1_791_000_000.0,
            run_id: "20261011T110000Z".into(),
            topology: "nat".into(),
            dialer: soak::Dialer::Node,
            planned_secs: 60.0,
            sample_secs: 5.0,
            udp_timeout: Some(5),
            git_sha: Some("def".into()),
            conntrack: None,
        })
        .unwrap();
        log.push(&soak::Event::Sample {
            t: 1_791_000_005.0,
            node: true,
            server: true,
            detail: None,
        })
        .unwrap();
        fs::create_dir_all(base.join("not a run")).unwrap();
        let runs = list_runs(&base);
        let ids: Vec<&str> = runs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["20261011T110000Z", "20261011T100000Z"]);
        assert_eq!(runs[0].kind, "soak");
        assert_eq!(runs[0].git_sha.as_deref(), Some("def"));
        assert_eq!(runs[1].kind, "run");
        assert_eq!(runs[1].headline["failed_scenarios"], 1);
        let d = run_detail(&base, "20261011T110000Z").unwrap();
        assert_eq!(d["soak"]["summary"]["samples"], 1);
        // An all-up sample is implied by the link sessions, so not sent.
        assert_eq!(d["soak"]["events"].as_array().unwrap().len(), 1);
        assert!(run_detail(&base, "nope").is_none());
        let r = route(&base, "/api/runs");
        assert_eq!(r.status, 200);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn page_is_self_contained() {
        let r = route(Path::new("/nonexistent"), "/");
        assert_eq!(r.status, 200);
        let html = String::from_utf8(r.body).unwrap();
        assert!(html.contains("prefers-color-scheme: dark"));
        // No CDN, no external fetches.
        assert!(!html.contains("http://") && !html.contains("https://"));
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("a%2Fb%20c"), "a/b c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }
}
