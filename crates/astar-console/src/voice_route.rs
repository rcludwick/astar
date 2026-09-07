// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The audio lane a digital-voice session (M17, D-Star, System Fusion) is
//! handed, opened on the station's ONE router.
//!
//! A session used to build its own `AudioRouter` over its own backend, on
//! its own thread, and so had to mirror meters, carry preferences, decide
//! when the microphone opened and key the gate — three sessions, three
//! answers, and every gap between them was a bug. Now the session gets the
//! channel ends and nothing else; everything about devices, meters, DSP
//! and keying lives here and in `ConsoleSession`, once.
//!
//! Capture policy: the lane opens at connect if a device resolves and opens
//! (gate closed), so the continuous input meter and VOX work from the first
//! second, as on IAX2 and M17. If it cannot, the route is receive-only for
//! now and every key-down retries — a machine with no usable microphone
//! must still receive.

use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::mpsc::Sender;

use astar_audio::mixer::MixCallId;
use astar_audio::{AudioRouter, CallAudio, MicId, OutputId, StreamConfig, StreamHandle};

/// One digital-voice session's lanes on the station router: an output bus
/// slot, and — when a capture device resolved — a mic lane whose gate is the
/// session's PTT.
pub(crate) struct VoiceRoute {
    out: OutputId,
    mix_id: MixCallId,
    /// The resolved capture device, `None` on a receive-only machine.
    mic: Option<MicId>,
    /// Parked until the lane opens, then handed to the router.
    mic_tx: Option<Sender<Vec<i16>>>,
    preroll_lead: Arc<AtomicU32>,
    config: StreamConfig,
    mic_open: bool,
}

impl VoiceRoute {
    /// Open the bus on `router` (and, if `mic` is `Some`, try the lane) and
    /// return the route plus the session's channel ends.
    ///
    /// # Errors
    /// Only the output bus is fatal; a capture lane that fails to open is
    /// logged and retried on key-down.
    pub(crate) fn open(
        router: &mut AudioRouter,
        mic: Option<MicId>,
        out: OutputId,
        config: StreamConfig,
    ) -> Result<(VoiceRoute, CallAudio), astar_audio::AudioError> {
        let (audio, mic_tx, mix_id) = router.open_monitor_call(&out, config)?;
        let mut route = VoiceRoute {
            out,
            mix_id,
            mic,
            mic_tx: Some(mic_tx),
            preroll_lead: Arc::clone(&audio.preroll_lead),
            config,
            mic_open: false,
        };
        // Eager, but not fatal.
        let _ = route.ensure_mic(router);
        Ok((route, audio))
    }

    pub(crate) fn out(&self) -> &OutputId {
        &self.out
    }

    /// The mic id whenever a device resolved — open or not — so meters and
    /// preferences address the lane the moment it exists.
    pub(crate) fn mic(&self) -> Option<&MicId> {
        self.mic.as_ref()
    }

    pub(crate) fn tx_capable(&self) -> bool {
        self.mic.is_some()
    }

    /// Open the capture lane if it isn't. `false` = this route cannot
    /// transmit right now (no device, or the open failed).
    pub(crate) fn ensure_mic(&mut self, router: &mut AudioRouter) -> bool {
        if self.mic_open {
            return true;
        }
        let Some(id) = self.mic.as_ref() else {
            return false;
        };
        let Some(tx) = self.mic_tx.take() else {
            return false;
        };
        match router.open_mic_lane(id, tx.clone(), Arc::clone(&self.preroll_lead), self.config) {
            Ok(()) => {
                self.mic_open = true;
                true
            }
            Err(e) => {
                // Park the sender again: the device may come back, or
                // permission may be granted, before the next key-down.
                self.mic_tx = Some(tx);
                tracing::warn!(
                    error = ?e,
                    "voice route: capture device would not open — receive only for now"
                );
                false
            }
        }
    }

    /// Key or unkey. `false` = refused (see [`Self::ensure_mic`]); the
    /// caller must not transmit.
    pub(crate) fn key(&mut self, router: &mut AudioRouter, on: bool) -> bool {
        if on && !self.ensure_mic(router) {
            return false;
        }
        if let Some(id) = self.mic.as_ref() {
            router.set_gate(id, on);
        }
        true
    }

    /// Unbind and close both lanes. Returns the stream handles for the
    /// caller to drop outside any lock.
    pub(crate) fn release(self, router: &mut AudioRouter) -> Vec<Box<dyn StreamHandle>> {
        let mut handles = Vec::with_capacity(2);
        if let Some(id) = self.mic.as_ref() {
            router.set_gate(id, false);
            router.unbind_mic(id);
            handles.extend(router.close_mic(id));
        }
        router.remove_from_bus(&self.out, self.mix_id);
        if let Some(handle) = router.close_output(&self.out) {
            handles.push(handle);
        } else {
            // The bus still carries a lane, so the stream stays open and its
            // handle stays with the router. The exclusion guards should make
            // that impossible for a voice route; say so rather than leak an
            // output device silently if they ever stop holding.
            tracing::warn!(
                bus = self.out.as_str(),
                "voice route: output bus still in use at release — stream left open"
            );
        }
        handles
    }
}
