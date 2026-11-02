// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The Porkbun client against a fake Porkbun on 127.0.0.1, and DNS-01's
//! nameserver check against a fake DNS server on 127.0.0.1. Nothing here
//! reaches Porkbun or any real DNS.

use super::*;
use astar_http::Response;
use astar_http::testing::{Stub, serve};
use std::sync::{Arc, Mutex};

use crate::public::acme::Dns01Solver;
use crate::public::dns::{DdnsChange, Dns01, DnsError};
use crate::public::dnsq::fake::FakeDns;

/// One DDNS pass with the client as both the provider and the IP source.
fn ddns_once(c: &Client, hosts: &[String]) -> Result<Vec<DdnsChange>, DnsError> {
    crate::public::dns::ddns_once(c, c, hosts)
}

const KEY: &str = "pk1_SENTINELapikey0001";
const SECRET: &str = "sk1_SENTINELsecret0002";

#[derive(Default)]
struct Zone {
    /// (id, sub, type, content)
    records: Vec<(u64, String, String, String)>,
    next_id: u64,
    /// (path, body) of every call
    calls: Vec<(String, Value)>,
    ip: String,
    /// Answer every call with an error that echoes the request body.
    fail: bool,
    /// How `/dns/create` answers once it has stored the record.
    create_reply: CreateReply,
    /// Ids whose `/dns/delete` answers with an error and keeps the record.
    fail_delete: Vec<u64>,
}

/// A create that succeeded at Porkbun but whose answer did not make it back.
#[derive(Default, Clone, Copy)]
enum CreateReply {
    #[default]
    Id,
    NullId,
    NoId,
    Lost,
}

pub(crate) struct FakePorkbun {
    base: String,
    zone: Arc<Mutex<Zone>>,
    /// Stops the fake when the `FakePorkbun` drops.
    _stub: Stub,
}

fn respond(v: &Value) -> Response {
    Response::json(200, v.to_string())
}

impl FakePorkbun {
    fn start(ip: &str) -> Self {
        let zone = Arc::new(Mutex::new(Zone {
            next_id: 1000,
            ip: ip.into(),
            ..Zone::default()
        }));
        let z = Arc::clone(&zone);
        let stub = serve(move |req| {
            let body: Value = serde_json::from_str(req.body_str()).unwrap_or_default();
            let path = req.target.clone();
            let mut zone = z.lock().unwrap();
            zone.calls.push((path.clone(), body.clone()));
            let authed = body["apikey"] == KEY && body["secretapikey"] == SECRET;
            if zone.fail || !authed {
                let echo = body.to_string();
                drop(zone);
                return respond(&json!({ "status": "ERROR", "code": "INVALID_API_KEYS_001",
                              "message": format!("Invalid API key. (you sent {echo})") }));
            }
            let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
            let answer = match parts.as_slice() {
                ["ping"] => json!({ "status": "SUCCESS", "yourIp": zone.ip }),
                ["dns", "retrieveByNameType", "example.net", t, sub @ ..] => {
                    let sub = sub.first().copied().unwrap_or("");
                    let recs: Vec<Value> = zone
                        .records
                        .iter()
                        .filter(|r| r.1 == sub && r.2 == *t)
                        .map(|r| {
                            let name = if r.1.is_empty() {
                                "example.net".to_string()
                            } else {
                                format!("{}.example.net", r.1)
                            };
                            json!({ "id": r.0.to_string(), "name": name, "type": r.2,
                                    "content": r.3, "ttl": "600", "prio": null })
                        })
                        .collect();
                    json!({ "status": "SUCCESS", "cloudflare": "disabled", "records": recs })
                }
                ["dns", "create", "example.net"] => {
                    zone.next_id += 1;
                    let id = zone.next_id;
                    let rec = (
                        id,
                        body["name"].as_str().unwrap_or("").to_string(),
                        body["type"].as_str().unwrap_or("").to_string(),
                        body["content"].as_str().unwrap_or("").to_string(),
                    );
                    zone.records.push(rec);
                    match zone.create_reply {
                        CreateReply::Id => json!({ "status": "SUCCESS", "id": id.to_string() }),
                        CreateReply::NullId => json!({ "status": "SUCCESS", "id": null }),
                        CreateReply::NoId => json!({ "status": "SUCCESS" }),
                        CreateReply::Lost => {
                            drop(zone);
                            return Response::new(502);
                        }
                    }
                }
                ["dns", "editByNameType", "example.net", t, sub @ ..] => {
                    let sub = sub.first().copied().unwrap_or("").to_string();
                    let content = body["content"].as_str().unwrap_or("").to_string();
                    for r in zone.records.iter_mut().filter(|r| r.1 == sub && r.2 == *t) {
                        r.3.clone_from(&content);
                    }
                    json!({ "status": "SUCCESS" })
                }
                ["dns", "delete", "example.net", id] => {
                    let id: u64 = id.parse().unwrap_or(0);
                    let failing = zone.fail_delete.contains(&id);
                    zone.records.retain(|r| failing || r.0 != id);
                    json!({ "status": if failing { "ERROR" } else { "SUCCESS" } })
                }
                _ => {
                    json!({ "status": "ERROR", "code": "NOT_FOUND", "message": "no such path" })
                }
            };
            drop(zone);
            respond(&answer)
        });
        Self {
            base: stub.url(),
            zone,
            _stub: stub,
        }
    }

