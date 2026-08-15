// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Walk every `*.pcap` / `*.pcapng` under `fixtures/` and replay it
//! through `astar_iax_core::{parse, encode}`.
//!
//! New captures dropped into `crates/astar-conformance/fixtures/` are picked
//! up automatically; no plumbing change is required.

use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;

use astar_conformance::{ReplayAssertion, ReplayFixture, play};

#[test]
fn replay_all_fixtures() {
    let dir = fixtures_dir();
    let mut entries: Vec<PathBuf> = Vec::new();
    collect_pcaps(&dir, &mut entries);

    if entries.is_empty() {
        eprintln!(
            "no .pcap/.pcapng fixtures found under {}; replay test is a no-op",
            dir.display()
        );
        return;
    }

    let mut total = 0usize;
    let mut full = 0usize;
    let mut mini = 0usize;

    for path in entries {
        let fix = ReplayFixture::from_pcap(&path)
            .unwrap_or_else(|e| panic!("loading {}: {e}", path.display()));
        // Fixtures captured from real Asterisk peers or the patched-C
        // iaxclient may carry vendor IEs or subclasses our encoder doesn't
        // model; assert parse-only there. Synthetic fixtures and any
        // future hand-built ones round-trip.
        let assertion = if path
            .components()
            .any(|c| matches!(c.as_os_str().to_str(), Some("asterisk" | "c-iaxclient")))
        {
            ReplayAssertion::ParseOnly
        } else {
            ReplayAssertion::ParseRoundTrip
        };
        let stats =
            play(&fix, &assertion).unwrap_or_else(|e| panic!("replaying {}: {e}", path.display()));
        assert_eq!(
            stats.frames_parsed, stats.frames_total,
            "{}: {} of {} frames parsed",
            fix.name, stats.frames_parsed, stats.frames_total,
        );
        eprintln!(
            "{}: {} frames ({} full, {} mini){}",
            fix.name,
            stats.frames_total,
            stats.full_frames,
            stats.mini_frames,
            fix.companion_md
                .as_ref()
                .map(|p| format!(" [companion: {}]", p.display()))
                .unwrap_or_default(),
        );
        total += stats.frames_total;
        full += stats.full_frames;
        mini += stats.mini_frames;
    }

    eprintln!("replay totals: {total} frames ({full} full, {mini} mini)");
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Walk `dir` recursively, appending every `.pcap` / `.pcapng` file.
fn collect_pcaps(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_pcaps(&path, out);
        } else if matches!(
            path.extension().and_then(OsStr::to_str),
            Some("pcap" | "pcapng")
        ) {
            out.push(path);
        }
    }
}
