// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Checks an application makes on a request before it answers it.

use crate::request::Request;
use std::net::{IpAddr, SocketAddr};

/// `Host` as compared: lower-cased, a trailing dot and a port stripped. An
/// IPv6 literal keeps its brackets and so never matches a name.
#[must_use]
pub fn normalize_host(host: &str) -> String {
    let h = host.trim().to_ascii_lowercase();
    let h = h.strip_suffix('.').unwrap_or(&h);
    let h = match h.rsplit_once(':') {
        Some((name, port))
            if !name.contains(':')
                && !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit()) =>
        {
            name
        }
        _ => h,
    };
    h.strip_suffix('.').unwrap_or(h).to_string()
}

/// Whether `host` (the raw header) names one of `names` (each already
/// normalised).
#[must_use]
pub fn host_allowed_raw(names: &[String], host: Option<&str>) -> bool {
    host.map(normalize_host).is_some_and(|h| names.contains(&h))
}

/// Whether the request's `Host` names one of `names`.
#[must_use]
pub fn host_allowed(names: &[String], req: &Request<'_>) -> bool {
    host_allowed_raw(names, req.header("Host"))
}

/// Why [`origin_rule`] refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginRefusal {
    /// A browser request whose body is not JSON.
    NotJson,
    /// An `Origin` that is not this `Host` (or is `null`).
    CrossOrigin,
}

impl OriginRefusal {
    /// The status the refusal is answered with.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::NotJson => 415,
            Self::CrossOrigin => 403,
        }
    }
}

/// The rule a state-changing request passes: a browser request (one with
/// `Origin` or `Sec-Fetch-Site`) must be JSON, and an `Origin`, when sent,
/// must name this `Host`. A request with neither (curl, scripts) passes.
///
/// # Errors
/// [`OriginRefusal`].
pub fn origin_rule(req: &Request<'_>) -> Result<(), OriginRefusal> {
    origin_rule_raw(
        req.header("Origin"),
        req.header("Sec-Fetch-Site"),
        req.header("Content-Type"),
        req.header("Host"),
    )
}

/// [`origin_rule`] over the raw headers. The JSON check runs first, so a
/// form post from a foreign page is told about the media type.
///
/// # Errors
/// [`OriginRefusal::NotJson`] for a browser request that is not JSON;
/// [`OriginRefusal::CrossOrigin`] for an `Origin` that does not match
/// `Host`, `null` included.
pub fn origin_rule_raw(
    origin: Option<&str>,
    sec_fetch_site: Option<&str>,
    content_type: Option<&str>,
    host: Option<&str>,
) -> Result<(), OriginRefusal> {
    let from_browser = origin.is_some() || sec_fetch_site.is_some();
    if from_browser && !is_json(content_type) {
        return Err(OriginRefusal::NotJson);
    }
    let Some(origin) = origin else {
        return Ok(());
    };
    let same = origin_authority(origin)
        .is_some_and(|auth| host.is_some_and(|host| auth.eq_ignore_ascii_case(host)));
    if same {
        Ok(())
    } else {
        Err(OriginRefusal::CrossOrigin)
    }
}

/// Whether a `Content-Type` is `application/json`, parameters ignored.
#[must_use]
pub fn is_json(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|ct| {
        ct.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("application/json")
    })
}

/// The `host[:port]` of an origin, or `None` when it has none — which
/// includes the literal `null` a sandboxed frame sends, and which must
/// never compare equal to anything.
fn origin_authority(origin: &str) -> Option<&str> {
    let after_scheme = origin.split_once("://")?.1;
    let authority = after_scheme.split('/').next()?;
    if authority.is_empty() {
        None
    } else {
        Some(authority)
    }
}

/// Whether the request came from this machine: decided from the peer
/// address only, and a `Forwarded` or `X-Forwarded-For` header always
/// disqualifies it. There is deliberately no helper that judges "local"
/// from `Host`, which the client chooses.
#[must_use]
pub fn is_loopback(req: &Request<'_>) -> bool {
    is_loopback_peer(Some(req.peer()), req.forwarded())
}

/// Whether a request is from a loopback peer: a loopback address, and no
/// sign that a proxy on this box relayed it from somewhere else.
///
/// A reverse proxy on the node makes every request look like it came from
/// `127.0.0.1`. Proxies that say so (`Forwarded`, `X-Forwarded-For`) lose
/// the bypass.
#[must_use]
pub fn is_loopback_peer(peer: Option<SocketAddr>, forwarded: bool) -> bool {
    !forwarded && peer.is_some_and(|p| is_loopback_ip(p.ip()))
}

/// Loopback, IPv4-mapped IPv6 loopback included (`::ffff:127.0.0.1`).
fn is_loopback_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}
