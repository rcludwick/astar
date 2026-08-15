// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `astar-inspect` — web operator console for an `AllStarLink` web-transceiver
//! call. Binds 127.0.0.1 only (the secret crosses the wire in plaintext).

use std::net::SocketAddr;

use astar_audio::CpalBackend;
use astar_inspect::server::{HarnessDefaults, ServerState, serve};

/// Read an env var, treating empty/whitespace as unset.
fn env_opt(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn main() {
    // Optional first arg: bind address. Default 127.0.0.1:8080.
    let addr: SocketAddr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8080".to_string())
        .parse()
        .unwrap_or_else(|e| {
            eprintln!("invalid bind address: {e}");
            std::process::exit(2);
        });
    if !addr.ip().is_loopback() {
        eprintln!("refusing to bind non-loopback address {addr}: the secret is sent in plaintext");
        std::process::exit(2);
    }

    // Form defaults from the environment (sourced by scripts/run-harness.sh
    // from .env). HARNESS_SECRET stays server-side — never sent to the browser.
    let callsign = env_opt("ASL_USER");
    let portal = match (
        env_opt("ASL_USER"),
        env_opt("ASL_PASS"),
        env_opt("ASL_NODE"),
    ) {
        (Some(user), Some(password), Some(node)) => Some(astar_asl3::PortalCredentials {
            user,
            password,
            node,
        }),
        _ => None,
    };
    let wt = portal.is_some();
    // Node-mode defaults (iax-64b6): bind port + answer policy for the inbound
    // listener. Unset → the /node/start handler applies 4569 / "auto".
    let node_port = env_opt("HARNESS_NODE_PORT").and_then(|s| s.parse::<u16>().ok());
    let node_answer = env_opt("HARNESS_NODE_ANSWER");
    // Node-registration defaults (iax-64b6 Register tab): registrar host:port +
    // the node number to register AS, prefilled in the browser; the registrar
    // password is held server-side and never sent to the browser.
    let node_registrar = env_opt("HARNESS_REGISTRAR");
    let node_username = env_opt("HARNESS_NODE_USER");
    let register_secret = env_opt("HARNESS_REGISTER_SECRET");
    let defaults = HarnessDefaults {
        node: env_opt("HARNESS_NODE"),
        calling_node: env_opt("HARNESS_CALLING_NODE"),
        name: env_opt("HARNESS_NAME"),
        input: env_opt("HARNESS_INPUT"),
        output: env_opt("HARNESS_OUTPUT"),
        secret: env_opt("HARNESS_SECRET"),
        callsign: callsign.clone(),
        wt,
        portal,
        node_port,
        node_answer,
        node_registrar,
        node_username,
        register_secret,
    };
    if defaults.secret.is_some() {
        println!("using HARNESS_SECRET from the environment (injected server-side)");
    }
    if defaults.register_secret.is_some() {
        println!("using HARNESS_REGISTER_SECRET from the environment (injected server-side)");
    }
    if let Some(n) = &defaults.node {
        println!("default node: {n}");
    }
    if wt {
        let cs = callsign.as_deref().unwrap_or("");
        println!("web transceiver mode: as {cs}");
    }

    let state = ServerState::with_defaults(Box::new(|| Box::new(CpalBackend::new())), defaults);

    // UCI150 serial PTT bridge (iax-8e3b): handset CTS keys the harness; RTS
    // keys the radio on receive. No-op when no serial device is present.
    let serial_cfg = astar_inspect::serial::SerialConfig::parse(|k| std::env::var(k).ok());
    let ptt_port = serial_cfg.port.clone();
    let ptt_transport = serial_cfg.transport;
    let serial = astar_inspect::serial::spawn(&state, serial_cfg);
    let transport_label = match ptt_transport {
        astar_inspect::serial::PttTransport::Tty => "tty/dext",
        astar_inspect::serial::PttTransport::Usb => "raw-USB (IOKit)",
    };
    println!(
        "PTT transport: {transport_label}; bridge {} (HARNESS_PTT_TRANSPORT=tty|usb)",
        if serial.is_some() { "up" } else { "disabled" },
    );
    let _serial = serial;

    // Read-only serial RX monitor (iax-b38e): tap incoming data bytes on a
    // dedicated monitor port (HARNESS_MONITOR_SERIAL) for the Serial tab.
    // Autodetect avoids the PTT port so two readers don't fight over RX bytes.
    let monitor_cfg =
        astar_inspect::serial_monitor::MonitorConfig::parse(|k| std::env::var(k).ok(), ptt_port);
    let _serial_monitor = astar_inspect::serial_monitor::spawn(&state.serial_monitor, &monitor_cfg);

    println!("astar-inspect listening on http://{addr}");
    println!("(press the Shutdown button in the UI, or Ctrl-C, to stop)");
    if let Err(e) = serve(addr, &state, None) {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
    println!("astar-inspect: shut down cleanly");
}
