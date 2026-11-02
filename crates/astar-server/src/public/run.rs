// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Starting and stopping the whole public side from `node.toml`: the
//! activity pump, the public listeners and their certificate, and dynamic
//! DNS. `main.rs` calls [`PublicSide::start`] once the controller is shared,
//! and [`PublicSide::stop`] after the control server returns.

use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::config::NodeFileConfig;
use crate::controller::NodeController;
use crate::public::http::{PublicCtx, StreamSource, now_ms};
use crate::public::serve::{PublicServer, PublicTlsStatus};
use crate::public::status::Activity;

/// How often the activity pump reads the snapshot.
pub const PUMP_EVERY: Duration = Duration::from_millis(250);

/// The running public side. Every part is optional: a node with no `[web]`
/// has none of it.
pub struct PublicSide {
    server: Option<PublicServer>,
    #[cfg(feature = "public-tls")]
    ddns: Option<crate::public::dns::Ddns>,
    pump: Option<JoinHandle<()>>,
    #[cfg(feature = "stream")]
    tap: Option<Arc<crate::stream::StreamHub>>,
    stop: Arc<AtomicBool>,
    ctx: Option<Arc<PublicCtx>>,
}

/// Feed the activity tracker from the snapshot every [`PUMP_EVERY`].
fn spawn_pump(
    ctrl: Arc<NodeController>,
    activity: Arc<Mutex<Activity>>,
    stop: Arc<AtomicBool>,
) -> Result<JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name("public-activity".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) && !ctrl.should_stop() {
                let snap = ctrl.snapshot();
                activity
                    .lock()
                    .expect("activity poisoned")
                    .observe(&snap, now_ms());
                std::thread::sleep(PUMP_EVERY);
            }
        })
        .map_err(|e| format!("cannot start the activity pump: {e}"))
}

impl PublicSide {
    /// Start what `cfg` asks for. `hub` replaces the stream tap (tests);
    /// `None` builds it from `[stream]` — a tap on `ctrl`'s conference when
    /// `enabled = true`, nothing otherwise. `acme_dir_override` replaces Let's Encrypt (tests).
    ///
    /// # Errors
    /// A public bind that cannot be bound; a sentence for stderr.
    pub fn start(
        cfg: &NodeFileConfig,
        state_dir: &Path,
        ctrl: &Arc<NodeController>,
        hub: Option<Arc<dyn StreamSource>>,
        acme_dir_override: Option<&str>,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let mut side = Self {
            server: None,
            #[cfg(feature = "public-tls")]
            ddns: None,
            pump: None,
            #[cfg(feature = "stream")]
            tap: None,
            stop: Arc::clone(&stop),
            ctx: None,
        };
        let Some(web) = cfg.web.as_ref() else {
            return Ok(side);
        };
        let status = Arc::new(Mutex::new(PublicTlsStatus {
            hosts: web.hostnames().to_vec(),
            challenge: web.challenge().as_str().to_string(),
            tls_built: cfg!(feature = "public-tls"),
            ..PublicTlsStatus::default()
        }));
        ctrl.attach_public_status(Arc::clone(&status));

        // The DNS provider: dynamic DNS and the DNS-01 solver share one client.
        #[cfg(feature = "public-tls")]
        let dns01 = match web.dns.as_ref() {
            None => None,
            Some(dns) => match crate::public::dns::from_config(dns, ctrl.secrets()) {
                Ok(setup) => {
                    if dns.ddns {
                        if let Some(ip) = setup.ip.clone() {
                            side.ddns = Some(crate::public::dns::Ddns::start(
                                Arc::clone(&setup.provider),
                                ip,
                                web.hostnames().to_vec(),
                                Duration::from_secs(dns.ddns_interval_secs),
                                Arc::clone(&stop),
                                Some(Arc::clone(&status)),
                            ));
                        } else {
                            let e = "web.dns: ddns needs an ip_source".to_string();
                            eprintln!("astar-server: {e}");
                            status.lock().expect("status poisoned").dns_error = Some(e);
                        }
                    }
                    Some(Arc::new(crate::public::dns::Dns01::new(setup.provider))
                        as Arc<dyn crate::public::DnsSolver>)
                }
                Err(e) => {
                    eprintln!("astar-server: web.dns: {e}");
                    status.lock().expect("status poisoned").dns_error = Some(e);
                    None
                }
            },
        };
        #[cfg(not(feature = "public-tls"))]
        let dns01: Option<Arc<dyn crate::public::DnsSolver>> = {
            if web.dns.is_some() {
                let e = "this build has no public-tls feature: [web.dns] is inert".to_string();
                eprintln!("astar-server: web.dns: {e}");
                status.lock().expect("status poisoned").dns_error = Some(e);
            }
            None
        };

        if web.public_bind.is_none() {
            return Ok(side);
        }
        let hub = match hub {
            Some(h) => Some(h),
            None => side.start_tap(cfg, ctrl),
        };
        let activity = Arc::new(Mutex::new(Activity::new(Instant::now())));
        let ctx = Arc::new(PublicCtx {
            ctrl: Arc::clone(ctrl),
            hub,
            activity: Arc::clone(&activity),
            hostnames: web.hostnames().to_vec(),
            tls: Arc::new(AtomicBool::new(false)),
            stop: Arc::clone(&stop),
            anonymous_status: web.anonymous_status(),
            guard: crate::public::guard::Guard::default(),
        });
        side.pump = Some(spawn_pump(Arc::clone(ctrl), activity, Arc::clone(&stop))?);
        let server = PublicServer::start_with_status(
            web,
            state_dir,
            &ctx,
            acme_dir_override,
            dns01.as_ref(),
            Some(status),
        )
        .map_err(|e| format!("web.public_bind: {e}"))?;
        eprintln!(
            "astar-server: public page on {} for {}",
            server.public_addr(),
            web.hostnames().join(", ")
        );
        side.server = Some(server);
        side.ctx = Some(ctx);
        Ok(side)
    }

