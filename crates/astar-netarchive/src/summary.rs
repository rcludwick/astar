// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `log.csv` and `checkins.txt`, the two summaries of a session's overs.
//!
//! A session writes them as it closes. A session that crashed never got that
//! far, so crash recovery rebuilds them from `log.jsonl`. Both go through this
//! file, so a recovered session reads the same as a finished one.

use std::fmt::Write as _;
use std::io;
use std::path::Path;

use crate::callsign::base_call;
use crate::log::replace_file;
use crate::model::{LogKind, LogRow};

pub const CSV_FILE: &str = "log.csv";
pub const CHECKINS_FILE: &str = "checkins.txt";

/// One finished over, as the summaries see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverRec {
    /// RFC 3339.
    pub start: String,
    pub end: String,
    pub duration_ms: u64,
    pub talker: Option<String>,
    /// The file's name under `overs/`.
    pub file: String,
}

/// The overs in `rows`, oldest first.
#[must_use]
pub fn overs_from_rows(rows: &[LogRow]) -> Vec<OverRec> {
    rows.iter()
        .filter_map(|row| match &row.kind {
            LogKind::Over {
                start,
                end,
                duration_ms,
                talker,
                file,
                ..
            } => Some(OverRec {
                start: start.clone(),
                end: end.clone(),
                duration_ms: *duration_ms,
                talker: talker.clone(),
                file: file.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// One CSV field. Talkers come off the air, so a leading `=`, `+`, `-` or `@`
/// (which a spreadsheet would run as a formula) gets a `'` in front, and
/// quotes, commas and line breaks are quoted.
fn csv_field(s: &str) -> String {
    let mut out = String::new();
    if s.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        out.push('\'');
    }
    out.push_str(s);
    if out.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", out.replace('"', "\"\""))
    } else {
        out
    }
}

fn csv_text(overs: &[OverRec]) -> String {
    let mut csv = String::from("start,end,duration_s,talker,file\n");
    for o in overs {
        let _ = writeln!(
            csv,
            "{},{},{}.{},{},{}",
            csv_field(&o.start),
            csv_field(&o.end),
            o.duration_ms / 1000,
            o.duration_ms % 1000 / 100,
            csv_field(o.talker.as_deref().unwrap_or("")),
            csv_field(&o.file),
        );
    }
    csv
}

/// Whether a network names who is talking. `network` is `session.json`'s.
#[must_use]
pub fn carries_callsigns(network: &str) -> bool {
    matches!(network, "m17" | "dstar")
}

fn checkins_text(overs: &[OverRec], network: &str) -> String {
    let mut text = String::new();
    if !carries_callsigns(network) {
        text.push_str("# This network carries no callsigns.\n");
    }
    let mut seen: Vec<String> = Vec::new();
    for base in overs
        .iter()
        .filter_map(|o| o.talker.as_deref().and_then(base_call))
    {
        if !seen.contains(&base) {
            text.push_str(&base);
            text.push('\n');
            seen.push(base);
        }
    }
    text
}

/// Write `log.csv`, replacing any file or link of that name.
///
/// # Errors
/// I/O errors writing or renaming.
pub fn write_log_csv(session_dir: &Path, overs: &[OverRec]) -> io::Result<()> {
    replace_file(session_dir, CSV_FILE, csv_text(overs).as_bytes())
}

/// Write `checkins.txt`: each callsign once, in the order it first spoke.
/// `network` is `session.json`'s.
///
/// # Errors
/// I/O errors writing or renaming.
pub fn write_checkins(session_dir: &Path, overs: &[OverRec], network: &str) -> io::Result<()> {
    let text = checkins_text(overs, network);
    replace_file(session_dir, CHECKINS_FILE, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn over(talker: Option<&str>) -> OverRec {
        OverRec {
            start: "2026-11-04T01:01:00Z".to_owned(),
            end: "2026-11-04T01:01:02Z".to_owned(),
            duration_ms: 2_050,
            talker: talker.map(str::to_owned),
            file: "001 19-01-00 x.flac".to_owned(),
        }
    }

    #[test]
    fn csv_neutralises_formulas_and_quotes_what_needs_it() {
        assert_eq!(csv_field("NC0NTL"), "NC0NTL");
        for hostile in ["=1+1", "+1", "-1", "@SUM(A1)"] {
            assert_eq!(csv_field(hostile), format!("'{hostile}"));
        }
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("a\"b"), "\"a\"\"b\"");
        assert_eq!(csv_field("=a,b"), "\"'=a,b\"");
    }

    #[test]
    fn csv_reads_tenths_of_a_second_and_no_talker_as_empty() {
        let text = csv_text(&[over(None)]);
        assert_eq!(
            text.lines().nth(1).unwrap(),
            "2026-11-04T01:01:00Z,2026-11-04T01:01:02Z,2.0,,001 19-01-00 x.flac"
        );
    }

    #[test]
    fn checkins_list_each_call_once_with_control_characters_dropped() {
        let overs = [over(Some("A\u{1b}B")), over(None), over(Some("A\u{1b}B"))];
        assert_eq!(checkins_text(&overs, "m17"), "AB\n");
        assert_eq!(checkins_text(&overs, "dstar"), "AB\n");
    }

    #[test]
    fn checkins_list_each_base_call_once() {
        let overs = [
            over(Some("KX0AAA  D")),
            over(Some("KX0AAA E")),
            over(Some("N0CALL-1")),
            over(None),
            over(Some("KX0AAA  D")),
        ];
        assert_eq!(checkins_text(&overs, "m17"), "KX0AAA\nN0CALL\n");
    }

    #[test]
    fn a_network_without_callsigns_says_so() {
        assert_eq!(
            checkins_text(&[], "allstar"),
            "# This network carries no callsigns.\n"
        );
    }
}
