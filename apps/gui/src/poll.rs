// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The poll subscription (astar-e39d).
//!
//! The core is poll-based: there are no callbacks, a consumer calls
//! `snapshot()` whenever it wants fresh state. We drive that at ~20 Hz (every
//! 50 ms) with an Iced time subscription that emits [`Message::Tick`]; the
//! `update` handler then pulls a fresh [`crate::snapshot::Snapshot`] into state.

use std::time::Duration;

use iced::time;
use iced::Subscription;

use crate::app::Message;

/// Poll cadence: 20 Hz. Fast enough that VU meters look live, cheap enough to
/// run continuously.
pub const POLL: Duration = Duration::from_millis(50);

/// The app subscription: a 50 ms heartbeat mapped to [`Message::Tick`].
pub fn subscription() -> Subscription<Message> {
    time::every(POLL).map(|_| Message::Tick)
}
