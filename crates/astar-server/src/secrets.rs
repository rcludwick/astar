// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Pluggable secret store for IAX2 credentials.
//!
//! `SecretProvider` is the single source of truth for username→secret mappings.
//! Credentials can be pushed at runtime (via a control-channel `ProvideSecret`
//! command) or loaded at start-up from well-known environment variables.
//!
//! The provider intentionally does **not** implement `Debug` (or derive it) so
//! that secrets can never be leaked through `{:?}` formatting or log macros.
//!
//! ## Namespaces
//!
//! Secrets are organized in flat key namespaces:
//! - **Inbound peer credentials:** bare username (e.g., `"55553"`), fed by
//!   `ALLSTAR_PEER_<NODE>` env vars or `POST /secrets`.
//! - **Outbound link credentials (iax-5029):** `"link:<node>"` namespace, fed by
//!   `ALLSTAR_LINK_<NODE>` env vars or `POST /secrets` with username `"link:<node>"`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

/// Shared, interior-mutable secret store the Station resolver consults.
#[derive(Clone, Default)]
pub struct SecretProvider {
    inner: Arc<Mutex<HashMap<String, String>>>,
}

impl SecretProvider {
    /// Create an empty provider.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Push one credential (control-channel `ProvideSecret`).
    pub fn put(&self, username: impl Into<String>, secret: impl Into<String>) {
        self.inner
            .lock()
            .expect("SecretProvider mutex poisoned")
            .insert(username.into(), secret.into());
    }

    /// Resolve `username` → secret.  Returns `""` if the username is unknown.
    #[must_use]
    pub fn resolve(&self, username: &str) -> String {
        self.inner
            .lock()
            .expect("SecretProvider mutex poisoned")
            .get(username)
            .cloned()
            .unwrap_or_default()
    }

    /// Resolve the OUTBOUND dial secret for linking to `node` (iax-5029):
    /// the `link:<node>` namespace, fed by `ALLSTAR_LINK_<NODE>` env vars or
    /// `POST /secrets` with username `link:<node>`. Returns `""` on miss —
    /// today's empty-secret dial, so unconfigured targets are unchanged.
    #[must_use]
    pub fn link_secret(&self, node: &str) -> String {
        self.resolve(&format!("link:{node}"))
    }

    /// Load `ALLSTAR_NODE`/`ALLSTAR_SECRET`, any `ALLSTAR_PEER_<NODE>`, and
    /// any `ALLSTAR_LINK_<NODE>` variables from the current process environment
    /// into the store.
    ///
    /// Variable semantics:
    /// - `ALLSTAR_NODE` + `ALLSTAR_SECRET` → `put(node, secret)`
    /// - `ALLSTAR_PEER_<NODE>=<secret>` → `put(node, secret)` for each match
    /// - `ALLSTAR_LINK_<NODE>=<secret>` → `put("link:<NODE>", secret)` for each match (iax-5029)
    pub fn load_env(&self) {
        // Primary node credential.
        if let (Ok(node), Ok(secret)) = (
            std::env::var("ALLSTAR_NODE"),
            std::env::var("ALLSTAR_SECRET"),
        ) {
            self.put(node, secret);
        }

        // Peer credentials: ALLSTAR_PEER_<NODE>=<secret>
        for (key, value) in std::env::vars() {
            if let Some(node) = key.strip_prefix("ALLSTAR_PEER_")
                && !node.is_empty()
            {
                self.put(node.to_owned(), value);
            }
        }

        // Outbound link credentials: ALLSTAR_LINK_<NODE>=<secret> — the
        // password WE present when dialing <NODE> (iax-5029). Stored under
        // the "link:" namespace so it can never collide with the inbound
        // ALLSTAR_PEER_<NODE> username keys.
        for (key, value) in std::env::vars() {
            if let Some(node) = key.strip_prefix("ALLSTAR_LINK_")
                && !node.is_empty()
            {
                self.put(format!("link:{node}"), value);
            }
        }
    }

    /// Load the inline credential from `[secrets] source = "config"`
    /// (iax-4703): `node_id` is the username to register as (mirrors
    /// `register.node_id`, same as the env path), `secret` is the value read
    /// straight from `node.toml`. A no-op when `secret` is empty — the config
    /// loader has already warned in that case at parse time
    /// (`NodeFileConfig::from_toml_str`); this just avoids seeding the store
    /// with an empty credential.
    pub fn load_config_secret(&self, node_id: &str, secret: &str) {
        if !secret.is_empty() {
            self.put(node_id.to_string(), secret.to_string());
        }
    }

