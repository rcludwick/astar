// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! A self-hosted P25 parrot: the P25 twin of `astar-nxdn`'s `nxdn_parrot`,
//! and the same idea as G4KLX's `P25Parrot`. Binds `127.0.0.1:<port>` in
//! [`Reflector::bind_parrot`] mode and runs until killed.
//!
//! # What this is for
//!
//! A bench loop that needs nobody else on the air. Link a client at
//! `127.0.0.1:<port>`, send an over, and it comes back to you 2 s after it
//! ends, one record every 20 ms, as `P25Parrot` sends it.
//!
//! * Every voice record and `0x80` from the current talker is also relayed
//!   verbatim to every other linked client, so a second client on the same
//!   port hears the over live. That is how astar's receive path is checked
//!   end to end without a transmitter of its own.
//! * Relay is verbatim, never re-encoded: a parrot that rewrote records
//!   would hide exactly the framing bugs a bench parrot exists to catch.
//!
//! # Loopback only
//!
//! It binds `127.0.0.1` and nothing else, so nothing off this machine can
//! reach it, and it never dials anything: it only answers. A client has to
//! name `127.0.0.1`, not `localhost`, which macOS resolves to `[::1]` first.
//!
//! Run: `cargo run -p astar-p25 --example p25_parrot -- --port <p>`
//!
//! No Ctrl-C handling beyond the OS default. The process parks the main
//! thread; `^C` kills it, and there is nothing to flush or persist.

use std::net::{Ipv4Addr, SocketAddr};
use std::process::ExitCode;
use std::time::Duration;

use astar_p25::Reflector;
use astar_p25::reflector::DEFAULT_CLIENT_TIMEOUT;

const USAGE: &str = "usage: p25_parrot --port <port> [--replay-ms <n>]\n\
    Answers on 127.0.0.1 only and dials nothing.";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut port: Option<u16> = None;
    let mut replay_ms: Option<u64> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                let v = args.next().ok_or(USAGE)?;
                port = Some(v.parse::<u16>().map_err(|_| format!("bad --port {v:?}"))?);
            }
            "--replay-ms" => {
                let v = args.next().ok_or(USAGE)?;
                replay_ms = Some(
                    v.parse::<u64>()
                        .map_err(|_| format!("bad --replay-ms {v:?}"))?,
                );
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(ExitCode::SUCCESS);
            }
            flag => return Err(format!("unknown argument: {flag}").into()),
        }
    }
    let port = port.ok_or("--port is required")?;

    let addr: SocketAddr = (Ipv4Addr::LOCALHOST, port).into();
    let reflector = match replay_ms {
        None => Reflector::bind_parrot(addr)?,
        Some(ms) => Reflector::bind_parrot_with_timeouts(
            addr,
            DEFAULT_CLIENT_TIMEOUT,
            Duration::from_millis(ms),
        )?,
    };
    let bound = reflector.local_addr();
    println!(
        "P25 parrot on {bound}: link a client at 127.0.0.1:{}. It dials nothing.",
        bound.port()
    );
    println!("Send an over and it comes back to you after it ends; a second client hears it live.");
    let _handle = reflector.run();

    loop {
        std::thread::park();
    }
}
