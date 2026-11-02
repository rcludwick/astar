// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The public, read-only side of astar-server: the listener page, its status
//! projection, and (behind `public-tls`) its HTTPS and DNS automation.
//!
//! # The two classes of route
//!
//! astar-server answers on two binds, and they never share a router:
//!
//! * **The admin bind** (`[control] bind`, loopback by default) runs
//!   [`crate::http::handle_request`]: every route, `POST /key` included. It
//!   is the node's root of trust and is reached from outside only through an
//!   ssh tunnel.
//! * **The public bind** (`[web] public_bind`) runs [`http::route`]: a closed
//!   list of GET routes that read, never act. Nothing in this module calls
//!   `NodeController::execute`.
//!
//! `/admin` on the public bind is a placeholder page today, saying how to
//! tunnel to the admin bind. It is the one seam meant to change: when the
//! passkey sign-in ships, [`http::PUBLIC_ROUTES`]'s `/admin` entry becomes the
//! sign-in page, and any route that sign-in unlocks is added to the public
//! router explicitly, behind its session check — never by forwarding to the
//! admin router.

#[cfg(feature = "public-tls")]
pub mod acme;
#[cfg(feature = "public-tls")]
pub mod dns;
#[cfg(all(test, feature = "public-tls"))]
mod dns_contract;
#[cfg(feature = "dns-providers")]
pub mod dns_update;
#[cfg(feature = "public-tls")]
pub mod dnsq;
pub mod guard;
pub mod http;
#[cfg(feature = "public-tls")]
pub mod porkbun;
pub mod run;
pub mod serve;
pub mod status;
#[cfg(feature = "public-tls")]
pub mod tls;

/// Writes `_acme-challenge` TXT records for DNS-01. A no-op trait object
/// without `public-tls`, so callers name one type either way.
#[cfg(feature = "public-tls")]
pub use acme::Dns01Solver as DnsSolver;

/// Without `public-tls` there is no ACME client, and no solver to pass.
#[cfg(not(feature = "public-tls"))]
pub trait DnsSolver: Send + Sync {}
