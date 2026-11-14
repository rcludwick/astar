// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A small synthetic archive, for tests and for previewing the pages.
//!
//! Two nets, three sessions, all with made-up callsigns and real 8 kHz FLAC:
//!
//! * **Tuesday M17 net**, public, about 90 s, with `peaks.json` and
//!   `my_callsigns`: ten overs (two of them short key-ups; the sixth, 34.8 s to
//!   40.8 s, is playing at 40 s), a join, a drop and rejoin, and a leave.
//! * **Thursday D-Star net**, private: a finished session shaped like stage
//!   1 wrote it (no `peaks.json`, no `my_callsigns` key), and a newer one
//!   still recording (`ended: None`).

use std::path::Path;

use astar_record::FlacWriter;
use jiff::Timestamp;
use jiff::tz::{Offset, TimeZone};

use crate::index::slug;
use crate::layout::{net_dir_name, over_file_name, session_dir_name};
use crate::log::{META_FILE, append_row, write_meta};
use crate::model::{Ended, LogKind, LogRow, META_FORMAT, SessionMeta};
use crate::peaks::{compute_peaks, write_peaks};
use crate::summary::{overs_from_rows, write_checkins, write_log_csv};

/// The slugs of what [`write_fixture`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixture {
    /// The public net (`tuesday-m17-net`).
    pub public_net: String,
    /// Its one session.
    pub public_session: String,
    /// The private net's finished, stage-1-shaped session.
    pub private_session: String,
    /// The private net's session that is still recording.
    pub open_session: String,
}

const RATE: u32 = 8_000;
const UTC_OFFSET_HOURS: i8 = -7;

/// One session to write.
struct Spec<'a> {
    net: &'a str,
    network: &'a str,
    visibility: &'a str,
    my_side: bool,
    /// `None` leaves the key out of `session.json`, as stage 1 did.
    my_callsigns: Option<&'a [&'a str]>,
    /// The local start, `2026-11-03T18:55:00` at [`UTC_OFFSET_HOURS`].
    local_start: &'a str,
    length_ms: u64,
    /// `(start_ms, duration_ms, talker)`, from the session's start.
    overs: &'a [(u64, u64, &'a str)],
    /// `(at_ms, kind)`.
    events: Vec<(u64, LogKind)>,
    ended: bool,
    peaks: bool,
}

/// Write the fixture archive under `root` (created if needed).
///
/// # Panics
/// When anything cannot be written: it is test and preview support.
#[must_use]
pub fn write_fixture(root: &Path) -> Fixture {
    let tuesday = Spec {
        net: "Tuesday M17 net",
        network: "m17",
        visibility: "public",
        my_side: true,
        my_callsigns: Some(&["N0CALL"]),
        local_start: "2026-11-03T18:55:00",
        length_ms: 90_000,
        overs: &[
            (2_000, 11_000, "NC0NTL"),
            (14_000, 8_500, "N0CALL"),
            (23_500, 7_000, "KX0AAA  D"),
            (31_500, 1_200, "KX0BBB-1"),
            (33_200, 1_100, "KX0AAA E"),
            (34_800, 6_000, "N0CALL"),
            (45_000, 11_000, "NC0NTL"),
            (57_000, 9_000, "KX0AAA E"),
            (67_000, 8_000, "KX0BBB-1"),
            (76_000, 9_500, "NC0NTL"),
        ],
        events: vec![
            (100, LogKind::Joined),
            (
                41_000,
                LogKind::Dropped {
                    reason: "keepalive lost from 203.0.113.7:17000".to_owned(),
                },
            ),
            (43_000, LogKind::Rejoined),
            (87_000, LogKind::Left),
        ],
        ended: true,
        peaks: true,
    };
    let thursday = Spec {
        net: "Thursday D-Star net",
        network: "dstar",
        visibility: "private",
        my_side: false,
        my_callsigns: None,
        local_start: "2026-11-05T19:00:00",
        length_ms: 40_000,
        overs: &[
            (1_000, 8_000, "NC0NTL"),
            (10_000, 1_200, "KX0AAA  D"),
            (12_000, 6_000, "KX0BBB-1"),
            (20_000, 1_000, "N0CALL"),
            (23_000, 8_000, "KX0AAA E"),
        ],
        events: vec![
            (100, LogKind::Joined),
            (
                32_000,
                LogKind::Error {
                    message: "vocoder failed at /var/lib/astar/thumbdv".to_owned(),
                },
            ),
            (38_000, LogKind::Left),
        ],
        ended: true,
        peaks: false,
    };
    let open = Spec {
        net: "Thursday D-Star net",
        network: "dstar",
        visibility: "private",
        my_side: false,
        my_callsigns: None,
        local_start: "2026-11-07T19:00:00",
        length_ms: 20_000,
        overs: &[(2_000, 6_000, "NC0NTL"), (9_000, 7_000, "KX0BBB-1")],
        events: vec![(100, LogKind::Joined)],
        ended: false,
        peaks: false,
    };
    let public_session = write_session(root, &tuesday);
    let private_session = write_session(root, &thursday);
    let open_session = write_session(root, &open);
    Fixture {
        public_net: slug(&net_dir_name(tuesday.net)),
        public_session,
        private_session,
        open_session,
    }
}

