// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `astar-server` — always-on node daemon.
//!
//! Usage:
//!   astar-server serve --config <path>
//!   astar-server tui   --config <path>
//!   astar-server vocoder list --config <path>
//!   astar-server vocoder add <serial> --config <path> [--yes]
//!
//! Loads a TOML config, builds a [`NodeController`], applies inbound + optional
//! registration, then either runs the HTTP+SSE serve loop or the interactive TUI.

use std::process::ExitCode;
use std::sync::{Arc, Mutex, atomic::Ordering};
use std::time::Duration;

use astar_server::{
    command::{DiscordAction, NodeCommand},
    config::NodeFileConfig,
    controller::NodeController,
    m17_directory::M17Settings,
    run::{install_signal_handler, reexec_self, run_serve},
    secrets::SecretProvider,
    settings::RedactedConfig,
    settings_store::SettingsStore,
    tui::run_tui,
};
use astar_station::ConsoleSession;

/// Install the tracing subscriber.
///
/// Without this the whole engine's `tracing` output is discarded: every
/// `warn!`/`info!` in astar-iax, astar-console and this crate compiles to a
/// no-op dispatch and goes nowhere. A node ran for two days emitting exactly
/// one line — a `println!` — which is why an inbound-call failure left no
/// evidence at all and had to be diagnosed by reading source.
///
/// `RUST_LOG` overrides; the default is `info`, which is the call-setup
/// narrative (offer, codec choice, accept/reject, hangup) and nothing
/// per-frame. Timestamps and no ANSI, because this lands in journald via
/// podman rather than a terminal.
fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_ansi(false)
        .init();
}

