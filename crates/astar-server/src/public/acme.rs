// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A small, synchronous ACME client (RFC 8555) and the certificate store.
//!
//! Synchronous on purpose: the plan's fallback was `instant-acme` on a
//! confined tokio runtime, but the node is thread-based throughout and its
//! default build is guarded tokio-free, so this speaks the protocol directly
//! over `ureq` (blocking, rustls) with `ring` for the ES256 account
//! signatures and `rcgen` for the CSR. The whole flow is one function,
//! [`issue`], and the Pebble test drives it end to end.
//!
//! Both challenge types are supported: HTTP-01 through the port-80
//! listener's [`ChallengeMap`], and DNS-01 through a [`Dns01Solver`] (the
//! Porkbun API). TXT records a DNS-01 run creates are always deleted, on
//! success and on every failure path.
//!
//! Nothing secret is logged: the account key and the certificate key never
//! leave the store, and logs carry hosts, dates and the CA's error text.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _};
use serde_json::{Value, json};

use crate::public::http::ChallengeMap;

/// Let's Encrypt production.
pub const LE_PROD: &str = "https://acme-v02.api.letsencrypt.org/directory";
/// Let's Encrypt staging: untrusted certificates, generous rate limits.
pub const LE_STAGING: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

/// Renew when fewer than this many days are left.
pub const RENEW_BEFORE_DAYS: u64 = 30;
/// The admin page warns when fewer than this many days are left.
pub const WARN_BEFORE_DAYS: u64 = 14;

/// How long one authorisation or the order may take to settle.
const POLL_LIMIT: Duration = Duration::from_secs(180);

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// ---- the certificate store ------------------------------------------------------

/// A certificate covering every public hostname (one SAN list).
#[derive(Clone)]
pub struct Cert {
    pub chain_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    pub not_after: SystemTime,
    /// Sorted.
    pub hosts: Vec<String>,
}

impl std::fmt::Debug for Cert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key is never printed.
        f.debug_struct("Cert")
            .field("hosts", &self.hosts)
            .field("not_after", &self.not_after)
            .finish_non_exhaustive()
    }
}

/// `<state>/acme/`: `account-<ca>.json` and `cert-bundle.json`, the
/// certificate's key, chain and hostnames in one file so a save is one
/// rename and can never leave a new key beside an old chain. Directory 0700,
/// files 0600, written atomically.
///
/// Stores saved before the bundle (`cert.pem`, `cert-key.pem`, `cert.json`,
/// written one after another) still load, and the first save removes them.
/// Either way a key that does not match its chain loads as no certificate,
/// so the manager issues a fresh one instead of serving a broken pair.
#[derive(Debug, Clone)]
pub struct CertStore {
    dir: PathBuf,
}

/// The three-file layout's `cert.json`.
#[derive(serde::Deserialize)]
struct CertMeta {
    hosts: Vec<String>,
    not_after_unix: u64,
}

/// `cert-bundle.json`: everything a certificate is, in one file.
#[derive(serde::Serialize, serde::Deserialize)]
struct CertBundle {
    hosts: Vec<String>,
    not_after_unix: u64,
    chain_pem: String,
    key_pem: String,
}

/// `issued.json`: when each recent certificate for one set of names was
/// issued, so a restart still knows how close the CA's weekly limit is.
#[derive(serde::Serialize, serde::Deserialize)]
struct IssuedFile {
    hosts: Vec<String>,
    issued_unix: Vec<u64>,
}

const BUNDLE: &str = "cert-bundle.json";
const ISSUED: &str = "issued.json";
const LEGACY: [&str; 3] = ["cert.json", "cert.pem", "cert-key.pem"];

#[derive(serde::Serialize, serde::Deserialize)]
struct AccountFile {
    directory: String,
    /// PKCS#8, base64url. The account's whole identity: kept 0600.
    pkcs8: String,
    kid: Option<String>,
}

/// Write `bytes` to `path` with mode 0600, through a temporary file.
///
/// # Errors
/// I/O.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Create `dir` (and parents) and make it 0700.
///
/// # Errors
/// I/O.
pub fn private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn sorted(hosts: &[String]) -> Vec<String> {
    let mut v = hosts.to_vec();
    v.sort();
    v.dedup();
    v
}

