// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! DMR primitives, designed in `docs/design/dmr-networks.md`.
//!
//! # Where this stands
//!
//! [`network`] only. It answers "which DMR do you mean" — the taxonomy, the
//! independent/BrandMeister split, and the consent gate that split exists for.
//! It has no I/O, no dependencies beyond `std`, and it cannot connect to
//! anything.
//!
//! The MMDVM/homebrew protocol, the AMBE+2 frame packing, the session and the
//! talkgroup dial grammar are not here yet. That order is deliberate: DMR is
//! the one network where the *address* is the hard part. A talkgroup number
//! names nothing on its own — TG 91 exists on several of these networks and is
//! a different room on each — so the thing that has to be right before any
//! wire code is written is what a target even is.
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
//! snapshot, an event, an error or a log. Nothing in this crate stores one, and
//! nothing in it should learn how.
//!
//! # On the protocol description
//!
//! When the wire lands here it will be established the same way YSF's was — by
//! reading the deployed reference implementations (G4KLX's `MMDVMHost` and
//! `DMRGateway`) as a specification and verifying each claim, never from
//! recall, and never by copying code. Those projects are GPL-2.0 and this one
//! is AGPL-3.0-only; the two do not mix.

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
