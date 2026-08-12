// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `register` subcommand. Drives the high-level [`Registrar`], prints lifecycle
//! events, and reports a terminal registration result.

use std::sync::Arc;
use std::time::Duration;

use astar_iax::{RegisterOptions, Registrar, RegistrationEvent};
use astar_iax_core::session::auth::Secret;

use crate::cli::{self, REGISTER_USAGE};

pub fn run(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut password: Option<String> = None;
    let mut refresh = Duration::from_secs(60);
    let mut timeout = Duration::from_secs(15);
    let mut positional: Vec<String> = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{REGISTER_USAGE}");
                return Ok(());
            }
            "--password" => password = Some(cli::flag_value(&mut args, "--password")?),
            "--refresh" => {
                let v = cli::flag_value(&mut args, "--refresh")?;
                refresh = Duration::from_secs(parse_secs(&v, "--refresh")?);
            }
            "--timeout" => {
                let v = cli::flag_value(&mut args, "--timeout")?;
                timeout = Duration::from_secs(parse_secs(&v, "--timeout")?);
            }
            flag if flag.starts_with("--") => {
                return Err(format!("unknown flag: {flag}\n\n{REGISTER_USAGE}"));
            }
            _ => positional.push(arg),
        }
    }

    let mut pos = positional.into_iter();
    let host = pos
        .next()
        .ok_or_else(|| format!("missing <host>\n\n{REGISTER_USAGE}"))?;
    let user = pos
        .next()
        .ok_or_else(|| format!("missing <user>\n\n{REGISTER_USAGE}"))?;
    let pass = password.or_else(|| pos.next()).unwrap_or_default();

    let peer = cli::resolve_peer(&host)?;
    let secret = Arc::new(Secret::new(pass));
    let options = RegisterOptions {
        refresh_request: refresh,
        ..RegisterOptions::default()
    };

    println!(
        "registering {user} @ {peer} (refresh {}s)…",
        refresh.as_secs()
    );
    let (reg, events) = Registrar::new(peer, user, secret)
        .with_options(options)
        .register()
        .map_err(|e| format!("failed to start registration: {e}"))?;

    // Wait for a terminal lifecycle event or the timeout. Registered / Failed /
    // Released are terminal for the purposes of a one-shot CLI; on success we
    // immediately deregister cleanly so we leave no dangling binding.
    let outcome = wait_for_result(&events, timeout);

    match &outcome {
        Outcome::Registered => {
            println!("deregistering…");
            reg.deregister()
                .map_err(|e| format!("deregister failed: {e}"))?;
            println!("done.");
            Ok(())
        }
        Outcome::Failed(reason) => {
            drop(reg);
            Err(format!("registration failed: {reason}"))
        }
        Outcome::Released => {
            drop(reg);
            Err("registrar released the binding".to_string())
        }
        Outcome::Timeout => {
            drop(reg);
            Err(format!(
                "timed out after {}s waiting for a registration result",
                timeout.as_secs()
            ))
        }
    }
}

enum Outcome {
    Registered,
    Failed(String),
    Released,
    Timeout,
}

fn wait_for_result(
    events: &std::sync::mpsc::Receiver<RegistrationEvent>,
    timeout: Duration,
) -> Outcome {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Outcome::Timeout;
        }
        match events.recv_timeout(remaining) {
            Ok(ev) => {
                print_event(&ev);
                match ev {
                    RegistrationEvent::Registered { .. } => return Outcome::Registered,
                    RegistrationEvent::Failed(r) => return Outcome::Failed(format!("{r:?}")),
                    RegistrationEvent::Released => return Outcome::Released,
                    _ => {}
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return Outcome::Timeout,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Outcome::Failed("registration thread exited unexpectedly".to_string());
            }
        }
    }
}

fn print_event(ev: &RegistrationEvent) {
    match ev {
        RegistrationEvent::Registering => println!("status: registering"),
        RegistrationEvent::Registered {
            refresh,
            apparent_addr,
        } => {
            print!("status: REGISTERED (refresh {}s)", refresh.as_secs());
            if let Some(addr) = apparent_addr {
                print!(" — peer sees us at {addr}");
            }
            println!();
        }
        RegistrationEvent::Refreshing => println!("status: refreshing"),
        RegistrationEvent::Refreshed => println!("status: refreshed"),
        RegistrationEvent::Failed(r) => println!("status: FAILED ({r:?})"),
        RegistrationEvent::Released => println!("status: released"),
    }
}

fn parse_secs(v: &str, flag: &str) -> Result<u64, String> {
    v.parse::<u64>()
        .map_err(|_| format!("{flag} expects a whole number of seconds, got {v:?}"))
}
