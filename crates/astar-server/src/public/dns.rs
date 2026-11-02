// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The DNS provider seam. ACME DNS-01 ([`Dns01`](crate::public::dns::Dns01)) and dynamic DNS
//! ([`Ddns`](crate::public::dns::Ddns)) talk only to [`DnsProvider`]; everything the security review
//! hardened lives above it, so every provider gets it: a TXT record is
//! registered for cleanup before it is created (`acme::Presented`), cleanup
//! runs on every exit path, and visibility is checked against the zone's
//! own nameservers (`dnsq`). Implementations: the built-in Porkbun client
//! (`porkbun.rs`) and, behind `dns-providers`, dns-update (`dns_update.rs`).
//!
//! Every [`DnsError`] message is scrubbed of credentials by the
//! implementation that made it ([`Scrub`]) before it leaves.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::public::acme::Dns01Solver;
use crate::public::dnsq;

/// What kind of failure a [`DnsError`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsErrorKind {
    /// The provider refused, or could not be reached.
    Failed,
    /// The provider cannot do this (list records, write AAAA, ...).
    Unsupported,
    /// No answer within the call's timeout.
    TimedOut,
}

/// A provider failure: one sentence with no credential in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsError {
    pub kind: DnsErrorKind,
    message: String,
}

impl DnsError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            kind: DnsErrorKind::Failed,
            message: message.into(),
        }
    }
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            kind: DnsErrorKind::Unsupported,
            message: message.into(),
        }
    }
    pub fn timed_out(message: impl Into<String>) -> Self {
        Self {
            kind: DnsErrorKind::TimedOut,
            message: message.into(),
        }
    }
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for DnsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DnsError {}

impl From<String> for DnsError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<DnsError> for String {
    fn from(e: DnsError) -> Self {
        e.message
    }
}

/// A TXT record this node created. `id` is the provider's record id when it
/// gave one; without it, removal goes by the exact value, never by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxtRecord {
    pub fqdn: String,
    pub value: String,
    pub id: Option<String>,
}

/// What one address write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressChange {
    Created,
    Updated,
    Unchanged,
    /// Written without knowing what was there (the provider cannot list).
    Set,
}

/// A DNS provider's API, synchronous. Names are absolute and inside
/// [`Self::zone`].
pub trait DnsProvider: Send + Sync {
    /// The zone this provider writes, e.g. `example.net`.
    fn zone(&self) -> &str;
    /// Make `name`'s A record (AAAA for IPv6, where supported) say `ip`.
    ///
    /// # Errors
    /// The provider refused; [`DnsErrorKind::Unsupported`] for a family it
    /// cannot write.
    fn set_address(&self, name: &str, ip: IpAddr) -> Result<AddressChange, DnsError>;
    /// Create the TXT record `fqdn` = `value`.
    ///
    /// # Errors
    /// The provider refused. The record may exist all the same.
    fn add_txt(&self, fqdn: &str, value: &str) -> Result<TxtRecord, DnsError>;
    /// Remove one TXT record: by id when it has one, else by exact value.
    /// Removing a record that is not there is not an error.
    ///
    /// # Errors
    /// The provider refused.
    fn remove_txt(&self, rec: &TxtRecord) -> Result<(), DnsError>;
    /// The TXT values the provider holds at `fqdn`.
    ///
    /// # Errors
    /// [`DnsErrorKind::Unsupported`] when the provider cannot list records.
    fn txt_values(&self, fqdn: &str) -> Result<Vec<String>, DnsError>;
    /// The zone's authoritative nameservers (host names), asked directly
    /// during DNS-01. Empty when unknown: DNS-01 then waits blind.
    fn nameservers(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Where this box's public IPv4 address comes from.
pub trait IpSource: Send + Sync {
    /// # Errors
    /// The source could not be asked or did not answer with an IPv4 address.
    fn public_ipv4(&self) -> Result<Ipv4Addr, DnsError>;
}

/// Removes credentials from text before it is logged or returned.
/// Never derive `Debug` on this: it holds the secrets.
pub struct Scrub(Vec<String>);

impl Scrub {
    /// Longest first, so a secret that contains another is removed whole.
    pub fn new<I: IntoIterator<Item = String>>(secrets: I) -> Self {
        let mut v: Vec<String> = secrets.into_iter().filter(|s| !s.is_empty()).collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.len()));
        Self(v)
    }

