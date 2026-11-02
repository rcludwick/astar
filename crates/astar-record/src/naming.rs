// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! File names. Talker names come off the air and can hold anything, so every
//! piece goes through a sanitiser before it reaches a path.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::clock::{civil, rfc3339_utc, unix_secs};
use crate::frames::Dir;
use crate::layout::Layout;

/// Bytes, not characters: with the date, a counter and ` rx.flac` the whole
/// name must stay under the 255-byte `NAME_MAX` of ext4.
const MAX_LABEL_BYTES: usize = 120;
const MAX_KEY_COMPONENT: usize = 16;

/// Unicode format characters (bidi overrides and isolates, zero-width marks,
/// the BOM) that can make a name read differently from what it is, e.g. an
/// RLO that disguises the extension.
fn is_format_char(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}' | '\u{FEFF}')
}

/// A label safe to put inside a file name: path separators, `:`, NUL and
/// control characters (and bidi/zero-width format characters) become `-`;
/// trimmed; at most 120 bytes of UTF-8, cut on a character boundary; empty
/// becomes `Recording`.
#[must_use]
pub fn sanitize_label(s: &str) -> String {
    let mapped: String = s
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '\0') || c.is_control() || is_format_char(c) {
                '-'
            } else {
                c
            }
        })
        .collect();
    let mut cut = String::new();
    for c in mapped.trim().chars() {
        if cut.len() + c.len_utf8() > MAX_LABEL_BYTES {
            break;
        }
        cut.push(c);
    }
    let cut = cut.trim();
    if cut.is_empty() {
        "Recording".to_owned()
    } else {
        cut.to_owned()
    }
}

/// `2026-10-26 19-00-04 AllStar 2000`, in the local zone `utc_offset_secs`.
#[must_use]
pub fn app_base_name(start: SystemTime, utc_offset_secs: i32, label: &str) -> String {
    let c = civil(unix_secs(start), utc_offset_secs);
    format!(
        "{:04}-{:02}-{:02} {:02}-{:02}-{:02} {}",
        c.year,
        c.month,
        c.day,
        c.hour,
        c.minute,
        c.second,
        sanitize_label(label)
    )
}

fn exists(base: &str, layout: Layout) -> bool {
    let any = |suffixes: &[&str]| {
        suffixes
            .iter()
            .any(|s| Path::new(&format!("{base}{s}")).exists())
    };
    match layout {
        Layout::Stereo | Layout::Mixed => any(&[".flac"]),
        Layout::TwoMono => any(&[" rx.flac", " tx.flac"]),
    }
}

/// `dir/base`, or `dir/base 2`, `dir/base 3`, … until none of the layout's
/// files exist. The result has no extension. This only looks: callers must
/// open the files with `create_new` so a racing writer cannot be overwritten.
#[must_use]
pub fn unique_base(dir: &Path, base: &str, layout: Layout) -> PathBuf {
    let stem = dir.join(base);
    let stem_s = stem.to_string_lossy().into_owned();
    if !exists(&stem_s, layout) {
        return stem;
    }
    let mut n = 2u64;
    loop {
        let cand = format!("{stem_s} {n}");
        if !exists(&cand, layout) {
            return PathBuf::from(cand);
        }
        n += 1;
    }
}

/// Only `[A-Za-z0-9]` survives; everything else becomes `_`. At most 16
/// characters; empty becomes `unknown`.
#[must_use]
pub fn key_component(s: &str) -> String {
    let out: String = s
        .chars()
        .take(MAX_KEY_COMPONENT)
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if out.is_empty() {
        "unknown".to_owned()
    } else {
        out
    }
}

