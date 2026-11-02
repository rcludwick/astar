// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! [`issue`] end to end against Pebble, Let's Encrypt's test CA, in podman.
//! Nothing leaves 127.0.0.1: the CA, its validation, and the DNS it asks
//! are all in a pod on this machine.
//!
//! * **HTTP-01** runs Pebble with `PEBBLE_VA_ALWAYS_VALID=1`: the CA skips
//!   the callback (there is no route from the pod to a test's port 80), so
//!   this proves the protocol — account, order, challenge, finalise,
//!   download — and the store.
//! * **DNS-01** runs Pebble beside `pebble-challtestsrv`, a fake DNS server
//!   whose records the test writes through its management API, and points
//!   Pebble's resolver at it. Validation is real: Pebble looks the TXT
//!   record up and checks the value.
//!
//! Skipped, with the reason printed, unless `podman` answers and both
//! images are already present (`just acme-test-images` pulls them; a test
//! never pulls). Set `CONTAINER_CONNECTION=podman-machine-default` on the
//! maintainer's Mac.

use super::*;
use std::net::{IpAddr, TcpListener};
use std::process::Command;
use std::sync::Mutex;

use crate::public::dns::{AddressChange, Dns01, DnsError, DnsProvider, TxtRecord};

const PEBBLE: &str = "ghcr.io/letsencrypt/pebble:latest";
const CHALLTESTSRV: &str = "ghcr.io/letsencrypt/pebble-challtestsrv:latest";

