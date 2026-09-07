// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
/// Errors surfaced by [`crate::Station`]. Maps 1:1 to the future C-ABI `IaxErr`
/// codes. Credentials never appear in these messages.
#[derive(Debug)]
pub enum StationError {
    /// `set_ptt`/`disconnect`-style call with no active call.
    NotConnected,
    /// `connect`/`connect_wt` called while a call is already live.
    AlreadyConnected,
    /// Portal/token-mint problem (no portal config, login failure, etc.).
    Portal(astar_asl3::Asl3Error),
    /// Node resolution (DNS) failed.
    Resolve(astar_asl3::Asl3Error),
    /// Audio enumeration / device error.
    Audio(String),
    /// Underlying IAX client error.
    Iax(String),
    /// Serial-PTT wiring error.
    Serial(String),
    /// Operation not supported in the current mode (e.g. `set_mode(Node)` before Node support lands).
    Unsupported,
    /// Inbound listener failed to bind (port in use, permission denied, etc.).
    Listen(String),
    /// Node is at its maximum call capacity; the inbound offer was rejected.
    AtCapacity,
    /// `answer`/`reject` called with no pending inbound offer.
    NoPendingCall,
    /// `send_dtmf` called with a character that is not a valid DTMF key
    /// (`0-9`, `*`, `#`, `A-D`).
    InvalidDigit,
    /// Link-layer failure (iax-1075) — secret-free, human-readable.
    Link(String),
    /// `send_dtmf_string` called while a previous sequence is still playing
    /// (iax-4b7a). Cancel it or wait for it to finish.
    DtmfBusy,
    /// M17 error (iax-f2b8 Task 4) — secret-free, human-readable. Also
    /// returned by `m17_connect`/`m17_disconnect` when the `m17` feature
    /// isn't compiled in.
    M17(String),
    /// D-Star error (iax-a9d4 Task 6 built RX; iax-2f6b added TX) —
    /// secret-free, human-readable. Returned by `dstar_connect`/
    /// `dstar_disconnect` when the `dstar` feature isn't compiled in, and
    /// for vocoder-availability failures during connect. `set_ptt` no
    /// longer refuses D-Star: a live D-Star session is full-transceive and
    /// keys/unkeys exactly like an M17/IAX2 call.
    Dstar(String),
    /// A YSF error (iax-e8a4) — secret-free, human-readable. Returned when
    /// the `ysf` feature isn't compiled in, or by the link layer classifying
    /// a callsign, resolve or bind failure. There is no YSF audio yet: the
    /// link carries frames and reports who is talking, and the vocoder is
    /// the missing half.
    Ysf(String),
    /// An NXDN error (iax-b9c2) — secret-free, human-readable. Returned when
    /// the `nxdn` feature isn't compiled in, when a callsign/radio id/
    /// talkgroup is not one the wire can carry, and for every
    /// vocoder-availability failure during connect. Receive only for now:
    /// `Station::set_ptt` refuses a key-down while an NXDN link is live (see
    /// `astar_console::nxdn`'s Transmit section).
    Nxdn(String),
}

impl std::fmt::Display for StationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Portal`/`Resolve` render generic text (no `{source}`) so a portal
        // password embedded in an underlying error string can never leak. The
        // underlying `Asl3Error` is retained in the variant for programmatic
        // matching only.
        match self {
            Self::NotConnected => write!(f, "no active call"),
            Self::AlreadyConnected => write!(f, "a call is already in progress"),
            Self::Portal(_) => write!(f, "portal/token error"),
            Self::Resolve(_) => write!(f, "node resolution failed"),
            Self::Audio(m) => write!(f, "audio error: {m}"),
            Self::Iax(m) => write!(f, "iax error: {m}"),
            Self::Link(m) => write!(f, "link error: {m}"),
            Self::Serial(m) => write!(f, "serial error: {m}"),
            Self::Unsupported => write!(f, "operation not supported in this mode"),
            Self::Listen(m) => write!(f, "listener bind failed: {m}"),
            Self::AtCapacity => write!(f, "node is at maximum call capacity"),
            Self::NoPendingCall => write!(f, "no pending inbound call to answer or reject"),
            Self::InvalidDigit => write!(f, "not a valid DTMF digit (0-9, *, #, A-D)"),
            Self::DtmfBusy => write!(f, "a DTMF sequence is already playing"),
            Self::M17(msg) => write!(f, "m17 error: {msg}"),
            Self::Dstar(msg) => write!(f, "dstar error: {msg}"),
            Self::Ysf(msg) => write!(f, "ysf error: {msg}"),
            Self::Nxdn(msg) => write!(f, "nxdn error: {msg}"),
        }
    }
}

impl std::error::Error for StationError {}
