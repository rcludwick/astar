// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Folder and file names inside the archive.

use astar_record::naming::{key_component, sanitize_label};

/// The folder for a net: sanitised, lowercased, spaces become `-`.
#[must_use]
pub fn net_dir_name(net_name: &str) -> String {
    sanitize_label(net_name).to_lowercase().replace(' ', "-")
}

/// The folder for a session, in local time: `2026-11-03 1855`.
#[must_use]
pub fn session_dir_name(local_start: &jiff::Zoned) -> String {
    local_start.strftime("%Y-%m-%d %H%M").to_string()
}

/// `001 19-00-12 NC0NTL.flac`; no talker reads `unknown`.
#[must_use]
pub fn over_file_name(seq: u32, local_start: &jiff::Zoned, talker: Option<&str>) -> String {
    let who = key_component(talker.unwrap_or(""));
    format!("{seq:03} {} {who}.flac", local_start.strftime("%H-%M-%S"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> jiff::Zoned {
        s.parse().unwrap()
    }

    #[test]
    fn over_file_name_is_seq_time_talker() {
        let t = at("2026-11-03T19:00:12-07:00[America/Denver]");
        assert_eq!(
            over_file_name(7, &t, Some("NC0NTL")),
            "007 19-00-12 NC0NTL.flac"
        );
        assert_eq!(over_file_name(7, &t, None), "007 19-00-12 unknown.flac");
    }

    #[test]
    fn session_dir_is_date_and_hhmm() {
        let t = at("2026-11-03T18:55:00-07:00[America/Denver]");
        assert_eq!(session_dir_name(&t), "2026-11-03 1855");
    }

    #[test]
    fn net_dir_name_is_lowercase_dashed() {
        assert_eq!(net_dir_name("Tuesday M17 net"), "tuesday-m17-net");
    }
}
