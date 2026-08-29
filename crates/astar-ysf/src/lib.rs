// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! System Fusion (C4FM) protocol primitives, plus a loopback reflector —
//! the engine half of `iax-e8a4`, designed in `docs/design/ysf-network.md`.
//!
//! # What is here, and what is not
//!
//! `crc`/`golay`/`conv`/`fich`/`frame`/`wire`/`fsm` have no I/O and no
//! dependencies beyond `std`: the CRC and the two forward-error-correcting
//! codes that protect a FICH, the FICH itself, the 120-byte radio frame it
//! heads, the `YSFReflector` datagrams that carry that frame over UDP, and a
//! client-side link state machine. `reflector` is the one module that owns
//! I/O — a UDP socket and its run-loop thread — mirroring
//! `astar_dstar::reflector`'s shape.
//!
//! **The ninety payload bytes are carried, not decoded.** Voice on YSF is
//! AMBE+2, which on astar means the AMBE-3000 in a `ThumbDV` and nothing
//! else; until the vocoder path exists, [`frame::Frame::payload`] hands the
//! bytes over intact and this crate makes no claim about what is in them.
//! That is deliberate. A payload parser with no vocoder behind it would be
//! untestable against anything real, and the honest state — frames arrive,
//! their FICH is readable, the audio is not yet — is a better place to
//! stand than a half-decoder that nothing calls.
//!
//! # DN and VW
//!
//! [`fich::DataType`] distinguishes the two half-rate voice-and-data modes
//! (DN, what nearly all traffic is) from full-rate voice (VW) and from
//! plain data. [`fich::DataType::is_half_rate_voice`] is the gate: when the
//! vocoder lands, a VW stream must be *reported*, not silently turned into
//! noise. Silence with a reason beats garbage.
//!
//! # On the protocol description
//!
//! The wire details here — packet tags and lengths, the FICH field layout,
//! the Golay and convolutional coding, the interleave, the poll cadence —
//! were established by reading the deployed reference implementations
//! (G4KLX's `YSFClients` and `MMDVMHost`) as a *specification of the wire*,
//! and then verified: the CRC and Golay implementations in this crate were
//! checked against those projects' tables before their spot-check vectors
//! were written down as tests. No code was copied. Those projects are
//! GPL-2.0, this one is AGPL-3.0-only, and the two do not mix — which is
//! the practical reason as well as the honest one to write the algorithms
//! out from their mathematical definitions instead of transcribing tables.

pub mod conv;
pub mod crc;
pub mod fich;
pub mod frame;
pub mod fsm;
pub mod golay;
pub mod reflector;
pub mod wire;

pub use fich::{DataType, FICH_LEN, Fich, FichError, FrameInfo};
pub use frame::{FRAME_LEN, Frame, PAYLOAD_LEN, SYNC};
pub use fsm::{FsmAction, LINK_TIMEOUT, LinkState, POLL_INTERVAL, YsfFsm};
pub use reflector::{Reflector, ReflectorHandle};
pub use wire::{CALLSIGN_LEN, Callsign, CallsignError, DATA_LEN, DataPacket, Packet};
