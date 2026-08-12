// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Validated `WireGuard` configuration.
//!
//! Secret-free by construction (house rule): configs carry a **private-key
//! reference**, never key material. The reference is resolved through an
//! injected [`SecretResolver`] exactly once, when the data plane is built
//! ([`crate::WgStack::new`]).

use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};

use base64::Engine as _;
use boringtun::x25519::{PublicKey, StaticSecret};

/// Resolve a secret reference (e.g. an env-var name or keychain label) to the
/// base64 private-key material. INJECTED by the app so key material never
/// lands in config/snapshot/event/log types. Mirrors the engine's
/// `SecretResolver` house pattern. Returning an empty string means "not set"
/// and fails resolution with a descriptive error.
pub type SecretResolver<'a> = dyn Fn(&str) -> String + 'a;

/// Validation errors for [`WgLinkConfig::new`] and private-key resolution.
#[derive(Debug, thiserror::Error)]
pub enum WgConfigError {
    #[error("invalid key: {0}")]
    Key(String),
    #[error("invalid tunnel address: {0}")]
    Address(String),
    #[error("invalid endpoint: {0}")]
    Endpoint(String),
    #[error("invalid allowed_ip: {0}")]
    AllowedIp(String),
}

fn decode_key32(b64: &str) -> Result<[u8; 32], WgConfigError> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| WgConfigError::Key(e.to_string()))?;
    let arr: [u8; 32] = raw
        .try_into()
        .map_err(|_| WgConfigError::Key("key must be 32 bytes".into()))?;
    Ok(arr)
}

/// Resolve `key_ref` through `resolver` and decode the base64 x25519 secret.
/// Errors carry the reference name (never the material) for diagnostics.
fn resolve_key(key_ref: &str, resolver: &SecretResolver) -> Result<StaticSecret, WgConfigError> {
    let b64 = resolver(key_ref);
    if b64.trim().is_empty() {
        return Err(WgConfigError::Key(format!(
            "{key_ref}: resolved to empty (is the secret set?)"
        )));
    }
    match decode_key32(&b64) {
        Ok(k) => Ok(StaticSecret::from(k)),
        Err(WgConfigError::Key(m)) => Err(WgConfigError::Key(format!("{key_ref}: {m}"))),
        Err(e) => Err(e),
    }
}

fn parse_cidr(s: &str) -> Option<(IpAddr, u8)> {
    let (ip, prefix) = s.split_once('/')?;
    let ip: IpAddr = ip.parse().ok()?;
    let prefix: u8 = prefix.parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    (prefix <= max).then_some((ip, prefix))
}

fn parse_endpoint(endpoint: &str) -> Result<SocketAddr, WgConfigError> {
    endpoint
        .to_socket_addrs()
        .map_err(|e| WgConfigError::Endpoint(e.to_string()))?
        .next()
        .ok_or_else(|| WgConfigError::Endpoint(format!("{endpoint}: no address")))
}

fn parse_allowed(allowed_ips: &[String]) -> Result<Vec<(IpAddr, u8)>, WgConfigError> {
    let mut parsed = Vec::with_capacity(allowed_ips.len());
    for a in allowed_ips {
        parsed.push(parse_cidr(a).ok_or_else(|| WgConfigError::AllowedIp(a.clone()))?);
    }
    Ok(parsed)
}

/// Configuration for the userspace [`crate::WgStack`] (iax-8516): a shared
/// single-peer tunnel with internal IPv4/UDP sockets — no TUN device.
///
/// House secret rule: `private_key_ref` is a reference, never key material,
/// resolved via a [`SecretResolver`] at [`crate::WgStack::new`] time. The
/// tunnel-inner network is IPv4-only for v1, so the tunnel address must be
/// IPv4.
pub struct WgLinkConfig {
    private_key_ref: String,
    peer_public_key: PublicKey,
    tunnel_ip: Ipv4Addr,
    tunnel_prefix: u8,
    endpoint: SocketAddr,
    /// Allowed-IPs CIDRs for the peer (advisory in userspace: the stack has no
    /// route table; demux is by bound inner port).
    allowed_ips: Vec<(IpAddr, u8)>,
    keepalive_secs: u16,
    /// Optional plain (non-tunnel) OS UDP listener address the embedding engine
    /// binds ALONGSIDE the tunnel listener for direct/LAN peers (default
    /// `None`). Carried here so one config describes the whole link transport;
    /// the stack itself ignores it.
    also_bind_udp: Option<SocketAddr>,
}