    fn client(&self) -> Client {
        Client::for_test(&self.base, KEY, SECRET, "example.net")
    }

    fn txt_records(&self, sub: &str) -> Vec<String> {
        self.zone
            .lock()
            .unwrap()
            .records
            .iter()
            .filter(|r| r.1 == sub && r.2 == "TXT")
            .map(|r| r.3.clone())
            .collect()
    }

    fn a_records(&self, sub: &str) -> Vec<String> {
        self.zone
            .lock()
            .unwrap()
            .records
            .iter()
            .filter(|r| r.1 == sub && r.2 == "A")
            .map(|r| r.3.clone())
            .collect()
    }

    fn writes(&self) -> Vec<String> {
        self.zone
            .lock()
            .unwrap()
            .calls
            .iter()
            .map(|c| c.0.clone())
            .filter(|p| !p.contains("retrieve") && p != "/ping")
            .collect()
    }
}

/// A fake Porkbun and a client for it, for the cross-provider contract.
pub(crate) fn fake_porkbun(ip: &str) -> (FakePorkbun, Client) {
    let pb = FakePorkbun::start(ip);
    let c = pb.client();
    (pb, c)
}

impl crate::public::dns_contract::ZoneView for FakePorkbun {
    fn txt(&self, fqdn: &str) -> Vec<String> {
        crate::public::dns::subdomain(fqdn, "example.net")
            .map(|sub| self.txt_records(sub))
            .unwrap_or_default()
    }
    fn a(&self, name: &str) -> Vec<String> {
        crate::public::dns::subdomain(name, "example.net")
            .map(|sub| self.a_records(sub))
            .unwrap_or_default()
    }
}

