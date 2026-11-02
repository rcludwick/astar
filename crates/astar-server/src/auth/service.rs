// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! [`AuthService`]: the in-memory store, the clock, sessions and the
//! principal a request acts as.
//!
//! The store in memory is authoritative while the server runs. Every change
//! goes through [`AuthService::mutate`], which applies it to a copy, writes
//! the copy to `auth.json` under the lock, and only then makes it current —
//! so a failed write changes nothing, in memory or on disk.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use uuid::Uuid;

use super::session;
use super::store::{self, AuditEntry, AuthData, Loaded, Role, Session};
use crate::config::NodeFileConfig;

/// Unix seconds, from the system clock or a test's.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// `last_seen` is written to disk when it has moved by this much, so a
/// page that polls does not rewrite the store every second.
pub const LAST_SEEN_FLUSH_SECS: u64 = 3600;

/// What sign-in is configured to do, from `node.toml`.
#[derive(Clone)]
pub struct AuthSettings {
    /// Where `auth.json` lives.
    pub state_dir: PathBuf,
    /// `[control] loopback_login`.
    pub loopback_login: bool,
    /// `[web] anonymous_status`.
    pub anonymous_status: bool,
    /// `[web] admin_on_public` (and a public bind exists).
    pub admin_on_public: bool,
    /// `[web] rp_id`, when a parent domain is configured.
    pub rp_id: Option<String>,
    /// The normalised `public_hostnames`.
    pub public_hostnames: Vec<String>,
    /// `session_idle_days`, in seconds.
    pub idle_secs: u64,
    /// `session_max_days`, in seconds.
    pub max_secs: u64,
    /// `step_up_minutes`, in seconds.
    pub step_up_secs: u64,
    /// This node's number, for "invited you as an admin of node 12345".
    pub node: Option<String>,
}

impl AuthSettings {
    /// The settings `cfg` asks for, with the store in `state_dir`.
    #[must_use]
    pub fn from_config(cfg: &NodeFileConfig, state_dir: &Path) -> Self {
        let web = cfg.web.as_ref();
        Self {
            state_dir: state_dir.to_path_buf(),
            loopback_login: cfg.control.loopback_login,
            anonymous_status: web.is_none_or(crate::config::WebCfg::anonymous_status),
            admin_on_public: web.is_some_and(crate::config::WebCfg::admin_on_public),
            rp_id: web.and_then(|w| w.rp_id().map(str::to_string)),
            public_hostnames: web.map(|w| w.hostnames().to_vec()).unwrap_or_default(),
            idle_secs: u64::from(web.map_or(7, |w| w.session_idle_days)) * 86_400,
            max_secs: u64::from(web.map_or(30, |w| w.session_max_days)) * 86_400,
            step_up_secs: u64::from(web.map_or(10, |w| w.step_up_minutes)) * 60,
            node: cfg.register.as_ref().map(|r| r.node_id.clone()),
        }
    }

    /// Defaults with the store in `state_dir`: what a test starts from.
    #[must_use]
    pub fn defaults(state_dir: &Path) -> Self {
        Self {
            state_dir: state_dir.to_path_buf(),
            loopback_login: false,
            anonymous_status: true,
            admin_on_public: true,
            rp_id: None,
            public_hostnames: Vec::new(),
            idle_secs: 7 * 86_400,
            max_secs: 30 * 86_400,
            step_up_secs: 10 * 60,
            node: None,
        }
    }
}

/// Which listener a request arrived on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// `[control] bind`.
    Control,
    /// `[web] public_bind`.
    Public,
}

