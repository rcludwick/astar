// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Admin sign-in (feature `auth`): passkeys, security keys and authenticator
//! apps, sessions, invites and the audit ring.
//!
//! The design is `designs/astar/2026-10-12-server-auth-design.md` in the
//! notes; `docs/site/server/admin.md` is the operator's side of it.
//!
//! Nothing here reaches the `Station`, a snapshot, an SSE event or a log
//! line with a secret in it: cookies, invite tokens, ceremony ids, TOTP
//! secrets and codes stay inside this module and the one response whose
//! purpose is to hand one over.

pub mod aaguid;
pub mod ceremony;
pub mod cli;
pub mod flows;
pub mod gate;
pub mod guard;
pub mod limits;
pub mod manage;
pub mod pages;
pub mod routes;
pub mod service;
pub mod session;
pub mod store;
pub mod totp;
pub mod webauthn;

#[cfg(test)]
mod tests_e2e;
#[cfg(test)]
mod tests_manage;
#[cfg(test)]
mod tests_secrets;
#[cfg(test)]
mod tests_totp;

pub use service::{AuthService, AuthSettings, Principal, Refusal, Req, Surface};

use crate::http::HttpResponse;

/// Answer a sign-in request — an `/auth/*` route or a sign-in page — or
/// `None` when the path is neither, for the caller to gate and route.
#[must_use]
pub fn handle(svc: &AuthService, req: &Req<'_>) -> Option<HttpResponse> {
    pages::handle(svc, req).or_else(|| routes::handle(svc, req))
}

/// A 200 JSON answer that no cache keeps.
#[must_use]
pub fn ok_json(v: &serde_json::Value) -> HttpResponse {
    HttpResponse::json(200, v).with_header("Cache-Control", "no-store")
}

/// A refusal as `{"error": <code>, "message": <sentence>}`.
#[must_use]
pub fn refusal_response(r: &Refusal) -> HttpResponse {
    HttpResponse::json(
        r.status,
        &serde_json::json!({ "error": r.code, "message": r.message }),
    )
    .with_header("Cache-Control", "no-store")
}
