// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Unified error type for the high-level astar-iax call API (iax-612e).

use astar_audio::AudioError;
use astar_iax_core::session::fsm::FailReason;

/// Every fallible operation on [`crate::Manager`] / [`crate::Call`] returns this.
#[derive(Debug)]
#[non_exhaustive]
pub enum IaxError {
    /// Socket / OS error setting up or running the call runtime.
    Io(std::io::Error),
    /// Audio device open/stream error from the injected backend.
    Audio(AudioError),
    /// `dial()` called while a call is already live (v1 is single-call).
    CallInProgress,
    /// A `Call` method was used after the call ended / before it began.
    NoActiveCall,
    /// The call failed to connect or was rejected/hung up by the peer.
    Dial(FailReason),
    /// The builder was missing a required field (e.g. peer address).
    MissingConfig(&'static str),
    /// No audio device name contains the requested substring (iax-fd34).
    DeviceNotFound {
        wanted: String,
        available: Vec<String>,
    },
    /// The requested substring matches more than one device (iax-fd34).
    DeviceAmbiguous {
        wanted: String,
        matches: Vec<String>,
    },
    /// `key()` was called on a monitor-only call (no mic routed); it has no
    /// transmit source (iax-42e9).
    NotRouted,
    /// A Link was keyed but its mode (or lack of a routed mic) makes it
    /// non-transmit-capable: a Monitor/LocalMonitor link, or a Transceive link
    /// with no mic routed. Consistent with [`IaxError::NotRouted`] (iax-ad2e).
    NotTransmitCapable,
    /// An announcement could not be produced (resolution failed) or has no TX
    /// path (a to-air request on a monitor-only call).
    AnnounceUnavailable,
}

impl std::fmt::Display for IaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Audio(e) => write!(f, "audio error: {e}"),
            Self::CallInProgress => write!(f, "a call is already in progress"),
            Self::NoActiveCall => write!(f, "no active call"),
            Self::Dial(r) => write!(f, "dial failed: {r}"),
            Self::MissingConfig(what) => write!(f, "missing required config: {what}"),
            Self::DeviceNotFound { wanted, available } => write!(
                f,
                "no audio device matching \"{wanted}\"; available: {}",
                available.join(", ")
            ),
            Self::DeviceAmbiguous { wanted, matches } => write!(
                f,
                "audio device name \"{wanted}\" is ambiguous; matches: {}",
                matches.join(", ")
            ),
            Self::NotRouted => write!(f, "call has no mic routed; cannot transmit"),
            Self::NotTransmitCapable => {
                write!(f, "link is not transmit-capable in its current mode")
            }
            Self::AnnounceUnavailable => {
                write!(
                    f,
                    "announcement unavailable: resolution failed or no TX path"
                )
            }
        }
    }
}

impl std::error::Error for IaxError {}

impl From<std::io::Error> for IaxError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<AudioError> for IaxError {
    fn from(e: AudioError) -> Self {
        Self::Audio(e)
    }
}
