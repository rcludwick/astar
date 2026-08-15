// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The `Call` handle and its event stream (iax-612e).
//!
//! iax-42e9 keystone: this module owns the ONE canonical [`Call`] handle that
//! every origination path yields — outbound `Manager::dial`, inbound
//! `accept`/auto-answer (iax-8baf), and `Manager::adopt`. The Manager-assigned
//! [`CallId`] is the process-level routing key (distinct from the wire
//! `CallNo`), and [`CallSnapshot`] is the single secret-free read surface that
//! the iax-ad2e Link roster and the iax-1075 FFI consume.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use astar_iax_core::VoiceFormat;
use astar_iax_core::session::fsm::{AppCommand, FailReason};

use crate::error::IaxError;
use crate::runtime::RuntimeCommand;

/// Manager-assigned stable process-level identity for a call. Equal to the
/// `ConnectionSpec.id` it was dialed/adopted from. DISTINCT from the wire
/// `CallNo` (per-socket, stays `CallNo(1)` — Q4). Newtype, never a bare int.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CallId(pub(crate) u64);

impl CallId {
    /// Construct a `CallId` from a raw process-level identifier. The value
    /// equals the `ConnectionSpec.id` the call is dialed/adopted from (Q5).
    #[must_use]
    pub fn from_raw(id: u64) -> Self {
        Self(id)
    }

    /// The raw identifier (for FFI / config round-trips).
    #[must_use]
    pub fn as_raw(self) -> u64 {
        self.0
    }
}

/// The call's mode as it appears in a secret-free snapshot. Mirrors
/// [`crate::CallMode`]'s shape without carrying any auth material.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum CallSnapshotMode {
    /// Standard IAX2 call.
    Direct,
    /// `AllStarLink` web-transceiver guest path.
    WebTransceiver,
}

/// Coarse connection state for a snapshot. Secret-free, serde-derivable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum CallSnapshotState {
    /// Dialing/handshaking; not yet answered.
    Connecting,
    /// Answered and media-active.
    Active,
    /// Ended (peer hangup, local hangup, or failure).
    Hungup,
}

/// Secret-free, serde-friendly snapshot of a call's routing-visible state.
/// THE single read surface for iax-ad2e (Link roster) and iax-1075 (FFI):
/// extend THIS, do not add parallel Manager accessors.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CallSnapshot {
    /// The Manager-assigned pool key.
    pub id: CallId,
    /// Node number / dial target (carried from the dial/adopt spec).
    pub node: String,
    /// Current call mode (secret-free).
    pub mode: CallSnapshotMode,
    /// Whether the call is keyed (PTT engaged).
    pub keyed: bool,
    /// Coarse connection state.
    pub state: CallSnapshotState,
    /// Routed mic (`MicId.as_str`), `None` = monitor-only.
    pub routed_mic: Option<String>,
    /// Output bus (`OutputId.as_str`).
    pub output: String,
    /// Cumulative voice-ts-ladder re-anchors (>80 ms drift events) on the
    /// outbound TX clock (iax-5530/iax-9e55). A non-zero, growing value points
    /// at choppy TX. A plain `u64` health counter, credential-free.
    pub tx_reanchors: u64,
    /// Cumulative cpal capture overruns (dropped input buffers — holes in the
    /// captured mic PCM) on the call's routed mic (iax-9e55). `0` while
    /// monitor-only (no mic). The lead suspect for choppy TX. A plain `u64`
    /// health counter, credential-free.
    pub tx_capture_overruns: u64,
    /// Negotiated voice codec, once known (`None` while connecting) —
    /// iax-31f7.
    pub negotiated_format: Option<VoiceFormat>,
    /// Remote peer's key state (true = the far end is transmitting / talking).
    /// Driven by the wire-side key/unkey control frames; `false` for calls
    /// whose runtime never reports it (iax-24e2 — the "who's talking" bit).
    #[cfg_attr(feature = "serde", serde(default))]
    pub remote_keyed: bool,
}

impl CallSnapshot {
    /// Whether the call is keyed (PTT engaged). ad2e/FFI accessor.
    #[must_use]
    pub fn is_keyed(&self) -> bool {
        self.keyed
    }
    /// Whether the call is media-active. ad2e/FFI accessor.
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(self.state, CallSnapshotState::Active)
    }
}

