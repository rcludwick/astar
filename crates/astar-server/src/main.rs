// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `astar-server` — always-on node daemon.
//!
//! Usage:
//!   astar-server serve --config <path>
//!   astar-server tui   --config <path>
//!
//! Loads a TOML config, builds a [`NodeController`], applies inbound + optional
//! registration, then either runs the HTTP+SSE serve loop or the interactive TUI.

use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use astar_server::{
    command::NodeCommand,
    config::NodeFileConfig,
    controller::NodeController,
    run::{install_signal_handler, run_serve},
    secrets::SecretProvider,
    tui::run_tui,
};
use astar_station::ConsoleSession;

/// How often the `WireGuard` status logger samples
/// [`astar_iax::Manager::wg_status`] (handshake age, traffic counters).
const WG_STATUS_INTERVAL: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Entry-point
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn main() -> ExitCode {
    let mut args = std::env::args().skip(1).peekable();

    let Some(subcommand) = args.next() else {
        eprintln!("astar-server: use `serve` or `tui` (see --help)");
        return ExitCode::FAILURE;
    };

    if matches!(subcommand.as_str(), "-h" | "--help" | "help") {
        print_usage();
        return ExitCode::SUCCESS;
    }

    if !matches!(subcommand.as_str(), "serve" | "tui") {
        eprintln!("astar-server: unknown subcommand {subcommand:?}");
        eprintln!("Use `serve` or `tui`.");
        return ExitCode::FAILURE;
    }

    // Parse --config <path>
    let config_path = match parse_config_flag(&mut args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("astar-server: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Load the TOML config — this is the shared load path for both `serve`
    // and `tui` (subcommand dispatch happens further below). If the path
    // does not exist, bootstraps a commented template there and continues
    // serving with its safe defaults (iax-4703 Task 9) instead of exiting,
    // which would crashloop under `--restart=always`.
    let cfg = match NodeFileConfig::load_or_bootstrap(std::path::Path::new(&config_path)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("astar-server: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Build InboundConfig / RegisterConfig eagerly — fail fast before touching audio.
    let inbound_cfg = match cfg.to_inbound() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("astar-server: {e}");
            return ExitCode::FAILURE;
        }
    };
    let register_cfg = match cfg.to_register() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("astar-server: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Build SecretProvider and load credentials per `[secrets] source`.
    let secrets = SecretProvider::new();
    if cfg.secrets.source.eq_ignore_ascii_case("env") {
        secrets.load_env();
    } else if cfg.secrets.source.eq_ignore_ascii_case("config") {
        // iax-4703: inline secret carried straight in node.toml. Username is
        // register.node_id (same identity the env path registers as).
        if let Some(reg) = &cfg.register {
            secrets.load_config_secret(&reg.node_id, cfg.secrets.secret.as_deref().unwrap_or(""));
        }
    }

    // Link transport (iax-580b): a `[wireguard]` section selects the userspace
    // WG stack for the WHOLE engine (outgoing + registrar + inbound); absent =
    // plain UDP. Built eagerly — fail fast on a bad section.
    let link_transport = match cfg.to_link_transport() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("astar-server: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Build the Station over a session Arc the node keeps (iax-580b): the
    // WireGuard cutover below needs engine-level access (`ensure_engine` →
    // `Manager::set_link_transport`) that the Station does not surface yet.
    // With a fresh session this is behaviorally identical to `Station::new` /
    // `Station::with_backend_factory`.
    //
    // `[audio] backend = "none"` selects the hardware-free NullBackend
    // (headless VPS/container); the default is the real CpalBackend.
    // `codec_policy` (iax-31f7) threads through either way so node outbound
    // links (e.g. `/connect`) honor the configured policy too. Inbound gets it
    // via `inbound_cfg.policy`, which — at `EnableInbound` below, the first
    // engine-building call on this path — also pins the station audio pipeline
    // rate (iax-4348: 16 kHz for `prefer_slin16`, else 8 kHz).
    let session = Arc::new(Mutex::new(ConsoleSession::new()));
    // `[portal]` → `StationConfig.portal` (iax-b7f2): the account password is
    // resolved from the NAMED env var at startup — never from the config file
    // — and consumed straight into the station config (never logged). Absent
    // section or unset env var ⇒ no minting; `wt-guest` link dials then fall
    // back to a tokenless CALLING_NAME, which WT contexts reject.
    let portal = cfg
        .portal
        .as_ref()
        .and_then(|p| match std::env::var(&p.credential_env) {
            Ok(pw) if !pw.is_empty() => Some(astar_station::PortalCredentials {
                user: p.user.clone(),
                password: pw,
                node: p.node.clone(),
            }),
            _ => {
                eprintln!(
                    "astar-server: [portal] configured but {} is unset — \
                     wt-guest links will dial without a WT token",
                    p.credential_env
                );
                None
            }
        });
    let station_cfg = astar_station::StationConfig {
        codec_policy: cfg.codec_policy,
        portal,
        ..astar_station::StationConfig::default()
    };
    let station = match cfg.audio.as_ref().and_then(|a| a.backend.as_deref()) {
        Some("none") => astar_station::Station::with_shared_session(
            station_cfg,
            Arc::clone(&session),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        ),
        _ => astar_station::Station::with_shared_session(
            station_cfg,
            Arc::clone(&session),
            Box::new(|| Box::new(astar_audio::CpalBackend::new())),
        ),
    };

    // Apply audio device selection from config.
    if let Some(audio) = &cfg.audio {
        station.set_devices(audio.input.clone(), audio.output.clone());
    }

    // Apply the conference-bridge mode (iax-647d). The DAEMON default is
    // `mode = "bridge"` (pure mix-minus, local radio off) even with no
    // `[bridge]` section — distinct from the library handset default. Validated
    // already by `from_toml_str`, so this cannot fail here.
    let bridge_cfg = cfg.to_bridge_config().expect("validated at load");
    if let Err(e) = station.set_bridge_config(bridge_cfg) {
        eprintln!("astar-server: failed to set bridge mode: {e}");
        return ExitCode::FAILURE;
    }

    // Per-target link dial shapes (iax-5029): built once from the `[links]`
    // table so `handle_link` never re-parses config at dial time.
    let link_shapes = cfg
        .links
        .keys()
        .map(|n| (n.clone(), cfg.link_shape(n)))
        .collect();

    // Build the NodeController — installs the secret resolver internally.
    let ctrl = NodeController::with_configs(
        station,
        secrets,
        inbound_cfg,
        register_cfg.clone(),
        cfg.announce.clone(),
        link_shapes,
        cfg.dtmf_enabled(),
        cfg.dtmf_inter_digit_timeout_ms(),
    );

    // Enable the inbound listener.
    if let Err(e) = ctrl.execute(NodeCommand::EnableInbound) {
        eprintln!("astar-server: failed to enable inbound: {}", e.message);
        return ExitCode::FAILURE;
    }

    // WireGuard cutover (iax-580b). Ordering matters: `EnableInbound` above
    // built the engine with the config's codec policy (pinning the pipeline
    // rate, iax-4348); the transport is set on THAT engine, then the listener
    // is bounced so everything (re)built from here on rides the shared stack.
    // Registration below starts only after the cutover.
    if matches!(link_transport, astar_iax::LinkTransport::Wireguard(_)) {
        // Secret-free config (iax-8516): the private key is resolved from the
        // env var named by `[wireguard] secret_ref` (default
        // WIREGUARD_PRIVATE_KEY), here, once — never stored in the config.
        let resolve_secret = |name: &str| std::env::var(name).unwrap_or_default();
        {
            let mut sess = session.lock().unwrap();
            debug_assert!(sess.has_engine(), "EnableInbound built the engine");
            let mgr = sess.ensure_engine(null_backend);
            if let Err(e) = mgr.set_link_transport(link_transport, &resolve_secret) {
                eprintln!("astar-server: wireguard: {e}");
                return ExitCode::FAILURE;
            }
        }
        // Bounce the listener so it is rebuilt against the tunnel-aware engine.
        if let Err(e) = ctrl.execute(NodeCommand::DisableInbound) {
            eprintln!("astar-server: wireguard: {}", e.message);
            return ExitCode::FAILURE;
        }
        if let Err(e) = ctrl.execute(NodeCommand::EnableInbound) {
            eprintln!(
                "astar-server: wireguard: failed to re-enable inbound: {}",
                e.message
            );
            return ExitCode::FAILURE;
        }
        eprintln!("astar-server: wireguard link transport up (userspace stack)");
        spawn_wg_status_logger(Arc::clone(&session));
    }

    // Register with the upstream registrar if configured.
    if register_cfg.is_some()
        && let Err(e) = ctrl.execute(NodeCommand::Register)
    {
        eprintln!("astar-server: registration failed: {}", e.message);
        // Non-fatal — continue serving.
    }

    match subcommand.as_str() {
        "serve" => run_serve_mode(ctrl, &cfg.control.bind),
        "tui" => {
            run_tui(&ctrl);
            ExitCode::SUCCESS
        }
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// WireGuard helpers (iax-580b)
// ---------------------------------------------------------------------------

/// Fallback backend factory for `ensure_engine` calls made AFTER the engine
/// exists (the factory is not invoked then — `ensure_engine` is idempotent).
fn null_backend() -> Box<dyn astar_audio::AudioBackend> {
    Box::new(astar_audio::NullBackend::new())
}

/// Periodically log the `WireGuard` stack status (handshake age, tx/rx/drop
/// counters) via `tracing` — the design's operational visibility hook (no
/// `AppEvent` variants in v1). Detached daemon thread; dies with the process.
fn spawn_wg_status_logger(session: Arc<Mutex<ConsoleSession>>) {
    std::thread::Builder::new()
        .name("iax-node-wg-status".into())
        .spawn(move || {
            loop {
                std::thread::sleep(WG_STATUS_INTERVAL);
                let status = {
                    let mut sess = session.lock().unwrap();
                    sess.has_engine()
                        .then(|| sess.ensure_engine(null_backend).wg_status())
                        .flatten()
                };
                if let Some(s) = status {
                    tracing::info!(
                        handshake_age_secs = s.last_handshake_age.map(|d| d.as_secs()),
                        tx_packets = s.tx_packets,
                        rx_packets = s.rx_packets,
                        dropped_packets = s.dropped_packets,
                        "wireguard status"
                    );
                }
            }
        })
        .expect("spawn wg-status logger thread");
}

// ---------------------------------------------------------------------------
// serve mode
// ---------------------------------------------------------------------------

fn run_serve_mode(ctrl: NodeController, bind: &str) -> ExitCode {
    let ctrl = Arc::new(ctrl);
    let stop = ctrl.stop_flag();

    // Install SIGINT/SIGTERM handler — sets stop flag + executes Shutdown.
    install_signal_handler(Arc::clone(&ctrl), Arc::clone(&stop));

    // Bind the HTTP control server; fail fast if the address is unavailable.
    let bind_str = bind.to_string();
    let server = match tiny_http::Server::http(&bind_str) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("astar-server: cannot bind control socket {bind_str:?}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let listen_addr = server
        .server_addr()
        .to_ip()
        .map_or_else(|| bind_str.clone(), |a| a.to_string());
    eprintln!("astar-server: HTTP control listening on http://{listen_addr}");

    // run_serve drives the accept loop and graceful teardown.
    run_serve(&ctrl, &server, &stop);

    eprintln!("astar-server: shutdown complete");
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// Arg-parsing helpers
// ---------------------------------------------------------------------------

fn parse_config_flag(args: &mut dyn Iterator<Item = String>) -> Result<String, String> {
    while let Some(arg) = args.next() {
        if arg == "--config" || arg == "-c" {
            return args
                .next()
                .ok_or_else(|| "--config requires a path argument".to_string());
        }
        if let Some(path) = arg.strip_prefix("--config=") {
            return Ok(path.to_string());
        }
    }
    Err("missing --config <path>".to_string())
}

fn print_usage() {
    println!(
        "astar-server — AllStarLink node daemon\n\
        \n\
        USAGE:\n\
        \x20   astar-server serve --config <path.toml>\n\
        \x20   astar-server tui   --config <path.toml>\n\
        \n\
        SUBCOMMANDS:\n\
        \x20   serve   Run HTTP+SSE control server (background-friendly)\n\
        \x20   tui     Interactive stdin menu\n\
        \n\
        OPTIONS:\n\
        \x20   --config <path>   Path to the node TOML config file\n\
        "
    );
}
