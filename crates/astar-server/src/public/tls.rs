// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The public bind's TLS: a certificate slot the listener reads on every
//! handshake, and the thread that fills it — load from the store, issue
//! when missing, renew 30 days before expiry, back off on failure.
//!
//! A renewal swaps the slot; the socket is never rebound and open streams
//! carry on. A failed renewal leaves the old certificate serving.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use astar_http::TlsConfig;
use rustls::sign::CertifiedKey;

use crate::public::acme::{self, AcmeClient, Cert, CertStore, Solver};
use crate::public::serve::{PublicTlsStatus, TlsWrap};

/// How often the manager re-checks a healthy certificate.
pub const CHECK_EVERY: Duration = Duration::from_secs(12 * 3600);
/// First retry after a failed issue; doubles up to [`BACKOFF_MAX`].
pub const BACKOFF_MIN: Duration = Duration::from_secs(60);
/// The longest wait between attempts.
pub const BACKOFF_MAX: Duration = Duration::from_secs(3600);
/// After the CA issued a certificate that will not install, no new order
/// for the same names for this long: retrying sooner only burns the CA's
/// weekly limit on certificates that fail the same way.
pub const INSTALL_HOLD: Duration = Duration::from_secs(6 * 3600);
/// The most certificates ordered for one set of names in any seven days.
/// Let's Encrypt allows five duplicates a week; this leaves two for a
/// person fixing things by hand. Counted in `<state>/acme/issued.json`.
pub const MAX_ISSUED_PER_WEEK: usize = 3;

/// The TLS side of the public bind: astar-http's certificate slot, filled
/// from the store and the CA.
pub struct TlsState {
    config: Arc<TlsConfig>,
}

impl TlsState {
    /// An empty slot: plain HTTP until [`Self::install`].
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            config: TlsConfig::new(),
        })
    }

    /// Serve `cert` from the next handshake on.
    ///
    /// # Errors
    /// A chain or key that does not load.
    pub fn install(&self, cert: &Cert) -> Result<(), String> {
        self.config.install(certified_key(cert)?);
        Ok(())
    }
}

impl TlsWrap for TlsState {
    fn config(&self) -> Arc<TlsConfig> {
        Arc::clone(&self.config)
    }

    fn ready(&self) -> bool {
        self.config.ready()
    }
}

/// A rustls key from a stored certificate.
///
/// # Errors
/// A chain with no certificate, a key rustls cannot use, or a key that is
/// not the one the leaf certificate was issued for.
pub fn certified_key(cert: &Cert) -> Result<CertifiedKey, String> {
    use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject as _};
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert.chain_pem)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("certificate chain: {e}"))?;
    if chain.is_empty() {
        return Err("certificate chain is empty".into());
    }
    let key = PrivateKeyDer::from_pem_slice(&cert.key_pem)
        .map_err(|_| "certificate key does not load".to_string())?;
    let signing = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|_| "certificate key type is not supported".to_string())?;
    let ck = CertifiedKey::new(chain, signing);
    ck.keys_match()
        .map_err(|_| "certificate key does not match the certificate".to_string())?;
    Ok(ck)
}

/// What the manager needs to issue.
pub struct Issuer {
    pub client: AcmeClient,
    pub email: Option<String>,
    pub hosts: Vec<String>,
    pub solver: Solver,
    pub store: CertStore,
}

/// The certificate thread.
pub struct TlsManager {
    thread: Option<JoinHandle<()>>,
}

fn sleep_unless_stopped(stop: &AtomicBool, d: Duration) {
    let slice = Duration::from_millis(200);
    let mut left = d;
    while !left.is_zero() && !stop.load(Ordering::Relaxed) {
        let nap = left.min(slice);
        std::thread::sleep(nap);
        left = left.saturating_sub(nap);
    }
}

/// Places one order and hands back the issued certificate, unsaved.
type OrderFn = Box<dyn FnMut() -> Result<Cert, String> + Send>;

/// The certificate loop's state, one [`Manager::step`] per wake-up.
struct Manager {
    hosts: Vec<String>,
    store: CertStore,
    order: OrderFn,
    tls: Arc<TlsState>,
    live: Arc<AtomicBool>,
    status: Arc<Mutex<PublicTlsStatus>>,
    /// The certificate being served.
    current: Option<Cert>,
    /// `current` is served but the store has not taken it yet; each pass
    /// retries the save, never the order.
    unsaved: bool,
    /// When each recent certificate for `hosts` was issued (Unix seconds,
    /// oldest first): the weekly cap's count, kept in memory so it holds
    /// even when the store cannot be written.
    issued: Vec<u64>,
    /// No new order before this: the last issued certificate would not
    /// install.
    hold_until: Option<SystemTime>,
    backoff: Duration,
}

