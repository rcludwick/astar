// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The public listeners: the public bind (HTTPS once a certificate exists,
//! plain HTTP until then) and the optional port-80 listener, each an
//! [`astar_http::Server`] with its own pool.
//!
//! Each accepted connection gets its own thread and exactly one request
//! ([`crate::public::http::handle`]). On the public bind the first byte
//! decides the protocol: a TLS `ClientHello` (`0x16`) is served over TLS
//! when a certificate is loaded; anything else is plain HTTP, which is the
//! page itself before a certificate exists and a redirect to HTTPS after. A
//! renewed certificate is swapped in under the server's TLS config, so the
//! socket is never rebound.

use std::io;
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::public::http::{ChallengeMap, Listening, PUBLIC_CSP, PublicCtx, handle};

pub use crate::public::guard::{MAX_CONNECTIONS, PORT80_CONNECTIONS};

/// The public bind's TLS, when a build has it.
pub trait TlsWrap: Send + Sync {
    /// The certificate slot the server reads on every handshake.
    #[cfg(feature = "public-tls")]
    fn config(&self) -> Arc<astar_http::TlsConfig>;
    /// A certificate is loaded.
    fn ready(&self) -> bool;
}

/// The running listeners. Dropping it does not end their streams; call
/// [`Self::stop`].
pub struct Listeners {
    stop: Arc<AtomicBool>,
    public: astar_http::Server,
    http80: Option<astar_http::Server>,
}

/// Bind one listener, naming the address in the error.
fn bind<H>(
    addr: &str,
    limits: astar_http::Limits,
    options: astar_http::ServerOptions,
    handler: H,
) -> io::Result<astar_http::Server>
where
    H: for<'a> Fn(&mut astar_http::Request<'a>) -> astar_http::Reply + Send + Sync + 'static,
{
    astar_http::Server::bind_with(addr, limits, options, handler)
        .map_err(|e| io::Error::new(e.kind(), format!("cannot bind {addr}: {e}")))
}

impl Listeners {
    /// Bind `public_bind` (and `http80_bind`, when given) and start
    /// accepting. `tls` is `None` on a build without `public-tls`, or when
    /// TLS is not wanted: then the public bind is plain HTTP for good.
    ///
    /// # Errors
    /// A bind that fails.
    pub fn start(
        public_bind: &str,
        http80_bind: Option<&str>,
        ctx: &Arc<PublicCtx>,
        challenges: &ChallengeMap,
        tls: Option<Arc<dyn TlsWrap>>,
    ) -> io::Result<Self> {
        let limits = ctx.guard.limits;
        // Both listeners count streams against the same per-address share.
        let options = |name| astar_http::ServerOptions {
            name,
            csp: Some(PUBLIC_CSP),
            streams: Some(Arc::clone(&ctx.guard.streams)),
            ..astar_http::ServerOptions::default()
        };
        #[cfg_attr(not(feature = "public-tls"), allow(unused_mut))]
        let mut public_options = options("public");
        #[cfg(feature = "public-tls")]
        {
            public_options.tls = tls.as_ref().map(|t| t.config());
        }
        let public = bind(public_bind, limits.http(false), public_options, {
            let (ctx, ch) = (Arc::clone(ctx), Arc::clone(challenges));
            move |req| {
                // Plain HTTP on the HTTPS port once a certificate is live:
                // the same answer port 80 gives, so a typed http:// URL
                // still lands.
                let listening = if !req.tls() && tls.as_ref().is_some_and(|t| t.ready()) {
                    Listening::Port80 { challenges: &ch }
                } else {
                    Listening::Public
                };
                handle(&ctx, req, &listening)
            }
        })?;
        // Its own server and pool: a full public bind never blocks the CA's
        // check.
        let http80 = http80_bind
            .map(|addr| {
                let (ctx, ch) = (Arc::clone(ctx), Arc::clone(challenges));
                bind(
                    addr,
                    limits.http(true),
                    options("public-http80"),
                    move |req| handle(&ctx, req, &Listening::Port80 { challenges: &ch }),
                )
            })
            .transpose()?;
        Ok(Self {
            stop: Arc::clone(&ctx.stop),
            public,
            http80,
        })
    }