    /// `text` with every secret replaced and at most 300 characters.
    #[must_use]
    pub fn clean(&self, text: &str) -> String {
        let mut t = text.to_string();
        for k in &self.0 {
            t = t.replace(k.as_str(), "[redacted]");
        }
        t.chars().take(300).collect()
    }
}

/// `host` relative to `domain`: `""` for the apex, `None` outside it.
#[must_use]
pub fn subdomain<'a>(host: &'a str, domain: &str) -> Option<&'a str> {
    if host == domain {
        return Some("");
    }
    host.strip_suffix(domain)?.strip_suffix('.')
}

// ---- dynamic DNS -------------------------------------------------------------------

/// What one DDNS pass did to one hostname.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DdnsChange {
    Created(String, Ipv4Addr),
    Updated(String, Ipv4Addr),
    Unchanged(String),
    /// Written without knowing what was there (the provider cannot list).
    Set(String, Ipv4Addr),
}

/// One pass: ask `ip` for this box's IPv4 address, then make each
/// hostname's A record say it, touching nothing the provider reports as
/// already matching.
///
/// # Errors
/// The first failure, scrubbed by the provider.
pub fn ddns_once(
    provider: &dyn DnsProvider,
    ip: &dyn IpSource,
    hosts: &[String],
) -> Result<Vec<DdnsChange>, DnsError> {
    let v4 = ip.public_ipv4()?;
    let mut changes = Vec::with_capacity(hosts.len());
    for host in hosts {
        if subdomain(host, provider.zone()).is_none() {
            return Err(DnsError::new(format!(
                "{host} is not under {}",
                provider.zone()
            )));
        }
        let change = match provider.set_address(host, IpAddr::V4(v4))? {
            AddressChange::Created => DdnsChange::Created(host.clone(), v4),
            AddressChange::Updated => DdnsChange::Updated(host.clone(), v4),
            AddressChange::Unchanged => DdnsChange::Unchanged(host.clone()),
            AddressChange::Set => DdnsChange::Set(host.clone(), v4),
        };
        match &change {
            DdnsChange::Unchanged(_) => {}
            c => tracing::info!("ddns: {host} A {v4} ({c:?})"),
        }
        changes.push(change);
    }
    Ok(changes)
}

/// The DDNS thread: a pass every `interval`; after a failure, retry sooner
/// and back off (30 s doubling, never past `interval`).
pub struct Ddns {
    thread: Option<JoinHandle<()>>,
    last_error: Arc<Mutex<Option<String>>>,
}

impl Ddns {
    /// # Panics
    /// If the thread cannot be spawned.
    #[must_use]
    pub fn start(
        provider: Arc<dyn DnsProvider>,
        ip: Arc<dyn IpSource>,
        hosts: Vec<String>,
        interval: Duration,
        stop: Arc<AtomicBool>,
        report: Option<Arc<Mutex<crate::public::serve::PublicTlsStatus>>>,
    ) -> Self {
        let last_error = Arc::new(Mutex::new(None));
        let errors = Arc::clone(&last_error);
        let thread = std::thread::Builder::new()
            .name("public-ddns".into())
            .spawn(move || {
                let first_retry = Duration::from_secs(30).min(interval);
                let mut retry = first_retry;
                while !stop.load(Ordering::Relaxed) {
                    let wait = match ddns_once(&*provider, &*ip, &hosts) {
                        Ok(_) => {
                            *errors.lock().expect("ddns poisoned") = None;
                            if let Some(r) = &report {
                                r.lock().expect("status poisoned").dns_error = None;
                            }
                            retry = first_retry;
                            interval
                        }
                        Err(e) => {
                            let e = e.to_string();
                            tracing::warn!("ddns: {e}; retrying in {}s", retry.as_secs());
                            if let Some(r) = &report {
                                r.lock().expect("status poisoned").dns_error = Some(e.clone());
                            }
                            *errors.lock().expect("ddns poisoned") = Some(e);
                            let w = retry;
                            retry = (retry * 2).min(interval);
                            w
                        }
                    };
                    let until = Instant::now() + wait;
                    while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            })
            .expect("spawn the ddns thread");
        Self {
            thread: Some(thread),
            last_error,
        }
    }

    /// The last pass's failure, until a pass succeeds.
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().expect("ddns poisoned").clone()
    }