    /// The stream tap for `[stream] enabled = true`, on a build with the
    /// `stream` feature. The tap itself refuses handset and parrot modes.
    #[cfg_attr(
        not(feature = "stream"),
        allow(
            clippy::unused_self,
            reason = "the tap lives in self only with streams"
        )
    )]
    fn start_tap(
        &mut self,
        cfg: &NodeFileConfig,
        ctrl: &Arc<NodeController>,
    ) -> Option<Arc<dyn StreamSource>> {
        let stream = cfg.stream.as_ref().filter(|s| s.enabled)?;
        #[cfg(feature = "stream")]
        {
            let hub = crate::stream::StreamHub::start(
                stream,
                Arc::clone(ctrl) as Arc<dyn crate::stream::TapSource>,
            );
            self.tap = Some(Arc::clone(&hub));
            Some(hub as Arc<dyn StreamSource>)
        }
        #[cfg(not(feature = "stream"))]
        {
            let _ = (stream, ctrl);
            eprintln!("astar-server: stream: this build has no stream feature; [stream] is inert");
            None
        }
    }

    /// The public bind's address, when there is one.
    #[must_use]
    pub fn public_addr(&self) -> Option<std::net::SocketAddr> {
        self.server.as_ref().map(PublicServer::public_addr)
    }

    /// The port-80 listener's address, when there is one.
    #[must_use]
    pub fn http80_addr(&self) -> Option<std::net::SocketAddr> {
        self.server.as_ref().and_then(PublicServer::http80_addr)
    }

    /// Stop every thread and wait for them.
    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(s) = self.server {
            s.stop();
        }
        #[cfg(feature = "public-tls")]
        if let Some(d) = self.ddns {
            d.join();
        }
        if let Some(p) = self.pump {
            let _ = p.join();
        }
        #[cfg(feature = "stream")]
        if let Some(t) = self.tap {
            t.stop();
        }
        drop(self.ctx);
    }
}
