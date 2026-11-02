// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Every DNS provider dns-update drives, behind `dns-providers`.
//!
//! dns-update is async (tokio, reqwest, hickory). The rest of astar-server
//! is threads, so this is contained the way astar-discord is: ONE thread
//! owns a current-thread runtime and takes jobs over `std::sync::mpsc`.
//! Every call has a timeout ([`CALL_TIMEOUT`]); a job that panics is caught
//! and the thread carries on; every error is scrubbed of the token.
//! Nothing here is reached in a build without the feature.

use std::future::Future;
use std::net::IpAddr;
use std::sync::mpsc;
use std::time::Duration;

use dns_update::{DnsRecord, DnsRecordType, DnsUpdater};

use crate::config::{DNS_UPDATE_PROVIDERS, DnsCfg};
use crate::public::dns::{AddressChange, DnsError, DnsProvider, Scrub, TxtRecord};
use crate::secrets::SecretProvider;

/// The most one provider call may take, retries inside dns-update included.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// dns-update's own HTTP timeout, under [`CALL_TIMEOUT`].
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

type Job = Box<dyn FnOnce(&tokio::runtime::Runtime) + Send>;

/// The thread that owns the runtime. Dropping it ends the thread.
pub struct Worker {
    tx: mpsc::Sender<Job>,
    timeout: Duration,
}

impl Worker {
    /// # Errors
    /// The thread or the runtime could not be started.
    pub fn start(timeout: Duration) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        std::thread::Builder::new()
            .name("dns-update".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => {
                        let _ = ready_tx.send(Ok(()));
                        rt
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("dns-update runtime: {e}")));
                        return;
                    }
                };
                // Jobs run strictly in submission order. That is what keeps
                // cleanup safe: a late `add_txt` is always followed, on this
                // same queue, by the Presented guard's `remove_txt`, which
                // removes by exact value. Never skip a job because its
                // caller's deadline passed, and any future deadline must
                // never apply to `remove_txt`.
                for job in rx {
                    // A provider bug must not take the worker with it: the
                    // caller sees its reply channel close, and the next job runs.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&rt)));
                }
            })
            .map_err(|e| format!("cannot start the dns-update thread: {e}"))?;
        ready_rx
            .recv()
            .map_err(|_| "the dns-update thread did not start".to_string())??;
        Ok(Self { tx, timeout })
    }

    /// Run the future `make` builds on the worker; its answer, or a
    /// timeout. The future is built on the worker, so it need not be `Send`.
    ///
    /// # Errors
    /// The future's own error; [`DnsError::timed_out`]; the worker gone.
    pub fn call<T, Fut, F>(&self, what: &'static str, make: F) -> Result<T, DnsError>
    where
        T: Send + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, DnsError>> + 'static,
    {
        let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<T, DnsError>>(1);
        let timeout = self.timeout;
        let job: Job = Box::new(move |rt| {
            let out = rt
                .block_on(async move { tokio::time::timeout(timeout, make()).await })
                .unwrap_or_else(|_| {
                    Err(DnsError::timed_out(format!(
                        "{what}: no answer from the DNS provider in {}s",
                        timeout.as_secs()
                    )))
                });
            let _ = reply_tx.send(out);
        });
        self.tx
            .send(job)
            .map_err(|_| DnsError::new(format!("{what}: the dns-update thread has stopped")))?;
        match reply_rx.recv_timeout(timeout + Duration::from_secs(5)) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(DnsError::timed_out(format!(
                "{what}: the dns-update thread is busy past {}s",
                timeout.as_secs()
            ))),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(DnsError::new(format!(
                "{what}: the DNS provider call failed inside dns-update"
            ))),
        }
    }
}

/// One dns-update provider for one zone.
pub struct DnsUpdateProvider {
    worker: Worker,
    updater: DnsUpdater,
    zone: String,
    ttl: u32,
    scrub: std::sync::Arc<Scrub>,
}

