// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A Discord voice channel as a receive-only member of an astar conference.
//!
//! The channel hears the net; the net never hears the channel. That is not a
//! configuration choice — [`NetLink::send`] exists to complete the trait and
//! is never called, and the conference member this crate enrolls is
//! registered non-contributing by `Manager::add_bridge_member`.
//!
//! **songbird's `receive` feature is enabled, and the guarantee above does
//! not move.** It is on for packet TIMING and nothing else: Discord has no
//! "stopped speaking" event, so the only way to learn that the mic holder
//! has gone quiet — and release the mic after `mic_silence_secs` — is that
//! their packets stopped arriving. The driver is configured
//! `DecodeMode::Pass` (`gateway::voice_config`, the one place a
//! `songbird::Config` is built in this crate), so **packets are never
//! decrypted and never decoded**. What the node learns is that an SSRC sent
//! a packet in a 20 ms tick; no Discord audio ever exists inside the
//! process, so there is none to relay even by mistake.
//!
//! **And it fails safe.** Receive is the reverse-engineered half of Discord
//! voice and may stop working without notice. Nothing depends on it holding
//! the mic closed: [`Commander`]'s hard time-out timer is a separate clock
//! that releases the mic whether or not a single talk edge ever arrives.
//!
//! This is the only crate in the workspace that knows about tokio, and only
//! under the `discord` feature. Everything it exposes speaks
//! `std::sync::mpsc`.
//!
//! Under that feature the connection and the voice session are separate
//! lifetimes: `Gateway` is the bot being online — one runtime thread, one
//! serenity client, reconnecting on its own — and `SongbirdLink`, taken
//! from `Gateway::voice`, is one voice session over it. Handles come and
//! go; the gateway stays until it is dropped.
//!
//! A gateway given `GatewayOptions` also carries commands, and the split
//! there is the same one: `gateway` translates and decides nothing,
//! [`Commander`] decides everything and touches no network. Between them
//! run plain values — [`BotCommand`], [`VoiceEvent`], [`GatewayEvent`] —
//! so every rule this node enforces is tested under default features with
//! no Discord in the build at all.

mod approvals;
mod bridge;
mod commander;
mod commands;
#[cfg(feature = "discord")]
mod gateway;
mod interactions;
mod net_link;
mod panel;
mod pipeline;
mod post_filter;
#[cfg(feature = "discord")]
mod songbird_link;
// Pure integer bookkeeping with no songbird type anywhere in it, so it
// compiles and its tests run with no Discord in the build — the same
// discipline as `commander`. It is gated only so that a default build does
// not carry a module nothing calls.
#[cfg(any(feature = "discord", test))]
mod talk;
mod voice_link;

pub use approvals::{ApprovalState, Approvals, ApprovalsError, UserEntry};
pub use bridge::{Bridge, BridgeStatus};
pub use commander::{
    AdminView, Commander, GatewayOps, LinkInfo, LinkRef, Network, NodeOps, PresentUser,
    RecordingOps, TestNode, link_labels,
};
pub use commands::{
    AdminAction, BotCommand, BotCommandKind, DialTarget, GatewayEvent, UserRef, VoiceEvent,
    validate_allstar_node, validate_module, validate_reflector,
};
#[cfg(feature = "discord")]
pub use gateway::{Gateway, GatewayHandle, GatewayOptions};
pub use net_link::{ConferenceLink, NetLink, TestLink, TestProbe};
pub use pipeline::FramePipeline;
pub use post_filter::{Bypass, PostFilter};
#[cfg(feature = "discord")]
pub use songbird_link::SongbirdLink;
pub use voice_link::{RecordingLink, VoiceLink};

/// Everything that can go wrong in the bridge. Carries no credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscordError {
    /// No bot token was supplied. Constructed by `Gateway::connect` when
    /// handed an empty token.
    NotConfigured,
    /// The conference the bridge would attach to no longer exists.
    NoConference,
    /// The voice link failed. The string is a human-readable reason and
    /// never contains the token.
    Voice(String),
}

impl std::fmt::Display for DiscordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => write!(f, "discord: no bot token supplied"),
            Self::NoConference => write!(
                f,
                "discord: no conference to join — the node must be started and in \
                 bridge or conference mode"
            ),
            Self::Voice(m) => write!(f, "discord: voice link failed: {m}"),
        }
    }
}

impl std::error::Error for DiscordError {}