/// Everything about a request the sign-in code reads, socket-independent.
#[derive(Clone, Copy)]
pub struct Req<'a> {
    pub method: &'a str,
    /// The path, without the query.
    pub path: &'a str,
    /// The query, without the `?`.
    pub query: &'a str,
    pub body: &'a [u8],
    pub host: Option<&'a str>,
    pub origin: Option<&'a str>,
    pub content_type: Option<&'a str>,
    pub sec_fetch_site: Option<&'a str>,
    pub cookie: Option<&'a str>,
    pub user_agent: Option<&'a str>,
    pub peer: Option<SocketAddr>,
    /// `Forwarded` or `X-Forwarded-For` was present.
    pub forwarded: bool,
    pub surface: Surface,
    /// The request arrived over TLS.
    pub tls: bool,
}

impl Req<'_> {
    /// The peer's IP as text, for the audit and rate limits.
    #[must_use]
    pub fn ip(&self) -> String {
        self.peer.map(|p| p.ip().to_string()).unwrap_or_default()
    }

    /// Who the rate limits, lockouts and ceremony quotas count this
    /// request as: an IPv4 address as it is, an IPv6 address by its /64
    /// (one subscriber's prefix, inside which a client can take a fresh
    /// address for every request), and an IPv4-mapped IPv6 address as the
    /// IPv4 address it is.
    #[must_use]
    pub fn client_key(&self) -> String {
        self.peer.map(|p| client_key_of(p.ip())).unwrap_or_default()
    }

    /// The `Host` with any port removed, lower-cased.
    #[must_use]
    pub fn hostname(&self) -> String {
        crate::public::http::normalize_host(self.host.unwrap_or_default())
    }
}

/// A signed-in user, as of this request.
#[derive(Clone)]
pub struct SessionUser {
    pub user_id: Uuid,
    pub name: String,
    pub role: Role,
    /// The session's list id (not the cookie).
    pub session_id: String,
    pub last_auth_at: u64,
}

/// Who a request acts as.
#[derive(Clone)]
pub enum Principal {
    /// A loopback peer on the control port: admin, step-up satisfied,
    /// audited as `console`.
    Console,
    /// A session.
    User(SessionUser),
}

impl Principal {
    /// Whether this principal may use admin routes.
    #[must_use]
    pub fn is_admin(&self) -> bool {
        match self {
            Self::Console => true,
            Self::User(u) => u.role == Role::Admin,
        }
    }

    /// The name the audit records.
    #[must_use]
    pub fn audit_name(&self) -> String {
        match self {
            Self::Console => "console".to_string(),
            Self::User(u) => u.name.clone(),
        }
    }

    /// Whether a credential was used within `window` seconds of `now`.
    #[must_use]
    pub fn stepped_up(&self, now: u64, window: u64) -> bool {
        match self {
            Self::Console => true,
            Self::User(u) => now.saturating_sub(u.last_auth_at) <= window,
        }
    }
}

/// An answer the API sends when it refuses: a status, a stable code the
/// pages switch on, and a sentence for people. Never carries a secret, so
/// it may be `Debug`.
#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    #[must_use]
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

struct Inner {
    data: AuthData,
    /// Why sign-in is off (an unreadable or untrusted store), or `None`.
    disabled: Option<String>,
    /// Session id → last seen, in memory between flushes.
    seen: HashMap<String, u64>,
}

/// The sign-in service. One per process; both routers share it through
/// [`crate::controller::NodeController::auth`].
#[allow(
    dead_code,
    reason = "the ceremony fields are read by the WebAuthn and TOTP flows"
)]
pub struct AuthService {
    pub settings: AuthSettings,
    clock: Clock,
    inner: Mutex<Inner>,
    pub(crate) ceremonies: Mutex<super::ceremony::Table>,
    pub(crate) limits: Mutex<super::limits::Limits>,
    /// Code lockouts for names nobody has.
    pub(crate) decoys: Mutex<super::totp::Decoys>,
    pub(crate) webauthn: super::webauthn::Instances,
    /// Keys the made-up credential ids an unknown name is answered with, so
    /// the same name always gets the same ones while this process runs.
    pub(crate) decoy_key: [u8; 32],
}