impl Manager {
    fn new(
        hosts: Vec<String>,
        store: CertStore,
        order: OrderFn,
        tls: Arc<TlsState>,
        live: Arc<AtomicBool>,
        status: Arc<Mutex<PublicTlsStatus>>,
    ) -> Self {
        let issued = store.issued(&hosts);
        Self {
            hosts,
            store,
            order,
            tls,
            live,
            status,
            current: None,
            unsaved: false,
            issued,
            hold_until: None,
            backoff: BACKOFF_MIN,
        }
    }

    /// Load the stored certificate and serve it at once, so a restart
    /// serves HTTPS from its first connection.
    fn load_stored(&mut self) {
        let Some(c) = self.store.load(&self.hosts) else {
            return;
        };
        match self.tls.install(&c) {
            Ok(()) => {
                self.serving(&c);
                self.current = Some(c);
            }
            Err(e) => tracing::warn!("public tls: the stored certificate does not load: {e}"),
        }
    }

    fn serving(&self, c: &Cert) {
        self.live.store(true, Ordering::Relaxed);
        let mut s = self.status.lock().expect("status poisoned");
        s.serving_tls = true;
        s.not_after_unix = Some(acme::unix(c.not_after));
    }

    fn set_error(&self, e: Option<String>) {
        self.status.lock().expect("status poisoned").last_error = e;
    }

    /// The next back-off, doubling the one after it.
    fn bump(&mut self) -> Duration {
        let d = self.backoff;
        self.backoff = (d * 2).min(BACKOFF_MAX);
        d
    }

    /// Save `current` if the store has not taken it yet. False when that
    /// save failed (the error is on the status).
    fn save_pending(&mut self) -> bool {
        let Some(c) = self.current.as_ref().filter(|_| self.unsaved) else {
            return true;
        };
        match self.store.save(c) {
            Ok(()) => {
                self.unsaved = false;
                self.backoff = BACKOFF_MIN;
                self.set_error(None);
                true
            }
            Err(e) => {
                let e = format!(
                    "the certificate is being served but could not be saved to {}: {e}",
                    self.store.dir().display()
                );
                tracing::warn!("public tls: {e}");
                self.set_error(Some(e));
                false
            }
        }
    }

    /// One pass: save what is unsaved, then issue or renew if due. Returns
    /// how long to sleep.
    ///
    /// Every certificate the CA hands back is kept: one the store refuses is
    /// served anyway and only its save is retried; one that will not install
    /// holds new orders for [`INSTALL_HOLD`]; and never more than
    /// [`MAX_ISSUED_PER_WEEK`] are ordered for the same names in seven days.
    fn step(&mut self, now: SystemTime) -> Duration {
        const WEEK: u64 = 7 * 86_400;
        let saved = self.save_pending();
        let due = self
            .current
            .as_ref()
            .is_none_or(|c| acme::needs_renewal(c, &self.hosts, now));
        if !due {
            return if saved { CHECK_EVERY } else { self.bump() };
        }
        if let Some(left) = self
            .hold_until
            .and_then(|t| t.duration_since(now).ok())
            .filter(|d| !d.is_zero())
        {
            return left;
        }
        let now_unix = acme::unix(now);
        self.issued.retain(|t| t + WEEK > now_unix);
        if self.issued.len() >= MAX_ISSUED_PER_WEEK {
            let free_at = self.issued[0] + WEEK;
            self.set_error(Some(format!(
                "{} certificates for these names were issued this week, the most \
                 astar-server orders in seven days; the next order waits until Unix {free_at}",
                self.issued.len()
            )));
            return Duration::from_secs((free_at - now_unix).max(1));
        }
        let c = match (self.order)() {
            Ok(c) => c,
            Err(e) => {
                let wait = self.bump();
                tracing::warn!(
                    "public tls: certificate for {:?} failed, retrying in {}s: {e}",
                    self.hosts,
                    wait.as_secs()
                );
                self.set_error(Some(e));
                return wait;
            }
        };
        self.issued.push(now_unix);
        if let Err(e) = self.store.save_issued(&self.hosts, &self.issued) {
            tracing::warn!("public tls: cannot record the issue time: {e}");
        }
        if let Err(e) = self.tls.install(&c) {
            let e = format!(
                "the CA issued a certificate that does not load ({e}); \
                 no new order for {} hours",
                INSTALL_HOLD.as_secs() / 3600
            );
            tracing::warn!("public tls: {e}");
            self.set_error(Some(e));
            self.hold_until = Some(now + INSTALL_HOLD);
            return INSTALL_HOLD;
        }
        self.hold_until = None;
        self.serving(&c);
        self.set_error(None);
        self.current = Some(c);
        self.unsaved = true;
        self.backoff = BACKOFF_MIN;
        if self.save_pending() {
            CHECK_EVERY
        } else {
            self.bump()
        }
    }
}