/// The zone and TTL only: the client holds the token, so it is never shown.
impl std::fmt::Debug for DnsUpdateProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DnsUpdateProvider")
            .field("zone", &self.zone)
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

/// The dns-update client for `name`, from its one token.
fn build_updater(name: &str, token: &str) -> dns_update::Result<DnsUpdater> {
    let t = Some(HTTP_TIMEOUT);
    match name {
        "cloudflare" => DnsUpdater::new_cloudflare(token, t),
        "digitalocean" => DnsUpdater::new_digitalocean(token, t),
        "desec" => DnsUpdater::new_desec(token, t),
        "hetzner" => DnsUpdater::new_hetzner(token, t),
        "linode" => DnsUpdater::new_linode(token, t),
        "gandi" => DnsUpdater::new_gandiv5(token, t),
        "vultr" => DnsUpdater::new_vultr(token, t),
        "ionos" => DnsUpdater::new_ionos(token, t),
        "bunny" => DnsUpdater::new_bunny(token, t),
        "gcore" => DnsUpdater::new_gcore(token, t),
        other => Err(dns_update::Error::Unsupported(format!(
            "no constructor for {other}"
        ))),
    }
}

impl DnsUpdateProvider {
    /// # Errors
    /// The token is not set, or the client cannot be built.
    pub fn from_config(
        name: &'static str,
        dns: &DnsCfg,
        secrets: &SecretProvider,
    ) -> Result<Self, String> {
        let spec = DNS_UPDATE_PROVIDERS
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("unknown DNS provider \"{name}\""))?;
        let token = secrets.resolve(spec.credential);
        if token.is_empty() {
            return Err(format!(
                "{} not set: {} (env {})",
                spec.title, spec.credential, spec.env
            ));
        }
        let scrub = Scrub::new([token.clone()]);
        let updater =
            build_updater(name, &token).map_err(|e| scrub.clean(&format!("{name}: {e}")))?;
        Self::build(updater, &dns.domain, dns.ttl, scrub, CALL_TIMEOUT)
    }

    #[cfg(test)]
    pub(crate) fn with_updater(
        updater: DnsUpdater,
        zone: &str,
        ttl: u32,
        scrub: Scrub,
        timeout: Duration,
    ) -> Result<Self, String> {
        Self::build(updater, zone, ttl, scrub, timeout)
    }

    fn build(
        updater: DnsUpdater,
        zone: &str,
        ttl: u32,
        scrub: Scrub,
        timeout: Duration,
    ) -> Result<Self, String> {
        Ok(Self {
            worker: Worker::start(timeout)?,
            updater,
            zone: zone.to_string(),
            ttl,
            scrub: std::sync::Arc::new(scrub),
        })
    }

    /// A dns-update error as a scrubbed [`DnsError`].
    #[cfg(test)]
    pub(crate) fn scrubbed(&self, what: &str, e: &dns_update::Error) -> DnsError {
        to_dns_error(&self.scrub, what, e)
    }
}

/// A dns-update error, scrubbed, keeping "cannot do this" apart.
fn to_dns_error(scrub: &Scrub, what: &str, e: &dns_update::Error) -> DnsError {
    let text = scrub.clean(&format!("{what}: {e}"));
    match e {
        dns_update::Error::Unsupported(_) => DnsError::unsupported(text),
        _ => DnsError::new(text),
    }
}

/// A list that finds nothing: some providers answer a missing record
/// with `NotFound` instead of an empty list.
fn empty_if_not_found(r: dns_update::Result<Vec<DnsRecord>>) -> dns_update::Result<Vec<DnsRecord>> {
    match r {
        Err(dns_update::Error::NotFound) => Ok(Vec::new()),
        other => other,
    }
}

/// Map a dns-update result inside a job (no `self` there).
fn map_err<T>(scrub: &Scrub, what: &str, r: dns_update::Result<T>) -> Result<T, DnsError> {
    r.map_err(|e| to_dns_error(scrub, what, &e))
}

