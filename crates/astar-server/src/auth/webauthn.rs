// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! One `Webauthn` per origin a user signs in on.
//!
//! The relying-party ID is the request's own hostname, which the `Host`
//! allow-list has already checked, or `[web] rp_id` when a parent domain is
//! configured. It is never taken from an unchecked `Host`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use webauthn_rs::prelude::{Url, Webauthn, WebauthnBuilder};

use super::service::{Refusal, Req};

/// The relying party's display name in the browser's prompt.
pub const RP_NAME: &str = "astar";

/// Cached instances, by origin and RP ID.
#[derive(Default)]
pub struct Instances {
    map: Mutex<HashMap<(String, String), Arc<Webauthn>>>,
}

impl Instances {
    /// The instance for `origin` with `rp_id`, built on first use.
    ///
    /// # Errors
    /// An origin webauthn-rs refuses: an IP address, or an RP ID that is not
    /// the origin's domain or a parent of it.
    pub fn get(&self, rp_id: &str, origin: &str) -> Result<Arc<Webauthn>, Refusal> {
        let key = (origin.to_string(), rp_id.to_string());
        let mut map = self.map.lock().expect("webauthn cache poisoned");
        if let Some(w) = map.get(&key) {
            return Ok(Arc::clone(w));
        }
        let url = Url::parse(origin).map_err(|_| no_passkeys_here())?;
        let w = WebauthnBuilder::new(rp_id, &url)
            .and_then(|b| b.rp_name(RP_NAME).build())
            .map_err(|_| no_passkeys_here())?;
        let w = Arc::new(w);
        map.insert(key, Arc::clone(&w));
        Ok(w)
    }
}

fn no_passkeys_here() -> Refusal {
    Refusal::new(
        400,
        "webauthn_unavailable",
        "Passkeys and security keys need this page opened by name (localhost, or the node's hostname), not by address. An authenticator app works anywhere.",
    )
}

/// The origin a browser on this request sees: scheme and `Host`.
#[must_use]
pub fn origin_of(req: &Req<'_>) -> String {
    let scheme = if req.tls { "https" } else { "http" };
    let host = req.host.unwrap_or_default().trim().to_ascii_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host);
    // A default port is not part of the origin a browser reports.
    let host = match (req.tls, host.rsplit_once(':')) {
        (true, Some((h, "443"))) | (false, Some((h, "80")))
            if !h.contains(':') || h.ends_with(']') =>
        {
            h
        }
        _ => host,
    };
    format!("{scheme}://{host}")
}

/// The RP ID for a request: `[web] rp_id` when the hostname sits under it,
/// else the hostname itself.
#[must_use]
pub fn rp_id_for(configured: Option<&str>, hostname: &str) -> String {
    match configured {
        Some(rp) if hostname == rp || hostname.ends_with(&format!(".{rp}")) => rp.to_string(),
        _ => hostname.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::service::{Surface, tests::req};

    #[test]
    fn origins_drop_default_ports_only() {
        let mut r = req(Surface::Public, None);
        r.host = Some("Node.Example.Test:443");
        assert_eq!(origin_of(&r), "https://node.example.test");
        r.host = Some("node.example.test:8443");
        assert_eq!(origin_of(&r), "https://node.example.test:8443");
        r.tls = false;
        r.host = Some("localhost:8730");
        assert_eq!(origin_of(&r), "http://localhost:8730");
    }

    #[test]
    fn the_rp_id_is_the_parent_only_when_the_host_is_under_it() {
        assert_eq!(
            rp_id_for(Some("example.org"), "node.example.org"),
            "example.org"
        );
        assert_eq!(rp_id_for(Some("example.org"), "example.org"), "example.org");
        assert_eq!(rp_id_for(Some("example.org"), "localhost"), "localhost");
        assert_eq!(rp_id_for(None, "node.example.org"), "node.example.org");
    }

    #[test]
    fn instances_are_cached_and_addresses_refused() {
        let i = Instances::default();
        let a = i
            .get("node.example.test", "https://node.example.test")
            .unwrap();
        let b = i
            .get("node.example.test", "https://node.example.test")
            .unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(i.get("127.0.0.1", "http://127.0.0.1:8730").is_err());
        assert!(i.get("other.test", "https://node.example.test").is_err());
    }
}
