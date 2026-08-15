// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Announcement injection primitives (iax-e30d): a finite PCM source pushed
//! into a call's TX path (Seize/MixUnder) or onto an output bus (monitor).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// How a to-air announcement interacts with live mic audio.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AnnouncePolicy {
    /// Take over TX: the announcement replaces captured mic audio for its
    /// duration (the live mic is muted while it plays).
    Seize,
    /// Sum the announcement over live audio at `gain_db` (negative = quieter).
    /// The CW-ID case.
    MixUnder { gain_db: f32 },
}

/// Caller-side handle to one in-flight announcement.
#[derive(Clone)]
pub struct AnnounceHandle {
    done: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

/// Lane-side cells (audio thread flips `done`; reads `cancel`).
pub(crate) struct AnnounceCells {
    pub(crate) done: Arc<AtomicBool>,
    pub(crate) cancel: Arc<AtomicBool>,
}

impl AnnounceHandle {
    #[must_use]
    pub(crate) fn new() -> (Self, AnnounceCells) {
        let done = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        (
            Self {
                done: Arc::clone(&done),
                cancel: Arc::clone(&cancel),
            },
            AnnounceCells { done, cancel },
        )
    }

    /// A handle that starts already done — used as a placeholder for queued
    /// (not-yet-started) announcements so the caller has a handle to poll.
    #[must_use]
    pub fn new_placeholder() -> Self {
        Self {
            done: Arc::new(AtomicBool::new(true)),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// `true` once the lane has consumed the whole PCM buffer (or it was cancelled).
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    /// Request early termination; the lane stops on its next callback.
    /// Also marks this handle as done immediately so `is_done()` returns `true`
    /// right away — the caller doesn't need to wait for the next audio callback.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.done.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_starts_active_and_cancel_is_observable() {
        let (h, cells) = AnnounceHandle::new();
        assert!(!h.is_done());
        h.cancel();
        assert!(cells.cancel.load(Ordering::Relaxed), "lane sees cancel");
        cells.done.store(true, Ordering::Relaxed);
        assert!(h.is_done(), "lane sees done via shared cell");
    }
}