impl std::fmt::Debug for WgLinkConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print key material (the ref is a name, not a secret).
        f.debug_struct("WgLinkConfig")
            .field("private_key_ref", &self.private_key_ref)
            .field("tunnel_ip", &self.tunnel_ip)
            .field("tunnel_prefix", &self.tunnel_prefix)
            .field("endpoint", &self.endpoint)
            .field("allowed_ips", &self.allowed_ips)
            .field("keepalive_secs", &self.keepalive_secs)
            .field("also_bind_udp", &self.also_bind_udp)
            .finish_non_exhaustive()
    }
}

impl WgLinkConfig {
    /// Validate and build a stack config.
    ///
    /// `address` is the node's tunnel address in CIDR form (IPv4 only, e.g.
    /// `"10.99.0.2/32"`); `private_key_ref` is an opaque reference resolved
    /// later via a [`SecretResolver`].
    pub fn new(
        private_key_ref: &str,
        address: &str,
        peer_public_key_b64: &str,
        endpoint: &str,
        allowed_ips: &[String],
        keepalive_secs: u16,
    ) -> Result<Self, WgConfigError> {
        let peer_public_key = PublicKey::from(decode_key32(peer_public_key_b64)?);

        let (tunnel_ip, tunnel_prefix) =
            parse_cidr(address).ok_or_else(|| WgConfigError::Address(address.to_string()))?;
        let IpAddr::V4(tunnel_ip) = tunnel_ip else {
            return Err(WgConfigError::Address(format!(
                "{address}: tunnel address must be IPv4"
            )));
        };

        Ok(Self {
            private_key_ref: private_key_ref.to_string(),
            peer_public_key,
            tunnel_ip,
            tunnel_prefix,
            endpoint: parse_endpoint(endpoint)?,
            allowed_ips: parse_allowed(allowed_ips)?,
            keepalive_secs,
            also_bind_udp: None,
        })
    }

    /// Set (or clear) the optional plain OS UDP listener address bound
    /// alongside the tunnel listener (see the field doc). Default `None`.
    #[must_use]
    pub fn with_also_bind_udp(mut self, addr: Option<SocketAddr>) -> Self {
        self.also_bind_udp = addr;
        self
    }

    /// The optional plain OS UDP listener address (default `None`).
    #[must_use]
    pub fn also_bind_udp(&self) -> Option<SocketAddr> {
        self.also_bind_udp
    }

    #[must_use]
    pub fn endpoint(&self) -> SocketAddr {
        self.endpoint
    }

    #[must_use]
    pub fn tunnel_ip(&self) -> Ipv4Addr {
        self.tunnel_ip
    }

    #[must_use]
    pub fn tunnel_prefix(&self) -> u8 {
        self.tunnel_prefix
    }

    #[must_use]
    pub fn allowed_ips(&self) -> &[(IpAddr, u8)] {
        &self.allowed_ips
    }

    #[must_use]
    pub fn keepalive(&self) -> u16 {
        self.keepalive_secs
    }

    /// The private-key reference resolved at stack build time.
    #[must_use]
    pub fn private_key_ref(&self) -> &str {
        &self.private_key_ref
    }

    /// Resolve the private key through `resolver`. Called once at
    /// [`crate::WgStack::new`]; the material is handed straight to boringtun.
    pub(crate) fn resolve_private_key(
        &self,
        resolver: &SecretResolver,
    ) -> Result<StaticSecret, WgConfigError> {
        resolve_key(&self.private_key_ref, resolver)
    }

