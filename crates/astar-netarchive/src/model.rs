// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The records stored in `session.json` and `log.jsonl`.

use serde::{Deserialize, Serialize};

/// The `session.json` format this build writes. A file with no `format` key
/// was written before the key existed, and it is format 1.
pub const META_FORMAT: u32 = 1;

const fn meta_format_one() -> u32 {
    1
}

/// How a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ended {
    Window,
    Stopped,
    Crashed,
    Error,
}

/// `session.json`: one recorded session of a net.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// See [`META_FORMAT`].
    #[serde(default = "meta_format_one")]
    pub format: u32,
    pub net: String,
    /// `"m17"` or `"dstar"`.
    pub network: String,
    /// `"host:port module"`.
    pub target: String,
    /// RFC 3339.
    pub window_start: String,
    pub window_end: String,
    pub visibility: String,
    pub my_side: bool,
    pub sample_rate: u32,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub ended: Option<Ended>,
    pub rejoins: u32,
    pub dropped_frames: u64,
    pub overs: u32,
    pub astar_version: String,
}

/// One `log.jsonl` line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRow {
    /// RFC 3339, UTC.
    pub t: String,
    #[serde(flatten)]
    pub kind: LogKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LogKind {
    Joined,
    Over {
        start: String,
        end: String,
        duration_ms: u64,
        offset_ms: u64,
        talker: Option<String>,
        file: String,
    },
    Dropped {
        reason: String,
    },
    Rejoined,
    Left,
    Error {
        message: String,
    },
}
