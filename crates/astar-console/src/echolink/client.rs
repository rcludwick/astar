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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use astar_audio::CallAudio;
use astar_echolink::PeerAddr;
use astar_echolink::directory::StationEntry;
use astar_echolink::qso::QsoState;
use tracing::warn;

use super::EchoLinkNetwork;
use super::link::{EchoLinkLink, PeerAudio, PeerShared};
use crate::ConsoleError;

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
    ptt: Arc<AtomicBool>,
    entry: StationEntry,
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
        let link = EchoLinkLink::start(&my_call, location, net).map_err(|e| {
            warn!(error = %e, "echolink: link thread did not start");
            ConsoleError::EchoLink(NO_THREAD.to_owned())
        })?;
        let ptt = Arc::new(AtomicBool::new(false));
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
        })
    }

    /// Request PTT on or off; the link applies it on its next pass, and only
    /// while connected.
    pub fn set_ptt(&mut self, on: bool) {
        self.ptt.store(on, Ordering::Release);
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
