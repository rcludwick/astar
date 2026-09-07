// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Standalone NXDN parrot reflector — the NXDN twin of `astar-ysf`'s
//! `ysf_parrot` and `astar-m17`'s `m17_parrot`, and the same idea as
//! `AllStar`'s 55553. Binds `[::]:<port>` in [`Reflector::bind_parrot`] mode
//! and runs until killed.
//!
//! # What this is for
//!
//! A bench loop that needs nobody else on the air: link a transmitter at
//! `127.0.0.1:<port>`, key up, unkey, and hear your own transmission
//! replayed back. One dongle is enough, because the replay is decoded after
//! the key-up ends, not during it. Nothing goes on the air, and nothing
//! depends on somebody happening to key up on a public reflector.
//!
//! * Any NXDN transmitter works — astar, a radio through a hotspot,
//!   Pi-Star, `DroidStar` — which is how astar's receive path gets checked
//!   against somebody else's encoder, carrying audio you produced, on a port
//!   you chose.
//! * Every `NXDND` for the parrot's talkgroup is relayed verbatim to every
//!   *other* registered client, so a second client linked to the same port
//!   hears the live transmission; the replay goes back to the sender.
//!
//! Verbatim relay is load-bearing here: a reflector that re-encoded frames
//! would hide exactly the framing bugs a bench parrot exists to catch. This
//! one does not — what comes back is what the transmitter put on the wire.
//!
//! The talkgroup matters. A poll naming any other talkgroup is dropped
//! without a reply, exactly as a real reflector drops it, so `--tg` here and
//! the talkgroup in the client have to agree or nothing links.
//!
//! # Dual-stack bind
//!
//! Binds the unspecified IPv6 address so one socket serves both stacks —
//! macOS resolves `localhost` to `[::1]` before `127.0.0.1`, and a v4-only
//! socket has no route to a v6 peer. `IPV6_V6ONLY` defaults off on
//! macOS/Linux; Windows defaults it on, so the dual-stack trick is a
//! macOS/Linux assumption. Dev-tool grade, accepted rather than plumbed
//! through a second socket — same call `ysf_parrot` makes, for the same
//! reason.
//!
//! Run: `cargo run -p astar-nxdn --example nxdn_parrot -- --port <p>`
//!
//! No Ctrl-C handling beyond the OS default: a dev-tool runnable, not a
//! daemon. The process parks the main thread; `^C` kills it, and there is
//! nothing to flush or persist.

use std::net::SocketAddr;
use std::process::ExitCode;

use astar_nxdn::Reflector;

const USAGE: &str = "usage: nxdn_parrot --port <port> [--tg <n>] [--replay-ms <n>]";

/// The talkgroup the parrot answers for unless `--tg` says otherwise.
/// 31313 is a US-wide NXDN talkgroup number, chosen only so the default is
/// a plausible one to type into a client.
const DEFAULT_TG: u16 = 31313;

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
    let mut talkgroup: u16 = DEFAULT_TG;
    let mut replay_ms: Option<u64> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                let v = args.next().ok_or(USAGE)?;
                port = Some(v.parse::<u16>().map_err(|_| format!("bad --port {v:?}"))?);
            }
            "--tg" => {
                let v = args.next().ok_or(USAGE)?;
                talkgroup = v.parse::<u16>().map_err(|_| format!("bad --tg {v:?}"))?;
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
        None => Reflector::bind_parrot(addr, talkgroup)?,
        Some(ms) => Reflector::bind_parrot_with_timeouts(
            addr,
            talkgroup,
            astar_nxdn::reflector::DEFAULT_CLIENT_TIMEOUT,
            std::time::Duration::from_millis(ms),
        )?,
    };
    let bound = reflector.local_addr();
    println!(
        "NXDN parrot on [::]:{} for TG {talkgroup} — link a transmitter at 127.0.0.1:{} (or localhost:{}),",
        bound.port(),
        bound.port(),
        bound.port()
    );
    println!("using that same talkgroup: a poll for any other one is dropped without a reply.");
    println!("Key up and you hear yourself; a hotspot or DroidStar works too.");
    let _handle = reflector.run();

    loop {
        std::thread::park();
    }
}
