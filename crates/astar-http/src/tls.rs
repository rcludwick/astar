// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! HTTPS on the same port as HTTP: a certificate slot every handshake reads,
//! so a renewed certificate is served without rebinding and open
//! connections carry on. rustls with the ring provider only.

use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::fmt;
use std::sync::{Arc, RwLock};

/// The certificate slot.
struct Slot(RwLock<Option<Arc<CertifiedKey>>>);

impl fmt::Debug for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Slot")
    }
}

impl ResolvesServerCert for Slot {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.0.read().expect("cert slot poisoned").clone()
    }
}

/// A server's TLS: TLS 1.2 and 1.3, ALPN `http/1.1`, and a certificate
/// that can be swapped while serving. Until one is installed a
/// `ClientHello` is closed and plain HTTP is still answered.
pub struct TlsConfig {
    slot: Arc<Slot>,
    config: Arc<rustls::ServerConfig>,
}

impl fmt::Debug for TlsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsConfig")
            .field("ready", &self.ready())
            .finish_non_exhaustive()
    }
}

impl TlsConfig {
    /// An empty slot: no TLS until [`Self::install`].
    ///
    /// # Panics
    /// Never in practice: the ring provider supports the default versions.
    #[must_use]
    pub fn new() -> Arc<Self> {
        let slot = Arc::new(Slot(RwLock::new(None)));
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring supports TLS 1.2 and 1.3")
        .with_no_client_auth()
        .with_cert_resolver(Arc::clone(&slot) as Arc<dyn ResolvesServerCert>);
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Arc::new(Self {
            slot,
            config: Arc::new(config),
        })
    }

    /// Serve `key` from the next handshake on. A resumed session keeps
    /// the certificate it began under; only a full handshake sees `key`.
    ///
    /// # Panics
    /// The slot's lock was poisoned by a panic while it was held.
    pub fn install(&self, key: CertifiedKey) {
        *self.slot.0.write().expect("cert slot poisoned") = Some(Arc::new(key));
    }

    /// A certificate is installed.
    ///
    /// # Panics
    /// The slot's lock was poisoned by a panic while it was held.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.slot.0.read().expect("cert slot poisoned").is_some()
    }

    /// A server connection for one handshake.
    pub(crate) fn connection(&self) -> Option<rustls::ServerConnection> {
        rustls::ServerConnection::new(Arc::clone(&self.config)).ok()
    }
}