fn podman(args: &[&str]) -> Option<String> {
    let out = Command::new("podman").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn skip_reason() -> Option<String> {
    if podman(&["info", "--format", "{{.Host.Arch}}"]).is_none() {
        return Some("podman does not answer (set CONTAINER_CONNECTION?)".into());
    }
    for image in [PEBBLE, CHALLTESTSRV] {
        if podman(&["image", "exists", image]).is_none() {
            return Some(format!("{image} is not pulled (just acme-test-images)"));
        }
    }
    None
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A pod, removed with everything in it when dropped.
struct Pod(String);

impl Drop for Pod {
    fn drop(&mut self) {
        let _ = Command::new("podman")
            .args(["pod", "rm", "-f", "-t", "0", &self.0])
            .output();
    }
}

struct Rig {
    _pod: Pod,
    dir_url: String,
    root_pem: Vec<u8>,
    mgmt: Option<u16>,
}

/// A client for the 127.0.0.1 management API that cannot hang a test.
fn short_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .into()
}

fn start(name: &str, dns: bool) -> Rig {
    let acme_port = free_port();
    let mgmt_port = free_port();
    let pod_name = format!("astar-pebble-{name}-{}", std::process::id());
    let mut create = vec![
        "pod".to_string(),
        "create".into(),
        "--name".into(),
        pod_name.clone(),
        "-p".into(),
        format!("127.0.0.1:{acme_port}:14000"),
    ];
    if dns {
        create.extend(["-p".into(), format!("127.0.0.1:{mgmt_port}:8055")]);
    }
    let create: Vec<&str> = create.iter().map(String::as_str).collect();
    podman(&create).expect("pod create");
    let pod = Pod(pod_name.clone());
    if dns {
        podman(&[
            "run",
            "-d",
            "--pull=never",
            "--pod",
            &pod_name,
            CHALLTESTSRV,
            "-defaultIPv4",
            "127.0.0.1",
            "-defaultIPv6",
            "",
            "-http01",
            "",
            "-https01",
            "",
            "-tlsalpn01",
            "",
            "-doh",
            "",
            "-dnsserver",
            ":8053",
            "-management",
            ":8055",
        ])
        .expect("challtestsrv");
    }
    let pebble_name = format!("{pod_name}-ca");
    let mut run = vec![
        "run",
        "-d",
        "--pull=never",
        "--pod",
        &pod_name,
        "--name",
        &pebble_name,
        "-e",
        "PEBBLE_VA_NOSLEEP=1",
    ];
    if !dns {
        run.extend(["-e", "PEBBLE_VA_ALWAYS_VALID=1"]);
    }
    run.extend([PEBBLE, "-config", "test/config/pebble-config.json"]);
    if dns {
        run.extend(["-dnsserver", "127.0.0.1:8053"]);
    }
    podman(&run).expect("pebble");
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root.pem");
    podman(&[
        "cp",
        &format!("{pebble_name}:/test/certs/pebble.minica.pem"),
        root.to_str().unwrap(),
    ])
    .expect("copy Pebble's root");
    let root_pem = std::fs::read(&root).unwrap();
    let dir_url = format!("https://localhost:{acme_port}/dir");
    let agent = agent_trusting(&root_pem).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while agent.get(&dir_url).call().is_err() {
        assert!(
            Instant::now() < deadline,
            "Pebble did not come up at {dir_url}"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    if dns {
        let deadline = Instant::now() + Duration::from_secs(30);
        while short_agent()
            .post(&format!("http://127.0.0.1:{mgmt_port}/clear-txt"))
            .send(r#"{"host":"ready.example.net."}"#)
            .is_err()
        {
            assert!(Instant::now() < deadline, "challtestsrv did not come up");
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    Rig {
        _pod: pod,
        dir_url,
        root_pem,
        mgmt: dns.then_some(mgmt_port),
    }
}

/// challtestsrv's management API as a [`DnsProvider`]. It has no list API,
/// so this keeps what it set and what it removed.
struct Challtestsrv {
    mgmt: u16,
    set: Mutex<Vec<(String, String)>>,
    removed: Mutex<Vec<String>>,
}

impl Challtestsrv {
    fn new(mgmt: u16) -> Self {
        Self {
            mgmt,
            set: Mutex::new(Vec::new()),
            removed: Mutex::new(Vec::new()),
        }
    }

    fn post(&self, path: &str, body: &Value) {
        short_agent()
            .post(&format!("http://127.0.0.1:{}{path}", self.mgmt))
            .send(body.to_string())
            .expect("challtestsrv management");
    }

    fn removed_sorted(&self) -> Vec<String> {
        let mut r = self.removed.lock().unwrap().clone();
        r.sort();
        r
    }
}

impl DnsProvider for Challtestsrv {
    fn zone(&self) -> &'static str {
        "example.net"
    }
    fn set_address(&self, _: &str, _: IpAddr) -> Result<AddressChange, DnsError> {
        Err(DnsError::unsupported("challtestsrv: TXT only"))
    }
    fn add_txt(&self, fqdn: &str, value: &str) -> Result<TxtRecord, DnsError> {
        self.post(
            "/set-txt",
            &json!({ "host": format!("{fqdn}."), "value": value }),
        );
        self.set.lock().unwrap().push((fqdn.into(), value.into()));
        Ok(TxtRecord {
            fqdn: fqdn.into(),
            value: value.into(),
            id: None,
        })
    }
    fn remove_txt(&self, rec: &TxtRecord) -> Result<(), DnsError> {
        self.post("/clear-txt", &json!({ "host": format!("{}.", rec.fqdn) }));
        self.removed.lock().unwrap().push(rec.fqdn.clone());
        Ok(())
    }
    fn txt_values(&self, fqdn: &str) -> Result<Vec<String>, DnsError> {
        Ok(self
            .set
            .lock()
            .unwrap()
            .iter()
            .filter(|(f, _)| f == fqdn)
            .map(|(_, v)| v.clone())
            .collect())
    }
}

/// The real DNS-01 solver over `provider`, with no nameserver to ask (the
/// provider's own list is the visibility check).
fn dns01(provider: Arc<dyn DnsProvider>) -> Solver {
    Solver::Dns01(Arc::new(
        Dns01::new(provider).with_nameservers(Vec::new(), Duration::from_secs(30)),
    ) as Arc<dyn Dns01Solver>)
}

fn check_issued(cert: &Cert, store: &CertStore, hosts: &[String]) {
    assert!(cert.not_after > SystemTime::now(), "{cert:?}");
    let back = store.load(hosts).expect("the store has it");
    assert_eq!(back.chain_pem, cert.chain_pem);
    crate::public::tls::certified_key(&back).expect("rustls loads the chain and key");
    assert!(
        std::fs::read_dir(store.dir()).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("account-")),
        "the account is kept for the next renewal"
    );
}

#[test]
fn pebble_issues_over_http01_and_reuses_the_account() {
    if let Some(why) = skip_reason() {
        eprintln!("skipped: {why}");
        return;
    }
    let rig = start("http", false);
    let client = AcmeClient::with_root(&rig.dir_url, &rig.root_pem).unwrap();
    let state = tempfile::tempdir().unwrap();
    let store = CertStore::new(state.path());
    let hosts = vec!["test.invalid".to_string()];
    let map: ChallengeMap = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let solver = Solver::Http01(Arc::clone(&map));
    let cert = issue(&client, Some("ops@example.net"), &hosts, &solver, &store).unwrap();
    check_issued(&cert, &store, &hosts);
    assert!(
        map.lock().unwrap().is_empty(),
        "challenge tokens are removed"
    );
    // A renewal reuses the stored account.
    let accounts = || std::fs::read_dir(store.dir()).unwrap().count();
    let before = accounts();
    let again = issue(&client, None, &hosts, &solver, &store).unwrap();
    // Pebble picks each certificate's validity window itself, so a renewal's
    // not_after is not ordered against the first one's; only freshness is.
    check_issued(&again, &store, &hosts);
    assert_ne!(
        again.chain_pem, cert.chain_pem,
        "a renewal issues a new certificate"
    );
    assert_eq!(accounts(), before, "no second account file");
}

#[test]
fn pebble_validates_dns01_against_a_fake_dns_and_removes_the_records() {
    if let Some(why) = skip_reason() {
        eprintln!("skipped: {why}");
        return;
    }
    let rig = start("dns", true);
    let client = AcmeClient::with_root(&rig.dir_url, &rig.root_pem).unwrap();
    let state = tempfile::tempdir().unwrap();
    let store = CertStore::new(state.path());
    let hosts = vec![
        "69586.example.net".to_string(),
        "node.example.net".to_string(),
    ];
    let ct = Arc::new(Challtestsrv::new(rig.mgmt.unwrap()));
    let solver = dns01(Arc::clone(&ct) as Arc<dyn DnsProvider>);
    let cert = issue(&client, None, &hosts, &solver, &store).unwrap();
    check_issued(&cert, &store, &hosts);
    // The CA picks the authorisation order; compare sorted.
    let mut set: Vec<String> = ct
        .set
        .lock()
        .unwrap()
        .iter()
        .map(|(f, _)| f.clone())
        .collect();
    set.sort();
    let want = [
        "_acme-challenge.69586.example.net",
        "_acme-challenge.node.example.net",
    ];
    assert_eq!(set, want);
    assert_eq!(ct.removed_sorted(), want);
}

#[test]
fn a_failed_dns01_validation_still_removes_the_records() {
    // A solver that writes the WRONG value: Pebble's real lookup refuses it.
    struct Wrong(Arc<Challtestsrv>);
    impl DnsProvider for Wrong {
        fn zone(&self) -> &str {
            self.0.zone()
        }
        fn set_address(&self, n: &str, ip: IpAddr) -> Result<AddressChange, DnsError> {
            self.0.set_address(n, ip)
        }
        fn add_txt(&self, fqdn: &str, _value: &str) -> Result<TxtRecord, DnsError> {
            self.0.add_txt(fqdn, "not-the-value")
        }
        fn remove_txt(&self, r: &TxtRecord) -> Result<(), DnsError> {
            self.0.remove_txt(r)
        }
        fn txt_values(&self, _: &str) -> Result<Vec<String>, DnsError> {
            // Cannot list, so the wait passes and Pebble's own lookup is
            // what refuses the wrong value.
            Err(DnsError::unsupported("cannot list"))
        }
    }

    if let Some(why) = skip_reason() {
        eprintln!("skipped: {why}");
        return;
    }
    let rig = start("dnsfail", true);
    let client = AcmeClient::with_root(&rig.dir_url, &rig.root_pem).unwrap();
    let state = tempfile::tempdir().unwrap();
    let store = CertStore::new(state.path());
    let ct = Arc::new(Challtestsrv::new(rig.mgmt.unwrap()));
    let solver = dns01(Arc::new(Wrong(Arc::clone(&ct))));
    let e = issue(
        &client,
        None,
        &["bad.example.net".to_string()],
        &solver,
        &store,
    )
    .unwrap_err();
    assert!(e.contains("dns-01 validation failed"), "{e}");
    assert_eq!(ct.removed_sorted(), ["_acme-challenge.bad.example.net"]);
    assert!(store.load(&["bad.example.net".to_string()]).is_none());
}

#[test]
fn a_txt_record_whose_create_reported_failure_is_still_removed() {
    // The provider stored the record but the answer was lost: present errs.
    struct Lost(Arc<Challtestsrv>);
    impl DnsProvider for Lost {
        fn zone(&self) -> &str {
            self.0.zone()
        }
        fn set_address(&self, n: &str, ip: IpAddr) -> Result<AddressChange, DnsError> {
            self.0.set_address(n, ip)
        }
        fn add_txt(&self, fqdn: &str, value: &str) -> Result<TxtRecord, DnsError> {
            self.0.add_txt(fqdn, value)?;
            Err(DnsError::new("cannot reach the provider: connection reset"))
        }
        fn remove_txt(&self, r: &TxtRecord) -> Result<(), DnsError> {
            self.0.remove_txt(r)
        }
        fn txt_values(&self, f: &str) -> Result<Vec<String>, DnsError> {
            self.0.txt_values(f)
        }
    }

    if let Some(why) = skip_reason() {
        eprintln!("skipped: {why}");
        return;
    }
    let rig = start("dnslost", true);
    let client = AcmeClient::with_root(&rig.dir_url, &rig.root_pem).unwrap();
    let state = tempfile::tempdir().unwrap();
    let store = CertStore::new(state.path());
    let ct = Arc::new(Challtestsrv::new(rig.mgmt.unwrap()));
    let solver = dns01(Arc::new(Lost(Arc::clone(&ct))));
    let e = issue(
        &client,
        None,
        &["lost.example.net".to_string()],
        &solver,
        &store,
    )
    .unwrap_err();
    assert!(e.contains("TXT record"), "{e}");
    assert_eq!(ct.set.lock().unwrap().len(), 1, "the record was stored");
    assert_eq!(ct.removed_sorted(), ["_acme-challenge.lost.example.net"]);
}

#[cfg(feature = "dns-providers")]
#[test]
fn pebble_validates_dns01_through_dns_update() {
    use crate::public::dns::Scrub;
    use crate::public::dns_update::DnsUpdateProvider;

    if let Some(why) = skip_reason() {
        eprintln!("skipped: {why}");
        return;
    }
    let rig = start("dnsupd", true);
    let client = AcmeClient::with_root(&rig.dir_url, &rig.root_pem).unwrap();
    let state = tempfile::tempdir().unwrap();
    let store = CertStore::new(state.path());
    let hosts = vec![
        "69586.example.net".to_string(),
        "node.example.net".to_string(),
    ];
    let provider = DnsUpdateProvider::with_updater(
        ::dns_update::DnsUpdater::new_pebble(
            format!("http://127.0.0.1:{}", rig.mgmt.unwrap()),
            Some(Duration::from_secs(10)),
        ),
        "example.net",
        600,
        Scrub::new(Vec::<String>::new()),
        Duration::from_secs(30),
    )
    .unwrap();
    let solver = dns01(Arc::new(provider));
    // Pebble only issues if it found the TXT values through dns-update's
    // challtestsrv client, so this checks create and validation. It cannot
    // observe cleanup: a failed cleanup does not fail the order.
    let cert = issue(&client, None, &hosts, &solver, &store).unwrap();
    check_issued(&cert, &store, &hosts);
}
