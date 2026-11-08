// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The on-disk archive format of recorded nets.

pub mod layout;
pub mod log;
pub mod model;
pub mod retain;
pub mod summary;

pub use layout::{net_dir_name, over_file_name, session_dir_name};
pub use log::{LOG_FILE, META_FILE, append_row, read_meta, read_rows, write_meta};
pub use model::{Ended, LogKind, LogRow, META_FORMAT, SessionMeta};
pub use retain::{
    MIN_FREE_BYTES, RetainPolicy, RetainReport, free_bytes, recover_unfinished, retain,
};
pub use summary::{OverRec, overs_from_rows, write_checkins, write_log_csv};
