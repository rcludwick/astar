// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Porkbun's JSON API v3: dynamic DNS for the public hostnames, and the
//! `_acme-challenge` TXT records for ACME DNS-01.
//!
//! Every call is checked against Porkbun's documentation,
//! <https://porkbun.com/api/json/v3/documentation> (full reference:
//! <https://porkbun.com/llms-full.txt>, DNS: <https://porkbun.com/llms/dns>),
//! as read on 2026-10-12:
//!
//! * Base URL `https://api.porkbun.com/api/json/v3`. Writes are `POST` with
//!   a JSON body; credentials go in that body as `apikey` and
//!   `secretapikey` (header auth also exists; the body is used here).
//! * `POST /ping` returns `yourIp`, "the caller's public IP address". The
//!   `api-ipv4.porkbun.com` host forces an IPv4 answer, which is what an A
//!   record needs, so the ping goes there.
//! * `POST /dns/retrieveByNameType/{domain}/{type}/{subdomain}` →
//!   `records[]` with `id`, `name` (the full hostname), `type`, `content`,
//!   `ttl` (`id` and `ttl` are strings).
//! * `POST /dns/editByNameType/{domain}/{type}/{subdomain}` with `content`
//!   (required) and `ttl` replaces the content of every match.
//! * `POST /dns/create/{domain}` with `name` (the subdomain, blank for the
//!   apex), `type`, `content`, `ttl` → `id`, "the numeric ID of the newly
//!   created record" (a string).
//! * `POST /dns/delete/{domain}/{id}` deletes one record by id. The TXT
//!   cleanup uses it rather than `deleteByNameType`, which would also take
//!   any TXT record the operator keeps at the same name.
//! * Errors: `status: "ERROR"` with `code` and `message`; HTTP 400/403/429
//!   (`Retry-After` on 429). `API_ACCESS_DISABLED` means the domain has not
//!   been opted in to API access in the account settings.
//! * The TTL minimum "is determined by account settings (typically 600)".
//! * A write to a domain not delegated to Porkbun's nameservers succeeds
//!   with a `warnings` entry; those are logged as written.
//!
//! The API keys never reach a log line, an error, or a snapshot: every
//! error sentence passes through [`crate::public::dns::Scrub`] before it leaves this module.

use std::net::Ipv4Addr;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::DnsCfg;
use crate::secrets::SecretProvider;

/// Porkbun's API.
pub const API_BASE: &str = "https://api.porkbun.com/api/json/v3";
/// The same API on an IPv4-only host: `/ping` answers with the IPv4 address.
pub const API_IPV4_BASE: &str = "https://api-ipv4.porkbun.com/api/json/v3";
/// Porkbun's authoritative nameservers, asked directly during DNS-01.
pub const NAMESERVERS: &[&str] = &[
    "curitiba.ns.porkbun.com",
    "fortaleza.ns.porkbun.com",
    "maceio.ns.porkbun.com",
    "salvador.ns.porkbun.com",
];
use crate::secrets::{
    PORKBUN_PK_ENV as ENV_API_KEY, PORKBUN_PK_KEY as API_KEY, PORKBUN_SK_ENV as ENV_SECRET_API_KEY,
    PORKBUN_SK_KEY as SECRET_API_KEY,
};

/// The two keys. `Debug` prints neither.
#[derive(Clone)]
pub struct Credentials {
    api_key: String,
    secret_api_key: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials(..)")
    }
}

impl Credentials {
    /// Both keys from the provider, or the names of what is missing.
    ///
    /// # Errors
    /// One or both unset: names only.
    pub fn from_provider(secrets: &SecretProvider) -> Result<Self, String> {
        let api_key = secrets.resolve(API_KEY);
        let secret_api_key = secrets.resolve(SECRET_API_KEY);
        let missing: Vec<&str> = [(API_KEY, &api_key), (SECRET_API_KEY, &secret_api_key)]
            .iter()
            .filter(|(_, v)| v.is_empty())
            .map(|(k, _)| *k)
            .collect();
        if missing.is_empty() {
            Ok(Self {
                api_key,
                secret_api_key,
            })
        } else {
            Err(format!(
                "Porkbun credentials not set: {} (env {ENV_API_KEY} / {ENV_SECRET_API_KEY})",
                missing.join(", ")
            ))
        }
    }
}

/// One DNS record as Porkbun returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Empty when Porkbun sent no usable id (see [`record_id`]).
    pub id: String,
    pub content: String,
}