    /// Wait for the thread (after the shared stop flag is set).
    pub fn join(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---- DNS-01 --------------------------------------------------------------------------

/// How long DNS-01 waits for the nameservers before giving up.
pub const VISIBLE_TIMEOUT: Duration = Duration::from_secs(300);
/// The wait when no nameserver can be asked at all (UDP 53 blocked).
pub const BLIND_WAIT: Duration = Duration::from_secs(60);

/// DNS-01 through any [`DnsProvider`].
///
/// **How "visible" is decided.** First the provider is read back until every
/// value is stored (when it can list records). Then each of the zone's
/// authoritative nameservers is asked directly (no recursion, no cache)
/// until each one that answers serves every value; that is exactly the
/// lookup Let's Encrypt makes, so when it passes, validation will see the
/// record. A fixed sleep alone either wastes a minute or, on a slow zone
/// push, is too short; a resolver query can be answered from a stale
/// negative cache. Only when no nameserver answers at all (outbound UDP 53
/// blocked, or the provider does not name its nameservers) does it fall back
/// to a blind [`BLIND_WAIT`].
pub struct Dns01 {
    provider: Arc<dyn DnsProvider>,
    nameservers: Vec<SocketAddr>,
    resolve_nameservers: bool,
    timeout: Duration,
    blind_wait: Duration,
    created: Mutex<HashMap<(String, String), TxtRecord>>,
}

impl Dns01 {
    #[must_use]
    pub fn new(provider: Arc<dyn DnsProvider>) -> Self {
        Self {
            provider,
            nameservers: Vec::new(),
            resolve_nameservers: true,
            timeout: VISIBLE_TIMEOUT,
            blind_wait: BLIND_WAIT,
            created: Mutex::new(HashMap::new()),
        }
    }

    /// Ask these servers instead of the provider's (tests: a fake on 127.0.0.1).
    #[cfg(test)]
    pub(crate) fn with_nameservers(mut self, ns: Vec<SocketAddr>, timeout: Duration) -> Self {
        self.nameservers = ns;
        self.resolve_nameservers = false;
        self.timeout = timeout;
        self.blind_wait = Duration::ZERO;
        self
    }

    fn nameserver_addrs(&self) -> Vec<SocketAddr> {
        if !self.resolve_nameservers {
            return self.nameservers.clone();
        }
        self.provider
            .nameservers()
            .iter()
            .filter_map(|ns| (ns.as_str(), 53).to_socket_addrs().ok())
            .flat_map(|it| it.filter(SocketAddr::is_ipv4).take(1))
            .collect()
    }
}

impl Dns01Solver for Dns01 {
    fn present(&self, fqdn: &str, value: &str) -> Result<(), String> {
        let rec = self.provider.add_txt(fqdn, value)?;
        self.created
            .lock()
            .expect("dns01 poisoned")
            .insert((fqdn.to_string(), value.to_string()), rec);
        tracing::info!("dns-01: created TXT {fqdn}");
        Ok(())
    }

    fn wait_visible(&self, records: &[(String, String)]) -> Result<(), String> {
        let deadline = Instant::now() + self.timeout;
        // 1. The provider has stored every value (when it can say).
        'records: for (fqdn, value) in records {
            loop {
                match self.provider.txt_values(fqdn) {
                    Ok(vals) if vals.iter().any(|v| v == value) => continue 'records,
                    Ok(_) => {}
                    Err(e) if e.kind == DnsErrorKind::Unsupported => {
                        tracing::info!(
                            "dns-01: the provider cannot list records; asking the nameservers only"
                        );
                        break 'records;
                    }
                    Err(e) => return Err(e.into()),
                }
                if Instant::now() >= deadline {
                    return Err(format!("dns-01: the provider does not list TXT {fqdn} yet"));
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
        // 2. Every authoritative nameserver that answers serves them.
        let servers = self.nameserver_addrs();
        let mut answered = vec![false; servers.len()];
        loop {
            let mut all = true;
            for (i, ns) in servers.iter().enumerate() {
                for (fqdn, value) in records {
                    match dnsq::query_txt(*ns, fqdn, Duration::from_secs(3)) {
                        Ok(vals) => {
                            answered[i] = true;
                            all &= vals.iter().any(|v| v == value);
                        }
                        // A server that has answered before and now does
                        // not is not done; one that never answers is
                        // ignored (unreachable from here).
                        Err(_) => {
                            if answered[i] {
                                all = false;
                            }
                        }
                    }
                }
            }
            if !answered.iter().any(|a| *a) {
                tracing::warn!(
                    "dns-01: no nameserver answered over UDP 53; waiting {}s instead",
                    self.blind_wait.as_secs()
                );
                std::thread::sleep(self.blind_wait);
                return Ok(());
            }
            if all {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "dns-01: the nameservers are not serving the TXT records after {}s",
                    self.timeout.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    }

    fn cleanup(&self, fqdn: &str, value: &str) {
        let rec = self
            .created
            .lock()
            .expect("dns01 poisoned")
            .remove(&(fqdn.to_string(), value.to_string()))
            // Not ours by record (an add whose answer was lost or refused):
            // remove it by its exact value, never by name alone.
            .unwrap_or_else(|| TxtRecord {
                fqdn: fqdn.to_string(),
                value: value.to_string(),
                id: None,
            });
        match self.provider.remove_txt(&rec) {
            Ok(()) => tracing::info!("dns-01: removed TXT {fqdn}"),
            Err(e) => tracing::warn!("dns-01: TXT {fqdn} was not removed: {e}"),
        }
    }
}

// ---- the IP source over a URL --------------------------------------------------------

/// The agent refuses any plain-HTTP request, so a redirect to `http://`
/// cannot deliver the address either (ureq checks every hop, redirects
/// included).
const HTTPS_ONLY: bool = true;

/// This box's public IPv4 address from an https URL that answers with it
/// as plain text (for providers that, unlike Porkbun, cannot say).
pub struct UrlIpSource {
    agent: ureq::Agent,
    url: String,
    /// scheme + host + path: no userinfo, no query. The only form of the
    /// URL that may appear in an error.
    shown: String,
    /// Removes the full URL, its query and any userinfo from error text
    /// (ureq's Display repeats the URL).
    scrub: Scrub,
}

impl UrlIpSource {
    /// # Errors
    /// Not an `https://` URL: a plain-HTTP answer could be anyone's.
    pub fn new(url: &str) -> Result<Self, String> {
        if !url.starts_with("https://") || url.len() <= "https://".len() {
            let shown = Self::build(url, true).shown;
            return Err(format!("ip_source {shown:?} is not an https:// URL"));
        }
        Ok(Self::build(url, HTTPS_ONLY))
    }

    #[cfg(test)]
    pub(crate) fn for_test(url: &str) -> Self {
        Self::build(url, false)
    }

    /// The agent `new` builds, pointed at any URL.
    #[cfg(test)]
    pub(crate) fn for_test_strict(url: &str) -> Self {
        Self::build(url, HTTPS_ONLY)
    }

    fn build(url: &str, https_only: bool) -> Self {
        let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
        let (authority_path, query) = match rest.split_once('?') {
            Some((a, q)) => (a, Some(q)),
            None => (rest, None),
        };
        let authority_path = authority_path.split('#').next().unwrap_or("");
        let (authority, path) = match authority_path.find('/') {
            Some(i) => authority_path.split_at(i),
            None => (authority_path, ""),
        };
        let (userinfo, host) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, authority),
        };
        let mut secrets = vec![url.to_string()];
        let withheld = crate::config::raw_delimiter_in_userinfo(rest);
        if withheld {
            // The split above cut a password in pieces; hide every piece.
            secrets.extend(rest.split(['/', '?', '#', '@']).map(str::to_string));
        }
        if let Some(q) = query {
            secrets.push(q.split('#').next().unwrap_or(q).to_string());
        }
        if let Some(u) = userinfo {
            secrets.push(u.to_string());
            secrets.extend(u.split(':').map(str::to_string));
        }
        Self {
            shown: if withheld {
                format!("{scheme}://(URL withheld)")
            } else {
                format!("{scheme}://{host}{path}")
            },
            scrub: Scrub::new(secrets),
            agent: ureq::Agent::config_builder()
                .proxy(None)
                .https_only(https_only)
                .timeout_global(Some(Duration::from_secs(15)))
                .user_agent(concat!("astar-server/", env!("CARGO_PKG_VERSION")))
                .http_status_as_error(true)
                .build()
                .into(),
            url: url.to_string(),
        }
    }
}

/// A ureq error in words that never contain the URL: ureq's own `Display`
/// prints its parsed, percent-encoded copy of it, which a scrub of the
/// configured string cannot be relied on to match.
fn ureq_kind(e: &ureq::Error) -> String {
    use ureq::Error as E;
    match e {
        E::StatusCode(n) => format!("the server answered status {n}"),
        E::Timeout(_) => "timed out".into(),
        E::HostNotFound => "host not found".into(),
        E::ConnectionFailed => "connection failed".into(),
        E::TooManyRedirects => "too many redirects".into(),
        E::RedirectFailed => "a redirect could not be followed".into(),
        E::RequireHttpsOnly(_) => "refused: not https".into(),
        E::BadUri(_) => "not a usable URL".into(),
        E::Tls(_) | E::Rustls(_) | E::Pem(_) => "TLS failure".into(),
        E::Io(io) => format!("I/O error ({:?})", io.kind()),
        _ => "request failed".into(),
    }
}

impl UrlIpSource {
    fn err(&self, text: &str) -> DnsError {
        DnsError::new(format!(
            "ip_source {}: {}",
            self.shown,
            self.scrub.clean(text)
        ))
    }
}

impl IpSource for UrlIpSource {
    fn public_ipv4(&self) -> Result<Ipv4Addr, DnsError> {
        use std::io::Read as _;
        let resp = self
            .agent
            .get(&self.url)
            .call()
            .map_err(|e| self.err(&ureq_kind(&e)))?;
        let mut text = String::new();
        // 64 bytes is far more than any address; a longer answer is refused.
        resp.into_body()
            .into_reader()
            .take(64)
            .read_to_string(&mut text)
            .map_err(|e| self.err(&format!("cannot read the answer ({:?})", e.kind())))?;
        text.trim().parse::<Ipv4Addr>().map_err(|_| {
            DnsError::new(format!(
                "ip_source {}: the answer is not an IPv4 address",
                self.shown
            ))
        })
    }
}

// ---- from [web.dns] -------------------------------------------------------------------

/// The provider and IP source `[web.dns]` asks for.
pub struct DnsSetup {
    pub provider: Arc<dyn DnsProvider>,
    pub ip: Option<Arc<dyn IpSource>>,
}

/// # Errors
/// Missing credentials, or a provider this build does not have.
pub fn from_config(
    dns: &crate::config::DnsCfg,
    secrets: &crate::secrets::SecretProvider,
) -> Result<DnsSetup, String> {
    use crate::config::{DnsProviderKind, IpSourceCfg};
    let url_ip = match dns.ip_source() {
        IpSourceCfg::Url(u) => Some(Arc::new(UrlIpSource::new(u)?) as Arc<dyn IpSource>),
        IpSourceCfg::Provider => None,
    };
    match dns.provider {
        DnsProviderKind::Porkbun => {
            let client = Arc::new(crate::public::porkbun::Client::from_config(dns, secrets)?);
            Ok(DnsSetup {
                ip: Some(url_ip.unwrap_or_else(|| Arc::clone(&client) as Arc<dyn IpSource>)),
                provider: client,
            })
        }
        #[cfg(feature = "dns-providers")]
        DnsProviderKind::Other(name) => Ok(DnsSetup {
            provider: Arc::new(crate::public::dns_update::DnsUpdateProvider::from_config(
                name, dns, secrets,
            )?),
            ip: url_ip,
        }),
        #[cfg(not(feature = "dns-providers"))]
        DnsProviderKind::Other(name) => Err(format!(
            "\"{name}\" needs a build with the dns-providers feature (the release build has it)"
        )),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A provider that remembers, and can be made to fail or to be unable
    /// to list.
    #[derive(Default)]
    pub(crate) struct MemProvider {
        pub(crate) txt: Mutex<Vec<(String, String)>>,
        pub(crate) a: Mutex<Vec<(String, Ipv4Addr)>>,
        pub(crate) cannot_list: bool,
        pub(crate) fail_add: bool,
    }

    impl DnsProvider for MemProvider {
        fn zone(&self) -> &'static str {
            "example.net"
        }
        fn set_address(&self, name: &str, ip: IpAddr) -> Result<AddressChange, DnsError> {
            let IpAddr::V4(ip) = ip else {
                return Err(DnsError::unsupported("v4 only"));
            };
            let mut a = self.a.lock().unwrap();
            match a.iter_mut().find(|(n, _)| n == name) {
                Some((_, old)) if *old == ip => Ok(AddressChange::Unchanged),
                Some((_, old)) => {
                    *old = ip;
                    Ok(AddressChange::Updated)
                }
                None => {
                    a.push((name.to_string(), ip));
                    Ok(AddressChange::Created)
                }
            }
        }
        fn add_txt(&self, fqdn: &str, value: &str) -> Result<TxtRecord, DnsError> {
            self.txt.lock().unwrap().push((fqdn.into(), value.into()));
            if self.fail_add {
                return Err(DnsError::new("refused after writing"));
            }
            Ok(TxtRecord {
                fqdn: fqdn.into(),
                value: value.into(),
                id: None,
            })
        }
        fn remove_txt(&self, rec: &TxtRecord) -> Result<(), DnsError> {
            self.txt
                .lock()
                .unwrap()
                .retain(|(f, v)| !(f == &rec.fqdn && v == &rec.value));
            Ok(())
        }
        fn txt_values(&self, fqdn: &str) -> Result<Vec<String>, DnsError> {
            if self.cannot_list {
                return Err(DnsError::unsupported("cannot list"));
            }
            Ok(self
                .txt
                .lock()
                .unwrap()
                .iter()
                .filter(|(f, _)| f == fqdn)
                .map(|(_, v)| v.clone())
                .collect())
        }
    }

    struct FixedIp(Ipv4Addr);
    impl IpSource for FixedIp {
        fn public_ipv4(&self) -> Result<Ipv4Addr, DnsError> {
            Ok(self.0)
        }
    }

    #[test]
    fn ddns_writes_each_host_and_refuses_one_outside_the_zone() {
        let p = MemProvider::default();
        let ip = FixedIp("203.0.113.9".parse().unwrap());
        let hosts = vec!["a.example.net".to_string(), "example.net".to_string()];
        let first = ddns_once(&p, &ip, &hosts).unwrap();
        assert!(
            matches!(first[0], DdnsChange::Created(..))
                && matches!(first[1], DdnsChange::Created(..))
        );
        let again = ddns_once(&p, &ip, &hosts).unwrap();
        assert!(again.iter().all(|c| matches!(c, DdnsChange::Unchanged(_))));
        let e = ddns_once(&p, &ip, &["a.example.org".to_string()]).unwrap_err();
        assert!(e.message().contains("not under example.net"), "{e}");
    }

    #[test]
    fn a_dns01_run_whose_provider_times_out_still_cleans_up() {
        use crate::public::acme::Dns01Solver;
        struct Slow(MemProvider);
        impl DnsProvider for Slow {
            fn zone(&self) -> &str {
                self.0.zone()
            }
            fn set_address(&self, n: &str, ip: IpAddr) -> Result<AddressChange, DnsError> {
                self.0.set_address(n, ip)
            }
            fn add_txt(&self, f: &str, v: &str) -> Result<TxtRecord, DnsError> {
                self.0.add_txt(f, v)?;
                Err(DnsError::timed_out("add TXT: no answer in 60s"))
            }
            fn remove_txt(&self, r: &TxtRecord) -> Result<(), DnsError> {
                self.0.remove_txt(r)
            }
            fn txt_values(&self, f: &str) -> Result<Vec<String>, DnsError> {
                self.0.txt_values(f)
            }
        }
        let p = Arc::new(Slow(MemProvider::default()));
        let d = Dns01::new(Arc::clone(&p) as Arc<dyn DnsProvider>);
        assert!(d.present("_acme-challenge.a.example.net", "v1").is_err());
        d.cleanup("_acme-challenge.a.example.net", "v1");
        assert!(p.0.txt.lock().unwrap().is_empty());
    }

    #[test]
    fn dns01_waits_on_the_nameservers_alone_when_the_provider_cannot_list() {
        use crate::public::acme::Dns01Solver;
        let p = Arc::new(MemProvider {
            cannot_list: true,
            ..MemProvider::default()
        });
        let d = Dns01::new(Arc::clone(&p) as Arc<dyn DnsProvider>)
            .with_nameservers(Vec::new(), Duration::from_secs(1));
        d.present("_acme-challenge.a.example.net", "v1").unwrap();
        d.wait_visible(&[("_acme-challenge.a.example.net".into(), "v1".into())])
            .unwrap();
        d.cleanup("_acme-challenge.a.example.net", "v1");
        assert!(p.txt.lock().unwrap().is_empty());
    }

    #[test]
    fn a_txt_whose_add_reported_failure_is_still_removed_by_value() {
        use crate::public::acme::Dns01Solver;
        let p = Arc::new(MemProvider {
            fail_add: true,
            ..MemProvider::default()
        });
        let d = Dns01::new(Arc::clone(&p) as Arc<dyn DnsProvider>);
        assert!(d.present("_acme-challenge.a.example.net", "v1").is_err());
        d.cleanup("_acme-challenge.a.example.net", "v1");
        assert!(p.txt.lock().unwrap().is_empty());
    }

    #[test]
    fn the_url_ip_source_reads_a_bounded_ipv4_answer() {
        use std::io::{Read as _, Write as _};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let answers: Vec<String> = vec![
            "203.0.113.7\n".into(),
            "not an address".into(),
            "9".repeat(10_000),
            "2001:db8::1".into(),
        ];
        let h = std::thread::spawn(move || {
            for body in answers {
                let (mut s, _) = l.accept().unwrap();
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        let src = UrlIpSource::for_test(&format!("http://{addr}/ip"));
        assert_eq!(src.public_ipv4().unwrap().to_string(), "203.0.113.7");
        for _ in 0..3 {
            assert!(src.public_ipv4().is_err());
        }
        h.join().unwrap();
    }

    #[test]
    fn the_strict_agent_refuses_plain_http_even_for_a_good_answer() {
        use std::io::{Read as _, Write as _};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            // Either no connection is made, or it gets a good answer that
            // must not be accepted.
            l.set_nonblocking(true).unwrap();
            for _ in 0..50 {
                if let Ok((mut s, _)) = l.accept() {
                    let _ = s.set_nonblocking(false);
                    let mut buf = [0u8; 1024];
                    let _ = s.read(&mut buf);
                    let _ = write!(
                        s,
                        "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n203.0.113.7"
                    );
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let src = UrlIpSource::for_test_strict(&format!("http://{addr}/ip"));
        let e = src.public_ipv4().unwrap_err().to_string();
        assert!(e.contains("not https"), "refused for the right reason: {e}");
        h.join().unwrap();
    }

    #[cfg(feature = "dns-providers")]
    #[test]
    fn a_dns_update_provider_comes_from_config() {
        let dns: crate::config::DnsCfg = toml::from_str(
            "provider = \"cloudflare\"\ndomain = \"example.net\"\nip_source = \"https://ip.example.org/\"\n",
        )
        .unwrap();
        let secrets = crate::secrets::SecretProvider::new();
        secrets.put("dns:cloudflare", "tok-from-config");
        let setup = from_config(&dns, &secrets).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(setup.provider.zone(), "example.net");
        assert!(setup.ip.is_some(), "the URL is the IP source");
    }

    #[cfg(feature = "dns-providers")]
    #[test]
    fn a_dns_update_provider_without_its_token_is_refused() {
        let dns: crate::config::DnsCfg =
            toml::from_str("provider = \"cloudflare\"\ndomain = \"example.net\"\n").unwrap();
        let Err(e) = from_config(&dns, &crate::secrets::SecretProvider::new()) else {
            panic!("no token must refuse");
        };
        assert!(e.contains("dns:cloudflare"), "{e}");
    }

    #[test]
    fn ip_source_errors_never_carry_the_query_or_userinfo() {
        for url in [
            "https://127.0.0.1:1/ip?token=SENTINEL",
            "https://user:SENTINEL@127.0.0.1:1/ip",
            // Reserved characters: ureq prints its percent-encoded form of
            // the URL, which a scrub of the raw string would miss.
            "https://127.0.0.1:1/ip?token=SEN%20TI+NEL&k=SEN TI/NEL",
            "https://us er:SEN TI+NEL@127.0.0.1:1/ip?token=SEN%20TI+NEL",
        ] {
            let src = UrlIpSource::new(url).unwrap();
            let e = src.public_ipv4().unwrap_err().to_string();
            for frag in [
                "SENTINEL", "SEN%20TI", "SEN TI", "SEN+TI", "TI+NEL", "TI%20NEL", "us er",
                "us%20er",
            ] {
                assert!(!e.contains(frag), "{frag}: {e}");
            }
            assert!(e.contains("127.0.0.1:1/ip"), "{e}");
        }
        // A raw `/`, `?` or `#` in the userinfo splits the password out of
        // the authority: the URL is withheld whole, not half-shown.
        for url in [
            "https://user:SENTINEL/TAIL@127.0.0.1:1/ip",
            "https://user:SENTINEL?TAIL@127.0.0.1:1/ip",
            "https://user:SENTINEL#TAIL@127.0.0.1:1/ip",
        ] {
            let src = UrlIpSource::new(url).unwrap();
            let e = src.public_ipv4().unwrap_err().to_string();
            for frag in ["SENTINEL", "TAIL", "user"] {
                assert!(!e.contains(frag), "{url}: {frag}: {e}");
            }
            assert!(e.contains("(URL withheld)"), "{url}: {e}");
        }
        let e = UrlIpSource::new("http://h/ip?token=SENTINEL")
            .err()
            .unwrap();
        assert!(!e.contains("SENTINEL"), "{e}");
    }

    #[test]
    fn a_url_ip_source_must_be_https() {
        assert!(UrlIpSource::new("http://example.net/ip").is_err());
        assert!(UrlIpSource::new("https://example.net/ip").is_ok());
    }
}