fn system_clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl AuthService {
    /// Open the store in `settings.state_dir`. A store that cannot be
    /// trusted leaves sign-in disabled, with the reason in [`Self::disabled`].
    #[must_use]
    pub fn open(settings: AuthSettings) -> Self {
        Self::with_clock(settings, Arc::new(system_clock))
    }

    /// [`Self::open`] with a clock of the caller's.
    #[must_use]
    pub fn with_clock(settings: AuthSettings, clock: Clock) -> Self {
        let (data, disabled) = match store::load(&settings.state_dir) {
            Loaded::Ok(d) => (d, None),
            Loaded::Disabled(why) => (AuthData::default(), Some(why)),
        };
        // Admins sign in with a passkey or key; one from before that rule
        // with only codes keeps them (see `AuthData::codes_sign_in`), so
        // the operator is told here, by name and nothing else.
        let legacy = data.admins_without_a_key();
        if !legacy.is_empty() {
            tracing::warn!(
                target: "astar_server::auth",
                "admin(s) {} sign in with authenticator-app codes only; codes will stop signing an admin in once they add a passkey or security key, so ask them to add one (account page, or `astar-server admin invite --user NAME`)",
                legacy.join(", ")
            );
        }
        Self {
            settings,
            clock,
            inner: Mutex::new(Inner {
                data,
                disabled,
                seen: HashMap::new(),
            }),
            ceremonies: Mutex::new(super::ceremony::Table::default()),
            limits: Mutex::new(super::limits::Limits::default()),
            decoys: Mutex::new(super::totp::Decoys::default()),
            webauthn: super::webauthn::Instances::default(),
            decoy_key: super::session::random_bytes::<32>(),
        }
    }

    /// Now, in Unix seconds.
    #[must_use]
    pub fn now(&self) -> u64 {
        (self.clock)()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("auth store poisoned")
    }

    /// Why sign-in is disabled, or `None`.
    #[must_use]
    pub fn disabled(&self) -> Option<String> {
        self.lock().disabled.clone()
    }

    /// Read the store.
    pub fn read<R>(&self, f: impl FnOnce(&AuthData) -> R) -> R {
        f(&self.lock().data)
    }

    /// Change the store: `f` runs on a copy, the copy is written under the
    /// lock, and only then does it become current.
    ///
    /// # Errors
    /// `f`'s refusal, a disabled store (503), or a failed write (500).
    pub fn mutate<R>(
        &self,
        f: impl FnOnce(&mut AuthData) -> Result<R, Refusal>,
    ) -> Result<R, Refusal> {
        let mut inner = self.lock();
        if inner.disabled.is_some() {
            return Err(Refusal::new(
                503,
                "signin_disabled",
                "Sign-in is disabled on this node; the admin page on the node itself says why.",
            ));
        }
        let mut copy = inner.data.clone();
        let out = f(&mut copy)?;
        let now = self.now();
        copy.sessions.retain(|s| {
            s.expires > now && !idle_expired(s, &inner.seen, now, self.settings.idle_secs)
        });
        store::save(&self.settings.state_dir, &copy).map_err(|e| {
            // The path and the OS's reason, which name nothing secret.
            tracing::warn!(target: "astar_server::auth", "auth store not saved: {e}");
            Refusal::new(
                500,
                "store_write_failed",
                "The sign-in store could not be saved.",
            )
        })?;
        inner.data = copy;
        Ok(out)
    }

    /// Append to the audit ring and save.
    pub fn audit(&self, event: &str, user: Option<&str>, detail: &str, ip: &str) {
        let at = self.now();
        let entry = AuditEntry {
            at,
            event: event.to_string(),
            user: user.map(str::to_string),
            detail: detail.to_string(),
            ip: ip.to_string(),
            times: 0,
            since: None,
        };
        let _ = self.mutate(|d| {
            d.audit(entry);
            Ok(())
        });
    }

    /// Record a failed sign-in or step-up, rolled up per source (see
    /// [`AuthData::audit_failure`]), and save.
    pub fn audit_failure(&self, event: &str, user: Option<&str>, detail: &str, req: &Req<'_>) {
        let entry = AuditEntry {
            at: self.now(),
            event: event.to_string(),
            user: user.map(str::to_string),
            detail: detail.to_string(),
            ip: req.client_key(),
            times: 0,
            since: None,
        };
        let _ = self.mutate(|d| {
            d.audit_failure(entry);
            Ok(())
        });
    }

    /// The principal a request acts as, or `None` for nobody.
    ///
    /// A loopback peer on the control port is the console unless
    /// `loopback_login` is on or a proxy header is present. Anything else
    /// needs a live session cookie of the right name for the scheme.
    #[must_use]
    pub fn principal(&self, req: &Req<'_>) -> Option<Principal> {
        if req.surface == Surface::Control
            && !self.settings.loopback_login
            && crate::access::is_trusted_loopback_peer(req.peer, req.forwarded)
        {
            return Some(Principal::Console);
        }
        let token = session::cookie_value(req.cookie, session::session_cookie(req.tls))?;
        self.session_user(token)
    }

    /// The user behind a session cookie value, touching its last-seen time.
    #[must_use]
    pub fn session_user(&self, token: &str) -> Option<Principal> {
        self.lookup_session(token, true)
    }

    /// [`Self::session_user`] without touching the last-seen time: what an
    /// open stream re-checks, so a forgotten tab does not keep an idle
    /// session alive.
    #[must_use]
    pub fn session_still_valid(&self, token: &str) -> Option<Principal> {
        self.lookup_session(token, false)
    }

    fn lookup_session(&self, token: &str, touch: bool) -> Option<Principal> {
        if !session::is_token(token) {
            return None;
        }
        let hash = session::hash_token(token);
        let now = self.now();
        let idle = self.settings.idle_secs;
        let (principal, flush) = {
            let mut inner = self.lock();
            if inner.disabled.is_some() {
                return None;
            }
            let s = inner.data.sessions.iter().find(|s| s.hash == hash)?.clone();
            if s.expires <= now || idle_expired(&s, &inner.seen, now, idle) {
                return None;
            }
            // The role is read from the user record on every request, so a
            // demotion or deletion takes effect at once.
            let user = inner.data.user(s.user_id)?.clone();
            if touch {
                inner.seen.insert(s.id.clone(), now);
            }
            let flush = touch && now.saturating_sub(s.last_seen) >= LAST_SEEN_FLUSH_SECS;
            (
                Principal::User(SessionUser {
                    user_id: user.id,
                    name: user.name,
                    role: user.role,
                    session_id: s.id,
                    last_auth_at: s.last_auth_at,
                }),
                flush,
            )
        };
        if flush {
            self.flush_last_seen();
        }
        Some(principal)
    }

    /// A session's last-seen time held in memory since the last flush.
    #[must_use]
    pub fn last_seen(&self, session_id: &str) -> Option<u64> {
        self.lock().seen.get(session_id).copied()
    }

    /// Write every in-memory last-seen time to the store: when one has moved
    /// by an hour, and at shutdown.
    pub fn flush_last_seen(&self) {
        let seen = self.lock().seen.clone();
        if seen.is_empty() {
            return;
        }
        let _ = self.mutate(|d| {
            for s in &mut d.sessions {
                if let Some(&t) = seen.get(&s.id) {
                    s.last_seen = s.last_seen.max(t);
                }
            }
            Ok(())
        });
    }

    /// Start a session for `user_id`, signed in with `credential`, inside a
    /// [`Self::mutate`] closure, and return the cookie value. The caller
    /// sets the cookie.
    pub(crate) fn new_session(
        &self,
        d: &mut AuthData,
        user_id: Uuid,
        credential: Option<String>,
        req: &Req<'_>,
    ) -> String {
        let now = self.now();
        let token = session::new_token();
        d.sessions.push(Session {
            id: session::new_id(),
            hash: session::hash_token(&token),
            user_id,
            created: now,
            last_seen: now,
            expires: now + self.settings.max_secs,
            last_auth_at: now,
            user_agent: req
                .user_agent
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect(),
            ip: req.ip(),
            credential,
        });
        token
    }

    /// The `Set-Cookie` for a new session.
    #[must_use]
    pub fn session_set_cookie(&self, token: &str, tls: bool) -> String {
        session::set_cookie(
            session::session_cookie(tls),
            token,
            tls,
            Some(self.settings.max_secs),
        )
    }

    /// Sign a user in for tests that need a session before any credential
    /// ceremony exists: creates the user if needed and returns the cookie
    /// value.
    #[cfg(test)]
    pub fn test_sign_in(&self, name: &str, role: Role) -> String {
        let req = Req {
            method: "POST",
            path: "/",
            query: "",
            body: b"",
            host: None,
            origin: None,
            content_type: None,
            sec_fetch_site: None,
            cookie: None,
            user_agent: Some("test"),
            peer: None,
            forwarded: false,
            surface: Surface::Public,
            tls: true,
        };
        let now = self.now();
        self.mutate(|d| {
            let id = if let Some(u) = d.user_by_name(name) {
                u.id
            } else {
                let id = Uuid::new_v4();
                d.users.push(store::User {
                    id,
                    name: name.to_string(),
                    display_name: name.to_string(),
                    role,
                    created: now,
                    last_sign_in: None,
                    totp: store::TotpFailures::default(),
                });
                id
            };
            Ok(self.new_session(d, id, None, &req))
        })
        .unwrap_or_else(|r| panic!("test sign-in: {}", r.message))
    }
}