    pub(crate) fn peer_public_key(&self) -> PublicKey {
        self.peer_public_key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A valid base64 of 32 zero bytes (length-valid x25519 key material).
    const KEY32: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    #[test]
    fn rejects_bad_peer_key() {
        let err = WgLinkConfig::new(
            "ref",
            "10.99.0.2/32",
            "not-base64!!",
            "127.0.0.1:51820",
            &[],
            25,
        )
        .unwrap_err();
        assert!(matches!(err, WgConfigError::Key(_)));
    }

    #[test]
    fn rejects_bad_address() {
        let err =
            WgLinkConfig::new("ref", "not-a-cidr", KEY32, "127.0.0.1:51820", &[], 25).unwrap_err();
        assert!(matches!(err, WgConfigError::Address(_)));
    }

    #[test]
    fn rejects_bad_endpoint() {
        let err = WgLinkConfig::new("ref", "10.99.0.2/32", KEY32, "nope", &[], 25).unwrap_err();
        assert!(matches!(err, WgConfigError::Endpoint(_)));
    }

    #[test]
    fn rejects_bad_allowed_ip() {
        let err = WgLinkConfig::new(
            "ref",
            "10.99.0.2/32",
            KEY32,
            "127.0.0.1:51820",
            &["xyz".into()],
            25,
        )
        .unwrap_err();
        assert!(matches!(err, WgConfigError::AllowedIp(_)));
    }

    #[test]
    fn resolve_private_key_calls_resolver_with_ref() {
        let cfg = WgLinkConfig::new("MY_REF", "10.99.0.2/32", KEY32, "127.0.0.1:51820", &[], 25)
            .expect("valid");
        let seen = std::cell::RefCell::new(Vec::new());
        let resolver = |name: &str| {
            seen.borrow_mut().push(name.to_string());
            KEY32.to_string()
        };
        cfg.resolve_private_key(&resolver).expect("resolves");
        assert_eq!(*seen.borrow(), vec!["MY_REF".to_string()]);
    }

    #[test]
    fn resolve_private_key_rejects_bad_material() {
        let cfg = WgLinkConfig::new("MY_REF", "10.99.0.2/32", KEY32, "127.0.0.1:51820", &[], 25)
            .expect("valid");
        let Err(err) = cfg.resolve_private_key(&|_| "not-base64!!".to_string()) else {
            panic!("bad key material must fail");
        };
        assert!(matches!(err, WgConfigError::Key(_)));
        assert!(err.to_string().contains("MY_REF"), "got: {err}");
    }

    #[test]
    fn resolve_private_key_rejects_empty_material() {
        let cfg = WgLinkConfig::new("MY_REF", "10.99.0.2/32", KEY32, "127.0.0.1:51820", &[], 25)
            .expect("valid");
        let Err(err) = cfg.resolve_private_key(&|_| String::new()) else {
            panic!("empty key material must fail");
        };
        assert!(err.to_string().contains("MY_REF"), "got: {err}");
    }

    #[test]
    fn link_config_parses_and_exposes_fields() {
        let cfg = WgLinkConfig::new(
            "WIREGUARD_PRIVATE_KEY",
            "10.99.0.2/32",
            KEY32,
            "127.0.0.1:51820",
            &["10.99.0.0/24".to_string()],
            25,
        )
        .expect("valid");
        assert_eq!(cfg.endpoint(), "127.0.0.1:51820".parse().unwrap());
        assert_eq!(cfg.tunnel_ip(), "10.99.0.2".parse::<Ipv4Addr>().unwrap());
        assert_eq!(cfg.tunnel_prefix(), 32);
        assert_eq!(cfg.keepalive(), 25);
        assert_eq!(cfg.private_key_ref(), "WIREGUARD_PRIVATE_KEY");
        assert_eq!(cfg.allowed_ips().len(), 1);
        assert_eq!(cfg.also_bind_udp(), None, "default is no extra UDP bind");
    }

    #[test]
    fn link_config_carries_also_bind_udp_when_set() {
        let extra: SocketAddr = "0.0.0.0:4569".parse().unwrap();
        let cfg = WgLinkConfig::new("ref", "10.99.0.2/32", KEY32, "127.0.0.1:51820", &[], 25)
            .expect("valid")
            .with_also_bind_udp(Some(extra));
        assert_eq!(cfg.also_bind_udp(), Some(extra));
        // And it clears back to None.
        let cfg = cfg.with_also_bind_udp(None);
        assert_eq!(cfg.also_bind_udp(), None);
    }

    #[test]
    fn link_config_rejects_ipv6_tunnel_address() {
        let err =
            WgLinkConfig::new("ref", "fd00::2/128", KEY32, "127.0.0.1:51820", &[], 25).unwrap_err();
        assert!(matches!(err, WgConfigError::Address(_)));
    }

    #[test]
    fn link_config_debug_contains_no_key_material() {
        let cfg = WgLinkConfig::new("MY_REF", "10.99.0.2/32", KEY32, "127.0.0.1:51820", &[], 25)
            .expect("valid");
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains(KEY32), "Debug leaked key material: {dbg}");
        assert!(dbg.contains("MY_REF"));
    }
}
