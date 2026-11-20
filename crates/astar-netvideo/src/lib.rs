// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Video frames for the net recorder. Receive-only: this crate draws pictures
//! and holds nothing that can reach a radio.

#[doc(hidden)]
pub mod demo;
pub mod ffmpeg;
pub mod flv;
pub mod font;
pub mod live;
pub mod render;
pub mod rtmp;
pub mod scene;
pub mod tap;
pub mod video;

pub use ffmpeg::{Ffmpeg, FfmpegError, find};
pub use flv::{FlvReader, FlvTag, TagKind};
pub use font::{Weight, draw_text, fit, measure};
pub use live::{LiveError, LiveStream, LiveSummary};
pub use render::{RenderStats, Renderer, Size};
pub use rtmp::{DestName, DestState, DestStatus, Destination, IngestUrl, Publisher, SeqHeaders};
pub use scene::{Bubble, FrameState, Mode, SceneBuilder, SessionInfo, Talking};
pub use tap::{LIVE_TAP_CAPACITY, LiveTap, TapMsg, TapReceiver, live_tap};
pub use video::{
    LADDER, Profile, RENDER_LOCK, RenderOptions, VIDEO_FILE, VIDEO_PART, VideoError, VideoReport,
    remove_leftover_part, render_session,
};