impl CertStore {
    /// The store under `<state_dir>/acme`.
    #[must_use]
    pub fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join("acme"),
        }
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The stored certificate, if it covers exactly `hosts` and its key
    /// matches its chain.
    #[must_use]
    pub fn load(&self, hosts: &[String]) -> Option<Cert> {
        let cert = match std::fs::read(self.dir.join(BUNDLE)) {
            Ok(bytes) => {
                let b: CertBundle = serde_json::from_slice(&bytes).ok()?;
                Self::cert(
                    &b.hosts,
                    b.not_after_unix,
                    b.chain_pem.into(),
                    b.key_pem.into(),
                )
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => self.load_legacy()?,
            Err(_) => return None,
        };
        if cert.hosts != sorted(hosts) {
            return None;
        }
        if let Err(e) = crate::public::tls::certified_key(&cert) {
            tracing::warn!("public tls: the stored certificate is unusable, ignoring it: {e}");
            return None;
        }
        Some(cert)
    }

    fn load_legacy(&self) -> Option<Cert> {
        let meta: CertMeta =
            serde_json::from_slice(&std::fs::read(self.dir.join("cert.json")).ok()?).ok()?;
        let chain_pem = std::fs::read(self.dir.join("cert.pem")).ok()?;
        let key_pem = std::fs::read(self.dir.join("cert-key.pem")).ok()?;
        Some(Self::cert(
            &meta.hosts,
            meta.not_after_unix,
            chain_pem,
            key_pem,
        ))
    }

    fn cert(hosts: &[String], not_after_unix: u64, chain_pem: Vec<u8>, key_pem: Vec<u8>) -> Cert {
        let not_after = first_cert_not_after(&chain_pem).unwrap_or(not_after_unix);
        Cert {
            chain_pem,
            key_pem,
            not_after: UNIX_EPOCH + Duration::from_secs(not_after),
            hosts: sorted(hosts),
        }
    }

    /// Save `cert` (one 0600 file in a 0700 directory, replaced by a single
    /// rename: a save cut short leaves the previous certificate whole).
    ///
    /// # Errors
    /// I/O.
    pub fn save(&self, cert: &Cert) -> io::Result<()> {
        let pem = |b: &[u8]| {
            String::from_utf8(b.to_vec())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "PEM is not text"))
        };
        let bundle = CertBundle {
            hosts: sorted(&cert.hosts),
            not_after_unix: unix(cert.not_after),
            chain_pem: pem(&cert.chain_pem)?,
            key_pem: pem(&cert.key_pem)?,
        };
        private_dir(&self.dir)?;
        write_private(
            &self.dir.join(BUNDLE),
            &serde_json::to_vec_pretty(&bundle).map_err(io::Error::other)?,
        )?;
        // The bundle wins from now on; the old files are only stale copies.
        for f in LEGACY {
            let _ = std::fs::remove_file(self.dir.join(f));
        }
        Ok(())
    }

    /// When the certificates recorded for exactly `hosts` were issued, in
    /// Unix seconds, oldest first. Empty when nothing is recorded.
    #[must_use]
    pub fn issued(&self, hosts: &[String]) -> Vec<u64> {
        let Some(f) = std::fs::read(self.dir.join(ISSUED))
            .ok()
            .and_then(|b| serde_json::from_slice::<IssuedFile>(&b).ok())
        else {
            return Vec::new();
        };
        if sorted(&f.hosts) != sorted(hosts) {
            return Vec::new();
        }
        let mut v = f.issued_unix;
        v.sort_unstable();
        v
    }

    /// Record the issue times for `hosts`, replacing any other names' record.
    ///
    /// # Errors
    /// I/O.
    pub fn save_issued(&self, hosts: &[String], issued_unix: &[u64]) -> io::Result<()> {
        let f = IssuedFile {
            hosts: sorted(hosts),
            issued_unix: issued_unix.to_vec(),
        };
        private_dir(&self.dir)?;
        write_private(
            &self.dir.join(ISSUED),
            &serde_json::to_vec_pretty(&f).map_err(io::Error::other)?,
        )
    }

    fn account_path(&self, directory: &str) -> PathBuf {
        let digest = ring::digest::digest(&ring::digest::SHA256, directory.as_bytes());
        let tag = digest.as_ref()[..6]
            .iter()
            .fold(String::new(), |mut acc, b| {
                use std::fmt::Write as _;
                let _ = write!(acc, "{b:02x}");
                acc
            });
        self.dir.join(format!("account-{tag}.json"))
    }
}

/// Renewal is due inside [`RENEW_BEFORE_DAYS`], or when the hostnames changed.
#[must_use]
pub fn needs_renewal(cert: &Cert, hosts: &[String], now: SystemTime) -> bool {
    if sorted(hosts) != cert.hosts {
        return true;
    }
    match cert.not_after.duration_since(now) {
        Ok(left) => left < Duration::from_secs(RENEW_BEFORE_DAYS * 86_400),
        Err(_) => true,
    }
}

// ---- a minimal DER reader, for notAfter -------------------------------------------

