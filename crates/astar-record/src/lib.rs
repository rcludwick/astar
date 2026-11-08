// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! Call recording for astar.

pub mod align;
#[cfg(feature = "writer")]
pub mod capture;
pub mod clock;
#[cfg(feature = "writer")]
pub mod disk;
#[cfg(feature = "writer")]
pub mod error;
#[cfg(feature = "writer")]
pub mod flac;
#[cfg(feature = "writer")]
pub mod frame_writer;
pub mod frames;
#[cfg(feature = "writer")]
pub mod layout;
#[cfg(feature = "writer")]
pub mod naming;
pub mod over;
#[cfg(feature = "writer")]
pub mod packet_log;
pub mod packet_pool;
pub mod packet_tap;
#[cfg(feature = "writer")]
pub mod pcap;
#[cfg(feature = "writer")]
pub mod recorder;
pub mod tap;
#[cfg(feature = "writer")]
pub mod tarball;

#[cfg(feature = "writer")]
pub use error::RecordError;
#[cfg(feature = "writer")]
pub use flac::{FLAC_BLOCK, FLAC_HEADER_LEN, FlacSummary, FlacWriter};

pub use align::{Aligner, MAX_BACKLOG_FRAMES, PRIME_FRAMES};
#[cfg(feature = "writer")]
pub use layout::{Layout, LayoutWriter};
pub use over::{OverEvent, OverSplitter, SplitterConfig};
#[cfg(feature = "writer")]
pub use recorder::{RecordState, Recorder, RecorderConfig, RecorderInputs};