/// Lifecycle + media-side notifications delivered on the call's event channel.
/// State is observed here, not encoded in the type (single-struct design).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum CallEvent {
    /// Peer is ringing (call accepted, not yet answered).
    Ringing,
    /// Peer answered; `format` is the negotiated voice codec.
    Answered { format: VoiceFormat },
    /// Inbound DTMF digit.
    Dtmf(char),
    /// Peer keyed/unkeyed (PTT) state changed.
    RemotePtt(bool),
    /// Inbound text frame.
    Text(String),
    /// Keepalive: inbound silence exceeded the loss deadline. Edge-triggered;
    /// the call is still up and recovers on its own (iax-a307).
    ConnectionLost,
    /// Keepalive: traffic resumed after a `ConnectionLost`.
    ConnectionRestored,
    /// Call ended (peer hangup, local hangup, or failure).
    Hangup { reason: FailReason },
}

/// Shared call-state cell encoding (published by the runtime loop, read by
/// [`Call::snapshot`]). `0` = Connecting, `1` = Active, `2` = Hungup.
pub(crate) const STATE_CONNECTING: u8 = 0;
pub(crate) const STATE_ACTIVE: u8 = 1;
pub(crate) const STATE_HUNGUP: u8 = 2;

/// Handle to a live call — the ONE canonical handle (iax-42e9 keystone) that
/// `Manager::dial`, inbound `accept`/auto-answer (iax-8baf), and
/// `Manager::adopt` all yield/consume. Owns the channel into the runtime
/// thread and the waker that interrupts its `poll()` so commands are processed
/// promptly.
///
/// The `id`/`node`/`mode` and the manager-set routing cells
/// (`routed_mic`/`output`) feed [`Call::snapshot`] — the single secret-free
/// read surface for iax-ad2e (roster) and iax-1075 (FFI).
pub struct Call {
    cmd_tx: Sender<RuntimeCommand>,
    waker: Arc<mio::Waker>,
    handle: Option<JoinHandle<()>>,
    ptt: crate::audio_bridge::PttGate,
    /// Shared smoothed-RTT cell published by the runtime loop (micros;
    /// `u32::MAX` = no sample yet). iax-a307.
    rtt_micros: Arc<AtomicU32>,
    /// Manager-assigned stable identity (keystone). `CallId(0)` for the
    /// non-pooled `Client::dial` path that never enters a `Manager`.
    id: CallId,
    /// Dial target / node number, carried for the snapshot. Empty for the
    /// non-pooled client path.
    node: String,
    /// Call mode for the snapshot (secret-free).
    mode: CallSnapshotMode,
    /// Shared connection-state cell published by the runtime loop.
    state: Arc<std::sync::atomic::AtomicU8>,
    /// Routing fact owned by the `Manager`: the routed mic id (`None` =
    /// monitor-only). Updated by `Manager::route`/`unroute`; read by `snapshot`.
    routed_mic: Arc<std::sync::Mutex<Option<String>>>,
    /// Routing fact owned by the `Manager`: the output bus id. Set on pooling.
    output: Arc<std::sync::Mutex<String>>,
    /// Cumulative voice-ts-ladder re-anchor counter published by the runtime
    /// loop (iax-9e55). Read by [`Call::snapshot`] as `tx_reanchors`.
    tx_reanchors: Arc<AtomicU64>,
    /// The routed mic's cumulative cpal capture-overrun cell (iax-9e55), set by
    /// `Manager::route` and cleared on unroute / re-route. `None` = monitor-only
    /// (no mic), reported as `0`. Mirrors `routed_mic`: a routing fact the
    /// Manager owns and the snapshot reads.
    capture_overruns: Arc<std::sync::Mutex<Option<Arc<AtomicU64>>>>,
    /// Negotiated-codec cell published by the run-loop once per event-handling
    /// pass (iax-31f7): format bits (`VoiceFormat::as_u32`), `0` = not yet
    /// negotiated. Read by [`Call::snapshot`] via `VoiceFormat::from_u32`.
    format_bits: Arc<AtomicU32>,
    /// Adopt path (iax-8baf): the RX `Receiver` the inbound run-loop fills,
    /// which `Manager::adopt` adds to an output bus mixer. `None` for the dial
    /// path (the router wires RX internally) and once taken.
    adopt_rx_source: Option<Receiver<Vec<i16>>>,
    /// Adopt path: the parked TX `Sender` a mic lane is bound to on `route`.
    adopt_tx_sender: Option<Sender<Vec<i16>>>,
    /// Adopt path (iax-4348): the inbound leg's bus sample rate (its listener
    /// policy's codec cap). `Manager::adopt` rejects a leg whose rate differs
    /// from the station bus. `None` for the dial path (never adopted).
    adopt_sample_rate: Option<u32>,
    /// Live remote-PTT-key cell (iax-feab parrot mode): the runtime stores
    /// into this whenever `AppEvent::RemotePtt` fires (dial path: `runtime.rs`;
    /// inbound path: `listener.rs`), BEFORE translating the event to a
    /// `CallEvent`. `Conference::add_member_keyed` reads it via
    /// [`Call::remote_keyed_handle`] so a parrot member's PTT gate reflects
    /// the peer's remote key state (not just local VOX).
    remote_keyed: Arc<AtomicBool>,
}

