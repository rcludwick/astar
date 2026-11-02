// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Certificates and clients for the TLS tests.
use rustls::sign::CertifiedKey;
use rustls_pki_types::pem::PemObject as _;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use std::sync::Arc;

/// A self-signed certificate for `host`: the key rustls serves, and the
/// certificate's DER for a client to trust and to compare against.
pub fn self_signed(host: &str) -> (CertifiedKey, CertificateDer<'static>) {
    let kp = rcgen::KeyPair::generate().unwrap();
    let params = rcgen::CertificateParams::new(vec![host.to_string()]).unwrap();
    let cert = params.self_signed(&kp).unwrap();
    let der = CertificateDer::from(cert.der().to_vec());
    let key = PrivateKeyDer::from_pem_slice(kp.serialize_pem().as_bytes()).unwrap();
    let signing = rustls::crypto::ring::sign::any_supported_type(&key).unwrap();
    (CertifiedKey::new(vec![der.clone()], signing), der)
}

pub fn client_config(roots: &[&CertificateDer<'static>]) -> Arc<rustls::ClientConfig> {
    let mut store = rustls::RootCertStore::empty();
    for r in roots {
        store.add((*r).clone()).unwrap();
    }
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(store)
    .with_no_client_auth();
    // Every handshake a full one: a resumed session would report the
    // certificate of the session it resumed.
    cfg.resumption = rustls::client::Resumption::disabled();
    Arc::new(cfg)
}