fn offset() -> Offset {
    Offset::from_hours(UTC_OFFSET_HOURS).expect("a valid offset")
}

fn at(start: Timestamp, ms: u64) -> Timestamp {
    start
        .checked_add(jiff::SignedDuration::from_millis(
            i64::try_from(ms).expect("a short fixture"),
        ))
        .expect("in range")
}

/// Speech-ish audio: a tone that swells and fades in syllables. `seed` keeps
/// talkers apart.
fn voice(len_ms: u64, seed: u64) -> Vec<i16> {
    let n = usize::try_from(len_ms * u64::from(RATE) / 1000).expect("small");
    let small = |v: u64| f64::from(u32::try_from(v).expect("small"));
    let freq = 180.0 + 40.0 * small(seed % 5);
    let syllable = 3.0 + small(seed % 3) * 0.7;
    (0..n)
        .map(|i| {
            let t = small(i as u64) / f64::from(RATE);
            let env = (std::f64::consts::PI * syllable * t).sin().abs().powf(1.5);
            let swell = 0.55 + 0.45 * (0.37 * t + small(seed % 1000)).sin();
            let s = (std::f64::consts::TAU * freq * t).sin()
                + 0.35 * (std::f64::consts::TAU * freq * 2.7 * t).sin();
            // |s| <= 1.35 and env, swell <= 1, so this is within ±19 000.
            #[allow(clippy::cast_possible_truncation)]
            let sample = (s * env * swell * 14_000.0) as i16;
            sample
        })
        .collect()
}

fn flac(path: &Path, samples: &[i16]) {
    let mut w = FlacWriter::create(path, RATE, 1).expect("create a FLAC");
    w.push_interleaved(samples).expect("write samples");
    w.finish().expect("finish a FLAC");
}

/// Write one session and return its slug.
fn write_session(root: &Path, spec: &Spec<'_>) -> String {
    let tz = TimeZone::fixed(offset());
    let start_local: jiff::civil::DateTime = spec.local_start.parse().expect("a local time");
    let start_zoned = start_local.to_zoned(tz.clone()).expect("in range");
    let start = start_zoned.timestamp();
    let session_name = session_dir_name(&start_zoned);
    let dir = root.join(net_dir_name(spec.net)).join(&session_name);
    std::fs::create_dir_all(dir.join("overs")).expect("make the session folder");

    let mut whole = vec![0i16; usize::try_from(spec.length_ms * 8).expect("small")];
    let mut rows: Vec<(u64, LogRow)> = Vec::new();
    for (i, &(start_ms, dur_ms, talker)) in spec.overs.iter().enumerate() {
        let seq = u32::try_from(i + 1).expect("few overs");
        let over_start = at(start, start_ms);
        let file = over_file_name(seq, &over_start.to_zoned(tz.clone()), Some(talker));
        let audio = voice(dur_ms, talker.bytes().map(u64::from).sum());
        flac(&dir.join("overs").join(&file), &audio);
        let first = usize::try_from(start_ms * 8).expect("small");
        for (k, s) in audio.iter().enumerate() {
            if let Some(slot) = whole.get_mut(first + k) {
                *slot = *s;
            }
        }
        let end = at(start, start_ms + dur_ms);
        rows.push((
            start_ms + dur_ms,
            LogRow {
                t: end.to_string(),
                kind: LogKind::Over {
                    start: over_start.to_string(),
                    end: end.to_string(),
                    duration_ms: dur_ms,
                    offset_ms: start_ms,
                    talker: Some(talker.to_owned()),
                    file,
                },
            },
        ));
    }
    for (ms, kind) in &spec.events {
        rows.push((
            *ms,
            LogRow {
                t: at(start, *ms).to_string(),
                kind: kind.clone(),
            },
        ));
    }
    rows.sort_by_key(|(ms, _)| *ms);
    let rows: Vec<LogRow> = rows.into_iter().map(|(_, r)| r).collect();
    for r in &rows {
        append_row(&dir, r).expect("append a row");
    }
    flac(&dir.join("session.flac"), &whole);

    let overs = overs_from_rows(&rows);
    if spec.ended {
        write_log_csv(&dir, &overs).expect("log.csv");
        write_checkins(&dir, &overs, spec.network).expect("checkins.txt");
    }
    if spec.peaks {
        write_peaks(&dir, &compute_peaks(&dir, &overs)).expect("peaks.json");
    }

    write_session_meta(&dir, spec, start, &rows, overs.len());
    slug(&session_name)
}

