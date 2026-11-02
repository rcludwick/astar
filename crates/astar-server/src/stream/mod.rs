// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The live audio streams: the tap on the conference, the encoders, and the
//! fan-out to listeners.

pub mod encode;
pub mod levels;
pub mod ring;
pub mod tap;

pub use crate::conference_tap::TapSource;
pub use tap::{StreamHub, TapState};
