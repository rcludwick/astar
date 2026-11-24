// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! P25 Phase 1 protocol primitives: the engine half of astar's P25 receive.
//!
//! # What is here, and what is not
//!
//! Four modules and no dependencies beyond `std`. Only [`reflector`] does
//! I/O:
//!
//! - [`wire`]: the `P25Reflector` datagrams. A poll (`F0` and a ten-byte
//!   callsign), an unlink (`F1`), the eighteen voice records, and the
//!   `0x80` end record.
//! - [`ldu`]: the eighteen voice records of a superframe, the table of their
//!   lengths and IMBE offsets, and the three fields worth reading out of
//!   them: talkgroup, source radio ID and algorithm ID.
//! - [`fsm`]: a client-side link state machine over those datagrams, driven
//!   by caller-supplied `Instant`s.
//! - [`reflector`]: a loopback `P25Reflector` with a `P25Parrot` mode, the
//!   bench and test peer. It binds where it is told and never dials out.
//!
//! **The IMBE frames are carried, not decoded.** Each record hands its 11
//! bytes over intact; the vocoder lives in `vendor/imbe-vocoder` and is
//! reached through `astar-codec`.
//!
//! **There is no encryption here, and there never will be.** This crate
//! reads a call's algorithm ID so a layer up can mute an encrypted call. It
//! depends on no cryptography crate and decrypts nothing.
//!
//! # On the protocol description
//!
//! The wire details here (tags, lengths, the record table, the header
//! bytes, the poll cadence and the reflector's peer timeout) were
//! established by reading G4KLX's `MMDVMHost` (`P25Network.cpp`,
//! `P25Defines.h`), `P25Clients` (`P25Gateway/P25Network.cpp`,
//! `P25Gateway/P25Gateway.cpp`, `P25Parrot`) and the `P25Reflector` in
//! `nostar/DVReflectors` (`P25Reflector.cpp`, `P25Reflector.h`) as a
//! *specification of the wire*. Each constant cites the file it was read
//! out of. No code was copied: those projects are GPL-2.0, this one is
//! AGPL-3.0-only, and the algorithms are written out from their definitions
//! instead.

pub mod fsm;
pub mod ldu;
pub mod reflector;
pub mod wire;

pub use fsm::{FsmAction, INITIAL_POLLS, LINK_TIMEOUT, LinkState, P25Fsm, POLL_INTERVAL};
pub use ldu::{
    ALGID_CLEAR, FIRST_TAG, LAST_TAG, RECORDS, Superframe, VoiceRecord, imbe_offset, record_len,
    superframe,
};
pub use reflector::{Reflector, ReflectorHandle};
pub use wire::{
    AddressError, CALLSIGN_LEN, Callsign, CallsignError, DEFAULT_PORT, END_LEN, POLL_LEN, Packet,
    TAG_END, TAG_POLL, TAG_UNLINK, end_record, is_media, parse, poll, unlink, with_default_port,
};
