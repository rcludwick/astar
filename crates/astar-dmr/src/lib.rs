// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! DMR primitives, designed in `docs/design/dmr-networks.md`.
//!
//! # Where this stands
//!
//! [`network`] answers "which DMR do you mean" — the taxonomy, the
//! independent/BrandMeister split, and the consent gate that split exists
//! for. It came first on purpose: DMR is the one network where the *address*
//! is the hard part, because a talkgroup number names nothing on its own —
//! TG 91 exists on several of these networks and is a different room on each
//! — so what a target even is had to be settled before any wire code was
//! written.
//!
//! [`wire`] and [`fsm`] are that wire code, specified byte by byte in
//! `docs/design/dmr-wire.md`: the homebrew handshake (`RPTL` → salt →
//! `RPTK` → `RPTC`), the `DMRD` data packet, and a client-side link state
//! machine over them. Neither does any I/O. [`master`] is the one module
//! that does — a UDP socket and its run-loop thread — and it is a loopback
//! *master*, the fixture the link is tested against, not a way to reach a
//! network.
//!
//! One dependency beyond `std`, and only one: `sha2`, for the login digest.
//! astar writes wire formats out from their definitions; it does not write
//! its own crypto.
//!
//! **The 33-byte burst is carried, not decoded.** Voice on DMR is AMBE+2,
//! which on astar means the AMBE-3000 in a `ThumbDV` and nothing else;
//! [`wire::DataPacket::burst`] hands its bytes over intact and this crate
//! makes no claim about what is inside them. The sync patterns, the embedded
//! LC and the three 9-byte vocoder frames are `docs/design/dmr-wire.md`
//! §5–§9's business and a later crate's. Transmit, the talkgroup dial
//! grammar and the session layer are not here yet.
//!
//! # The identity this assumes
//!
//! DMR does not put a callsign on the air. It addresses radios by a numeric ID
//! registered at radioid.net against a verified licence, which is a separate
//! credential from the callsign, with its own registration story and its own
//! failure mode when absent or wrong. astar carries both, independently
//! (`CallSession.operatorCallsign` and `CallSession.dmrRadioID`); that question
//! is settled and `nxdn-network.md` and `p25-network.md` inherit the answer.
//!
//! # Secrets
//!
//! Each network wants its own account and its own hotspot password. Those are
//! connect-time in-args and nothing else: never held on a `Station`, never in a
//! snapshot, an event, an error or a log.
//!
//! One type in this crate touches a password at all — [`fsm::DmrFsm`], which
//! takes it by value, spends it on the `RPTK` digest and nothing else, and
//! never returns, formats or logs it. Its `Debug` is hand written for that
//! reason and a test pins that the password cannot reach it. Nothing else
//! here stores a secret, and nothing else should learn how.
//!
//! # On the protocol description
//!
//! The wire details here — tags and lengths, the 302-byte config table, the
//! `DMRD` layout and its bits byte, the handshake chain, the cadences — were
//! established the same way YSF's and NXDN's were: by reading the deployed
//! reference implementations as a *specification of the wire* and verifying
//! each claim against them, never from recall and never by copying code.
//! Every constant in this crate cites the file and function it was read out
//! of, and `docs/design/dmr-wire.md` records the fetch, the disagreements
//! between references, and how each was settled.
//!
//! | project | file(s) read | licence |
//! |---|---|---|
//! | `g4klx/DMRGateway` | `DMRNetwork.cpp`, `DMRNetwork.h` | GPL-2.0 |
//! | `g4klx/MMDVMHost` | `DMRDefines.h`, `Sync.cpp`, and the FEC and LC sources | GPL-2.0 |
//! | `nostar/DroidStar` | `dmr.cpp`, `dmr.h`, `serialambe.cpp` | GPL-3.0 |
//! | `HBLink-org/hblink3` | `hblink.py`, `const.py`, `playback.py` | GPL-3.0 |
//!
//! Those projects are GPL-2.0 and GPL-3.0; this one is AGPL-3.0-only, and
//! the licences do not mix. That is the practical reason as well as the
//! honest one to write each algorithm out from its definition — generator
//! polynomial, parity equations, field — rather than transcribing somebody's
//! table.

pub mod fsm;
pub mod master;
pub mod network;
pub mod wire;

pub use fsm::{
    DmrFsm, FailureStage, FsmAction, LINK_TIMEOUT, LOGIN_RETRY, LinkState, PING_INTERVAL,
};
pub use master::{Master, MasterHandle};
pub use network::{ALL, DmrNetwork, NetworkClass, dialable};
pub use wire::{
    BURST_LEN, CallType, ConfigFields, DATA_LEN, DataPacket, FrameType, Packet, RadioId,
    RadioIdError, Timeslot,
};