/// How often the `WireGuard` status logger samples
/// [`astar_iax::Manager::wg_status`] (handshake age, traffic counters).
const WG_STATUS_INTERVAL: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Entry-point
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn main() -> ExitCode {
    init_logging();

    let mut args = std::env::args().skip(1).peekable();

    let Some(subcommand) = args.next() else {
        eprintln!("astar-server: use `serve` or `tui` (see --help)");
        return ExitCode::FAILURE;
    };

    if matches!(subcommand.as_str(), "-h" | "--help" | "help") {
        print_usage();
        return ExitCode::SUCCESS;
    }
    if matches!(subcommand.as_str(), "-V" | "--version" | "version") {
        println!("astar-server {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if subcommand == "--features" {
        println!("{}", astar_server::build_info::FEATURES.join(" "));
        return ExitCode::SUCCESS;
    }
    if subcommand == "--print-systemd-unit" {
        print!("{}", astar_server::build_info::SYSTEMD_UNIT);
        return ExitCode::SUCCESS;
    }

    // `admin`: invites, users and sessions from the console — the way in
    // for the first admin, and when every credential is lost.
    if subcommand == "admin" {
        let rest: Vec<String> = args.collect();
        #[cfg(feature = "auth")]
        return astar_server::auth::cli::run(&rest);
        #[cfg(not(feature = "auth"))]
        {
            let _ = rest;
            eprintln!(
                "astar-server: admin: this build has no auth feature (sign-in is not built in)"
            );
            return ExitCode::FAILURE;
        }
    }

    if subcommand == "vocoder" {
        return run_vocoder(args.collect());
    }

    if !matches!(subcommand.as_str(), "serve" | "tui") {
        eprintln!("astar-server: unknown subcommand {subcommand:?}");
        eprintln!("Use `serve`, `tui`, `admin` or `vocoder`.");
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

    // Build SecretProvider and load credentials per `[secrets] source`
    // (NodeFileConfig::load_credentials, where the rules and their tests
    // live). A credentials file that cannot be loaded is one line naming the
    // path and the fix, and the node boots with no credentials, which the
    // settings page then shows as "not set".
    let secrets = SecretProvider::new();
    match cfg.load_credentials(std::path::Path::new(&config_path), &secrets) {
        Some(Ok((path, n))) => eprintln!(
            "astar-server: loaded {n} credential(s) from {}",
            path.display()
        ),
        Some(Err(e)) => eprintln!("astar-server: {e}"),
        None => {}
    }
    if let Some(w) = cfg.inline_credentials_mode_warning(std::path::Path::new(&config_path)) {
        eprintln!("astar-server: {w}");
    }
    // `ASTAR_ECHOLINK_PASSWORD` → `echolink:<CALL>`, under any source, the
    // way the portal password is seeded. Only with an `[echolink]` section,
    // whose callsign is already uppercased and suffix-checked.
    if let Some(e) = cfg.echolink.as_ref() {
        secrets.load_echolink_env(&e.callsign);
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
    // read from the provider under every source (NodeFileConfig::
    // portal_credentials) and consumed straight into the station config,
    // never logged, never from node.toml. Absent section or unset credential
    // ⇒ no minting; `wt-guest` link dials then fall back to a tokenless
    // CALLING_NAME, which WT contexts reject.
    let portal = match cfg.portal_credentials(&secrets) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("astar-server: {e}");
            None
        }
    };
    // `[vocoder] trusted_serials` (on-air safety): the only AMBE sticks the
    // engine may open. Absent ⇒ none, and nothing is ever scanned for.
    let station_cfg = astar_station::StationConfig {
        codec_policy: cfg.codec_policy,
        portal,
        vocoder_trust: cfg.vocoder_trust(),
        ..astar_station::StationConfig::default()
    };
    let station = match cfg.audio.as_ref().and_then(|a| a.backend.as_deref()) {
        Some("none") => astar_station::Station::with_shared_session(
            station_cfg,
            Arc::clone(&session),
            Box::new(|| Box::new(astar_audio::NullBackend::new())),
        ),
        #[cfg(feature = "device-audio")]
        _ => astar_station::Station::with_shared_session(
            station_cfg,
            Arc::clone(&session),
            Box::new(|| Box::new(astar_audio::CpalBackend::new())),
        ),
        // The single static release has no sound-device stack: a node server
        // runs `backend = "none"`, and this build says so instead of failing.
        #[cfg(not(feature = "device-audio"))]
        _ => {
            eprintln!(
                "astar-server: this build has no device-audio feature; using [audio] backend = \"none\""
            );
            astar_station::Station::with_shared_session(
                station_cfg,
                Arc::clone(&session),
                Box::new(|| Box::new(astar_audio::NullBackend::new())),
            )
        }
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

    // `[m17] codec_dirs` goes to the station here, before `with_configs`
    // takes ownership of it, so the first decoder probe and the first
    // monitor both search the operator's directories. Restart-tier on
    // purpose: it points dlopen at a directory.
    if let Some(m) = cfg.m17.as_ref() {
        station.set_codec_dirs(m.codec_dirs.iter().map(std::path::PathBuf::from).collect());
    }

    // Build the NodeController — installs the secret resolver internally.
    let mut ctrl = NodeController::with_configs(
        station,
        secrets,
        inbound_cfg,
        register_cfg.clone(),
        cfg.announce.clone(),
        link_shapes,
        cfg.dtmf_enabled(),
        cfg.dtmf_inter_digit_timeout_ms(),
    );
    ctrl.set_discord_config(cfg.discord.clone());
    // `[m17]` absent ⇒ M17 off. A bad table entry was already fatal at load,
    // and a directory file that fails to load degrades to "table only" with
    // one log line inside `from_cfg`, so an `Err` here is a real
    // misconfiguration and stops the node rather than silently disabling M17.
    ctrl.set_m17_config(
        match cfg.m17.as_ref().map(M17Settings::from_cfg).transpose() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("astar-server: m17: {e}");
                return ExitCode::FAILURE;
            }
        },
    );
    // `[echolink]` absent ⇒ EchoLink dormant: no login, every POST
    // /echolink refused. The login itself starts below, after the engine.
    ctrl.set_echolink_config(cfg.echolink.clone());
    // The settings page edits THIS file and reports what differs from what
    // this process actually loaded, so its store is built from the same
    // `cfg` every consumer above was built from — here, before the Arc,
    // beside the Discord section for the same reason.
    ctrl.set_settings_store(SettingsStore::new(
        std::path::PathBuf::from(&config_path),
        cfg.credentials_path(std::path::Path::new(&config_path)),
        RedactedConfig::from_config(&cfg),
    ));

    // `[discord] approvers` turns on the slash-command half: the rule
    // engine, the persistent gateway, and channel muting. The approval
    // state is loaded HERE, before the controller is wrapped in an `Arc` and
    // before anything can act on it, because a state file that exists and
    // does not parse must stop the node rather than silently become "nobody
    // is approved" — or, far worse, "nobody is muted by us" with people
    // still server-muted guild-wide and nothing left that remembers why.
    #[cfg(feature = "discord")]
    if let Some(discord) = cfg.discord.as_ref()
        && !discord.approvers.is_empty()
    {
        let approvals = if let Some(path) = discord.state_file.as_deref() {
            // A missing FILE is an empty state — a first run has no
            // approvals and that is not an error. A missing DIRECTORY is a
            // typo: `load` reads it as NotFound just the same, the node
            // boots looking healthy, and the operator finds out at the
            // first approval that nothing can be written down. Refused
            // here, where the other config mistakes are refused.
            if let Some(problem) =
                astar_server::config::state_file_parent_problem(std::path::Path::new(path))
            {
                eprintln!("astar-server: discord: state_file {path}: {problem}");
                return ExitCode::FAILURE;
            }
            match astar_discord::Approvals::load(std::path::Path::new(path)) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("astar-server: discord: state_file {path}: {e}");
                    return ExitCode::FAILURE;
                }
            }
        } else {
            // Said once, plainly, at start-up: an operator who approves
            // people and then restarts is entitled to know beforehand that
            // it will not stick — and the half that is easy to miss is the
            // MUTES, not the approvals.
            //
            // Without a state file `muted_by_us` is memory-only. Everyone
            // in the channel is server-muted by the one-mic rule while the
            // node runs; after a restart they are swept again on `Ready`
            // already carrying `server_muted: true` and NOT in
            // `muted_by_us`, which the rule engine reads — correctly, on
            // the evidence it has — as a mute a moderator applied by hand.
            // It never records it and never lifts it. That is a guild-wide
            // mute left on real people, so it is a WARNING, not a note.
            eprintln!(
                "astar-server: discord: WARNING: no state_file — approvals AND the record of \
                 who this node muted are in memory only. After a restart, members this node \
                 muted stay server-muted until a moderator unmutes them by hand."
            );
            astar_discord::Approvals::in_memory()
        };
        if let Err(e) = ctrl.set_discord_approvals(approvals) {
            eprintln!("astar-server: {e}");
            return ExitCode::FAILURE;
        }
    }
    // A config that asks for slash commands on a build that cannot serve
    // them is worth one line rather than silence — the keys parse either
    // way, so nothing else would ever say so.
    #[cfg(not(feature = "discord"))]
    if cfg
        .discord
        .as_ref()
        .is_some_and(|d| !d.approvers.is_empty())
    {
        eprintln!(
            "astar-server: discord: this build has no discord feature; \
             [discord] approvers is inert"
        );
    }

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

    // `[echolink]`: log in to the directory, so the node is in the station
    // list and an admin can dial stations in. Logging in calls nobody;
    // every call is a person's `POST /echolink`. Returns at once (the login
    // has its own thread), and with no password held it starts nothing and
    // `/status` names the secret to set. A no-op without the section.
    ctrl.start_echolink_login();

    // `[discord] join_at_start`: issue exactly the join `POST /discord`
    // issues, and issue it HERE — after `EnableInbound` (and after the
    // WireGuard bounce, which rebuilds the listener), because the conference
    // member the bridge enrolls does not exist until the engine does.
    //
    // Non-fatal, always: a node must come up on the air whether or not
    // Discord is reachable, so a failure is logged and start-up continues.
    //
    // It is synchronous, and it runs on `main`: an unreachable Discord
    // delays the control server binding by however long this takes. That is
    // `SongbirdLink::join`'s reply timeout, ~15 s, and every await behind it
    // is inside the join deadline — so a Discord that is merely down or slow
    // costs at most that. The one case NOT covered is a runtime thread
    // wedged in a `block_on` that never returns: the reply timeout still
    // fires here, but the failing join drops the `Bridge` and the controller
    // then drops the `Gateway` (unless it is persistent), and
    // `Gateway::drop` joins that thread with no timeout, because
    // `std::thread::JoinHandle::join` admits none. Start-up would hang
    // there. See that `Drop` for why nothing short of leaving no unbounded
    // await in the thread can close it.
    //
    // On a build without the `discord` feature the command reaches the
    // refusing `handle_discord` and the operator is told the build has no
    // such feature, instead of the key being silently ignored.
    if cfg.discord_join_at_start() {
        match ctrl.execute(NodeCommand::Discord {
            action: DiscordAction::Join,
        }) {
            Ok(_) => eprintln!("astar-server: discord: joined at start"),
            Err(e) => eprintln!(
                "astar-server: discord: join_at_start failed, continuing: {}",
                e.message
            ),
        }
    }

    // With approvers configured the bot has to be online whether or not it
    // is in the voice channel — a gateway that goes down with the voice
    // session cannot answer a slash command. Idempotent: `join_at_start`
    // above already reaches `ensure_discord_gateway` through the join path,
    // so when both are set this call finds a healthy gateway and connects
    // nothing. A node with no approvers returns from it immediately and
    // keeps the lazy, join-time start that shipped first.
    //
    // Non-fatal, like the join above and for the same reason: Discord being
    // unreachable — or the token not having arrived yet under `[secrets]
    // source = "control"`, in which case `ProvideSecret` retries this — must
    // never stop a node coming up on the air.
    #[cfg(feature = "discord")]
    if let Err(e) = ctrl.start_discord_gateway() {
        eprintln!(
            "astar-server: discord: the bot did not come online, continuing: {}",
            e.message
        );
    }

    match subcommand.as_str() {
        "serve" => run_serve_mode(ctrl, &cfg, std::path::Path::new(&config_path)),
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

fn run_serve_mode(
    ctrl: NodeController,
    cfg: &NodeFileConfig,
    config_path: &std::path::Path,
) -> ExitCode {
    let bind = cfg.control.bind.as_str();
    let ctrl = Arc::new(ctrl);
    let stop = ctrl.stop_flag();

    // Install SIGINT/SIGTERM handler — sets stop flag + executes Shutdown.
    install_signal_handler(Arc::clone(&ctrl), Arc::clone(&stop));

    // Sign-in: the store in the state directory. A store that cannot be
    // trusted leaves sign-in off, says why once, and the node carries on:
    // the console (a loopback peer) still reaches everything.
    #[cfg(feature = "auth")]
    {
        let settings = astar_server::auth::AuthSettings::from_config(
            cfg,
            &astar_server::config::state_dir(config_path),
        );
        let auth = Arc::new(astar_server::auth::AuthService::open(settings));
        if let Some(why) = auth.disabled() {
            eprintln!("astar-server: sign-in: {why}");
        }
        ctrl.set_auth(auth);
    }

    // Bind the HTTP control server; fail fast if the address is unavailable.
    let bind_str = bind.to_string();
    let server = match astar_server::server::bind_control(&ctrl, &bind_str) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("astar-server: cannot bind control socket {bind_str:?}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let listen_addr = server.local_addr().to_string();
    eprintln!("astar-server: HTTP control listening on http://{listen_addr}");

    // The node logger (`[recording]`), once the control port is bound so a
    // failed bind leaves no thread behind. Outside the `auth` block on purpose:
    // recording needs neither sign-in nor the listener stream. It enrols a
    // non-contributing conference member, so it can put nothing on the air.
    let recordings = cfg.recording.as_ref().filter(|r| r.enabled).map(|r| {
        let logger = astar_server::recording::logger::NodeLogger::start(
            r,
            Arc::clone(&ctrl) as Arc<dyn astar_server::recording::logger::RecordSource>,
        );
        ctrl.set_recordings(Arc::clone(&logger));
        eprintln!("astar-server: recording to {}", r.dir.display());
        logger
    });

    // The public, read-only side: `[web]` (page, HTTPS, dynamic DNS). It
    // needs the shared controller, so it starts here; a bind it cannot take
    // is logged and the node carries on without it — the control port and
    // the radio side must never depend on the public page.
    let public = match astar_server::public::run::PublicSide::start(
        cfg,
        &astar_server::config::state_dir(config_path),
        &ctrl,
        None,
        None,
    ) {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("astar-server: public page not started: {e}");
            None
        }
    };

    // run_serve serves until shutdown, then tears down gracefully.
    run_serve(&ctrl, server, &stop);
    if let Some(p) = public {
        p.stop();
    }
    // Finishes and indexes the over in progress before a restart or exit.
    if let Some(logger) = recordings {
        logger.stop();
    }
    #[cfg(feature = "auth")]
    if let Some(auth) = ctrl.auth() {
        auth.flush_last_seen();
    }

    // The settings page's Restart. Every consumer bound before the Arc —
    // listener, register, announce, Discord — is rebuilt by the new image
    // from the file the page just wrote; the exec keeps the PID, the
    // environment and the supervisor's view of one continuous process. If
    // it fails, this is exactly the exit it always was. `stop` is
    // `ctrl.stop_flag()`, the controller's own flag; its `Acquire` load
    // pairs with the `Release` stores in the `Restart` arm, as in
    // `run_serve`, so a restart is never read as a plain exit.
    if stop.load(Ordering::Acquire) && ctrl.restart_requested() {
        eprintln!("astar-server: restarting");
        let e = reexec_self();
        eprintln!("astar-server: restart exec failed, exiting instead: {e}");
    }

    eprintln!("astar-server: shutdown complete");
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// vocoder list / add
// ---------------------------------------------------------------------------

/// `astar-server vocoder list|add`. Never bootstraps a config: a typo in
/// `--config` must not leave a fresh template behind. `list` opens nothing;
/// `add` opens the one port only after the operator says `yes` (or passes
/// `--yes`), and writes `node.toml` only when an AMBE-3000 answered.
fn run_vocoder(args: Vec<String>) -> ExitCode {
    let mut action = None;
    let mut serial = None;
    let mut yes = false;
    let mut rest = Vec::new();
    for a in args {
        if a == "--yes" || a == "-y" {
            yes = true;
        } else if action.is_none() && !a.starts_with('-') {
            action = Some(a);
        } else if action.as_deref() == Some("add")
            && serial.is_none()
            && !a.starts_with('-')
            && rest
                .last()
                .is_none_or(|p: &String| p != "--config" && p != "-c")
        {
            serial = Some(a);
        } else {
            rest.push(a);
        }
    }
    let config_path = match parse_config_flag(&mut rest.into_iter()) {
        Ok(p) => std::path::PathBuf::from(p),
        Err(e) => {
            eprintln!("astar-server: vocoder: {e}");
            return ExitCode::FAILURE;
        }
    };
    let load = || -> Result<NodeFileConfig, String> {
        let text = std::fs::read_to_string(&config_path)
            .map_err(|e| format!("cannot read config {}: {e}", config_path.display()))?;
        NodeFileConfig::from_toml_str(&text)
    };
    match (action.as_deref(), serial) {
        (Some("list"), None) => {
            let cfg = match load() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("astar-server: vocoder: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let candidates = astar_station::list_vocoder_candidates();
            print!(
                "{}",
                astar_server::vocoder::list_report(&candidates, &cfg.vocoder_trust())
            );
            if candidates.is_empty() && !cfg!(feature = "vocoder") {
                println!(
                    "(This astar-server may have been built without its `vocoder` feature, \
                     which is what lists and probes sticks.)"
                );
            }
            ExitCode::SUCCESS
        }
        (Some("add"), Some(serial)) => {
            let serial = serial.trim().to_string();
            if !yes && !confirm_enrol(&serial) {
                eprintln!("astar-server: vocoder: not confirmed; nothing was opened");
                return ExitCode::FAILURE;
            }
            match astar_server::vocoder::add(&config_path, &serial, astar_station::enrol_vocoder) {
                Ok(astar_server::vocoder::AddOutcome::Added { enrolment }) => {
                    println!("{}", enrolment.message());
                    println!(
                        "Trusted {serial} in {}. Restart the node for it to take effect.",
                        config_path.display()
                    );
                    ExitCode::SUCCESS
                }
                Ok(astar_server::vocoder::AddOutcome::AlreadyTrusted) => {
                    println!("{serial} is already trusted; nothing was opened.");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("astar-server: vocoder: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!(
                "astar-server: vocoder: use `vocoder list --config <path>` or \
                 `vocoder add <serial> --config <path> [--yes]`"
            );
            ExitCode::FAILURE
        }
    }
}

/// Ask on the terminal before enrolment opens a port. Refuses when stdin is
/// not a terminal: a script must say `--yes` in words.
fn confirm_enrol(serial: &str) -> bool {
    use std::io::{BufRead as _, IsTerminal as _, Write as _};
    let port = astar_station::list_vocoder_candidates()
        .into_iter()
        .find(|c| c.serial == serial)
        .map(|c| c.port);
    println!(
        "About to probe the USB device with serial {serial}{}.",
        port.map(|p| format!(" at {p}")).unwrap_or_default()
    );
    println!("{}", astar_server::vocoder::CABLE_WARNING);
    if !std::io::stdin().is_terminal() {
        eprintln!("astar-server: vocoder: stdin is not a terminal; pass --yes to confirm");
        return false;
    }
    print!("Type yes to continue: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok() && line.trim() == "yes"
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
        \x20   astar-server vocoder list --config <path.toml>\n\
        \x20   astar-server vocoder add <serial> --config <path.toml> [--yes]\n\
        \x20   astar-server admin invite --role admin|viewer [--name N] [--hours H] [--config <path>]\n\
        \x20   astar-server admin invite --user N | admin users | admin revoke-sessions --user N\n\
        \x20   astar-server --version | --features | --print-systemd-unit\n\
        \n\
        SUBCOMMANDS:\n\
        \x20   serve   Run HTTP+SSE control server (background-friendly)\n\
        \x20   tui     Interactive stdin menu\n\
        \x20   vocoder list   List vocoder-class USB devices and which are trusted\n\
        \x20                  in [vocoder] trusted_serials (opens nothing)\n\
        \x20   vocoder add    Probe the stick with that USB serial and, if an\n\
        \x20                  AMBE-3000 answers, trust it in node.toml. Asks first:\n\
        \x20                  never point it at a radio's CAT/PTT cable\n\
        \x20   admin   Sign-in invites, users and sessions (the first admin starts here)\n\
        \n\
        OPTIONS:\n\
        \x20   --config <path>   Path to the node TOML config file\n\
        "
    );
}
