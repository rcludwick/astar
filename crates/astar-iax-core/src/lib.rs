// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! IAX2 protocol framing — pure wire-format codec, no I/O, no state.
//!
//! See [RFC 5456](https://datatracker.ietf.org/doc/html/rfc5456) and the
//! `AllStarLink` dialect notes in `docs/spec/`.
//!
//! # Layout
//!
//! - [`frame`] — top-level [`Frame`], [`FullFrame`], [`MiniFrame`], plus
//!   the [`parse`] and [`encode`] entry points.
//! - [`ie`] — typed [`Ies`] container and the `IE_*` id constants.
//! - [`subclass`] — typed subclass enums ([`IaxCommand`],
//!   [`ControlSubclass`], [`VoiceFormat`]) and the RFC 5456 §6.2 subclass
//!   compression helpers.
//! - [`error`] — [`ParseError`] and [`EncodeError`].
//! - [`text`] — typed decoder for `AST_FRAME_TEXT` payloads (`AllStar`
//!   K-status and friends).
//!
//! The crate is byte-perfect: `parse(encode(frame)) == frame` for every
//! representable frame. Property tests pin that invariant.

pub mod error;
pub mod frame;
pub mod ie;
pub mod session;
pub mod subclass;
pub mod text;
pub mod trace;
pub mod uri;

pub use error::{EncodeError, ParseError};
pub use frame::{
    Frame, FullFrame, MiniFrame, OwnedFrame, OwnedFullFrame, OwnedMiniFrame, Subclass, encode,
    parse,
};
pub use ie::Ies;
pub use subclass::{ControlSubclass, FrameType, IaxCommand, VoiceFormat};
pub use text::{OwnedTextEvent, TextEvent, TextParseError};