fn der_tlv(buf: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = buf.split_first()?;
    let (&l0, rest) = rest.split_first()?;
    let (len, rest) = if l0 < 0x80 {
        (usize::from(l0), rest)
    } else {
        let n = usize::from(l0 & 0x7F);
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let len = rest[..n]
            .iter()
            .fold(0usize, |acc, b| (acc << 8) | usize::from(*b));
        (len, &rest[n..])
    };
    if rest.len() < len {
        return None;
    }
    Some((tag, &rest[..len], &rest[len..]))
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn parse_time(tag: u8, v: &[u8]) -> Option<u64> {
    let s = std::str::from_utf8(v).ok()?;
    let s = s.strip_suffix('Z')?;
    let (year, rest) = match tag {
        0x17 => {
            let yy: i64 = s.get(..2)?.parse().ok()?;
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, s.get(2..)?)
        }
        0x18 => (s.get(..4)?.parse().ok()?, s.get(4..)?),
        _ => return None,
    };
    let num = |r: std::ops::Range<usize>| -> Option<u32> { rest.get(r)?.parse().ok() };
    let (mo, d, h, mi, se) = (num(0..2)?, num(2..4)?, num(4..6)?, num(6..8)?, num(8..10)?);
    let days = days_from_civil(year, mo, d);
    let secs = days * 86_400 + i64::from(h) * 3600 + i64::from(mi) * 60 + i64::from(se);
    u64::try_from(secs).ok()
}

/// `notAfter` of one DER certificate, in Unix seconds.
#[must_use]
pub fn der_not_after(der: &[u8]) -> Option<u64> {
    let (_, cert, _) = der_tlv(der)?;
    let (_, tbs, _) = der_tlv(cert)?;
    let mut rest = tbs;
    let (tag, _, r) = der_tlv(rest)?;
    if tag == 0xA0 {
        rest = r; // explicit version
    }
    // serial, signature algorithm, issuer
    for _ in 0..3 {
        rest = der_tlv(rest)?.2;
    }
    let (_, validity, _) = der_tlv(rest)?;
    let (_, _, after_nb) = der_tlv(validity)?;
    let (tag, v, _) = der_tlv(after_nb)?;
    parse_time(tag, v)
}

/// `notAfter` of the first certificate in a PEM chain.
#[must_use]
pub fn first_cert_not_after(chain_pem: &[u8]) -> Option<u64> {
    use rustls_pki_types::{CertificateDer, pem::PemObject as _};
    let der = CertificateDer::pem_slice_iter(chain_pem).next()?.ok()?;
    der_not_after(der.as_ref())
}

// ---- the account key ---------------------------------------------------------------

struct AccountKey {
    pkcs8: Vec<u8>,
    pair: EcdsaKeyPair,
    rng: SystemRandom,
}

impl AccountKey {
    fn generate() -> Result<Self, String> {
        let rng = SystemRandom::new();
        let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .map_err(|_| "cannot generate an account key".to_string())?;
        Self::from_pkcs8(doc.as_ref())
    }

    fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, String> {
        let rng = SystemRandom::new();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8, &rng)
            .map_err(|_| "the stored account key does not load".to_string())?;
        Ok(Self {
            pkcs8: pkcs8.to_vec(),
            pair,
            rng,
        })
    }

    fn jwk(&self) -> Value {
        let p = self.pair.public_key().as_ref(); // 0x04 || x || y
        json!({ "crv": "P-256", "kty": "EC", "x": b64(&p[1..33]), "y": b64(&p[33..65]) })
    }

    /// RFC 7638 thumbprint: SHA-256 over the required members in
    /// lexicographic order, no whitespace.
    fn thumbprint(&self) -> String {
        let p = self.pair.public_key().as_ref();
        let canonical = format!(
            r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
            b64(&p[1..33]),
            b64(&p[33..65])
        );
        b64(ring::digest::digest(&ring::digest::SHA256, canonical.as_bytes()).as_ref())
    }

    fn sign(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        self.pair
            .sign(&self.rng, data)
            .map(|s| s.as_ref().to_vec())
            .map_err(|_| "signing failed".to_string())
    }
}

/// The key authorisation for `token`, and the DNS-01 TXT value for it.
fn key_authorization(key: &AccountKey, token: &str) -> String {
    format!("{token}.{}", key.thumbprint())
}

/// `base64url(SHA-256(key authorisation))`, the `_acme-challenge` TXT value.
#[must_use]
pub fn dns01_value(key_authorization: &str) -> String {
    b64(ring::digest::digest(&ring::digest::SHA256, key_authorization.as_bytes()).as_ref())
}

// ---- challenges -----------------------------------------------------------------------

/// Writes and removes `_acme-challenge` TXT records.
pub trait Dns01Solver: Send + Sync {
    /// Create the TXT record `fqdn` = `value`.
    ///
    /// # Errors
    /// The provider's refusal, in words with no credential in them.
    fn present(&self, fqdn: &str, value: &str) -> Result<(), String>;
    /// Wait until every record presented is served by the zone's
    /// authoritative nameservers, so the CA's lookup will see it.
    ///
    /// # Errors
    /// Timed out, or the provider refused.
    fn wait_visible(&self, records: &[(String, String)]) -> Result<(), String>;
    /// Remove a TXT record [`Self::present`] created. Best effort; logged.
    fn cleanup(&self, fqdn: &str, value: &str);
}

/// How [`issue`] proves control of the names.
#[derive(Clone)]
pub enum Solver {
    Http01(ChallengeMap),
    Dns01(Arc<dyn Dns01Solver>),
}