fn hosts(h: &[&str]) -> Vec<String> {
    h.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn every_call_carries_both_keys_in_the_body() {
    let pb = FakePorkbun::start("203.0.113.9");
    ddns_once(&pb.client(), &hosts(&["69586.example.net"])).unwrap();
    let zone = pb.zone.lock().unwrap();
    assert_ne!(
        zone.calls,
        [] as [(std::string::String, serde_json::Value); 0]
    );
    for (path, body) in &zone.calls {
        assert_eq!(body["apikey"], KEY, "{path}");
        assert_eq!(body["secretapikey"], SECRET, "{path}");
    }
}

#[test]
fn ddns_creates_a_missing_record_then_leaves_it_alone() {
    let pb = FakePorkbun::start("203.0.113.9");
    let c = pb.client();
    let ip: Ipv4Addr = "203.0.113.9".parse().unwrap();
    assert_eq!(
        ddns_once(&c, &hosts(&["69586.example.net", "example.net"])).unwrap(),
        [
            DdnsChange::Created("69586.example.net".into(), ip),
            DdnsChange::Created("example.net".into(), ip),
        ]
    );
    assert_eq!(pb.a_records("69586"), ["203.0.113.9"]);
    assert_eq!(pb.a_records(""), ["203.0.113.9"]);
    let writes_before = pb.writes().len();
    assert_eq!(
        ddns_once(&c, &hosts(&["69586.example.net"])).unwrap(),
        [DdnsChange::Unchanged("69586.example.net".into())]
    );
    assert_eq!(
        pb.writes().len(),
        writes_before,
        "a matching record is not written"
    );
}

#[test]
fn ddns_edits_a_record_that_points_elsewhere() {
    let pb = FakePorkbun::start("198.51.100.20");
    pb.zone
        .lock()
        .unwrap()
        .records
        .push((7, "69586".into(), "A".into(), "192.0.2.1".into()));
    let changes = ddns_once(&pb.client(), &hosts(&["69586.example.net"])).unwrap();
    assert_eq!(
        changes,
        [DdnsChange::Updated(
            "69586.example.net".into(),
            "198.51.100.20".parse().unwrap()
        )]
    );
    assert_eq!(pb.a_records("69586"), ["198.51.100.20"]);
    assert_eq!(pb.writes(), ["/dns/editByNameType/example.net/A/69586"]);
}

#[test]
fn ddns_refuses_an_ipv6_answer_and_a_name_outside_the_zone() {
    let pb = FakePorkbun::start("2001:db8::1");
    let e = ddns_once(&pb.client(), &hosts(&["69586.example.net"])).unwrap_err();
    assert!(e.message().contains("not an IPv4"), "{e}");
    let pb = FakePorkbun::start("203.0.113.9");
    let e = ddns_once(&pb.client(), &hosts(&["node.example.org"])).unwrap_err();
    assert!(e.message().contains("not under example.net"), "{e}");
    assert_eq!(pb.writes(), [] as [std::string::String; 0]);
}

#[test]
fn txt_records_are_created_seen_and_deleted_by_id() {
    let pb = FakePorkbun::start("203.0.113.9");
    let dns = FakeDns::start();
    // An operator's own TXT record at the same name must survive cleanup.
    pb.zone.lock().unwrap().records.push((
        5,
        "_acme-challenge.69586".into(),
        "TXT".into(),
        "operator-owned".into(),
    ));
    let solver = Dns01::new(Arc::new(pb.client()) as Arc<dyn DnsProvider>)
        .with_nameservers(vec![dns.addr], Duration::from_secs(10));
    let fqdn = "_acme-challenge.69586.example.net";
    solver.present(fqdn, "value-1").unwrap();
    // The nameserver has it already: wait_visible returns.
    dns.records
        .lock()
        .unwrap()
        .insert(fqdn.into(), vec!["operator-owned".into(), "value-1".into()]);
    solver
        .wait_visible(&[(fqdn.to_string(), "value-1".to_string())])
        .unwrap();
    solver.cleanup(fqdn, "value-1");
    let left: Vec<String> = pb
        .zone
        .lock()
        .unwrap()
        .records
        .iter()
        .map(|r| r.3.clone())
        .collect();
    assert_eq!(left, ["operator-owned"]);
    let writes = pb.writes();
    assert_eq!(writes[0], "/dns/create/example.net");
    assert!(
        writes[1].starts_with("/dns/delete/example.net/"),
        "{writes:?}"
    );
}

#[test]
fn a_record_the_nameservers_never_serve_times_out_and_cleanup_still_deletes_it() {
    let pb = FakePorkbun::start("203.0.113.9");
    let dns = FakeDns::start(); // never gets the record
    let solver = Dns01::new(Arc::new(pb.client()) as Arc<dyn DnsProvider>)
        .with_nameservers(vec![dns.addr], Duration::from_secs(1));
    let fqdn = "_acme-challenge.node.example.net";
    solver.present(fqdn, "v").unwrap();
    let e = solver
        .wait_visible(&[(fqdn.to_string(), "v".to_string())])
        .unwrap_err();
    assert!(e.contains("not serving"), "{e}");
    // What issue()'s guard does on that error:
    solver.cleanup(fqdn, "v");
    assert_eq!(
        pb.zone.lock().unwrap().records,
        [] as [(
            u64,
            std::string::String,
            std::string::String,
            std::string::String
        ); 0]
    );
}

#[test]
fn a_failed_delete_does_not_stop_the_other_matching_records_going() {
    let pb = FakePorkbun::start("203.0.113.9");
    {
        let mut z = pb.zone.lock().unwrap();
        z.records
            .push((8, "_acme-challenge".into(), "TXT".into(), "dup".into()));
        z.records
            .push((9, "_acme-challenge".into(), "TXT".into(), "dup".into()));
        z.fail_delete.push(8);
    }
    let rec = TxtRecord {
        fqdn: "_acme-challenge.example.net".into(),
        value: "dup".into(),
        id: None,
    };
    let c = pb.client();
    assert!(
        DnsProvider::remove_txt(&c, &rec).is_err(),
        "first error returned"
    );
    let left: Vec<u64> = pb
        .zone
        .lock()
        .unwrap()
        .records
        .iter()
        .map(|r| r.0)
        .collect();
    assert_eq!(left, [8], "id 9 was still deleted");
}

#[test]
fn cleanup_without_an_id_deletes_by_exact_value_only() {
    let pb = FakePorkbun::start("203.0.113.9");
    {
        let mut z = pb.zone.lock().unwrap();
        z.records
            .push((8, "_acme-challenge".into(), "TXT".into(), "lost".into()));
        z.records
            .push((9, "_acme-challenge".into(), "TXT".into(), "keep".into()));
    }
    let solver = Dns01::new(Arc::new(pb.client()) as Arc<dyn DnsProvider>);
    solver.cleanup("_acme-challenge.example.net", "lost");
    let left: Vec<String> = pb
        .zone
        .lock()
        .unwrap()
        .records
        .iter()
        .map(|r| r.3.clone())
        .collect();
    assert_eq!(left, ["keep"]);
}

#[test]
fn a_create_whose_answer_is_lost_or_has_no_id_is_still_removed_by_value() {
    for reply in [CreateReply::Lost, CreateReply::NullId, CreateReply::NoId] {
        let pb = FakePorkbun::start("203.0.113.9");
        {
            let mut z = pb.zone.lock().unwrap();
            z.create_reply = reply;
            z.records
                .push((9, "_acme-challenge".into(), "TXT".into(), "keep".into()));
        }
        let solver = Dns01::new(Arc::new(pb.client()) as Arc<dyn DnsProvider>);
        let fqdn = "_acme-challenge.example.net";
        // Whatever present says, issue() cleans up what it asked for.
        let _ = solver.present(fqdn, "v1");
        solver.cleanup(fqdn, "v1");
        let left: Vec<String> = pb
            .zone
            .lock()
            .unwrap()
            .records
            .iter()
            .map(|r| r.3.clone())
            .collect();
        assert_eq!(left, ["keep"]);
        let writes = pb.writes();
        assert!(
            !writes
                .iter()
                .any(|w| w.ends_with("/null") || w.ends_with('/')),
            "{writes:?}"
        );
    }
}

/// A tracing writer into a shared buffer.
#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The process-wide log capture. Global, not `with_default`: tracing caches
/// per-callsite interest across threads, and a scoped subscriber races the
/// other tests' threads for it. Every test in this binary then logs here,
/// which only makes the "no key in any log" check stronger.
fn capture() -> Arc<Mutex<Vec<u8>>> {
    static BUF: std::sync::OnceLock<Arc<Mutex<Vec<u8>>>> = std::sync::OnceLock::new();
    Arc::clone(BUF.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let writer = Capture(Arc::clone(&buf));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        buf
    }))
}