/// `astar-air/20261026-190004-dstar-W1ABC-rx`, in UTC.
#[must_use]
pub fn capture_key(start: SystemTime, network: &str, talker: Option<&str>, dir: Dir) -> String {
    let stamp = rfc3339_utc(start);
    // 2026-10-26T19:00:04Z -> 20261026-190004
    let digits: String = stamp.chars().filter(char::is_ascii_digit).collect();
    format!(
        "astar-air/{}-{}-{}-{}-{}",
        &digits[..8],
        &digits[8..14],
        key_component(network),
        key_component(talker.unwrap_or("")),
        dir.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn start() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_793_062_804) // 2026-10-27T01:00:04Z
    }

    #[test]
    fn app_base_name_matches_the_spec_example() {
        assert_eq!(
            app_base_name(start(), -6 * 3600, "AllStar 2000"),
            "2026-10-26 19-00-04 AllStar 2000"
        );
    }

    #[test]
    fn unique_base_appends_a_counter() {
        let d = std::env::temp_dir().join(format!("astar-naming-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let b = "2026-10-26 19-00-04 X";
        assert_eq!(unique_base(&d, b, Layout::Stereo), d.join(b));
        std::fs::write(d.join(format!("{b}.flac")), b"").unwrap();
        assert_eq!(unique_base(&d, b, Layout::Stereo), d.join(format!("{b} 2")));
        assert_eq!(unique_base(&d, b, Layout::Mixed), d.join(format!("{b} 2")));
        // TwoMono looks at its own files only.
        assert_eq!(unique_base(&d, b, Layout::TwoMono), d.join(b));
        std::fs::write(d.join(format!("{b} rx.flac")), b"").unwrap();
        assert_eq!(
            unique_base(&d, b, Layout::TwoMono),
            d.join(format!("{b} 2"))
        );
        std::fs::write(d.join(format!("{b} 2 tx.flac")), b"").unwrap();
        assert_eq!(
            unique_base(&d, b, Layout::TwoMono),
            d.join(format!("{b} 3"))
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn sanitize_label_strips_path_characters() {
        assert_eq!(sanitize_label("XLX/458 B:"), "XLX-458 B-");
        assert_eq!(sanitize_label("../.."), "..-..");
        assert_eq!(sanitize_label(""), "Recording");
        assert_eq!(sanitize_label("   "), "Recording");
        assert_eq!(sanitize_label("a\\b\0c\nd"), "a-b-c-d");
        assert_eq!(sanitize_label(&"x".repeat(200)).len(), 120);
        let emoji = sanitize_label(&"\u{1F600}".repeat(64));
        assert_eq!(emoji.len(), 120);
        assert!(
            app_base_name(start(), 0, &"\u{1F600}".repeat(64)).len() + " 12 rx.flac".len() <= 255
        );
        for hostile in ["/", "\"", "..", "a b", "é/ü", "\u{202e}x"] {
            let s = sanitize_label(hostile);
            assert!(!s.chars().any(is_format_char), "{s}");
            assert!(!s.contains(['/', '\\', ':', '\0']), "{s}");
        }
        assert_eq!(
            sanitize_label("a\u{202e}b\u{200b}c\u{feff}d\u{2066}e"),
            "a-b-c-d-e"
        );
        // a bare ".." label is only ever a suffix of the dated base name
        let n = app_base_name(start(), 0, "..");
        assert!(n.starts_with("2026-10-27 01-00-04 "));
        assert!(!n.contains('/'));
    }

    #[test]
    fn key_component_keeps_only_letters_and_digits() {
        assert_eq!(key_component("W1ABC/P"), "W1ABC_P");
        assert_eq!(key_component(""), "unknown");
        assert_eq!(key_component("\"; rm"), "___rm");
        assert_eq!(key_component("ÆØ"), "__");
        assert_eq!(key_component(&"a".repeat(40)).len(), 16);
        assert_eq!(key_component(".."), "__");
    }

    #[test]
    fn capture_key_has_a_corpus_and_passes_unamblify_rules() {
        let t0 = UNIX_EPOCH + Duration::from_secs(1_793_041_204); // 2026-10-26T19:00:04Z
        let k = capture_key(t0, "dstar", Some("W1ABC"), Dir::Rx);
        assert_eq!(k, "astar-air/20261026-190004-dstar-W1ABC-rx");
        let hostile = [
            None,
            Some(""),
            Some("../../etc/passwd"),
            Some("a/b\\c\"d e"),
            Some("ÆØ\0\n"),
            Some(".."),
        ];
        for t in hostile {
            for net in ["dstar", "../x", "", "m 17"] {
                let k = capture_key(start(), net, t, Dir::Tx);
                assert!(
                    k.bytes().all(|b| b.is_ascii_alphanumeric()
                        || matches!(b, b'_' | b'.' | b'/' | b'+' | b'-')),
                    "{k}"
                );
                assert!(
                    k.split('/').all(|c| !c.is_empty() && c != "." && c != ".."),
                    "{k}"
                );
                assert_eq!(k.split('/').count(), 2, "{k}");
            }
        }
    }
}
