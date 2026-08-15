// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! High-level IAX2 client API.
//!
//! Drives [`astar_iax_core::session::Fsm`] over a `mio`-based event loop
//! (one blocking thread per call, no async runtime). Builder-style
//! configuration, channel-style event streams.

mod announce;
mod audio_bridge;
mod call;
mod call_mode;
mod codec_edge;
mod error;
pub mod link;
pub mod link_control;
pub mod listener;
pub mod manager;
mod parrot;
mod raw_dial;
pub mod registration;
pub mod routing_config;
mod runtime;
pub mod trace;
mod transport;

pub use announce::AnnouncePolicy as AnnouncePolicyReq;
pub use announce::tts::{TtsConfig, TtsEngine, TtsError};
pub use announce::{AnnounceRequest, Destination, Phrase};
pub use announce::{AnnouncementService, ResolveError, Resolved, ResolverConfig, ServiceConfig};
pub use astar_iax_core::session::CodecPolicy;
pub use astar_iax_core::session::reg::{RegFailReason, RegState, RegisterOptions};
pub use astar_wireguard::{UdpTransport, WgConfigError, WgLinkConfig, WgStackStatus};
pub use call::{Call, CallEvent, CallId, CallSnapshot, CallSnapshotMode, CallSnapshotState};
pub use call_mode::CallMode;
pub use error::IaxError;
pub use link::{Link, LinkError, LinkMode, LinkRoster, LinkSnapshot, LinkState};
pub use link_control::{
    KnownNodes, LinkAdmission, LinkEvent, LinkResolver, LinkSpec, LinkValidator, SecretResolver,
};
pub use listener::{
    IncomingAuthPolicy, IncomingCall, IncomingCallEvent, IncomingCallListener,
    IncomingCallListenerBuilder, IncomingCallPolicy, IncomingCallTokenPolicy,
    IncomingDecisionPolicy,
};
pub use manager::{BridgeConfig, BridgeMode, DialSpec, LinkTransport, Manager, ManagerSnapshot};
pub use parrot::{ParrotConfig, run_parrot};
pub use raw_dial::{RawDial, TxFrames, dial_raw, dial_raw_with_policy};
pub use registration::{Registrar, Registration, RegistrationEvent};
pub use routing_config::{ConnectionId, ConnectionSpec, RoutingConfig};
pub use trace::{Direction, TracedFrame};
pub use transport::{LinkSocket, NetStack, OsNetStack, WgNetStack};