/// [`Req::client_key`] for an address.
#[must_use]
pub fn client_key_of(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V4(v4) => v4.to_string(),
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4.to_string();
            }
            let s = v6.segments();
            let prefix = std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0);
            format!("{prefix}/64")
        }
    }
}

/// Whether a session has gone unused for longer than `idle`.
fn idle_expired(s: &Session, seen: &HashMap<String, u64>, now: u64, idle: u64) -> bool {
    let last = seen.get(&s.id).copied().unwrap_or(0).max(s.last_seen);
    now.saturating_sub(last) > idle
}

/// Remove a user with everything that hangs off them: credentials,
/// sessions, their pending re-enrol invites, and the pending invites they
/// made (withdrawn, so the list still shows them).
pub fn remove_user(d: &mut AuthData, id: Uuid) {
    d.withdraw_invites_by(id, false);
    d.users.retain(|u| u.id != id);
    d.credentials.retain(|c| c.user_id != id);
    d.sessions.retain(|s| s.user_id != id);
    d.invites.retain(|i| {
        !matches!(i.target, store::InviteTarget::User { user_id } if user_id == id)
            || i.used_at.is_some()
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A service over a temp dir with a clock the test moves.
    pub(crate) struct Fixture {
        pub svc: AuthService,
        pub clock: Arc<AtomicU64>,
        pub _dir: tempfile::TempDir,
    }

    impl Fixture {
        pub(crate) fn new() -> Self {
            Self::with(|_| {})
        }

        pub(crate) fn with(f: impl FnOnce(&mut AuthSettings)) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mut settings = AuthSettings::defaults(dir.path());
            settings.public_hostnames = vec!["node.example.test".to_string()];
            f(&mut settings);
            let clock = Arc::new(AtomicU64::new(1_800_000_000));
            let c = Arc::clone(&clock);
            let svc = AuthService::with_clock(settings, Arc::new(move || c.load(Ordering::SeqCst)));
            Self {
                svc,
                clock,
                _dir: dir,
            }
        }

        pub(crate) fn advance(&self, secs: u64) {
            self.clock.fetch_add(secs, Ordering::SeqCst);
        }
    }

    pub(crate) fn req(surface: Surface, cookie: Option<&str>) -> Req<'_> {
        Req {
            method: "GET",
            path: "/admin/",
            query: "",
            body: b"",
            host: Some("node.example.test"),
            origin: None,
            content_type: None,
            sec_fetch_site: None,
            cookie,
            user_agent: Some("test browser"),
            peer: Some("203.0.113.9:40000".parse().unwrap()),
            forwarded: false,
            surface,
            tls: surface == Surface::Public,
        }
    }

    fn cookie_for(token: &str) -> String {
        format!("{}={token}", session::SESSION_COOKIE_TLS)
    }

    #[test]
    fn a_session_is_found_by_its_cookie_and_stored_hashed() {
        let f = Fixture::new();
        let token = f.svc.test_sign_in("W1ABC", Role::Admin);
        let c = cookie_for(&token);
        let p = f
            .svc
            .principal(&req(Surface::Public, Some(&c)))
            .expect("signed in");
        assert!(p.is_admin());
        assert_eq!(p.audit_name(), "W1ABC");
        let on_disk =
            std::fs::read_to_string(store::store_path(&f.svc.settings.state_dir)).unwrap();
        assert!(!on_disk.contains(&token), "the cookie is never stored");
        assert!(on_disk.contains(&session::hash_token(&token)));
    }

    #[test]
    fn a_wrong_or_misnamed_cookie_is_nobody() {
        let f = Fixture::new();
        let token = f.svc.test_sign_in("W1ABC", Role::Admin);
        let plain = format!("{}={token}", session::SESSION_COOKIE_PLAIN);
        assert!(
            f.svc
                .principal(&req(Surface::Public, Some(&plain)))
                .is_none(),
            "over TLS only the __Host- cookie counts"
        );
        let other = cookie_for(&session::new_token());
        assert!(
            f.svc
                .principal(&req(Surface::Public, Some(&other)))
                .is_none()
        );
        assert!(
            f.svc
                .principal(&req(Surface::Public, Some("garbage")))
                .is_none()
        );
    }

    #[test]
    fn idle_and_absolute_expiry() {
        let f = Fixture::new();
        let token = f.svc.test_sign_in("W1ABC", Role::Admin);
        let c = cookie_for(&token);
        // Used every six days, it lives past the seven-day idle limit...
        for _ in 0..4 {
            f.advance(6 * 86_400);
            assert!(f.svc.principal(&req(Surface::Public, Some(&c))).is_some());
        }
        // ...but not past the 30-day absolute cap.
        f.advance(6 * 86_400 + 1);
        assert!(f.svc.principal(&req(Surface::Public, Some(&c))).is_none());

        let token = f.svc.test_sign_in("W2DEF", Role::Admin);
        let c = cookie_for(&token);
        f.advance(7 * 86_400 + 1);
        assert!(
            f.svc.principal(&req(Surface::Public, Some(&c))).is_none(),
            "idle for more than seven days"
        );
    }

    #[test]
    fn last_seen_is_flushed_hourly_not_per_request() {
        let f = Fixture::new();
        let token = f.svc.test_sign_in("W1ABC", Role::Admin);
        let c = cookie_for(&token);
        let path = store::store_path(&f.svc.settings.state_dir);
        let before = std::fs::read(&path).unwrap();
        f.advance(60);
        assert!(f.svc.principal(&req(Surface::Public, Some(&c))).is_some());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "a minute later: no write"
        );
        f.advance(LAST_SEEN_FLUSH_SECS);
        assert!(f.svc.principal(&req(Surface::Public, Some(&c))).is_some());
        assert_ne!(
            std::fs::read(&path).unwrap(),
            before,
            "an hour later: written"
        );
    }

    #[test]
    fn the_role_is_read_per_request_and_deleting_a_user_ends_their_sessions() {
        let f = Fixture::new();
        let token = f.svc.test_sign_in("W1ABC", Role::Admin);
        let c = cookie_for(&token);
        let id = f.svc.read(|d| d.user_by_name("W1ABC").unwrap().id);
        f.svc
            .mutate(|d| {
                d.user_mut(id).unwrap().role = Role::Viewer;
                Ok(())
            })
            .ok()
            .unwrap();
        let p = f.svc.principal(&req(Surface::Public, Some(&c))).unwrap();
        assert!(!p.is_admin(), "demoted at once");
        f.svc
            .mutate(|d| {
                remove_user(d, id);
                Ok(())
            })
            .ok()
            .unwrap();
        assert!(f.svc.principal(&req(Surface::Public, Some(&c))).is_none());
        assert!(f.svc.read(|d| d.sessions.is_empty()));
    }

    #[test]
    fn a_loopback_peer_on_the_control_port_is_the_console() {
        let f = Fixture::new();
        let mut r = req(Surface::Control, None);
        r.peer = Some("127.0.0.1:5555".parse().unwrap());
        assert!(matches!(f.svc.principal(&r), Some(Principal::Console)));
        let mut fwd = r;
        fwd.forwarded = true;
        assert!(
            f.svc.principal(&fwd).is_none(),
            "a proxy header ends the bypass"
        );
        let mut public = r;
        public.surface = Surface::Public;
        assert!(
            f.svc.principal(&public).is_none(),
            "never on the public bind"
        );
        let mut lan = r;
        lan.peer = Some("192.168.1.9:5555".parse().unwrap());
        assert!(
            f.svc.principal(&lan).is_none(),
            "a LAN peer needs a session"
        );

        let strict = Fixture::with(|s| s.loopback_login = true);
        assert!(strict.svc.principal(&r).is_none(), "loopback_login = true");
    }

    #[test]
    fn a_disabled_store_trusts_no_session_but_the_console_still_works() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(store::store_path(dir.path()), b"{\"version\": 9}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                store::store_path(dir.path()),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        let svc = AuthService::open(AuthSettings::defaults(dir.path()));
        assert!(svc.disabled().unwrap().contains("version 9"));
        let mut r = req(Surface::Control, None);
        r.peer = Some("127.0.0.1:5555".parse().unwrap());
        assert!(matches!(svc.principal(&r), Some(Principal::Console)));
        assert!(svc.mutate(|_| Ok(())).is_err(), "and nothing is written");
    }

    #[test]
    fn the_client_key_is_an_ipv4_address_or_an_ipv6_64() {
        let key = |p: &str| {
            let mut r = req(Surface::Public, None);
            r.peer = Some(p.parse().unwrap());
            r.client_key()
        };
        assert_eq!(key("203.0.113.9:1"), "203.0.113.9");
        assert_eq!(key("[2001:db8:1:2:aaaa::1]:1"), "2001:db8:1:2::/64");
        assert_eq!(
            key("[2001:db8:1:2:ffff:ffff:ffff:ffff]:1"),
            "2001:db8:1:2::/64",
            "anywhere in one /64 is one client"
        );
        assert_ne!(key("[2001:db8:1:3::1]:1"), key("[2001:db8:1:2::1]:1"));
        assert_eq!(
            key("[::ffff:203.0.113.9]:1"),
            "203.0.113.9",
            "mapped v4 is v4"
        );
        let mut none = req(Surface::Public, None);
        none.peer = None;
        assert_eq!(none.client_key(), "");
    }

    #[test]
    fn step_up_freshness() {
        let f = Fixture::new();
        let token = f.svc.test_sign_in("W1ABC", Role::Admin);
        let c = cookie_for(&token);
        let p = f.svc.principal(&req(Surface::Public, Some(&c))).unwrap();
        assert!(p.stepped_up(f.svc.now(), f.svc.settings.step_up_secs));
        f.advance(f.svc.settings.step_up_secs + 1);
        let p = f.svc.principal(&req(Surface::Public, Some(&c))).unwrap();
        assert!(!p.stepped_up(f.svc.now(), f.svc.settings.step_up_secs));
        assert!(Principal::Console.stepped_up(f.svc.now(), 0));
    }
}