#[test]
fn the_keys_never_reach_a_log_line_or_an_error() {
    let buf = capture();
    let pb = FakePorkbun::start("203.0.113.9");
    let mut errors = Vec::new();
    {
        let c = pb.client();
        ddns_once(&c, &hosts(&["69586.example.net"])).unwrap();
        let solver = Dns01::new(Arc::new(pb.client()) as Arc<dyn DnsProvider>);
        solver
            .present("_acme-challenge.69586.example.net", "v")
            .unwrap();
        solver.cleanup("_acme-challenge.69586.example.net", "v");
        // Now every call fails, and the fake echoes the request body,
        // keys included, back in its error message.
        pb.zone.lock().unwrap().fail = true;
        errors.push(
            ddns_once(&c, &hosts(&["69586.example.net"]))
                .unwrap_err()
                .message()
                .to_string(),
        );
        errors.push(
            solver
                .present("_acme-challenge.69586.example.net", "w")
                .unwrap_err(),
        );
        solver.cleanup("_acme-challenge.69586.example.net", "w");
        tracing::warn!(
            "{:?} {:?}",
            Credentials::from_provider(&SecretProvider::new()),
            c
        );
    }
    let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("ddns: 69586.example.net A 203.0.113.9"),
        "{logs}"
    );
    assert!(logs.contains("dns-01: removed TXT"), "{logs}");
    assert!(
        logs.contains("[redacted]"),
        "the echo was scrubbed, not dropped: {logs}"
    );
    for e in &errors {
        assert!(e.contains("INVALID_API_KEYS_001"), "{e}");
    }
    let all = format!("{logs}\n{}", errors.join("\n"));
    for needle in [KEY, SECRET, "SENTINEL"] {
        assert!(!all.contains(needle), "{needle} leaked:\n{all}");
    }
}