/// A record id fit to put in a delete URL: digits or letters, as a string
/// or a number. A missing, null or odd id is `None`, never `"null"`.
fn record_id(v: &Value) -> Option<String> {
    let id = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(id)
}

/// A client for one zone.
pub struct Client {
    agent: ureq::Agent,
    base: String,
    ipv4_base: String,
    creds: Credentials,
    domain: String,
    ttl: u32,
    scrub: crate::public::dns::Scrub,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("porkbun::Client")
            .field("domain", &self.domain)
            .finish_non_exhaustive()
    }
}

pub use crate::public::dns::subdomain;

impl Client {
    /// A client for `[web.dns]`'s zone with the credentials from `secrets`.
    ///
    /// # Errors
    /// The credentials are not set.
    pub fn from_config(dns: &DnsCfg, secrets: &SecretProvider) -> Result<Self, String> {
        Ok(Self::with_bases(
            API_BASE,
            API_IPV4_BASE,
            Credentials::from_provider(secrets)?,
            &dns.domain,
            dns.ttl,
        ))
    }

    fn with_bases(base: &str, ipv4_base: &str, creds: Credentials, domain: &str, ttl: u32) -> Self {
        let scrub =
            crate::public::dns::Scrub::new([creds.api_key.clone(), creds.secret_api_key.clone()]);
        Self {
            agent: ureq::Agent::config_builder()
                .proxy(None)
                .timeout_global(Some(Duration::from_secs(30)))
                .user_agent(concat!("astar-server/", env!("CARGO_PKG_VERSION")))
                .http_status_as_error(false)
                .build()
                .into(),
            base: base.trim_end_matches('/').to_string(),
            ipv4_base: ipv4_base.trim_end_matches('/').to_string(),
            creds,
            domain: domain.to_string(),
            ttl,
            scrub,
        }
    }

    /// A client against a fake server. Test builds only: production always
    /// talks to [`API_BASE`].
    #[cfg(test)]
    pub(crate) fn for_test(base: &str, api_key: &str, secret: &str, domain: &str) -> Self {
        Self::with_bases(
            base,
            base,
            Credentials {
                api_key: api_key.into(),
                secret_api_key: secret.into(),
            },
            domain,
            600,
        )
    }

    #[must_use]
    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// POST `body` plus the credentials to `base + path`; the JSON answer,
    /// or a scrubbed sentence.
    fn call(&self, base: &str, path: &str, mut body: Value) -> Result<Value, String> {
        let scrub = &self.scrub;
        body["apikey"] = json!(self.creds.api_key);
        body["secretapikey"] = json!(self.creds.secret_api_key);
        let url = format!("{base}{path}");
        let mut resp = match self
            .agent
            .post(&url)
            .header("Content-Type", "application/json")
            .send(body.to_string())
        {
            Ok(r) => r,
            Err(e) => return Err(scrub.clean(&format!("cannot reach Porkbun: {e}"))),
        };
        let http = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or_default();
        if v["status"] == "SUCCESS" {
            if let Some(w) = v["warnings"].as_array() {
                for w in w.iter().filter_map(Value::as_str) {
                    tracing::warn!("porkbun: {}", scrub.clean(w));
                }
            }
            return Ok(v);
        }
        let code = v["code"].as_str().unwrap_or("");
        let message = v["message"].as_str().unwrap_or("no message");
        let hint = if code == "API_ACCESS_DISABLED" {
            " (turn on API Access for this domain in Porkbun's domain management)"
        } else {
            ""
        };
        Err(scrub.clean(&format!(
            "Porkbun {path}: HTTP {http} {code}: {message}{hint}"
        )))
    }

    /// This box's public IPv4 address, as Porkbun sees it.
    ///
    /// # Errors
    /// The call failed or the answer is not an IPv4 address.
    pub fn ping_ipv4(&self) -> Result<Ipv4Addr, String> {
        let v = self.call(&self.ipv4_base, "/ping", json!({}))?;
        v["yourIp"]
            .as_str()
            .and_then(|s| s.trim().parse().ok())
            .ok_or_else(|| "Porkbun /ping: yourIp is not an IPv4 address".to_string())
    }

