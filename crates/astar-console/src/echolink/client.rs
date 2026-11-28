// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The app's one `EchoLink` call.
//!
//! [`EchoLinkSession`] calls one station over an [`EchoLinkLink`] of its own
//! and plugs it into the voice route: the route's capture frames go out while
//! PTT is applied, and every received frame goes to the route's speaker. The
//! session itself only requests PTT; the link thread applies it, and
//! [`EchoLinkCallState::ptt`] reports what it actually applied.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, MutexGuard, PoisonError};

use astar_audio::CallAudio;
use astar_codec::gsm::FRAME_SAMPLES;
use astar_echolink::PeerAddr;
use astar_echolink::directory::StationEntry;
use astar_echolink::qso::QsoState;
use astar_iax::{CutReason, IaxError, PictureEnd, PicturePacing, PictureTx};
use astar_record::packet_tap::{PacketCallId, PacketSlot};
use tracing::warn;

use super::EchoLinkNetwork;
use super::link::{EchoLinkLink, PeerAudio, PeerShared, PttRequest};
use crate::{ConsoleError, PictureTarget};

/// The rate an `EchoLink` call's audio runs at, and so the rate a picture's
/// audio must be at: GSM 06.10 is 8 kHz.
const PICTURE_RATE: u32 = 8_000;

pub(super) const BIND_FAILED: &str =
    "Couldn't open UDP ports 5198–5199 — is another EchoLink program running?";
const NO_CALLSIGN: &str = "Set your callsign before calling an EchoLink station";
const NO_THREAD: &str = "Couldn't start the EchoLink link";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where a call stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallLink {
    /// SDES sent, no answer yet.
    Connecting,
    Connected,
    /// Hung up, refused, or timed out. The session stays readable.
    Closed,
}

/// One line of the call's text chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatLine {
    pub from: String,
    pub text: String,
    /// True for a line this station sent.
    pub mine: bool,
}

/// A snapshot of the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoLinkCallState {
    pub link: CallLink,
    /// The station called.
    pub peer: String,
    pub node: u32,
    /// PTT as applied by the link, not as requested.
    pub ptt: bool,
    /// The far end is talking.
    pub receiving: bool,
    /// The far station's information text, once it has sent one.
    pub info: Option<String>,
    pub chat: Vec<ChatLine>,
    /// Why the link stopped transmitting on its own (the transmit cap), a
    /// plain sentence; cleared by the next over.
    pub notice: Option<String>,
}

/// The app's call to one `EchoLink` station.
pub struct EchoLinkSession {
    link: EchoLinkLink,
    shared: Arc<PeerShared>,
    ptt: Arc<PttRequest>,
    entry: StationEntry,
    packet_slot: PacketSlot,
    /// The last picture handed to the link, so a second one is refused
    /// while it runs.
    picture: Option<PictureTx>,
}

impl EchoLinkSession {
    /// Call `station` as `my_call`, sending `location` as our information
    /// text once it answers. Returns as soon as the first SDES is out; the
    /// answer arrives later and shows in [`EchoLinkSession::state`].
    pub fn connect(
        station: StationEntry,
        my_call: &str,
        location: &str,
        net: &EchoLinkNetwork,
        audio: CallAudio,
    ) -> Result<EchoLinkSession, ConsoleError> {
        let my_call = my_call.trim().to_uppercase();
        if my_call.is_empty() {
            return Err(ConsoleError::EchoLink(NO_CALLSIGN.to_owned()));
        }
        let packet_slot = PacketSlot::default();
        let link =
            EchoLinkLink::start(&my_call, location, net, packet_slot.clone()).map_err(|e| {
                warn!(error = %e, "echolink: link thread did not start");
                ConsoleError::EchoLink(NO_THREAD.to_owned())
            })?;
        let ptt = Arc::new(PttRequest::default());
        let shared = link
            .add(
                station.clone(),
                PeerAudio::Client {
                    call_audio: audio,
                    ptt: Arc::clone(&ptt),
                },
            )
            .map_err(|e| {
                warn!(error = %e, "echolink: could not open the UDP ports");
                ConsoleError::EchoLink(BIND_FAILED.to_owned())
            })?;
        Ok(EchoLinkSession {
            link,
            shared,
            ptt,
            entry: station,
            packet_slot,
            picture: None,
        })
    }

