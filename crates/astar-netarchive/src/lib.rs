// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The on-disk archive format of recorded nets.

pub mod callsign;
#[cfg(any(test, feature = "fixture"))]
pub mod fixture;
pub mod index;
pub mod layout;
pub mod log;
pub mod model;
pub mod peaks;
pub mod retain;
pub mod summary;
pub mod view;
pub mod web;

pub use callsign::{Call, base_call, split_call};
pub use index::{Access, Archive, SessionEntry, is_public, slug};
pub use layout::{net_dir_name, over_file_name, session_dir_name};
pub use log::{LOG_FILE, META_FILE, append_row, read_meta, read_rows, write_meta};
pub use model::{Ended, LogKind, LogRow, META_FORMAT, SessionMeta};
pub use peaks::{PEAK_BINS, PEAKS_FILE, Peaks, compute_peaks, read_peaks, write_peaks};
pub use retain::{
    DeleteRefusal, MIN_FREE_BYTES, RetainPolicy, RetainReport, delete_session, free_bytes,
    recover_unfinished, retain,
};
pub use summary::{OverRec, carries_callsigns, overs_from_rows, write_checkins, write_log_csv};
pub use view::{
    AudioSrc, Item, ListView, NetView, SHORT_OVER_MS, SessionCard, SessionView, ViewOpts,
    list_view, session_view,
};
pub use web::{NETS_CSP, NetsReply, NetsRequest, Pages};
