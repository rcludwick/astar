// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Standalone YSF parrot reflector — the System Fusion twin of
//! `astar-m17`'s `m17_parrot`, and the same idea as `AllStar`'s 55553.
//! Binds `[::]:<port>` in [`Reflector::bind_parrot`] mode and runs until
//! killed.
//!
//! # What this is for
//!
//! astar transmits YSF — DN mode, through the `ThumbDV` — so this is the bench
//! loop `m17_parrot` gives M17: link astar at `127.0.0.1:<port>`, key up,
//! unkey, and hear your own transmission replayed back. One dongle is
//! enough, because YSF here is half-duplex: the replay is decoded after the
//! key-up ends, not during it. Nothing goes on the air, and nothing depends
//! on somebody else happening to key up on a public reflector.
//!
//! * As the transmitter, astar keys into it and hears itself — one round
//!   trip through vocode, framing, the link layer and decode.
//! * Any other YSF transmitter works just as well — a Yaesu radio through a
//!   hotspot, Pi-Star, `DroidStar` — which is how you check astar's receive
//!   path against somebody else's encoder, carrying audio you produced, on a
//!   port you chose.
//! * Point `astar-cli ysf-listen` at the same port to watch from a second
//!   client. Every `YSFD` is relayed verbatim to every *other* registered
//!   client, so the listener hears the live transmission; the parrot replay
//!   goes back to the sender.
//!
//! Verbatim relay is load-bearing here: a reflector that re-encoded frames
//! would hide exactly the framing bugs a bench parrot exists to catch. This
//! one does not — what comes back is what astar put on the wire.
//!
//! # Dual-stack bind
//!
//! Binds the unspecified IPv6 address so one socket serves both stacks —
//! macOS resolves `localhost` to `[::1]` before `127.0.0.1`, and a v4-only
//! socket has no route to a v6 peer. `IPV6_V6ONLY` defaults off on
//! macOS/Linux; Windows defaults it on, so the dual-stack trick is a
//! macOS/Linux assumption. Dev-tool grade, accepted rather than plumbed
//! through a second socket — same call `m17_parrot` makes, for the same
//! reason.
//!
//! Run: `cargo run -p astar-ysf --example ysf_parrot -- --port <p>`
//! (or `just ysf-parrot <port>`).
//!
//! No Ctrl-C handling beyond the OS default: a dev-tool runnable, not a
//! daemon. The process parks the main thread; `^C` kills it, and there is
//! nothing to flush or persist.

use std::net::SocketAddr;
use std::process::ExitCode;

use astar_ysf::Reflector;

const USAGE: &str = "usage: ysf_parrot --port <port> [--replay-ms <n>]";

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
            flag => return Err(format!("unknown argument: {flag}\n{USAGE}").into()),
        }
    }
    let port = port.ok_or(USAGE)?;

    // Unspecified IPv6 — see the module doc's dual-stack note.
    let addr: SocketAddr = (std::net::Ipv6Addr::UNSPECIFIED, port).into();
    let reflector = match replay_ms {
        None => Reflector::bind_parrot(addr)?,
        Some(ms) => Reflector::bind_parrot_with_timeouts(
            addr,
            astar_ysf::reflector::DEFAULT_CLIENT_TIMEOUT,
            std::time::Duration::from_millis(ms),
        )?,
    };
    let bound = reflector.local_addr();
    println!(
        "YSF parrot on [::]:{} — link a transmitter at 127.0.0.1:{} (or localhost:{}),",
        bound.port(),
        bound.port(),
        bound.port()
    );
    println!(
        "and listen with:  just ysf-listen 127.0.0.1:{} <YOURCALL>",
        bound.port()
    );
    println!("Key up from astar and you hear yourself; a hotspot or DroidStar works too.");
    let _handle = reflector.run();

    loop {
        std::thread::park();
    }
}