impl TlsManager {
    /// Load the stored certificate at once (so a restart serves HTTPS from
    /// its first connection), then keep it current on a thread.
    ///
    /// # Panics
    /// If the thread cannot be spawned.
    #[must_use]
    pub fn start(
        issuer: Issuer,
        tls: Arc<TlsState>,
        live: Arc<AtomicBool>,
        status: Arc<Mutex<PublicTlsStatus>>,
        stop: Arc<AtomicBool>,
    ) -> Self {
        let Issuer {
            client,
            email,
            hosts,
            solver,
            store,
        } = issuer;
        let order: OrderFn = {
            let (hosts, store) = (hosts.clone(), store.clone());
            Box::new(move || acme::order(&client, email.as_deref(), &hosts, &solver, &store))
        };
        let mut m = Manager::new(hosts, store, order, tls, live, status);
        m.load_stored();
        let thread = std::thread::Builder::new()
            .name("public-acme".into())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let wait = m.step(SystemTime::now());
                    sleep_unless_stopped(&stop, wait);
                }
            })
            .expect("spawn the acme thread");
        Self {
            thread: Some(thread),
        }
    }

    /// Wait for the thread (after the shared stop flag is set).
    pub fn join(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NodeFileConfig;
    use crate::controller::NodeController;
    use crate::public::http::PublicCtx;
    use crate::public::serve::PublicServer;
    use crate::public::status::Activity;
    use crate::secrets::SecretProvider;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::{Instant, UNIX_EPOCH};

    fn ctx(hosts: &[&str]) -> Arc<PublicCtx> {
        ctx_with(hosts, crate::public::guard::Limits::default())
    }

    fn ctx_with(hosts: &[&str], limits: crate::public::guard::Limits) -> Arc<PublicCtx> {
        let station = astar_station::Station::with_backend_factory(
            astar_station::StationConfig::default(),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        );
        Arc::new(PublicCtx {
            ctrl: Arc::new(NodeController::new(station, SecretProvider::new())),
            hub: None,
            activity: Arc::new(Mutex::new(Activity::new(Instant::now()))),
            hostnames: hosts.iter().map(|h| (*h).to_string()).collect(),
            tls: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            anonymous_status: true,
            guard: crate::public::guard::Guard::new(limits),
        })
    }

    fn web(extra: &str) -> crate::config::WebCfg {
        let text = format!(
            "[listener]\nbind = \"0.0.0.0:4569\"\nanswer = \"auto\"\nmax_calls = 2\nauth = \"off\"\n\
             [control]\nbind = \"127.0.0.1:8730\"\n[secrets]\nsource = \"env\"\n\
             [web]\npublic_bind = \"127.0.0.1:0\"\npublic_hostnames = [\"test.invalid\"]\n{extra}"
        );
        NodeFileConfig::from_toml_str(&text).unwrap().web.unwrap()
    }

    fn get(addr: std::net::SocketAddr, host: &str, path: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n").unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    #[test]
    fn no_certificate_and_an_unreachable_ca_serves_plain_http_and_says_why() {
        let state = tempfile::tempdir().unwrap();
        let w = web("acme_http_bind = \"127.0.0.1:0\"\n");
        let c = ctx(&["test.invalid"]);
        let server =
            PublicServer::start(&w, state.path(), &c, Some("http://127.0.0.1:9/dir"), None)
                .unwrap();
        let resp = get(server.public_addr(), "test.invalid", "/");
        assert!(
            resp.starts_with("HTTP/1.1 200") && resp.contains("id=\"hero\""),
            "{resp}"
        );
        // Port 80 serves the page too while there is no certificate.
        let resp = get(server.http80_addr().unwrap(), "test.invalid", "/");
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        let deadline = Instant::now() + Duration::from_secs(10);
        while server.status().last_error.is_none() {
            assert!(Instant::now() < deadline, "no error recorded");
            std::thread::sleep(Duration::from_millis(50));
        }
        let st = server.status();
        assert!(!st.serving_tls && st.tls_built, "{st:?}");
        assert_eq!(st.challenge, "http-01");
        assert!(st.last_error.unwrap().contains("cannot reach the CA"));
        assert!(!c.tls.load(Ordering::Relaxed));
        server.stop();
    }

    fn self_signed(host: &str) -> (Cert, Vec<u8>) {
        let kp = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(vec![host.to_string()]).unwrap();
        let cert = params.self_signed(&kp).unwrap();
        (
            Cert {
                chain_pem: cert.pem().into_bytes(),
                key_pem: kp.serialize_pem().into_bytes(),
                not_after: UNIX_EPOCH + Duration::from_secs(4_102_444_800),
                hosts: vec![host.to_string()],
            },
            cert.pem().into_bytes(),
        )
    }

    #[test]
    fn a_stored_certificate_is_served_at_once_and_plain_http_redirects() {
        let state = tempfile::tempdir().unwrap();
        let (cert, root) = self_signed("test.invalid");
        crate::public::acme::CertStore::new(state.path())
            .save(&cert)
            .unwrap();
        let w = web("");
        let c = ctx(&["test.invalid"]);
        let server =
            PublicServer::start(&w, state.path(), &c, Some("http://127.0.0.1:9/dir"), None)
                .unwrap();
        assert!(server.status().serving_tls);
        assert!(c.tls.load(Ordering::Relaxed));

        // HTTPS on the public bind, trusting only the test certificate.
        let mut roots = rustls::RootCertStore::empty();
        for der in rustls_pki_types::CertificateDer::pem_slice_iter(&root) {
            roots.add(der.unwrap()).unwrap();
        }
        let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let name = rustls_pki_types::ServerName::try_from("test.invalid").unwrap();
        let conn = rustls::ClientConnection::new(Arc::new(cfg), name).unwrap();
        let tcp = TcpStream::connect(server.public_addr()).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut tls = rustls::StreamOwned::new(conn, tcp);
        tls.write_all(b"GET /public/status HTTP/1.1\r\nHost: test.invalid\r\n\r\n")
            .unwrap();
        let mut out = String::new();
        let _ = tls.read_to_string(&mut out);
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        assert!(out.contains("\"secure\":true"), "{out}");
        assert!(
            out.contains("\r\nStrict-Transport-Security: max-age=31536000\r\n"),
            "{out}"
        );

        // Plain HTTP on the same port is redirected once TLS is live.
        let resp = get(server.public_addr(), "test.invalid", "/x");
        assert!(
            resp.starts_with("HTTP/1.1 404") || resp.starts_with("HTTP/1.1 301"),
            "{resp}"
        );
        let resp = get(server.public_addr(), "test.invalid", "/");
        assert!(resp.contains("Location: https://test.invalid/"), "{resp}");
        assert!(!resp.contains("Strict-Transport-Security"), "{resp}");
        server.stop();
    }

    use std::sync::atomic::AtomicUsize;

    /// A manager whose orders are counted and answered by `make`, at
    /// `<state>/acme`.
    fn manager(state: &std::path::Path, make: fn() -> Cert) -> (Manager, Arc<AtomicUsize>) {
        let orders = Arc::new(AtomicUsize::new(0));
        let n = Arc::clone(&orders);
        let order: OrderFn = Box::new(move || {
            n.fetch_add(1, Ordering::Relaxed);
            Ok(make())
        });
        let m = Manager::new(
            vec!["test.invalid".to_string()],
            CertStore::new(state),
            order,
            TlsState::new(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(PublicTlsStatus::default())),
        );
        (m, orders)
    }

    fn good() -> Cert {
        self_signed("test.invalid").0
    }

    /// Issued fine by the CA, but its key is not the leaf's: it never installs.
    fn broken() -> Cert {
        Cert {
            key_pem: good().key_pem,
            ..good()
        }
    }

    /// Run `m` for `span` of simulated time from `t`, the way the thread
    /// would, but never sleeping longer than `cap` (an early wake-up).
    fn run_for(m: &mut Manager, t: &mut SystemTime, span: Duration, cap: Duration) {
        let end = *t + span;
        while *t < end {
            let wait = m.step(*t).min(cap).max(Duration::from_secs(1));
            *t += wait;
        }
    }

    #[test]
    fn a_certificate_the_store_refuses_is_still_served_and_never_reordered() {
        let state = tempfile::tempdir().unwrap();
        // `<state>/acme` is a file: no save can succeed, even as root.
        std::fs::write(state.path().join("acme"), b"").unwrap();
        let (mut m, orders) = manager(state.path(), good);
        let mut t = SystemTime::now();
        run_for(&mut m, &mut t, Duration::from_secs(6 * 3600), CHECK_EVERY);
        assert_eq!(
            orders.load(Ordering::Relaxed),
            1,
            "one order, however many passes"
        );
        assert!(m.tls.ready(), "the unsaved certificate is served");
        assert!(m.live.load(Ordering::Relaxed));
        let st = m.status.lock().unwrap().clone();
        assert!(st.serving_tls, "{st:?}");
        assert!(st.last_error.unwrap().contains("could not be saved"));

        // Once the store takes writes again, the same certificate lands there.
        std::fs::remove_file(state.path().join("acme")).unwrap();
        run_for(&mut m, &mut t, BACKOFF_MAX, BACKOFF_MAX);
        assert_eq!(orders.load(Ordering::Relaxed), 1);
        let saved = m.store.load(&m.hosts).expect("saved on a later pass");
        assert_eq!(saved.chain_pem, m.current.as_ref().unwrap().chain_pem);
        assert!(m.status.lock().unwrap().last_error.is_none());
    }

    #[test]
    fn a_failing_order_backs_off_doubling() {
        let state = tempfile::tempdir().unwrap();
        let (mut m, _) = manager(state.path(), good);
        m.order = Box::new(|| Err("cannot reach the CA".into()));
        let t = SystemTime::now();
        let waits: Vec<u64> = (0..8).map(|_| m.step(t).as_secs()).collect();
        assert_eq!(waits, [60, 120, 240, 480, 960, 1920, 3600, 3600]);
    }

    #[test]
    fn a_certificate_that_will_not_install_holds_new_orders_for_hours() {
        let state = tempfile::tempdir().unwrap();
        let (mut m, orders) = manager(state.path(), broken);
        let mut t = SystemTime::now();
        // Woken every minute for the first hour: still one order.
        run_for(&mut m, &mut t, Duration::from_secs(3600), BACKOFF_MIN);
        assert_eq!(orders.load(Ordering::Relaxed), 1);
        assert!(!m.tls.ready());
        assert!(m.status.lock().unwrap().last_error.is_some());
        run_for(&mut m, &mut t, INSTALL_HOLD, BACKOFF_MIN);
        assert_eq!(orders.load(Ordering::Relaxed), 2, "one more after the hold");
    }

    #[test]
    fn no_more_than_a_few_certificates_a_week_for_the_same_names() {
        let state = tempfile::tempdir().unwrap();
        let (mut m, orders) = manager(state.path(), broken);
        let mut t = SystemTime::now();
        let day = Duration::from_secs(86_400);
        run_for(&mut m, &mut t, 6 * day, BACKOFF_MIN);
        assert_eq!(orders.load(Ordering::Relaxed), MAX_ISSUED_PER_WEEK);
        let e = m.status.lock().unwrap().last_error.clone().unwrap();
        assert!(e.contains("this week"), "{e}");

        // The count survives a restart: a new manager on the same store waits.
        let (mut again, more) = manager(state.path(), broken);
        run_for(
            &mut again,
            &mut t.clone(),
            Duration::from_secs(3600),
            BACKOFF_MIN,
        );
        assert_eq!(more.load(Ordering::Relaxed), 0);

        // A week after the first, one more is allowed.
        run_for(&mut m, &mut t, 2 * day, BACKOFF_MIN);
        assert!(orders.load(Ordering::Relaxed) > MAX_ISSUED_PER_WEEK);
    }

    #[test]
    fn the_public_bind_cuts_off_a_trickled_handshake() {
        let state = tempfile::tempdir().unwrap();
        let (cert, _root) = self_signed("test.invalid");
        crate::public::acme::CertStore::new(state.path())
            .save(&cert)
            .unwrap();
        let c = ctx_with(
            &["test.invalid"],
            crate::public::guard::Limits {
                head_deadline: Duration::from_millis(300),
                ..crate::public::guard::Limits::default()
            },
        );
        let server = PublicServer::start(
            &web("acme_http_bind = \"127.0.0.1:0\"\n"),
            state.path(),
            &c,
            Some("http://127.0.0.1:9/dir"),
            None,
        )
        .unwrap();
        let mut s = TcpStream::connect(server.public_addr()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut w = s.try_clone().unwrap();
        // A handshake record header promising 512 bytes, then a byte every
        // 50 ms: rustls waits for the record, one short read at a time.
        let trickle = std::thread::spawn(move || {
            let _ = w.write_all(&[0x16, 0x03, 0x01, 0x02, 0x00]);
            for _ in 0..80 {
                if w.write_all(&[0]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let started = Instant::now();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        let took = started.elapsed();
        assert!(took < Duration::from_millis(1500), "held for {took:?}");
        drop(s);
        let _ = trickle.join();
        server.stop();
    }

    use rustls_pki_types::pem::PemObject as _;
}
