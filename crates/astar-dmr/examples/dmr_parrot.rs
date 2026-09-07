// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Standalone DMR parrot **master** — the DMR twin of `astar-ysf`'s
//! `ysf_parrot`, `astar-nxdn`'s `nxdn_parrot` and `astar-m17`'s `m17_parrot`,
//! and the same idea as `AllStar`'s 55553. Binds `[::]:<port>` in
//! [`Master::bind_parrot_with_timeouts`] mode and runs until killed.
//!
//! # This is a master, not a client
//!
//! Every other parrot in astar is a reflector a client links *to*; this one
//! is the far end of the homebrew protocol itself. **It binds and waits. It
//! never dials anything.** Nothing here can reach a real network, by
//! construction: there is no outbound connect in this file, no hostname to
//! resolve and no address to mistype. That matters more for DMR than for the
//! others, because a real master authenticates a registered radio ID and a
//! stray packet is attributable to somebody's licence.
//!
//! # What this is for
//!
//! A bench loop that needs nobody else on the air: point a DMR client at
//! `127.0.0.1:<port>` with the password below, key up, unkey, and hear your
//! own transmission replayed back. One dongle is enough, because the replay
//! is decoded after the key-up ends, not during it.
//!
//! * The whole handshake is real — salt, `SHA256(salt ‖ password)`, config,
//!   ping/pong — so a client that gets any step wrong fails here exactly as
//!   it would fail against a live master, on a port nobody else can hear.
//! * Every `DMRD` from a connected peer is relayed verbatim to every *other*
//!   connected peer, so a second client on the same port hears the live
//!   transmission; the replay goes back to the sender.
//!
//! Verbatim relay is load-bearing: a master that re-encoded frames would hide
//! exactly the framing bugs a bench parrot exists to catch. This one does not
//! — what comes back is what the transmitter put on the wire.
//!
//! # Dual-stack bind
//!
//! Binds the unspecified IPv6 address so one socket serves both stacks —
//! macOS resolves `localhost` to `[::1]` before `127.0.0.1`, and a v4-only
//! socket has no route to a v6 peer. `IPV6_V6ONLY` defaults off on
//! macOS/Linux; Windows defaults it on, so the dual-stack trick is a
//! macOS/Linux assumption. Dev-tool grade, accepted rather than plumbed
//! through a second socket — the same call `ysf_parrot` and `nxdn_parrot`
//! make, for the same reason.
//!
//! Run: `cargo run -p astar-dmr --example dmr_parrot -- --port <p>`
//!
//! No Ctrl-C handling beyond the OS default: a dev-tool runnable, not a
//! daemon. The process parks the main thread; `^C` kills it, and there is
//! nothing to flush or persist.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use astar_dmr::Master;
use astar_dmr::master::{DEFAULT_PARROT_REPLAY_DELAY, DEFAULT_PEER_TIMEOUT};

const USAGE: &str = "usage: dmr_parrot [--port <port>] [--password <pass>] [--replay-ms <n>]";

/// The homebrew master port every MMDVM client already defaults to.
const DEFAULT_PORT: u16 = 62031;
/// The password a bench parrot asks for. It guards nothing — it is on
/// loopback and it is in this file — and it exists only because the
/// handshake has a digest step that must be exercised, not skipped.
const DEFAULT_PASSWORD: &str = "passw0rd";

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
    let mut port: u16 = DEFAULT_PORT;
    let mut password: String = DEFAULT_PASSWORD.to_string();
    let mut replay = DEFAULT_PARROT_REPLAY_DELAY;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                let v = args.next().ok_or(USAGE)?;
                port = v.parse::<u16>().map_err(|_| format!("bad --port {v:?}"))?;
            }
            "--password" => {
                password = args.next().ok_or(USAGE)?;
            }
            "--replay-ms" => {
                let v = args.next().ok_or(USAGE)?;
                let ms = v
                    .parse::<u64>()
                    .map_err(|_| format!("bad --replay-ms {v:?}"))?;
                replay = Duration::from_millis(ms);
            }
            flag => return Err(format!("unknown argument: {flag}\n{USAGE}").into()),
        }
    }

    // Unspecified IPv6 — see the module doc's dual-stack note.
    let addr: SocketAddr = (std::net::Ipv6Addr::UNSPECIFIED, port).into();
    let master = Master::bind_parrot_with_timeouts(addr, &password, DEFAULT_PEER_TIMEOUT, replay)?;
    let bound = master.local_addr();
    println!(
        "DMR parrot master on [::]:{} — point a client at 127.0.0.1:{} (or localhost:{}),",
        bound.port(),
        bound.port(),
        bound.port()
    );
    println!("with any radio ID and the password {password:?}: the handshake is the real one.");
    println!("Key up and you hear yourself; a hotspot or DroidStar works too.");
    let _handle = master.run();

    loop {
        std::thread::park();
    }
}
