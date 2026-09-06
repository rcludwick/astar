// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The capture side of a digital-voice session, opened lazily.
//!
//! Written for D-Star (iax-2f6b) and shared with System Fusion rather than
//! copied. Nothing in it is D-Star-shaped: it resolves a capture device,
//! opens it on the first key-down rather than at connect, and gates it on
//! PTT. A second copy would be a second answer to "when does the microphone
//! actually open", and those two answers would drift.
//!
//! Lazy on purpose, for two reasons that apply to every network:
//!
//! * a receive-only session must work on a machine with no usable microphone
//!   — none attached, permission denied, or already held exclusively — so
//!   resolving or opening one at connect made that fatal;
//! * a live microphone should exist only while it can be used, not for the
//!   whole lifetime of a session that may never key.

use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::mpsc::Sender;

use astar_audio::{AudioRouter, CallAudio, MicId, StreamConfig};

/// The capture side of a session, opened LAZILY (see the module docs): a
/// resolved-but-unopened device id, the parked TX `Sender`
/// [`AudioRouter::open_monitor_call`] handed back, and whether the stream has
/// actually been opened yet.
///
/// Two reasons this isn't just an open mic:
///
/// - a receive-only D-Star session must work on a machine with no usable
///   microphone (none attached, permission denied, or already exclusively
///   held) — resolving/opening one at connect made that fatal;
/// - a live microphone should exist only while it can actually be used, not
///   for the whole lifetime of a session that may never key.
pub(crate) struct MicLane {
    /// The resolved capture device, `None` when none could be resolved.
    ///
    /// `pub(crate)` because the run loops read it to meter the lane.
    pub(crate) id: Option<MicId>,
    /// Parked until the lane is opened, then handed to the router.
    tx: Option<Sender<Vec<i16>>>,
    /// The call's VOX pre-roll cell, carried into the lane on open.
    preroll_lead: Arc<AtomicU32>,
    config: StreamConfig,
    /// `true` once the capture stream is open (and therefore once
    /// `set_gate` means anything).
    opened: bool,
}

impl MicLane {
    pub(crate) fn new(
        id: Option<MicId>,
        tx: Sender<Vec<i16>>,
        call_audio: &CallAudio,
        config: StreamConfig,
    ) -> MicLane {
        MicLane {
            id,
            tx: Some(tx),
            preroll_lead: Arc::clone(&call_audio.preroll_lead),
            config,
            opened: false,
        }
    }

    /// A lane that reports itself already open, for unit tests that drive
    /// [`apply_ptt_edge`] directly against an unopened `NullBackend` router
    /// (`set_gate` on a mic the router never opened is a documented no-op —
    /// the same "valid if inert stand-in" idiom `crate::m17`'s own tests
    /// use). Never constructed outside tests.
    #[cfg(test)]
    /// A lane for which no capture device was ever resolved — the shape a
    /// session gets on a machine with no usable microphone. Exists so tests
    /// can build that case without the module exposing its fields.
    #[cfg(test)]
    pub(crate) fn unresolved(config: StreamConfig) -> MicLane {
        MicLane {
            id: None,
            tx: None,
            preroll_lead: Arc::new(AtomicU32::new(0)),
            config,
            opened: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn opened_stub(id: &str) -> MicLane {
        MicLane {
            id: Some(MicId::new(id)),
            tx: None,
            preroll_lead: Arc::new(AtomicU32::new(0)),
            config: StreamConfig::default(),
            opened: true,
        }
    }

    /// Open the capture stream if it isn't already, returning `false` when
    /// this session cannot transmit at all (no device resolved, or the open
    /// failed). Callers must treat `false` as "refuse this key-down": a
    /// transmission with no possible audio is worse than none, since it puts
    /// an RF header and a stream of silence on the reflector.
    pub(crate) fn ensure_open(&mut self, router: &mut AudioRouter) -> bool {
        if self.opened {
            return true;
        }
        let Some(id) = self.id.as_ref() else {
            tracing::error!(
                "dstar: PTT requested but no capture device was resolved for this session — \
                 refusing to key"
            );
            return false;
        };
        let Some(tx) = self.tx.take() else {
            tracing::error!("dstar: mic lane's TX sender already consumed — refusing to key");
            return false;
        };
        match router.open_mic_lane(id, tx.clone(), Arc::clone(&self.preroll_lead), self.config) {
            Ok(()) => {
                self.opened = true;
                true
            }
            Err(e) => {
                // Park the sender again so a later key-down can retry (the
                // device may come back, or permission may be granted).
                self.tx = Some(tx);
                tracing::error!(
                    error = ?e,
                    "dstar: could not open the capture device for transmit — refusing to key"
                );
                false
            }
        }
    }

    /// Key/unkey the lane's gate. A no-op before the lane is opened — which
    /// is exactly right for the unkey direction (nothing can be capturing).
    pub(crate) fn set_gate(&self, router: &AudioRouter, keyed: bool) {
        if let Some(id) = self.id.as_ref() {
            router.set_gate(id, keyed);
        }
    }
}