impl DnsUpdateProvider {
    /// What every job needs, cloned so it can move onto the worker.
    fn job_parts(&self) -> (DnsUpdater, std::sync::Arc<Scrub>, String) {
        (
            self.updater.clone(),
            std::sync::Arc::clone(&self.scrub),
            self.zone.clone(),
        )
    }
}

impl DnsProvider for DnsUpdateProvider {
    fn zone(&self) -> &str {
        &self.zone
    }

    fn set_address(&self, name: &str, ip: IpAddr) -> Result<AddressChange, DnsError> {
        let (rtype, record) = match ip {
            IpAddr::V4(v4) => (DnsRecordType::A, DnsRecord::A(v4)),
            IpAddr::V6(v6) => (DnsRecordType::AAAA, DnsRecord::AAAA(v6)),
        };
        let (updater, scrub, zone) = self.job_parts();
        let name = name.to_string();
        let ttl = self.ttl;
        self.worker.call("set address", move || async move {
            // Read first, so an unchanged address costs no write; a provider
            // that cannot list is written every pass.
            let before = match empty_if_not_found(updater.list_rrset(&name, rtype, &zone).await) {
                Ok(records) => Some(records),
                Err(dns_update::Error::Unsupported(_)) => None,
                Err(e) => return Err(to_dns_error(&scrub, "list", &e)),
            };
            if before.as_deref() == Some(std::slice::from_ref(&record)) {
                return Ok(AddressChange::Unchanged);
            }
            map_err(
                &scrub,
                "set",
                updater
                    .set_rrset(&name, rtype, ttl, vec![record], &zone)
                    .await,
            )?;
            Ok(match before {
                None => AddressChange::Set,
                Some(records) if records.is_empty() => AddressChange::Created,
                Some(_) => AddressChange::Updated,
            })
        })
    }

    fn add_txt(&self, fqdn: &str, value: &str) -> Result<TxtRecord, DnsError> {
        let (updater, scrub, zone) = self.job_parts();
        let fqdn = fqdn.to_string();
        let value = value.to_string();
        let ttl = self.ttl;
        self.worker.call("add TXT", move || async move {
            let records = vec![DnsRecord::TXT(value.clone())];
            map_err(
                &scrub,
                "add TXT",
                updater
                    .add_to_rrset(&fqdn, DnsRecordType::TXT, ttl, records, &zone)
                    .await,
            )?;
            Ok(TxtRecord {
                fqdn,
                value,
                id: None,
            })
        })
    }

    fn remove_txt(&self, rec: &TxtRecord) -> Result<(), DnsError> {
        let (updater, scrub, zone) = self.job_parts();
        let rec = rec.clone();
        self.worker.call("remove TXT", move || async move {
            let records = vec![DnsRecord::TXT(rec.value.clone())];
            map_err(
                &scrub,
                "remove TXT",
                updater
                    .remove_from_rrset(&rec.fqdn, DnsRecordType::TXT, records, &zone)
                    .await,
            )
        })
    }

