// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Draws frame A offscreen, to `target/netvideo-preview/`:
//!
//! * the four golden scenes (`live-talking-1080.png`, ...), and
//! * `walk-NNN.png`: the synthetic archive's Tuesday net (the fixture the
//!   recordings page tests use) played through the recorded-video renderer,
//!   one frame every three seconds, with its real overs, audio and peaks.
//!
//! `cargo run -p astar-netvideo --example frame_preview`

use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use astar_netarchive::fixture::write_fixture;
use astar_netarchive::{
    LogKind, OverRec, PEAKS_FILE, overs_from_rows, read_meta, read_peaks, read_rows,
};
use astar_netvideo::{Mode, Renderer, SceneBuilder, SessionInfo, Size, demo};

fn write_png(path: &Path, size: Size, rgba: &[u8]) {
    let file = fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(BufWriter::new(file), size.width, size.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .expect("png header")
        .write_image_data(rgba)
        .expect("png data");
}

fn decode(path: &Path) -> Vec<i16> {
    let Ok(mut r) = claxon::FlacReader::open(path) else {
        return Vec::new();
    };
    r.samples()
        .map_while(Result::ok)
        .map(|s| i16::try_from(s).unwrap_or(0))
        .collect()
}

/// The session directory holding `session.json` for `net`.
fn find_session(root: &Path, net: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(root).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if let Some(found) = find_session(&p, net) {
                return Some(found);
            }
        } else if p.file_name().is_some_and(|n| n == "session.json")
            && read_meta(root).is_ok_and(|m| m.net == net)
        {
            return Some(root.to_path_buf());
        }
    }
    None
}

fn walk(out: &Path) {
    let root = out.join("fixture-archive");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("fixture dir");
    let _ = write_fixture(&root);
    let dir = find_session(&root, "Tuesday M17 net").expect("the fixture's public session");
    let meta = read_meta(&dir).expect("session.json");
    let rows = read_rows(&dir).expect("log.jsonl");
    let overs: Vec<OverRec> = overs_from_rows(&rows);
    let peaks = read_peaks(&dir).ok().flatten();
    let info = SessionInfo::from_meta(&meta, &[]).expect("the fixture's session.json");
    let total_ms = 90_000;
    let mut renderer = Renderer::new(Size::P1080);
    let mut b = SceneBuilder::new(info, Mode::Recorded { total_ms });
    b.set_capacity(renderer.column_capacity());

    // (seq, offset, duration, talker, samples, file), in order.
    let mut timeline = Vec::new();
    let mut seq = 0u32;
    for row in &rows {
        if let LogKind::Over {
            offset_ms,
            duration_ms,
            talker,
            file,
            ..
        } = &row.kind
        {
            seq += 1;
            let samples = decode(&dir.join("overs").join(file));
            timeline.push((seq, *offset_ms, *duration_ms, talker.clone(), samples, file));
        }
    }
    let mut next = 0usize;
    let mut fed: Vec<u64> = vec![0; timeline.len()];
    let mut n = 0;
    for t in (0..=total_ms).step_by(3_000) {
        // Start every over that has begun, feed audio up to `t`, and end
        // the ones that are over.
        while next < timeline.len() && timeline[next].1 <= t {
            let (s, at, _, talker, _, _) = &timeline[next];
            b.over_start(*s, talker.as_deref(), *at);
            next += 1;
        }
        for (i, (s, at, dur, _, samples, file)) in timeline.iter().enumerate().take(next) {
            let until = (*dur).min(t - at);
            let from = fed[i];
            if until > from {
                let lo = usize::try_from(from * 8)
                    .unwrap_or(usize::MAX)
                    .min(samples.len());
                let hi = usize::try_from(until * 8)
                    .unwrap_or(usize::MAX)
                    .min(samples.len());
                b.audio(at + from, &samples[lo..hi]);
                fed[i] = until;
                if until == *dur {
                    b.over_end(*s, *dur);
                    if let Some(p) = peaks.as_ref().and_then(|p| p.overs.get(file.as_str())) {
                        b.over_peaks(*s, p);
                    }
                }
            }
        }
        let frame = b.frame_at(t);
        write_png(
            &out.join(format!("walk-{n:03}.png")),
            Size::P1080,
            renderer.render(&frame),
        );
        n += 1;
    }
    println!(
        "walk-000..walk-{:03}.png: {} overs, {} with peaks from {PEAKS_FILE}",
        n - 1,
        overs.len(),
        peaks.map_or(0, |p| p.overs.len())
    );
}

fn main() {
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/netvideo-preview");
    fs::create_dir_all(&out).expect("preview dir");
    let cap = |s: Size| Renderer::new(s).column_capacity();
    let scenes = [
        (
            "live-talking-1080",
            Size::P1080,
            demo::live_talking(cap(Size::P1080)),
        ),
        (
            "recorded-gap-720",
            Size::P720,
            demo::recorded_gap(cap(Size::P720)),
        ),
        (
            "hostile-names-720",
            Size::P720,
            demo::hostile_names(cap(Size::P720)),
        ),
        (
            "live-talking-720",
            Size::P720,
            demo::live_talking(cap(Size::P720)),
        ),
    ];
    for (name, size, state) in scenes {
        let mut r = Renderer::new(size);
        let path = out.join(format!("{name}.png"));
        write_png(&path, size, r.render(&state));
        println!("{}", path.display());
    }
    // A bubble 150 ms into scrolling in: the column part-way up, the new
    // bubble part-way faded in.
    let mut r = Renderer::new(Size::P1080);
    let mut b = SceneBuilder::new(
        SessionInfo::from_meta(&demo::meta("Tuesday hub net", "M17-XXX C"), &[])
            .expect("demo session"),
        Mode::Live,
    );
    b.set_capacity(r.column_capacity());
    let overs = [
        (1, "NC0NTL", 12_000, Some(102_000)),
        (2, "N0CALL", 121_000, Some(18_000)),
        (3, "KX0AAA", 144_000, Some(20_000)),
        (4, "KX0BBB", 170_000, None),
    ];
    let t = 170_000 + 1_500 + 150;
    demo::feed(&mut b, &overs, t);
    let path = out.join("live-scrolling-1080.png");
    write_png(&path, Size::P1080, r.render(&b.frame_at(t)));
    println!("{}", path.display());
    walk(&out);
}