    /// The public bind's real address (an ephemeral port resolved).
    #[must_use]
    pub const fn public_addr(&self) -> SocketAddr {
        self.public.local_addr()
    }

    /// The port-80 listener's real address, when there is one.
    #[must_use]
    pub fn http80_addr(&self) -> Option<SocketAddr> {
        self.http80.as_ref().map(astar_http::Server::local_addr)
    }

    /// Stop accepting and wait for the accept threads. Open streams end at
    /// their next write.
    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        self.public.stop();
        if let Some(s) = self.http80 {
            s.stop();
        }
    }
}

// ---- the whole public side --------------------------------------------------------

/// The public listener's TLS state, for the admin snapshot and page. No
/// key material and no credential: hosts, a date, and an error sentence.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct PublicTlsStatus {
    /// The expanded hostnames the page answers to.
    pub hosts: Vec<String>,
    /// `"http-01"` or `"dns-01"`.
    pub challenge: String,
    /// The served certificate's expiry.
    pub not_after_unix: Option<u64>,
    /// Why the last issue or renewal failed, until one succeeds.
    pub last_error: Option<String>,
    /// HTTPS is being served.
    pub serving_tls: bool,
    /// Built with `public-tls`; without it the public page is plain HTTP.
    pub tls_built: bool,
    /// Why the last dynamic-DNS pass failed, until one succeeds.
    pub dns_error: Option<String>,
}

/// The listeners plus, with `public-tls`, the certificate thread.
pub struct PublicServer {
    listeners: Listeners,
    status: Arc<std::sync::Mutex<PublicTlsStatus>>,
    #[cfg(feature = "public-tls")]
    tls: Option<crate::public::tls::TlsManager>,
    stop: Arc<AtomicBool>,
}

/// Where the ACME directory comes from: the config's choice of Let's
/// Encrypt, or — in tests only — `ASTAR_ACME_DIRECTORY`.
#[must_use]
pub fn acme_directory(web: &crate::config::WebCfg, override_url: Option<&str>) -> String {
    if let Some(u) = override_url {
        return u.to_string();
    }
    #[cfg(any(test, feature = "test-support"))]
    if let Ok(u) = std::env::var("ASTAR_ACME_DIRECTORY") {
        return u;
    }
    #[cfg(feature = "public-tls")]
    {
        if web.acme_staging {
            crate::public::acme::LE_STAGING.to_string()
        } else {
            crate::public::acme::LE_PROD.to_string()
        }
    }
    #[cfg(not(feature = "public-tls"))]
    {
        let _ = web;
        String::new()
    }
}

