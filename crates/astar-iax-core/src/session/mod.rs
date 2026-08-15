// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Session-layer state machine for an IAX2 call.
//!
//! See `docs/design/session-fsm.md` for the design. Sub-modules:
//!
//! - [`fsm`] — the pure call FSM ([`Fsm::handle`]).
//! - [`reg`] — the pure registration FSM ([`reg::RegFsm::handle`]).
//! - [`reliability`] — OSeqno/ISeqno bookkeeping below the FSM.
//! - [`keepalive`] — pure liveness/RTT bookkeeping for an active call.
//! - [`auth`] — challenge/response helpers.
//! - [`call_no`] — 15-bit call-number newtype.
//! - [`call_profile`] — mode-varying NEW frame parameters.

pub mod auth;
mod builders;
pub mod call_no;
pub mod call_profile;
pub mod codec_policy;
pub mod fsm;
mod handlers_inbound;
mod handlers_outbound;
pub mod keepalive;
pub mod reg;
pub mod reliability;

pub use auth::{AuthMethods, Credentials, Secret};
pub use call_no::CallNo;
pub use call_profile::CallProfile;
pub use codec_policy::CodecPolicy;
pub use fsm::{
    AcceptSentData, Action, ActiveData, AnswerSentData, AppCommand, AppEvent, AuthRepSentData,
    AuthReqReceivedData, AuthReqSentData, CallProgress, CallToken, CallTokenIssuedData,
    CallTokenReceivedData, CodecMask, Event, FailReason, Fsm, HangupData, HangupOrigin,
    InboundPolicy, IncomingOffer, NewReceivedData, NewResentData, NewSentData, OfferReject,
    SessionState, TimerKind,
};
pub use keepalive::{KeepaliveConfig, KeepaliveState, KeepaliveTick};
pub use reg::{
    RegAction, RegAppCommand, RegAppEvent, RegEvent, RegFailReason, RegFsm, RegState,
    RegisterOptions,
};
pub use reliability::{Reliability, ReliabilityConfig, RxOutcome, TickOutcome, encode_ack};
