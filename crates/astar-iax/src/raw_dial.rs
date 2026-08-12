// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Headless raw-frame outbound dial (iax-64b6): dial a peer and expose the
//! call's PCM TX/RX frame channels directly, with NO audio devices. The
//! building block for an echo/parrot test client and for future relay/server
//! work that needs frame-level access. Codec transcode happens at the network
//! edge (iax-31f7). Vendor-neutral; the secret is a call-time argument, never stored.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use astar_audio::CallAudio;
use astar_iax_core::session::CodecPolicy;
use astar_iax_core::session::call_no::CallNo;
use mio::{Poll, Token, Waker};

use crate::audio_bridge::PttGate;
use crate::call::{CallId, CallSnapshotMode};
use crate::error::IaxError;
use crate::runtime::{SpawnParams, spawn_call_runtime};
use crate::{Call, CallEvent, CallMode};

/// A dialed call with its raw µ-law frame channels exposed (no devices).
pub struct RawDial {
    /// The call handle (`snapshot`, `set_ptt`, `hangup`, `rtt`).
    pub call: Call,
    /// Lifecycle/media events.
    pub events: Receiver<CallEvent>,
    /// Send 20 ms PCM (i16) frames here to transmit them on the call.
    pub tx_frames: TxFrames,
    /// Decoded inbound PCM (i16) frames arrive here.
    pub rx_frames: Receiver<Vec<i16>>,
}

/// A sender for outbound PCM frames that wakes the call's run-loop on every
/// send. Essential for a device-free caller: the run-loop blocks in `poll()`
/// until the next keepalive (~seconds), and the `Manager`'s mic lane normally
/// wakes it per captured frame via `bind_mic`. A raw-dial caller has no mic
/// lane, so without this wake its frames sit undrained — and a parrot's
/// playback never reaches a peer that is otherwise quiet. (iax-64b6.)
pub struct TxFrames {
    tx: Sender<Vec<i16>>,
    waker: Arc<Waker>,
}

impl TxFrames {
    /// Queue a 20 ms PCM (i16) frame for transmission and wake the run-loop
    /// so it drains and sends promptly.
    ///
    /// # Errors
    /// [`std::sync::mpsc::SendError`] if the run-loop has gone away.
    pub fn send(&self, frame: Vec<i16>) -> Result<(), std::sync::mpsc::SendError<Vec<i16>>> {
        self.tx.send(frame)?;
        let _ = self.waker.wake();
        Ok(())
    }
}

/// Mirror of `manager::snapshot_mode` for the modes `dial_raw` accepts. The
/// secret-free snapshot mode a [`CallMode`] lowers to (Standard → `Direct`).
fn snapshot_mode(mode: &CallMode) -> CallSnapshotMode {
    match mode {
        CallMode::Standard => CallSnapshotMode::Direct,
        CallMode::WebTransceiver { .. } => CallSnapshotMode::WebTransceiver,
    }
}

/// Dial `peer` with an explicit codec-negotiation policy (iax-e6f1 follow-up:
/// the slin16 parrot). `PreferSlin16` also sizes the PCM bus at 16 kHz so a
/// negotiated slin16 call resamples nothing at the codec edge.
///
/// # Errors
/// [`IaxError`] if the runtime thread/socket cannot start.
// `mode` is taken by value for API symmetry with `DialSpec.mode` (owned); it is
// logically consumed (lowered to a profile + snapshot mode), but `CallMode`'s
// accessors borrow `&self`, so clippy can't see the move.
#[allow(clippy::needless_pass_by_value)]
pub fn dial_raw_with_policy(
    peer: SocketAddr,
    caller_id: impl Into<String>,
    dest: impl Into<String>,
    secret: impl Into<String>,
    mode: CallMode,
    codec_policy: CodecPolicy,
) -> Result<RawDial, IaxError> {
    // Channel wiring: the run-loop drains `tx_frames` (the Receiver) onto the
    // wire, so the caller holds the paired Sender; the run-loop fills
    // `rx_frames` (the Sender) with decoded inbound audio, so the caller holds
    // the paired Receiver.
    let (tx_to_wire, tx_frames) = std::sync::mpsc::channel();
    let (rx_from_wire, rx_frames) = std::sync::mpsc::channel();
    let audio = CallAudio {
        tx_frames,
        rx_frames: rx_from_wire,
        // Raw `Client::dial` path: an injected sink does its own framing (no
        // router MicLane), so nothing drives the VOX pre-roll lead — a parked
        // cell keeps the run-loop's media-clock ladder on the lead=0 path
        // (byte-identical to pre-iax-2733).
        preroll_lead: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
    };

    // Lower the call mode (Standard → default profile, no forced dest).
    let (mut profile, forced_dest) = mode.resolve();
    profile.codec_policy = codec_policy;
    let dest = forced_dest.map_or_else(|| dest.into(), ToString::to_string);
    let snap_mode = snapshot_mode(&mode);

    let poll = Poll::new()?;
    let waker = Arc::new(Waker::new(poll.registry(), Token(1))?);
    // A clone for the TX path so each pushed frame wakes the run-loop.
    let tx_waker = Arc::clone(&waker);
    let gate = PttGate::new();

    let (call, events) = spawn_call_runtime(SpawnParams {
        peer,
        caller_id: caller_id.into(),
        dest,
        secret: secret.into(),
        profile,
        call_no: CallNo::new(1).expect("CallNo::new(1) is valid"),
        poll,
        waker,
        gate,
        audio,
        frame_observer: None,
        id: CallId::from_raw(0),
        node: String::new(),
        mode: snap_mode,
        pooled: false,
        sample_rate: codec_policy.max_sample_rate(),
        // Transport seam (iax-b6f5): plain OS UDP, byte-identical default.
        net: Arc::new(crate::transport::OsNetStack),
    })?;

    Ok(RawDial {
        call,
        events,
        tx_frames: TxFrames {
            tx: tx_to_wire,
            waker: tx_waker,
        },
        rx_frames,
    })
}

/// Dial with the default (µ-law) policy — the pre-existing narrowband path.
pub fn dial_raw(
    peer: SocketAddr,
    caller_id: impl Into<String>,
    dest: impl Into<String>,
    secret: impl Into<String>,
    mode: CallMode,
) -> Result<RawDial, IaxError> {
    dial_raw_with_policy(peer, caller_id, dest, secret, mode, CodecPolicy::default())
}
