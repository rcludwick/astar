// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! `EchoLink`: logging in to the directory ([`EchoLinkPresence`]) and
//! calling a station ([`EchoLinkSession`]).
//!
//! Every socket this module opens goes through [`net`], which in a test build
//! refuses any address that is not loopback. That is what makes "no test ever
//! contacts `EchoLink`" a property of the code rather than a promise.

use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use astar_echolink::{AUDIO_PORT, CONTROL_PORT};

mod client;
mod link;
pub(crate) mod net;
mod node;
mod presence;

// `astar-station` has no normal dependency on `astar-echolink`, so the types
// its `EchoLink` surface hands out are re-exported from here.
pub use astar_echolink::directory::{StationEntry, StationKind, StationStatus};
pub use astar_echolink::{PeerAddr, Timings};
pub use client::{CallLink, ChatLine, EchoLinkCallState, EchoLinkSession};
pub use link::{TX_CAP_NOTICE, member_cap_notice};
pub use node::{EchoLinkNodeEdge, EchoLinkNodeLeg, EchoLinkPeerStatus};
pub use presence::EchoLinkPresence;

/// True in a build with the `echolink-loopback-only` feature, which every test
/// build of this crate turns on: every `EchoLink` bind, connect, send and
/// server lookup then refuses an address that is not loopback.
pub const LOOPBACK_ONLY: bool = cfg!(feature = "echolink-loopback-only");

/// Who logs in to the directory, and where.
///
/// There is no password here on purpose: it is handed to
/// [`EchoLinkPresence::start`] on its own and lives only inside the presence
/// thread, so this struct can be printed and copied freely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoLinkLogin {
    /// The station's callsign, as registered with `EchoLink`.
    pub callsign: String,
    /// Directory servers, tried in turn: `IP`, `IP:port`, `name` or
    /// `name:port` (port 5200 when left out).
    pub servers: Vec<String>,
    /// The location text other stations see beside the callsign.
    pub location: String,
}

/// Where the `EchoLink` link binds and how it finds a station's ports.
///
/// Production binds the standard ports and calls every station on them;
/// tests swap in loopback binds and a lookup that points at a fake.
#[derive(Clone)]
pub struct EchoLinkNetwork {
    pub audio_bind: SocketAddr,
    pub control_bind: SocketAddr,
    /// The two UDP endpoints to call for a listed station.
    pub peer_addr: Arc<dyn Fn(&StationEntry) -> PeerAddr + Send + Sync>,
    /// Match an incoming datagram to a station by IP alone (production, where
    /// a NAT may rewrite the source port) rather than by full address.
    pub match_by_ip: bool,
    pub timings: Timings,
}

impl EchoLinkNetwork {
    /// The real thing: ports 5198 and 5199 on every interface, stations
    /// called on their listed IP at the standard ports, matched by IP.
    #[must_use]
    pub fn production() -> Self {
        Self {
            audio_bind: SocketAddr::from((Ipv4Addr::UNSPECIFIED, AUDIO_PORT)),
            control_bind: SocketAddr::from((Ipv4Addr::UNSPECIFIED, CONTROL_PORT)),
            peer_addr: Arc::new(|entry: &StationEntry| PeerAddr::fixed(entry.ip)),
            match_by_ip: true,
            timings: Timings::default(),
        }
    }
}

impl fmt::Debug for EchoLinkNetwork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EchoLinkNetwork")
            .field("audio_bind", &self.audio_bind)
            .field("control_bind", &self.control_bind)
            .field("match_by_ip", &self.match_by_ip)
            .field("timings", &self.timings)
            .finish_non_exhaustive()
    }
}

/// Where the directory login stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginState {
    LoggedOut,
    LoggingIn,
    LoggedIn,
    /// The directory said no, or no server answered; `error` says which.
    Failed,
}

/// What the presence thread reports. It never holds the password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceSnapshot {
    pub state: LoginState,
    pub callsign: String,
    /// A plain sentence for a person to read, set when `state` is `Failed`.
    pub error: Option<String>,
    /// `true` while the login is backing off after a transport failure (no
    /// server answered, or a reply could not be read) and will try again by
    /// itself. `false` when the directory refused the login (the thread has
    /// halted and only a new login helps), and while logged in or out. This
    /// is what tells a client whether "Log in again" would do anything.
    pub retrying: bool,
    /// How long ago the station list in hand arrived; `None` before the first.
    pub stations_age: Option<Duration>,
}

/// Everything the console knows about `EchoLink` at one moment: where the
/// directory login stands and, while a call is up, the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoLinkState {
    pub login: PresenceSnapshot,
    /// `None` when no station has been called.
    pub call: Option<EchoLinkCallState>,
}