    /// Return a `Box<dyn Fn(&str) -> String + Send + Sync>` over this provider,
    /// suitable for passing to `Station::set_secret_resolver`.
    #[must_use]
    pub fn resolver(&self) -> Box<dyn Fn(&str) -> String + Send + Sync> {
        let provider = self.clone();
        Box::new(move |username: &str| provider.resolve(username))
    }

    /// Return an `Arc<dyn Fn(&str) -> String + Send + Sync>` over this provider,
    /// suitable for `IncomingCallPolicy::credential_resolver` (iax-99cd). Shares
    /// the same backing store the env loader and `POST /secrets` feed, so
    /// inbound `auth=Required`/`Optional` authenticate against runtime creds.
    #[must_use]
    pub fn resolver_arc(&self) -> Arc<dyn Fn(&str) -> String + Send + Sync> {
        let provider = self.clone();
        Arc::new(move |username: &str| provider.resolve(username))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_and_resolve() {
        let p = SecretProvider::new();
        p.put("55553", "hunter2");
        assert_eq!(p.resolve("55553"), "hunter2");
        assert_eq!(p.resolve("nobody"), "");
    }

    #[test]
    fn load_env_reads_allstar_vars() {
        // SAFETY: single-threaded test; set then clear.
        unsafe {
            std::env::set_var("ALLSTAR_NODE", "1234");
            std::env::set_var("ALLSTAR_SECRET", "sekret");
        }
        let p = SecretProvider::new();
        p.load_env();
        assert_eq!(p.resolve("1234"), "sekret");
        unsafe {
            std::env::remove_var("ALLSTAR_NODE");
            std::env::remove_var("ALLSTAR_SECRET");
        }
    }

    #[test]
    fn load_env_reads_peer_vars() {
        unsafe {
            std::env::set_var("ALLSTAR_PEER_9876", "peersecret");
        }
        let p = SecretProvider::new();
        p.load_env();
        assert_eq!(p.resolve("9876"), "peersecret");
        unsafe {
            std::env::remove_var("ALLSTAR_PEER_9876");
        }
    }

    #[test]
    fn resolver_closure_shares_state() {
        let p = SecretProvider::new();
        let resolver = p.resolver();
        // Insert after creating the resolver; they share the same Arc.
        p.put("live-node", "live-secret");
        assert_eq!(resolver("live-node"), "live-secret");
        assert_eq!(resolver("ghost"), "");
    }

    #[test]
    fn clone_shares_store() {
        let p1 = SecretProvider::new();
        let p2 = p1.clone();
        p1.put("shared", "value");
        assert_eq!(p2.resolve("shared"), "value");
    }

    // -- iax-4703: [secrets] source = "config" (inline secret) -------------

    #[test]
    fn load_config_secret_reaches_resolver() {
        // Mirrors `load_env_reads_allstar_vars`: a config-sourced secret must
        // reach the same seam (`resolve`) that the env path feeds.
        let p = SecretProvider::new();
        p.load_config_secret("77777", "hunter2xyz");
        assert_eq!(p.resolve("77777"), "hunter2xyz");
    }

    #[test]
    fn load_config_secret_empty_is_noop() {
        let p = SecretProvider::new();
        p.load_config_secret("77777", "");
        assert_eq!(p.resolve("77777"), "");
    }

    // -- iax-5029: outbound link-secret namespace ---------------------------

    #[test]
    fn load_env_reads_link_vars_into_link_namespace() {
        unsafe {
            std::env::set_var("ALLSTAR_LINK_1999", "outbound-pw");
        }
        let p = SecretProvider::new();
        p.load_env();
        assert_eq!(p.link_secret("1999"), "outbound-pw");
        // The namespaces must not collide: no bare-username entry appears.
        assert_eq!(p.resolve("1999"), "");
        unsafe {
            std::env::remove_var("ALLSTAR_LINK_1999");
        }
    }

    #[test]
    fn link_secret_miss_is_empty() {
        let p = SecretProvider::new();
        assert_eq!(p.link_secret("nobody"), "");
    }

    #[test]
    fn link_secret_reachable_via_provide_secret_key() {
        // POST /secrets with username "link:<node>" feeds the same seam.
        let p = SecretProvider::new();
        p.put("link:55000", "pushed-pw");
        assert_eq!(p.link_secret("55000"), "pushed-pw");
    }
}