impl Call {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        cmd_tx: Sender<RuntimeCommand>,
        waker: Arc<mio::Waker>,
        handle: JoinHandle<()>,
        ptt: crate::audio_bridge::PttGate,
        rtt_micros: Arc<AtomicU32>,
        tx_reanchors: Arc<AtomicU64>,
        format_bits: Arc<AtomicU32>,
        remote_keyed: Arc<AtomicBool>,
    ) -> Self {
        Self {
            cmd_tx,
            waker,
            handle: Some(handle),
            ptt,
            rtt_micros,
            id: CallId(0),
            node: String::new(),
            mode: CallSnapshotMode::Direct,
            state: Arc::new(std::sync::atomic::AtomicU8::new(STATE_CONNECTING)),
            routed_mic: Arc::new(std::sync::Mutex::new(None)),
            output: Arc::new(std::sync::Mutex::new(String::new())),
            tx_reanchors,
            capture_overruns: Arc::new(std::sync::Mutex::new(None)),
            format_bits,
            adopt_rx_source: None,
            adopt_tx_sender: None,
            adopt_sample_rate: None,
            remote_keyed,
        }
    }

    /// Build the canonical handle for a dialed pooled call. The `Manager`
    /// supplies the stable `id`, the `node`, the `mode`, and the shared `state`
    /// cell published by the runtime. The router has already wired the channel
    /// ends into the run-loop (dial path), so no `CallAudio` is carried.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_pooled(
        cmd_tx: Sender<RuntimeCommand>,
        waker: Arc<mio::Waker>,
        handle: JoinHandle<()>,
        ptt: crate::audio_bridge::PttGate,
        rtt_micros: Arc<AtomicU32>,
        id: CallId,
        node: String,
        mode: CallSnapshotMode,
        state: Arc<std::sync::atomic::AtomicU8>,
        tx_reanchors: Arc<AtomicU64>,
        format_bits: Arc<AtomicU32>,
        remote_keyed: Arc<AtomicBool>,
    ) -> Self {
        Self {
            cmd_tx,
            waker,
            handle: Some(handle),
            ptt,
            rtt_micros,
            id,
            node,
            mode,
            state,
            routed_mic: Arc::new(std::sync::Mutex::new(None)),
            output: Arc::new(std::sync::Mutex::new(String::new())),
            tx_reanchors,
            capture_overruns: Arc::new(std::sync::Mutex::new(None)),
            format_bits,
            adopt_rx_source: None,
            adopt_tx_sender: None,
            adopt_sample_rate: None,
            remote_keyed,
        }
    }

    /// Build the canonical handle for an INBOUND leg (iax-8baf `accept` /
    /// auto-answer), to be handed to [`crate::Manager::adopt`]. Carries the same
    /// secret-free snapshot surface as a dialed call plus the router-wiring ends
    /// the Manager consumes: `rx_source` (the inbound RX `Receiver` joined to an
    /// output bus mixer) and `tx_sender` (the parked TX `Sender` a mic lane is
    /// bound to on `route`). The `CallAudio` the inbound run-loop drains/fills is
    /// the same `(tx_sender's paired Receiver, rx_source's paired Sender)`.
    ///
    /// Ungated by iax-8baf Phase F: the runtime `Listener`'s `answer()` /
    /// auto-answer path builds the inbound `Call` here (the `test_support`
    /// `fake_inbound_call` still exercises the no-socket pooling path).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_inbound(
        cmd_tx: Sender<RuntimeCommand>,
        waker: Arc<mio::Waker>,
        handle: JoinHandle<()>,
        ptt: crate::audio_bridge::PttGate,
        rtt_micros: Arc<AtomicU32>,
        id: CallId,
        node: String,
        mode: CallSnapshotMode,
        state: Arc<std::sync::atomic::AtomicU8>,
        rx_source: Receiver<Vec<i16>>,
        tx_sender: Sender<Vec<i16>>,
        format_bits: Arc<AtomicU32>,
        sample_rate: u32,
        remote_keyed: Arc<AtomicBool>,
    ) -> Self {
        Self {
            cmd_tx,
            waker,
            handle: Some(handle),
            ptt,
            rtt_micros,
            id,
            node,
            mode,
            state,
            routed_mic: Arc::new(std::sync::Mutex::new(None)),
            output: Arc::new(std::sync::Mutex::new(String::new())),
            // Inbound legs ride the simpler `+=20` ladder that can't re-anchor,
            // so this stays a parked 0 cell (iax-9e55). Capture overruns are
            // still meaningful once a handset mic is routed.
            tx_reanchors: Arc::new(AtomicU64::new(0)),
            capture_overruns: Arc::new(std::sync::Mutex::new(None)),
            format_bits,
            adopt_rx_source: Some(rx_source),
            adopt_tx_sender: Some(tx_sender),
            adopt_sample_rate: Some(sample_rate),
            remote_keyed,
        }
    }

    /// The Manager-assigned stable identity (keystone). Distinct from the wire
    /// `CallNo`.
    #[must_use]
    pub fn call_id(&self) -> CallId {
        self.id
    }

    /// Adopt path (iax-8baf integration point): the inbound RX `Receiver` the
    /// `Manager` adds to an output bus mixer. `None` for the dial path.
    pub(crate) fn take_adopt_rx_source(&mut self) -> Option<Receiver<Vec<i16>>> {
        self.adopt_rx_source.take()
    }

    /// Adopt path: the parked TX `Sender` a mic lane is bound to on `route`.
    pub(crate) fn take_adopt_tx_sender(&mut self) -> Option<Sender<Vec<i16>>> {
        self.adopt_tx_sender.take()
    }

    /// Adopt path (iax-4348): the inbound leg's bus sample rate, for the
    /// Manager's rate-match guard. `None` on the dial path (never adopted).
    pub(crate) fn adopt_sample_rate(&self) -> Option<u32> {
        self.adopt_sample_rate
    }

    /// The per-call run-loop waker, handed to the router's `bind_mic` so a
    /// routed mic wakes THIS call's loop after a frame send (Q1).
    pub(crate) fn waker(&self) -> Arc<mio::Waker> {
        Arc::clone(&self.waker)
    }

    /// Live remote-PTT-key handle (iax-feab parrot mode): `true` while the
    /// peer is keyed, updated by the runtime the moment `AppEvent::RemotePtt`
    /// fires. `Manager::adopt` / the handset→conference live switch pass this
    /// to `Conference::add_member_keyed` so a parrot member's take is gated on
    /// the PEER's PTT, not just local VOX.
    pub(crate) fn remote_keyed_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.remote_keyed)
    }

    /// Manager-side setter: record the routed mic id (`None` = monitor-only).
    pub(crate) fn set_routed_mic(&self, mic: Option<String>) {
        *self.routed_mic.lock().expect("routed_mic mutex") = mic;
    }

    /// Manager-side setter: bind the routed mic's cumulative capture-overrun
    /// cell (`None` = monitor-only) so [`Call::snapshot`] reports
    /// `tx_capture_overruns` for the current mic (iax-9e55). Mirrors
    /// [`Call::set_routed_mic`]; set on `route`, cleared on unroute / re-route.
    pub(crate) fn set_capture_overruns(&self, cell: Option<Arc<AtomicU64>>) {
        *self
            .capture_overruns
            .lock()
            .expect("capture_overruns mutex") = cell;
    }

    /// Manager-side setter: record the output bus id.
    pub(crate) fn set_output(&self, out: String) {
        *self.output.lock().expect("output mutex") = out;
    }

    /// Manager-side setter: stamp the assigned pool identity onto an adopted
    /// inbound leg. Inbound legs (iax-8baf) arrive carrying the placeholder
    /// `CallId(0)`; `Manager::adopt` mints the real key and stamps it here so
    /// `call_id()`/`snapshot().id` stay consistent with the pool.
    pub(crate) fn set_call_id(&mut self, id: CallId) {
        self.id = id;
    }

    /// Secret-free snapshot of this call's routing-visible state. THE single
    /// read surface for iax-ad2e (roster) and iax-1075 (FFI).
    #[must_use]
    pub fn snapshot(&self) -> CallSnapshot {
        let state = match self.state.load(Ordering::Relaxed) {
            STATE_ACTIVE => CallSnapshotState::Active,
            STATE_HUNGUP => CallSnapshotState::Hungup,
            _ => CallSnapshotState::Connecting,
        };
        let tx_capture_overruns = self
            .capture_overruns
            .lock()
            .expect("capture_overruns mutex")
            .as_ref()
            .map_or(0, |c| c.load(Ordering::Relaxed));
        CallSnapshot {
            id: self.id,
            node: self.node.clone(),
            mode: self.mode,
            keyed: self.ptt.engaged(),
            state,
            routed_mic: self.routed_mic.lock().expect("routed_mic mutex").clone(),
            output: self.output.lock().expect("output mutex").clone(),
            tx_reanchors: self.tx_reanchors.load(Ordering::Relaxed),
            tx_capture_overruns,
            negotiated_format: VoiceFormat::from_u32(self.format_bits.load(Ordering::Relaxed)),
            remote_keyed: self.remote_keyed.load(Ordering::Relaxed),
        }
    }

    /// Hand an `AppCommand` to the runtime thread and wake it out of `poll()`.
    fn send(&self, cmd: AppCommand) -> Result<(), IaxError> {
        self.cmd_tx
            .send(RuntimeCommand::App(cmd))
            .map_err(|_| IaxError::NoActiveCall)?;
        self.waker.wake().map_err(IaxError::Io)?;
        Ok(())
    }

    /// Engage/release push-to-talk. Engaged = captured audio is encoded and
    /// sent to the peer; released = silence. Also notifies the peer (`SendPtt`).
    ///
    /// # Errors
    /// Returns [`IaxError`] if the runtime thread has exited.
    pub fn set_ptt(&self, engaged: bool) -> Result<(), IaxError> {
        self.ptt.set(engaged);
        self.send(AppCommand::SendPtt(engaged))
    }

    /// Send a DTMF digit to the peer.
    ///
    /// # Errors
    /// Returns [`IaxError`] if the runtime thread has exited.
    pub fn send_dtmf(&self, digit: char) -> Result<(), IaxError> {
        self.send(AppCommand::SendDtmf {
            digit,
            now: std::time::Instant::now(),
        })
    }

    /// Smoothed round-trip estimate from the keepalive PING/PONG (and
    /// LAGRQ/LAGRP) exchange. `None` until the first echo arrives, and
    /// after the runtime thread exits it returns the last published value.
    #[must_use]
    pub fn rtt(&self) -> Option<Duration> {
        let v = self.rtt_micros.load(Ordering::Relaxed);
        (v != u32::MAX).then(|| Duration::from_micros(u64::from(v)))
    }

    /// Hang up and join the runtime thread.
    ///
    /// # Errors
    /// Returns [`IaxError::Io`] if the runtime thread panicked while joining.
    pub fn hangup(mut self, cause: Option<String>) -> Result<(), IaxError> {
        let _ = self.send(AppCommand::Hangup {
            cause,
            now: std::time::Instant::now(),
        });
        let _ = self.cmd_tx.send(RuntimeCommand::Shutdown);
        let _ = self.waker.wake();
        if let Some(h) = self.handle.take() {
            h.join()
                .map_err(|_| IaxError::Io(std::io::Error::other("call thread panicked")))?;
        }
        Ok(())
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(RuntimeCommand::Shutdown);
        let _ = self.waker.wake();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod call_handle_tests {
    use super::*;

    #[test]
    fn call_id_is_a_newtype_distinct_from_call_no() {
        // CallId is a process-level routing key; CallNo is the wire number.
        let id = CallId(7);
        assert_eq!(id, CallId(7));
        assert_ne!(CallId(7), CallId(8));
        // No `From<CallNo>` / `From<CallId>` bridge — they are different domains.
    }

    #[cfg(feature = "serde")]
    #[test]
    fn snapshot_is_secret_free_and_serde_friendly() {
        // CallSnapshot exposes node#, mode, keyed, state — no secret.
        let snap = CallSnapshot {
            id: CallId(1),
            node: "55553".into(),
            mode: CallSnapshotMode::Direct,
            keyed: false,
            state: CallSnapshotState::Connecting,
            routed_mic: None,
            output: "out:s".into(),
            tx_reanchors: 0,
            tx_capture_overruns: 0,
            negotiated_format: None,
            remote_keyed: false,
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.to_lowercase().contains("secret"));
        assert!(!json.contains("allstar"));
        // is_active / is_keyed / routed_mic are the accessors ad2e/ffi read.
        assert!(!snap.is_keyed());
        assert!(!snap.is_active());
        assert!(snap.routed_mic.is_none());
        // TX health counters start at zero and are plain u64s (no secret).
        assert_eq!(snap.tx_reanchors, 0);
        assert_eq!(snap.tx_capture_overruns, 0);
        // negotiated_format is None while connecting (iax-31f7).
        assert_eq!(snap.negotiated_format, None);
    }
}