    fn txt_values(&self, fqdn: &str) -> Result<Vec<String>, DnsError> {
        let (updater, scrub, zone) = self.job_parts();
        let fqdn = fqdn.to_string();
        self.worker.call("list TXT", move || async move {
            let records = map_err(
                &scrub,
                "list TXT",
                empty_if_not_found(updater.list_rrset(&fqdn, DnsRecordType::TXT, &zone).await),
            )?;
            Ok(records
                .into_iter()
                .filter_map(|r| match r {
                    DnsRecord::TXT(t) => Some(t),
                    _ => None,
                })
                .collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_not_found_list_is_an_empty_list_and_other_errors_stay() {
        assert_eq!(
            empty_if_not_found(Err(dns_update::Error::NotFound)).unwrap(),
            Vec::<DnsRecord>::new()
        );
        assert!(empty_if_not_found(Err(dns_update::Error::Unauthorized)).is_err());
        let one = vec![DnsRecord::TXT("x".into())];
        assert_eq!(empty_if_not_found(Ok(one.clone())).unwrap(), one);
    }

    #[test]
    fn a_call_returns_its_value() {
        let w = Worker::start(Duration::from_secs(2)).unwrap();
        assert_eq!(w.call("add", || async { Ok(2 + 2) }).unwrap(), 4);
    }

    #[test]
    fn a_call_that_never_finishes_times_out() {
        let w = Worker::start(Duration::from_millis(200)).unwrap();
        let t = std::time::Instant::now();
        let e = w
            .call("hang", || async {
                std::future::pending::<()>().await;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(e.kind, crate::public::dns::DnsErrorKind::TimedOut);
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        // The worker still answers afterwards.
        assert_eq!(w.call("after", || async { Ok(1) }).unwrap(), 1);
    }

    #[test]
    fn a_panicking_call_leaves_the_worker_running() {
        let w = Worker::start(Duration::from_secs(2)).unwrap();
        let e = w
            .call("boom", || async {
                let bug = true;
                assert!(!bug, "provider bug");
                Ok(())
            })
            .unwrap_err();
        assert!(e.message().contains("boom"), "{e}");
        assert_eq!(w.call("after", || async { Ok(7) }).unwrap(), 7);
    }

    #[test]
    fn a_token_in_a_provider_error_is_redacted() {
        let p = DnsUpdateProvider::with_updater(
            dns_update::DnsUpdater::new_in_memory(std::sync::Arc::default()),
            "example.net",
            600,
            Scrub::new(["tok-SENTINEL-123".to_string()]),
            Duration::from_secs(2),
        )
        .unwrap();
        let e = p.scrubbed(
            "cloudflare",
            &dns_update::Error::Api("bad token tok-SENTINEL-123".into()),
        );
        assert!(!e.message().contains("tok-SENTINEL-123"), "{e}");
        assert!(e.message().contains("[redacted]"));
    }

    #[test]
    fn records_go_through_the_worker_to_dns_update() {
        let store = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let p = DnsUpdateProvider::with_updater(
            dns_update::DnsUpdater::new_in_memory(std::sync::Arc::clone(&store)),
            "example.net",
            600,
            Scrub::new([]),
            Duration::from_secs(2),
        )
        .unwrap();
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        // The in-memory provider cannot list, so the write is blind.
        assert_eq!(
            p.set_address("69586.example.net", ip).unwrap(),
            AddressChange::Set
        );
        let rec = p
            .add_txt("_acme-challenge.69586.example.net", "token-value")
            .unwrap();
        assert_eq!(store.lock().unwrap().len(), 2);
        p.remove_txt(&rec).unwrap();
        let left = store.lock().unwrap();
        assert_eq!(left.len(), 1, "only the A record is left");
        assert_eq!(left[0].record, DnsRecord::A("203.0.113.7".parse().unwrap()));
        drop(left);
        assert_eq!(
            p.txt_values("_acme-challenge.69586.example.net")
                .unwrap_err()
                .kind,
            crate::public::dns::DnsErrorKind::Unsupported
        );
    }

    #[test]
    fn every_listed_provider_has_a_client() {
        for p in DNS_UPDATE_PROVIDERS {
            assert!(
                build_updater(p.name, "prefix.secret").is_ok(),
                "{} is listed but has no constructor",
                p.name
            );
        }
    }

    #[test]
    fn missing_credentials_name_the_key_and_the_env_var() {
        let dns: crate::config::DnsCfg =
            toml::from_str("provider = \"cloudflare\"\ndomain = \"example.net\"\n").unwrap();
        let e = DnsUpdateProvider::from_config(
            "cloudflare",
            &dns,
            &crate::secrets::SecretProvider::new(),
        )
        .unwrap_err();
        assert!(
            e.contains("dns:cloudflare") && e.contains("ASTAR_DNS_CLOUDFLARE_TOKEN"),
            "{e}"
        );
    }
}
