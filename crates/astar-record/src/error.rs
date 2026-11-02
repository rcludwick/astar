// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

/// What can go wrong while recording.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Encode(String),
    #[error("disk has only {} MB free; recording needs 500 MB", free_bytes / 1_000_000)]
    DiskLow { free_bytes: u64 },
    #[error("already recording")]
    AlreadyRecording,
    #[error("cannot use the recordings folder: {0}")]
    Folder(String),
}