impl Solver {
    fn kind(&self) -> &'static str {
        match self {
            Self::Http01(_) => "http-01",
            Self::Dns01(_) => "dns-01",
        }
    }
}

/// Undoes everything a run presented, whatever way the run ends.
struct Presented<'a> {
    solver: &'a Solver,
    http: Vec<String>,
    dns: Vec<(String, String)>,
}

impl Drop for Presented<'_> {
    fn drop(&mut self) {
        match self.solver {
            Solver::Http01(map) => {
                let mut m = map.lock().expect("challenges poisoned");
                for t in &self.http {
                    m.remove(t);
                }
            }
            Solver::Dns01(dns) => {
                for (fqdn, value) in &self.dns {
                    dns.cleanup(fqdn, value);
                }
            }
        }
    }
}

// ---- the protocol ----------------------------------------------------------------------

/// The ACME client: one CA directory, one HTTP agent.
pub struct AcmeClient {
    agent: ureq::Agent,
    directory_url: String,
}

struct Resp {
    status: u16,
    location: Option<String>,
    nonce: Option<String>,
    retry_after: Option<u64>,
    body: String,
}

fn read_resp(mut r: ureq::http::Response<ureq::Body>) -> Resp {
    let header = |r: &ureq::http::Response<ureq::Body>, n: &str| {
        r.headers()
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let status = r.status().as_u16();
    let location = header(&r, "Location");
    let nonce = header(&r, "Replay-Nonce");
    let retry_after = header(&r, "Retry-After").and_then(|v| v.trim().parse().ok());
    let body = r.body_mut().read_to_string().unwrap_or_default();
    Resp {
        status,
        location,
        nonce,
        retry_after,
        body,
    }
}

fn call_get(req: ureq::RequestBuilder<ureq::typestate::WithoutBody>) -> Result<Resp, String> {
    req.call()
        .map(read_resp)
        .map_err(|e| format!("cannot reach the CA: {e}"))
}

fn call_post(
    req: ureq::RequestBuilder<ureq::typestate::WithBody>,
    body: &str,
) -> Result<Resp, String> {
    req.send(body)
        .map(read_resp)
        .map_err(|e| format!("cannot reach the CA: {e}"))
}

/// The CA's own words from a problem document, never more than a line.
fn problem_text(r: &Resp) -> String {
    use std::fmt::Write as _;
    let v: Value = serde_json::from_str(&r.body).unwrap_or_default();
    let detail = v["detail"].as_str().unwrap_or("").trim();
    let kind = v["type"].as_str().unwrap_or("");
    let mut s = format!("HTTP {}", r.status);
    if !kind.is_empty() {
        let _ = write!(s, " {kind}");
    }
    if !detail.is_empty() {
        let _ = write!(s, ": {detail}");
    }
    s.chars().take(300).collect()
}

struct Session<'a> {
    client: &'a AcmeClient,
    key: AccountKey,
    kid: Option<String>,
    nonce: Option<String>,
    dir: Value,
}

impl Session<'_> {
    fn url(&self, name: &str) -> Result<String, String> {
        self.dir[name]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("the CA directory has no {name}"))
    }

    fn fresh_nonce(&mut self) -> Result<String, String> {
        if let Some(n) = self.nonce.take() {
            return Ok(n);
        }
        let url = self.url("newNonce")?;
        let r = call_get(self.client.agent.head(&url))?;
        r.nonce.ok_or_else(|| "the CA sent no nonce".to_string())
    }

    /// A JWS POST. `payload` `None` is POST-as-GET. Retries on a
    /// `badNonce` with the fresh nonce that came with it, as RFC 8555 §6.5
    /// expects clients to (Pebble rejects a share of good nonces on
    /// purpose to prove it).
    fn post(&mut self, url: &str, payload: Option<&Value>) -> Result<Resp, String> {
        const ATTEMPTS: usize = 5;
        for attempt in 0..ATTEMPTS {
            let nonce = self.fresh_nonce()?;
            let mut protected = json!({ "alg": "ES256", "nonce": nonce, "url": url });
            match &self.kid {
                Some(kid) => protected["kid"] = json!(kid),
                None => protected["jwk"] = self.key.jwk(),
            }
            let protected = b64(protected.to_string().as_bytes());
            let payload = payload.map_or_else(String::new, |p| b64(p.to_string().as_bytes()));
            let sig = self.key.sign(format!("{protected}.{payload}").as_bytes())?;
            let body =
                json!({ "protected": protected, "payload": payload, "signature": b64(&sig) });
            let r = call_post(
                self.client
                    .agent
                    .post(url)
                    .header("Content-Type", "application/jose+json"),
                &body.to_string(),
            )?;
            self.nonce.clone_from(&r.nonce);
            let bad_nonce =
                r.status == 400 && r.body.contains("urn:ietf:params:acme:error:badNonce");
            if bad_nonce && attempt + 1 < ATTEMPTS {
                continue;
            }
            return Ok(r);
        }
        unreachable!("the loop returns on its last pass")
    }

    fn post_ok(&mut self, url: &str, payload: Option<&Value>, what: &str) -> Result<Resp, String> {
        let r = self.post(url, payload)?;
        if (200..300).contains(&r.status) {
            Ok(r)
        } else {
            Err(format!("{what}: {}", problem_text(&r)))
        }
    }

    fn json(r: &Resp, what: &str) -> Result<Value, String> {
        serde_json::from_str(&r.body).map_err(|_| format!("{what}: the CA's answer is not JSON"))
    }

    /// POST-as-GET `url` until `status` leaves `pending`/`processing`.
    fn poll(&mut self, url: &str, what: &str, deadline: Instant) -> Result<Value, String> {
        loop {
            let r = self.post_ok(url, None, what)?;
            let v = Self::json(&r, what)?;
            match v["status"].as_str() {
                Some("pending" | "processing") => {}
                _ => return Ok(v),
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "{what}: still {} after {POLL_LIMIT:?}",
                    v["status"]
                ));
            }
            std::thread::sleep(Duration::from_secs(r.retry_after.unwrap_or(2).clamp(1, 10)));
        }
    }
}

