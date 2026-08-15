// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `AllStarLink` (ASL3) service layer (iax-53da).
//!
//! Pure services for ASL3 consumers: mint a Web Transceiver token from the
//! `AllStarLink` portal ([`mint_wt_token`]) and resolve a node number to its
//! IAX2 address via the DNS node directory ([`resolve_node`]).
//!
//! Design contract: **config in, values out**. This crate never reads
//! environment variables or files (except the system resolver config), and
//! never logs credentials. Callers (the inspect harness, astar) own their
//! configuration story.

mod dns;
mod error;
mod mint;
mod resolve;

pub use error::Asl3Error;
pub use mint::{PortalCredentials, mint_wt_token, mint_wt_token_at};
pub use resolve::{resolve_addr, resolve_node};
