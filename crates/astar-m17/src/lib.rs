// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! M17 digital-voice protocol primitives, plus a loopback reflector.
//!
//! `address`/`control`/`crc`/`frame`/`fsm` have no I/O and no dependencies
//! beyond `std`: callsign addressing, CRC-16/M17, IP-network framing (LSF +
//! stream packets), reflector control packets, and a client session state
//! machine. `reflector` is the one module in this crate that DOES own I/O
//! (a `UdpSocket` and its run-loop thread) and pulls in `rand` (`StreamID`
//! generation) — an `mrefd`-compatible loopback reflector server, with an
//! optional "parrot" echo mode (iax-91f4), needed by `astar-console`'s
//! dev-tests (and later a standalone daemon) to exercise a real reflector
//! rather than a scripted fake.

pub mod address;
pub mod control;
pub mod crc;
pub mod frame;
pub mod fsm;
pub mod reflector;

pub use address::{BROADCAST, decode_callsign, encode_callsign};
pub use control::ControlPacket;
pub use crc::crc16_m17;
pub use frame::{Lsf, StreamPacket};
pub use fsm::{FsmAction, LinkState, SessionFsm};
pub use reflector::{Reflector, ReflectorHandle};