impl AcmeClient {
    /// A client for `directory_url` that trusts the usual web roots.
    #[must_use]
    pub fn new(directory_url: &str) -> Self {
        Self {
            agent: ureq::Agent::config_builder()
                .proxy(None)
                .timeout_global(Some(Duration::from_secs(30)))
                .user_agent(concat!("astar-server/", env!("CARGO_PKG_VERSION")))
                .http_status_as_error(false)
                .build()
                .into(),
            directory_url: directory_url.to_string(),
        }
    }

    /// A client that trusts only `root_pem` — a test CA (Pebble).
    ///
    /// # Errors
    /// A PEM that holds no certificate.
    pub fn with_root(directory_url: &str, root_pem: &[u8]) -> Result<Self, String> {
        Ok(Self {
            agent: agent_trusting(root_pem)?,
            directory_url: directory_url.to_string(),
        })
    }

    #[must_use]
    pub fn directory_url(&self) -> &str {
        &self.directory_url
    }
}

/// A `ureq` agent whose only trust anchor is `root_pem`.
///
/// # Errors
/// A PEM with no certificate in it.
pub fn agent_trusting(root_pem: &[u8]) -> Result<ureq::Agent, String> {
    use rustls_pki_types::{CertificateDer, pem::PemObject as _};
    let mut roots = Vec::new();
    for c in CertificateDer::pem_slice_iter(root_pem) {
        let c = c.map_err(|e| format!("root PEM: {e}"))?;
        roots.push(ureq::tls::Certificate::from_der(c.as_ref()).to_owned());
    }
    if roots.is_empty() {
        return Err("root PEM holds no certificate".into());
    }
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(ureq::tls::RootCerts::Specific(Arc::new(roots)))
        .build();
    Ok(ureq::Agent::config_builder()
        .proxy(None)
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .tls_config(tls)
        .build()
        .into())
}

/// Issue one certificate for `hosts` with [`order`] and save it.
///
/// # Errors
/// As [`order`], or the store refusing the save.
pub fn issue(
    client: &AcmeClient,
    email: Option<&str>,
    hosts: &[String],
    solver: &Solver,
    store: &CertStore,
) -> Result<Cert, String> {
    let cert = order(client, email, hosts, solver, store)?;
    store
        .save(&cert)
        .map_err(|e| format!("{}: {e}", store.dir().display()))?;
    Ok(cert)
}