impl PublicServer {
    /// Start the public side for `web`: the port-80 listener (when there is
    /// one), the public bind, and the certificate thread. Until a
    /// certificate exists the public bind serves plain HTTP.
    ///
    /// `dns01` is the DNS-01 solver when `web` uses DNS-01 (the Porkbun
    /// client, built by the caller from `[web.dns]` and the credentials).
    ///
    /// # Errors
    /// No `public_bind`, or a bind that fails.
    #[allow(
        unused_variables,
        reason = "the TLS arguments are inert without public-tls"
    )]
    pub fn start(
        web: &crate::config::WebCfg,
        state_dir: &std::path::Path,
        ctx: &Arc<PublicCtx>,
        acme_dir_override: Option<&str>,
        dns01: Option<&Arc<dyn crate::public::DnsSolver>>,
    ) -> io::Result<Self> {
        Self::start_with_status(web, state_dir, ctx, acme_dir_override, dns01, None)
    }

    /// [`Self::start`], reporting into `status` when given (the dynamic-DNS
    /// thread writes into the same one).
    ///
    /// # Errors
    /// No `public_bind`, or a bind that fails.
    #[allow(
        unused_variables,
        reason = "the TLS arguments are inert without public-tls"
    )]
    pub fn start_with_status(
        web: &crate::config::WebCfg,
        state_dir: &std::path::Path,
        ctx: &Arc<PublicCtx>,
        acme_dir_override: Option<&str>,
        dns01: Option<&Arc<dyn crate::public::DnsSolver>>,
        status: Option<Arc<std::sync::Mutex<PublicTlsStatus>>>,
    ) -> io::Result<Self> {
        let bind = web
            .public_bind
            .as_deref()
            .ok_or_else(|| io::Error::other("[web] public_bind is not set"))?;
        let challenges: ChallengeMap =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::default()));
        let status =
            status.unwrap_or_else(|| Arc::new(std::sync::Mutex::new(PublicTlsStatus::default())));
        {
            let mut s = status.lock().expect("status poisoned");
            s.hosts = web.hostnames().to_vec();
            s.challenge = web.challenge().as_str().to_string();
            s.tls_built = cfg!(feature = "public-tls");
        }

        #[cfg(feature = "public-tls")]
        {
            use crate::config::AcmeChallenge;
            use crate::public::acme::{AcmeClient, CertStore, Solver};
            use crate::public::tls::{Issuer, TlsManager, TlsState};
            let tls_state = TlsState::new();
            let solver = match web.challenge() {
                AcmeChallenge::Http01 => Some(Solver::Http01(Arc::clone(&challenges))),
                AcmeChallenge::Dns01 => dns01.map(|d| Solver::Dns01(Arc::clone(d))),
            };
            let listeners = Listeners::start(
                bind,
                web.http_bind(),
                ctx,
                &challenges,
                Some(Arc::clone(&tls_state) as Arc<dyn TlsWrap>),
            )?;
            let tls = if let Some(solver) = solver {
                Some(TlsManager::start(
                    Issuer {
                        client: AcmeClient::new(&acme_directory(web, acme_dir_override)),
                        email: web.acme_email.clone().filter(|e| !e.is_empty()),
                        hosts: web.hostnames().to_vec(),
                        solver,
                        store: CertStore::new(state_dir),
                    },
                    tls_state,
                    Arc::clone(&ctx.tls),
                    Arc::clone(&status),
                    Arc::clone(&ctx.stop),
                ))
            } else {
                status.lock().expect("status poisoned").last_error = Some(
                    "dns-01 needs the DNS provider's API keys, and they are not set".to_string(),
                );
                None
            };
            Ok(Self {
                listeners,
                status,
                tls,
                stop: Arc::clone(&ctx.stop),
            })
        }
        #[cfg(not(feature = "public-tls"))]
        {
            tracing::warn!(
                "public page: this build has no public-tls feature; serving plain HTTP on {bind}"
            );
            status.lock().expect("status poisoned").last_error =
                Some("this build has no public-tls feature: plain HTTP only".to_string());
            let listeners = Listeners::start(bind, web.http_bind(), ctx, &challenges, None)?;
            Ok(Self {
                listeners,
                status,
                stop: Arc::clone(&ctx.stop),
            })
        }
    }

    /// The TLS state now.
    #[must_use]
    pub fn status(&self) -> PublicTlsStatus {
        self.status.lock().expect("status poisoned").clone()
    }

    /// A handle the admin snapshot reads.
    #[must_use]
    pub fn status_handle(&self) -> Arc<std::sync::Mutex<PublicTlsStatus>> {
        Arc::clone(&self.status)
    }

    #[must_use]
    pub fn public_addr(&self) -> SocketAddr {
        self.listeners.public_addr()
    }

    #[must_use]
    pub fn http80_addr(&self) -> Option<SocketAddr> {
        self.listeners.http80_addr()
    }

    /// Stop everything and wait for the threads.
    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        self.listeners.stop();
        #[cfg(feature = "public-tls")]
        if let Some(t) = self.tls {
            t.join();
        }
    }
}
