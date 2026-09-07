// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! NXDN protocol primitives, plus a loopback reflector — the engine half of
//! astar's fourth digital network, designed in `docs/design/nxdn-network.md`
//! and specified byte by byte in `docs/design/nxdn-wire.md`.
//!
//! # What is here, and what is not
//!
//! `wire` and `fsm` have no I/O and no dependencies beyond `std`: the three
//! `NXDNReflector` datagrams that carry NXDN over UDP, and a client-side
//! link state machine over them. `reflector` is the one module that owns I/O
//! — a UDP socket and its run-loop thread — mirroring
//! `astar_ysf::reflector`'s shape exactly.
//!
//! **The two 14-byte blocks of a frame are carried, not decoded.** Voice on
//! NXDN is AMBE+2, which on astar means the AMBE-3000 in a `ThumbDV` and
//! nothing else; [`wire::DataPacket::frame`] hands its 33 bytes over intact
//! and this crate makes no claim about what is in them. The vocoder layer
//! lives in `astar-codec`, which already speaks this frame's dialect: NXDN's
//! 49-bit AMBE+2 frame and YSF DN's are the same object, same RATEP word and
//! same channel packet, so `VocoderMode::YsfDn` is reused rather than
//! duplicated — see the ruling in `docs/design/nxdn-wire.md`.
//!
//! That split is deliberate. A payload parser sitting in the protocol crate
//! with no vocoder behind it would be untestable against anything real, and
//! the honest state — frames arrive, their routing is readable, the audio is
//! decoded a layer up — is a better place to stand than a half-decoder that
//! nothing calls.
//!
//! # On the protocol description
//!
//! The wire details here — datagram tags and lengths, the field layout of a
//! poll and of a data packet, the flag bits, the poll cadence, the
//! reflector's registration and relay rules — were established by reading
//! the deployed reference implementations (G4KLX's `NXDNClients` and
//! `MMDVMHost`, the `NXDNReflector` that used to live alongside them, and
//! `DroidStar`) as a *specification of the wire*, and each constant in this
//! crate cites the file and function it was read out of. No code was copied.
//! Those projects are GPL-2.0/GPL-3.0, this one is AGPL-3.0-only, and the
//! two do not mix — which is the practical reason as well as the honest one
//! to write the algorithms out from their definitions instead of
//! transcribing them.

pub mod frame;
pub mod fsm;
pub mod reflector;
pub mod wire;

pub use frame::{BLOCK_LEN, BLOCKS, FrameKind, Lich, NetFrame, SACCH_LEN};
pub use fsm::{FsmAction, INITIAL_POLLS, LINK_TIMEOUT, LinkState, NxdnFsm, POLL_INTERVAL};
pub use reflector::{Reflector, ReflectorHandle};
pub use wire::{
    CALLSIGN_LEN, Callsign, CallsignError, DATA_LEN, DataPacket, FRAME_LEN, POLL_LEN, Packet,
};