/// Order one certificate for `hosts` (all names on one SAN list) and return
/// it, **unsaved**: the caller keeps it whether or not the store then takes
/// it, because every certificate this returns counts against the CA's
/// weekly limit. `store` holds the ACME account. `email` is the optional
/// contact. Every challenge `solver` presents is removed before this
/// returns, whatever happens.
///
/// # Errors
/// A sentence for the admin page: the step that failed and the CA's words.
#[allow(
    clippy::too_many_lines,
    reason = "RFC 8555's flow in order, one step after another, reads best in one place"
)]
pub fn order(
    client: &AcmeClient,
    email: Option<&str>,
    hosts: &[String],
    solver: &Solver,
    store: &CertStore,
) -> Result<Cert, String> {
    if hosts.is_empty() {
        return Err("no hostnames to certify".into());
    }
    private_dir(store.dir()).map_err(|e| format!("{}: {e}", store.dir().display()))?;
    let dir: Value = {
        let r = call_get(client.agent.get(&client.directory_url))?;
        if r.status != 200 {
            return Err(format!("CA directory: HTTP {}", r.status));
        }
        serde_json::from_str(&r.body).map_err(|_| "CA directory is not JSON".to_string())?
    };

    // The account: reuse this CA's, or register a new one.
    let account_path = store.account_path(&client.directory_url);
    let stored: Option<AccountFile> = std::fs::read(&account_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let (key, kid) = match stored {
        Some(a) if a.directory == client.directory_url => {
            let pkcs8 = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(a.pkcs8)
                .map_err(|_| "the stored account key is damaged".to_string())?;
            (AccountKey::from_pkcs8(&pkcs8)?, a.kid)
        }
        _ => (AccountKey::generate()?, None),
    };
    let mut s = Session {
        client,
        key,
        kid: None,
        nonce: None,
        dir,
    };
    let kid = if let Some(kid) = kid {
        kid
    } else {
        let mut body = json!({ "termsOfServiceAgreed": true });
        if let Some(e) = email.filter(|e| !e.is_empty()) {
            body["contact"] = json!([format!("mailto:{e}")]);
        }
        let url = s.url("newAccount")?;
        let r = s.post_ok(&url, Some(&body), "new account")?;
        let kid = r
            .location
            .ok_or_else(|| "new account: the CA sent no account URL".to_string())?;
        let file = AccountFile {
            directory: client.directory_url.clone(),
            pkcs8: b64(&s.key.pkcs8),
            kid: Some(kid.clone()),
        };
        write_private(
            &account_path,
            &serde_json::to_vec_pretty(&file).unwrap_or_default(),
        )
        .map_err(|e| format!("{}: {e}", account_path.display()))?;
        tracing::info!(
            "acme: registered a new account with {}",
            client.directory_url
        );
        kid
    };
    s.kid = Some(kid);

    // The order.
    let identifiers: Vec<Value> = hosts
        .iter()
        .map(|h| json!({ "type": "dns", "value": h }))
        .collect();
    let url = s.url("newOrder")?;
    let r = s.post_ok(
        &url,
        Some(&json!({ "identifiers": identifiers })),
        "new order",
    )?;
    let order_url = r
        .location
        .clone()
        .ok_or_else(|| "new order: the CA sent no order URL".to_string())?;
    let order = Session::json(&r, "new order")?;
    let authz_urls: Vec<String> = order["authorizations"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|u| u.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let finalize = order["finalize"]
        .as_str()
        .ok_or_else(|| "new order: no finalize URL".to_string())?
        .to_string();

    // Present every challenge first, then wait once, then answer them all.
    let mut presented = Presented {
        solver,
        http: Vec::new(),
        dns: Vec::new(),
    };
    let mut to_answer: BTreeMap<String, String> = BTreeMap::new(); // authz → challenge url
    for authz_url in &authz_urls {
        let r = s.post_ok(authz_url, None, "authorization")?;
        let authz = Session::json(&r, "authorization")?;
        if authz["status"] == "valid" {
            continue;
        }
        let name = authz["identifier"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let ch = authz["challenges"]
            .as_array()
            .and_then(|cs| cs.iter().find(|c| c["type"] == solver.kind()))
            .ok_or_else(|| format!("{name}: the CA offers no {} challenge", solver.kind()))?;
        let token = ch["token"].as_str().unwrap_or_default().to_string();
        let ch_url = ch["url"].as_str().unwrap_or_default().to_string();
        if token.is_empty()
            || ch_url.is_empty()
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(format!("{name}: the CA sent a malformed challenge"));
        }
        let ka = key_authorization(&s.key, &token);
        match solver {
            Solver::Http01(map) => {
                map.lock()
                    .expect("challenges poisoned")
                    .insert(token.clone(), ka);
                presented.http.push(token);
            }
            Solver::Dns01(dns) => {
                let fqdn = format!("_acme-challenge.{}", name.trim_start_matches("*."));
                let value = dns01_value(&ka);
                // Registered first: a create that fails may still have
                // written the record, and the guard must remove it.
                presented.dns.push((fqdn.clone(), value.clone()));
                dns.present(&fqdn, &value)
                    .map_err(|e| format!("{name}: TXT record: {e}"))?;
            }
        }
        to_answer.insert(authz_url.clone(), ch_url);
    }
    if let Solver::Dns01(dns) = solver
        && !presented.dns.is_empty()
    {
        dns.wait_visible(&presented.dns)?;
    }
    for (authz_url, ch_url) in &to_answer {
        s.post_ok(ch_url, Some(&json!({})), "challenge")?;
        let authz = s.poll(authz_url, "authorization", Instant::now() + POLL_LIMIT)?;
        if authz["status"] != "valid" {
            let detail = authz["challenges"]
                .as_array()
                .and_then(|cs| cs.iter().find_map(|c| c["error"]["detail"].as_str()))
                .unwrap_or("no detail");
            let name = authz["identifier"]["value"].as_str().unwrap_or("?");
            return Err(format!(
                "{name}: {} validation failed: {}",
                solver.kind(),
                detail.chars().take(200).collect::<String>()
            ));
        }
    }

    // Finalise with a fresh key and CSR.
    let order = s.poll(&order_url, "order", Instant::now() + POLL_LIMIT)?;
    if order["status"] != "ready" && order["status"] != "valid" {
        return Err(format!("order is {}", order["status"]));
    }
    let cert_key = rcgen::KeyPair::generate().map_err(|e| format!("certificate key: {e}"))?;
    let params = rcgen::CertificateParams::new(hosts.to_vec()).map_err(|e| format!("CSR: {e}"))?;
    let csr = params
        .serialize_request(&cert_key)
        .map_err(|e| format!("CSR: {e}"))?;
    if order["status"] == "ready" {
        s.post_ok(
            &finalize,
            Some(&json!({ "csr": b64(csr.der()) })),
            "finalize",
        )?;
    }
    let order = s.poll(&order_url, "order", Instant::now() + POLL_LIMIT)?;
    let cert_url = order["certificate"]
        .as_str()
        .ok_or_else(|| format!("order is {} with no certificate", order["status"]))?
        .to_string();
    let r = s.post_ok(&cert_url, None, "certificate download")?;
    let chain_pem = r.body.into_bytes();
    let not_after = first_cert_not_after(&chain_pem)
        .ok_or_else(|| "the CA's certificate does not parse".to_string())?;
    drop(presented); // challenges removed before the certificate is handed back
    let cert = Cert {
        chain_pem,
        key_pem: cert_key.serialize_pem().into_bytes(),
        not_after: UNIX_EPOCH + Duration::from_secs(not_after),
        hosts: sorted(hosts),
    };
    tracing::info!(
        hosts = ?cert.hosts,
        not_after_unix = not_after,
        "acme: certificate issued"
    );
    Ok(cert)
}

/// Days left on `cert` at `now`, rounded down; 0 when expired.
#[must_use]
pub fn days_left(cert: &Cert, now: SystemTime) -> u64 {
    cert.not_after
        .duration_since(now)
        .map_or(0, |d| d.as_secs() / 86_400)
}

/// Seconds since the epoch, for the admin snapshot.
#[must_use]
pub fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The current Unix time; exposed so callers and tests agree.
#[must_use]
pub fn now() -> u64 {
    now_unix()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jose_base64url_is_unchanged_across_the_base64_bump() {
        // JOSE wants unpadded base64url; 0xfb 0xff 0xfe hit '-' and '_'.
        assert_eq!(b64(&[0xfb, 0xff, 0xfe, 0x01]), "-__-AQ");
        assert_eq!(b64(b"astar"), "YXN0YXI");
    }

    fn self_signed(hosts: &[&str], days: i64) -> Cert {
        let kp = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(
            hosts.iter().map(|h| (*h).to_string()).collect::<Vec<_>>(),
        )
        .unwrap();
        let now = SystemTime::now();
        let secs = now.duration_since(UNIX_EPOCH).unwrap().as_secs();
        let after = i64::try_from(secs).unwrap() + days * 86_400;
        params.not_after =
            rcgen::date_time_ymd(1970, 1, 1) + Duration::from_secs(u64::try_from(after).unwrap());
        let cert = params.self_signed(&kp).unwrap();
        Cert {
            chain_pem: cert.pem().into_bytes(),
            key_pem: kp.serialize_pem().into_bytes(),
            not_after: UNIX_EPOCH + Duration::from_secs(u64::try_from(after).unwrap()),
            hosts: sorted(&hosts.iter().map(|h| (*h).to_string()).collect::<Vec<_>>()),
        }
    }

    fn hosts(h: &[&str]) -> Vec<String> {
        h.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn renewal_due_inside_30_days_or_on_host_change() {
        let now = SystemTime::now();
        assert!(!needs_renewal(
            &self_signed(&["a.example.net"], 31),
            &hosts(&["a.example.net"]),
            now
        ));
        assert!(needs_renewal(
            &self_signed(&["a.example.net"], 29),
            &hosts(&["a.example.net"]),
            now
        ));
        assert!(needs_renewal(
            &self_signed(&["a.example.net"], -1),
            &hosts(&["a.example.net"]),
            now
        ));
        assert!(needs_renewal(
            &self_signed(&["a.example.net"], 80),
            &hosts(&["a.example.net", "b.example.net"]),
            now
        ));
    }

    #[test]
    fn not_after_is_read_from_the_der() {
        let c = self_signed(&["a.example.net"], 45);
        let parsed = first_cert_not_after(&c.chain_pem).unwrap();
        assert_eq!(parsed, unix(c.not_after));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2026, 10, 12), 20_738);
        assert_eq!(parse_time(0x17, b"261012000000Z"), Some(20_738 * 86_400));
        assert_eq!(
            parse_time(0x18, b"20261012000001Z"),
            Some(20_738 * 86_400 + 1)
        );
    }

    #[test]
    fn cert_store_round_trips_with_0600_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = CertStore::new(dir.path());
        let c = self_signed(&["b.example.net", "a.example.net"], 60);
        store.save(&c).unwrap();
        let back = store
            .load(&hosts(&["a.example.net", "b.example.net"]))
            .unwrap();
        assert_eq!(back.chain_pem, c.chain_pem);
        assert_eq!(back.key_pem, c.key_pem);
        assert_eq!(unix(back.not_after), unix(c.not_after));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.dir().join("cert-bundle.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
            let mode = std::fs::metadata(store.dir()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        assert!(
            !format!("{back:?}").contains("PRIVATE"),
            "Debug prints no key"
        );
    }

    /// What today's deployments have on disk: three files, saved one by one.
    fn save_legacy(store: &CertStore, chain_pem: &[u8], key_pem: &[u8], c: &Cert) {
        private_dir(store.dir()).unwrap();
        write_private(&store.dir().join("cert-key.pem"), key_pem).unwrap();
        write_private(&store.dir().join("cert.pem"), chain_pem).unwrap();
        let meta = json!({ "hosts": c.hosts, "not_after_unix": unix(c.not_after) });
        write_private(&store.dir().join("cert.json"), meta.to_string().as_bytes()).unwrap();
    }

    #[test]
    fn a_store_in_the_three_file_layout_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let store = CertStore::new(dir.path());
        let c = self_signed(&["a.example.net"], 60);
        save_legacy(&store, &c.chain_pem, &c.key_pem, &c);
        let back = store.load(&hosts(&["a.example.net"])).unwrap();
        assert_eq!(back.chain_pem, c.chain_pem);
        assert_eq!(back.key_pem, c.key_pem);
    }

    #[test]
    fn a_key_that_does_not_match_the_chain_is_no_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let store = CertStore::new(dir.path());
        let a = self_signed(&["a.example.net"], 60);
        let b = self_signed(&["a.example.net"], 60);
        // A save torn between its renames: the new key beside the old chain.
        save_legacy(&store, &a.chain_pem, &b.key_pem, &a);
        assert!(store.load(&hosts(&["a.example.net"])).is_none());
        let torn = Cert {
            key_pem: b.key_pem.clone(),
            ..a.clone()
        };
        let e = crate::public::tls::certified_key(&torn).unwrap_err();
        assert!(e.contains("does not match"), "{e}");
        // The same pair saved whole is refused on load just the same.
        store.save(&torn).unwrap();
        assert!(store.load(&hosts(&["a.example.net"])).is_none());
    }

    #[test]
    fn a_save_cut_short_leaves_the_previous_certificate_whole() {
        let dir = tempfile::tempdir().unwrap();
        let store = CertStore::new(dir.path());
        let a = self_signed(&["a.example.net"], 60);
        let b = self_signed(&["a.example.net"], 70);
        store.save(&a).unwrap();
        // Every temporary file a save could write is a directory, so the
        // save fails at whichever write comes after the first rename.
        for tmp in ["cert.tmp", "cert-bundle.tmp"] {
            std::fs::create_dir(store.dir().join(tmp)).unwrap();
        }
        let _ = store.save(&b);
        let back = store.load(&hosts(&["a.example.net"])).unwrap();
        let whole = |c: &Cert| back.chain_pem == c.chain_pem && back.key_pem == c.key_pem;
        assert!(whole(&a) || whole(&b), "a torn pair loaded");
        crate::public::tls::certified_key(&back).unwrap();

        // A crash before the one rename leaves a stray temporary file; the
        // certificate already in place is what loads.
        for tmp in ["cert.tmp", "cert-bundle.tmp"] {
            std::fs::remove_dir(store.dir().join(tmp)).unwrap();
        }
        store.save(&a).unwrap();
        std::fs::write(store.dir().join("cert-bundle.tmp"), b"{\"hosts\":[").unwrap();
        let back = store.load(&hosts(&["a.example.net"])).unwrap();
        assert_eq!(back.key_pem, a.key_pem);
        assert_eq!(back.chain_pem, a.chain_pem);
    }

    #[test]
    fn store_ignores_a_cert_for_other_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let store = CertStore::new(dir.path());
        store.save(&self_signed(&["a.example.net"], 60)).unwrap();
        assert!(store.load(&hosts(&["b.example.net"])).is_none());
        assert!(
            CertStore::new(&dir.path().join("none"))
                .load(&hosts(&["a"]))
                .is_none()
        );
    }

    #[test]
    fn the_jwk_thumbprint_and_dns01_value_follow_the_rfcs() {
        // RFC 8555 §8.4: TXT = base64url(SHA-256(key authorisation)).
        assert_eq!(
            dns01_value("token.thumb"),
            b64(ring::digest::digest(&ring::digest::SHA256, b"token.thumb").as_ref())
        );
        let k = AccountKey::generate().unwrap();
        let again = AccountKey::from_pkcs8(&k.pkcs8).unwrap();
        assert_eq!(k.thumbprint(), again.thumbprint());
        assert_eq!(k.thumbprint().len(), 43);
        assert_eq!(
            key_authorization(&k, "abc"),
            format!("abc.{}", k.thumbprint())
        );
        let sig = k.sign(b"x").unwrap();
        assert_eq!(sig.len(), 64, "ES256 is r||s, fixed width");
    }
}

#[cfg(test)]
#[path = "acme_pebble.rs"]
mod pebble;
