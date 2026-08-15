// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! D-Star digital-voice protocol primitives, plus a loopback reflector.
//! RX (decode) has been the whole story until now; `tx` adds TX-side
//! stream framing (research §3, §6) — encode/PTT/session plumbing to
//! actually drive it still live elsewhere (see that module's docs for what
//! it does and does not decide).
//!
//! `header`/`dsvt`/`slowdata`/`fsm`/`tx` have no I/O and no dependencies
//! beyond `std`: the 41-byte RF header (callsigns + CRC-CCITT), the "DSVT"
//! network framing used by `DExtra`/`DPlus` to carry it plus 12-byte
//! voice/data frames, the slow-data channel carried in each voice frame's
//! trailing 3 bytes (RX reassembly and TX sync/filler), a `DExtra` client
//! link state machine, and TX stream sequencing. `reflector` is the one
//! module in this crate that DOES own I/O (a `UdpSocket` and its run-loop
//! thread) — a `DExtra`-compatible loopback reflector server, with an
//! optional "parrot" echo mode, mirroring `astar_m17::reflector`'s
//! shape (see that module's doc for the fuller rationale write-up).

pub mod dsvt;
pub mod fsm;
pub mod header;
pub mod reflector;
pub mod slowdata;
pub mod tx;

pub use dsvt::{DSVT_MAGIC, DsvtError, DsvtPacket, SYNC_INTERVAL};
pub use fsm::{DextraFsm, FsmAction, FsmError, LinkState};
pub use header::{HeaderError, RfHeader, crc_ccitt};
pub use reflector::{Reflector, ReflectorHandle};
pub use slowdata::{SCRAMBLE, SlowDataRx};
pub use tx::{CQCQCQ, NULL_AMBE, TxStream, generate_stream_id, repeater_fields};