    /// Request PTT on or off; the link applies it on its next pass, and only
    /// while connected.
    pub fn set_ptt(&mut self, on: bool) {
        self.ptt.set(on);
    }

    /// The call's transmit side, for a picture sender: the rate its audio
    /// must be at (8 kHz) and whether PTT is requested right now. `None`
    /// unless the call is connected.
    #[must_use]
    pub fn picture_target(&self) -> Option<PictureTarget> {
        (*lock(&self.shared.state) == QsoState::Connected).then(|| PictureTarget {
            rate: PICTURE_RATE,
            keyed: self.ptt.is_on(),
        })
    }

    /// Send a whole picture's audio, at 8 kHz, in place of the microphone.
    /// The same contract as the `AllStarLink` call's
    /// `Manager::send_picture_audio`: the caller has already keyed (if PTT
    /// is not requested, the handle has already ended with
    /// [`CutReason::Unkeyed`]); this neither keys nor unkeys; if the key
    /// comes up or the call ends mid-picture, the picture is cut and the
    /// handle says why. The link paces it with the same pacer an
    /// `AllStarLink` call uses (see the link module's "Sending a picture").
    ///
    /// # Errors
    /// - [`ConsoleError::NotConnected`] unless the call is connected.
    /// - [`ConsoleError::Iax`] carrying [`IaxError::PictureBusy`] while an
    ///   earlier picture is still running, or [`IaxError::Io`] if the pacer
    ///   thread could not be spawned. These are the `AllStarLink` path's own
    ///   errors, so a caller handles one set for both networks.
    pub fn send_picture_audio(&mut self, pcm: Arc<[i16]>) -> Result<PictureTx, ConsoleError> {
        if *lock(&self.shared.state) != QsoState::Connected {
            return Err(ConsoleError::NotConnected);
        }
        if self.picture.as_ref().is_some_and(|p| p.ended().is_none()) {
            return Err(ConsoleError::Iax(IaxError::PictureBusy));
        }
        // One GSM frame, 20 ms at 8 kHz: the unit the pacer and the packets
        // count in.
        let frame_len = FRAME_SAMPLES;
        let total = pcm.len().div_ceil(frame_len);
        let tx = if total == 0 {
            PictureTx::already_ended(0, PictureEnd::Sent)
        } else if !self.ptt.is_on() {
            PictureTx::already_ended(total, PictureEnd::Cut(CutReason::Unkeyed))
        } else {
            // The link polls on a short timeout while a picture runs, so the
            // pacer has nothing to wake.
            let (tx, pacing) = PicturePacing::start(pcm, frame_len, || {})
                .map_err(|e| ConsoleError::Iax(IaxError::Io(e)))?;
            self.link.send_picture(&self.entry.call, pacing);
            tx
        };
        self.picture = Some(tx.clone());
        Ok(tx)
    }

    /// Send a chat line. Dropped unless the call is connected.
    pub fn send_chat(&self, text: &str) {
        self.link.chat(text);
    }

    #[must_use]
    pub fn state(&self) -> EchoLinkCallState {
        let link = match *lock(&self.shared.state) {
            QsoState::Connecting => CallLink::Connecting,
            QsoState::Connected => CallLink::Connected,
            QsoState::Closed(_) => CallLink::Closed,
        };
        EchoLinkCallState {
            link,
            peer: self.entry.call.clone(),
            node: self.entry.node,
            ptt: self.shared.ptt.load(Ordering::Relaxed),
            receiving: self.shared.talking.load(Ordering::Relaxed),
            info: lock(&self.shared.info).clone(),
            chat: lock(&self.shared.chat).clone(),
            notice: lock(&self.shared.notice).clone(),
        }
    }

    /// The slot the call's packet tap offers to. Install a tap to log it.
    #[must_use]
    pub fn packet_slot(&self) -> PacketSlot {
        self.packet_slot.clone()
    }

    /// The call's id in the packet log.
    #[must_use]
    pub fn packet_call(&self) -> PacketCallId {
        self.shared.packet_call
    }

    /// The link's bound audio and control addresses, or `None` once the call
    /// has ended and the ports are closed. For tests, which aim an uninvited
    /// fake at the real ephemeral ports.
    #[doc(hidden)]
    #[must_use]
    pub fn local_addrs(&self) -> Option<PeerAddr> {
        self.link.local_addrs()
    }

    /// Hang up (a BYE, unless the call already ended) and stop the link.
    pub fn disconnect(self) {
        drop(self);
    }
}
