// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

#![cfg(feature = "writer")]
//! Our manifest row must parse as unamblify's `CaptureRow`
//! (`unamblify/crates/unamblify/src/manifest.rs`), mirrored here field for
//! field. `mode` is a `String` because unamblify's `VocoderMode` enum
//! serialises to the same kebab-case names.

use astar_record::capture::{AstarExtra, CaptureRow, LostRun};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct UnamblifyCaptureRow {
    key: String,
    mode: String,
    frames: u32,
    port: String,
    prodid: String,
    version: String,
    encode_ms: u64,
    decode_ms: u64,
    #[serde(default)]
    roundtrip_ms: Option<u64>,
    sha256_ambe: String,
    sha256_wav: String,
    captured_at: String,
    attempts: u32,
    #[serde(default)]
    warm_state: Option<String>,
    #[serde(default)]
    aug: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct StrictMirror {
    key: String,
    mode: String,
    frames: u32,
    port: String,
    prodid: String,
    version: String,
    encode_ms: u64,
    decode_ms: u64,
    #[serde(default)]
    roundtrip_ms: Option<u64>,
    sha256_ambe: String,
    sha256_wav: String,
    captured_at: String,
    attempts: u32,
    #[serde(default)]
    warm_state: Option<String>,
    #[serde(default)]
    aug: Option<serde_json::Value>,
    astar: serde_json::Value,
}

fn row() -> CaptureRow {
    CaptureRow {
        key: "astar-air/20261026-190004-dstar-W1ABC-rx".into(),
        mode: "dstar".into(),
        frames: 50,
        port: "astar:dstar".into(),
        prodid: "AMBE3000".into(),
        version: "0.1.16-beta".into(),
        encode_ms: 0,
        decode_ms: 0,
        sha256_ambe: "a".repeat(64),
        sha256_wav: "b".repeat(64),
        captured_at: "2026-10-26T19:00:09Z".into(),
        attempts: 1,
        astar: AstarExtra {
            talker: None,
            network: "dstar".into(),
            direction: "rx".into(),
            received_at: "2026-10-26T19:00:04Z".into(),
            lost: vec![LostRun {
                at_frame: 3,
                frames: 1,
            }],
            vocoder: "hardware".into(),
            trimmed_tail: 2,
            dropped_pcm: 0,
            pcm: "decoded".into(),
        },
    }
}

#[test]
fn our_row_parses_as_unamblifys_capture_row() {
    let r = row();
    let json = serde_json::to_string(&r).unwrap();
    let u: UnamblifyCaptureRow = serde_json::from_str(&json).unwrap();
    assert_eq!(u.key, r.key);
    assert_eq!(u.mode, r.mode);
    assert_eq!(u.frames, r.frames);
    assert_eq!(u.port, r.port);
    assert_eq!(u.prodid, r.prodid);
    assert_eq!(u.version, r.version);
    assert_eq!((u.encode_ms, u.decode_ms), (0, 0));
    assert_eq!(u.sha256_ambe, r.sha256_ambe);
    assert_eq!(u.sha256_wav, r.sha256_wav);
    assert_eq!(u.captured_at, r.captured_at);
    assert_eq!(u.attempts, 1);
    assert_eq!((u.roundtrip_ms, u.warm_state, u.aug), (None, None, None));
}

#[test]
fn our_row_has_no_misspelled_fields() {
    let json = serde_json::to_string(&row()).unwrap();
    let s: StrictMirror = serde_json::from_str(&json).unwrap();
    assert_eq!(s.astar["network"], "dstar");
}

#[test]
fn mode_dir_names_match_unamblifys_vocoder_modes() {
    use astar_record::frames::CaptureMode::*;
    // VocoderMode names and aliases in unamblify/src/lib.rs.
    let accepted = [
        "dstar",
        "ysf-dmr",
        "ysf-dn",
        "ysf",
        "dmr",
        "codec2-3200",
        "codec2-1600",
    ];
    for m in [Dstar, YsfDmr, Codec2_3200] {
        assert!(accepted.contains(&m.dir_name()), "{}", m.dir_name());
    }
    // These stay outside unamblify by design: it skips their folders.
    for m in [Nxdn, Dmr72, Gsm] {
        assert!(!accepted.contains(&m.dir_name()), "{}", m.dir_name());
    }
}