    /// Records of `rtype` at `sub` (`""` = apex).
    ///
    /// # Errors
    /// The call failed.
    pub fn retrieve(&self, rtype: &str, sub: &str) -> Result<Vec<Record>, String> {
        let v = self.call(
            &self.base,
            &format!("/dns/retrieveByNameType/{}/{rtype}/{sub}", self.domain),
            json!({}),
        )?;
        Ok(v["records"]
            .as_array()
            .map(|rs| {
                rs.iter()
                    .map(|r| Record {
                        id: record_id(&r["id"]).unwrap_or_default(),
                        content: r["content"].as_str().unwrap_or_default().to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Create a record; its id, or `None` when Porkbun's answer carries no
    /// usable one (the record may still exist).
    ///
    /// # Errors
    /// The call failed. The record may have been created all the same.
    pub fn create(&self, rtype: &str, sub: &str, content: &str) -> Result<Option<String>, String> {
        let v = self.call(
            &self.base,
            &format!("/dns/create/{}", self.domain),
            json!({ "name": sub, "type": rtype, "content": content, "ttl": self.ttl }),
        )?;
        Ok(record_id(&v["id"]))
    }

    /// Set the content of every `rtype` record at `sub`.
    ///
    /// # Errors
    /// The call failed.
    pub fn edit_by_name_type(&self, rtype: &str, sub: &str, content: &str) -> Result<(), String> {
        self.call(
            &self.base,
            &format!("/dns/editByNameType/{}/{rtype}/{sub}", self.domain),
            json!({ "content": content, "ttl": self.ttl }),
        )
        .map(|_| ())
    }

    /// Delete one record by id.
    ///
    /// # Errors
    /// The call failed.
    pub fn delete(&self, id: &str) -> Result<(), String> {
        self.call(
            &self.base,
            &format!("/dns/delete/{}/{id}", self.domain),
            json!({}),
        )
        .map(|_| ())
    }
}

use crate::public::dns::{AddressChange, DnsError, DnsProvider, IpSource, TxtRecord};

impl Client {
    fn sub<'a>(&self, fqdn: &'a str) -> Result<&'a str, DnsError> {
        subdomain(fqdn, &self.domain)
            .ok_or_else(|| DnsError::new(format!("{fqdn} is not under {}", self.domain)))
    }
}

impl DnsProvider for Client {
    fn zone(&self) -> &str {
        &self.domain
    }

    fn set_address(&self, name: &str, ip: std::net::IpAddr) -> Result<AddressChange, DnsError> {
        let std::net::IpAddr::V4(ip) = ip else {
            return Err(DnsError::unsupported(
                "Porkbun: only A (IPv4) records are written",
            ));
        };
        let sub = self.sub(name)?;
        let want = ip.to_string();
        let records = self.retrieve("A", sub)?;
        if records.is_empty() {
            self.create("A", sub, &want)?;
            Ok(AddressChange::Created)
        } else if records.iter().any(|r| r.content != want) {
            self.edit_by_name_type("A", sub, &want)?;
            Ok(AddressChange::Updated)
        } else {
            Ok(AddressChange::Unchanged)
        }
    }

    fn add_txt(&self, fqdn: &str, value: &str) -> Result<TxtRecord, DnsError> {
        let id = self.create("TXT", self.sub(fqdn)?, value)?;
        Ok(TxtRecord {
            fqdn: fqdn.to_string(),
            value: value.to_string(),
            id,
        })
    }

    fn remove_txt(&self, rec: &TxtRecord) -> Result<(), DnsError> {
        let ids: Vec<String> = match &rec.id {
            Some(id) => vec![id.clone()],
            // Not ours by id: find it by its exact value, never by name alone.
            None => self
                .retrieve("TXT", self.sub(&rec.fqdn)?)?
                .into_iter()
                .filter(|r| r.content == rec.value && !r.id.is_empty())
                .map(|r| r.id)
                .collect(),
        };
        // Try every id; a record left behind is worse than a second error.
        let mut first_err = None;
        for id in ids {
            if let Err(e) = self.delete(&id) {
                first_err.get_or_insert(e);
            }
        }
        first_err.map_or(Ok(()), |e| Err(e.into()))
    }

    fn txt_values(&self, fqdn: &str) -> Result<Vec<String>, DnsError> {
        Ok(self
            .retrieve("TXT", self.sub(fqdn)?)?
            .into_iter()
            .map(|r| r.content)
            .collect())
    }

    fn nameservers(&self) -> Vec<String> {
        NAMESERVERS.iter().map(|s| (*s).to_string()).collect()
    }
}

impl IpSource for Client {
    fn public_ipv4(&self) -> Result<std::net::Ipv4Addr, DnsError> {
        Ok(self.ping_ipv4()?)
    }
}

#[cfg(test)]
#[path = "porkbun_tests.rs"]
pub(crate) mod tests;