#[test]
fn credentials_come_from_the_provider_and_name_what_is_missing() {
    let secrets = SecretProvider::new();
    let e = Credentials::from_provider(&secrets).unwrap_err();
    assert!(e.contains(API_KEY) && e.contains(SECRET_API_KEY), "{e}");
    secrets.put(API_KEY, KEY);
    let e = Credentials::from_provider(&secrets).unwrap_err();
    assert!(!e.contains(KEY) && e.contains(SECRET_API_KEY), "{e}");
    secrets.put(SECRET_API_KEY, SECRET);
    let c = Credentials::from_provider(&secrets).unwrap();
    assert_eq!(format!("{c:?}"), "Credentials(..)");
}

#[test]
fn subdomains_are_relative_to_the_zone() {
    assert_eq!(subdomain("example.net", "example.net"), Some(""));
    assert_eq!(subdomain("69586.example.net", "example.net"), Some("69586"));
    assert_eq!(
        subdomain("_acme-challenge.a.b.example.net", "example.net"),
        Some("_acme-challenge.a.b")
    );
    assert_eq!(subdomain("badexample.net", "example.net"), None);
    assert_eq!(subdomain("example.org", "example.net"), None);
}

use crate::public::dns::{AddressChange, DnsProvider, IpSource, Scrub, TxtRecord};

#[test]
fn porkbun_sets_an_address_through_the_trait() {
    let pb = FakePorkbun::start("203.0.113.9");
    let c = pb.client();
    let ip: std::net::IpAddr = "203.0.113.9".parse().unwrap();
    assert_eq!(
        c.set_address("69586.example.net", ip).unwrap(),
        AddressChange::Created
    );
    assert_eq!(
        c.set_address("69586.example.net", ip).unwrap(),
        AddressChange::Unchanged
    );
    let moved: std::net::IpAddr = "203.0.113.10".parse().unwrap();
    assert_eq!(
        c.set_address("69586.example.net", moved).unwrap(),
        AddressChange::Updated
    );
    assert_eq!(pb.a_records("69586"), vec!["203.0.113.10".to_string()]);
    assert_eq!(c.public_ipv4().unwrap().to_string(), "203.0.113.9");
}

#[test]
fn porkbun_refuses_an_ipv6_address_for_now() {
    let pb = FakePorkbun::start("203.0.113.9");
    let e = pb
        .client()
        .set_address("69586.example.net", "2001:db8::1".parse().unwrap())
        .unwrap_err();
    assert_eq!(e.kind, crate::public::dns::DnsErrorKind::Unsupported);
}

#[test]
fn porkbun_removes_a_txt_by_id_and_by_value_and_leaves_the_operators_own() {
    let pb = FakePorkbun::start("203.0.113.9");
    let c = pb.client();
    let fqdn = "_acme-challenge.69586.example.net";
    let theirs = c.add_txt(fqdn, "operator-kept").unwrap();
    let ours = c.add_txt(fqdn, "acme-value").unwrap();
    assert!(ours.id.is_some());
    c.remove_txt(&ours).unwrap();
    let mut left = c.txt_values(fqdn).unwrap();
    left.sort();
    assert_eq!(left, vec!["operator-kept".to_string()]);
    let by_value = TxtRecord { id: None, ..theirs };
    c.remove_txt(&by_value).unwrap();
    assert_eq!(c.txt_values(fqdn).unwrap(), [] as [std::string::String; 0]);
}

#[test]
fn scrub_redacts_every_secret_and_caps_the_length() {
    let s = Scrub::new([KEY.to_string(), SECRET.to_string(), String::new()]);
    let t = s.clean(&format!("a {KEY} b {SECRET} c {}", "x".repeat(400)));
    assert!(!t.contains(KEY) && !t.contains(SECRET), "{t}");
    assert!(t.contains("[redacted]"));
    assert!(t.chars().count() <= 300);
}