/// `session.json` for a written session.
fn write_session_meta(
    dir: &Path,
    spec: &Spec<'_>,
    start: Timestamp,
    rows: &[LogRow],
    overs: usize,
) {
    let tz = TimeZone::fixed(offset());
    let start_zoned = start.to_zoned(tz.clone());
    let window_end = at(start, 3_600_000).to_zoned(tz.clone());
    let meta = SessionMeta {
        format: META_FORMAT,
        net: spec.net.to_owned(),
        network: spec.network.to_owned(),
        target: "reflector.example.org:17000 C".to_owned(),
        window_start: start_zoned
            .timestamp()
            .display_with_offset(offset())
            .to_string(),
        window_end: window_end
            .timestamp()
            .display_with_offset(offset())
            .to_string(),
        visibility: spec.visibility.to_owned(),
        my_side: spec.my_side,
        sample_rate: RATE,
        started_at: start.to_string(),
        ended_at: spec.ended.then(|| at(start, spec.length_ms).to_string()),
        ended: spec.ended.then_some(Ended::Window),
        rejoins: u32::try_from(
            rows.iter()
                .filter(|r| matches!(r.kind, LogKind::Rejoined))
                .count(),
        )
        .expect("few"),
        dropped_frames: 0,
        overs: if spec.ended {
            u32::try_from(overs).expect("few")
        } else {
            0
        },
        astar_version: "0.1.20-beta".to_owned(),
        my_callsigns: spec
            .my_callsigns
            .unwrap_or_default()
            .iter()
            .map(|c| (*c).to_owned())
            .collect(),
    };
    write_meta(dir, &meta).expect("session.json");
    if spec.my_callsigns.is_none() {
        // Stage 1 never wrote the key at all.
        let mut v: serde_json::Value =
            serde_json::to_value(&meta).expect("session.json as a value");
        if let Some(o) = v.as_object_mut() {
            o.remove("my_callsigns");
        }
        let text = serde_json::to_vec_pretty(&v).expect("session.json bytes");
        std::fs::write(dir.join(META_FILE), text).expect("session.json");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{read_meta, read_rows};
    use crate::peaks::read_peaks;

    #[test]
    fn fixture_writes_three_sessions() {
        let root = std::env::temp_dir().join(format!("netarchive-fixture-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let f = write_fixture(&root);
        assert_eq!(f.public_net, "tuesday-m17-net");
        assert_eq!(f.public_session, "2026-11-03-1855");
        let tue = root.join("tuesday-m17-net").join("2026-11-03 1855");
        let m = read_meta(&tue).unwrap();
        assert_eq!(m.window_start, "2026-11-03T18:55:00-07:00");
        assert_eq!(m.started_at, "2026-11-04T01:55:00Z");
        assert_eq!(m.my_callsigns, vec!["N0CALL".to_owned()]);
        assert_eq!(read_rows(&tue).unwrap().len(), 14);
        assert_eq!(read_peaks(&tue).unwrap().unwrap().overs.len(), 10);
        let thu = root.join("thursday-d-star-net").join("2026-11-05 1900");
        let text = std::fs::read_to_string(thu.join(META_FILE)).unwrap();
        assert!(!text.contains("my_callsigns"), "{text}");
        assert!(read_peaks(&thu).unwrap().is_none());
        let open = root.join("thursday-d-star-net").join("2026-11-07 1900");
        assert_eq!(read_meta(&open).unwrap().ended, None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
