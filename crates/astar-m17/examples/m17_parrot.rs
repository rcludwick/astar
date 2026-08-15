// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Standalone M17 parrot reflector (iax-91f4): Rob's self-hosted echo test,
//! the M17 equivalent of `AllStar`'s 55553 parrot. Binds the unspecified
//! IPv6 address `[::]:<port>` in [`Reflector::bind_parrot`] mode and runs
//! until killed with Ctrl-C.
//!
//! # Dual-stack bind (iax-m17-localhost)
//!
//! This used to bind `0.0.0.0:<port>` (IPv4-only), which broke dialing
//! `localhost:<port>/<module>`: macOS resolves `"localhost"` to `[::1]`
//! BEFORE `127.0.0.1`, and a v4-only socket has no route to an IPv6 peer at
//! all. Binding the unspecified IPv6 address instead serves both stacks from
//! ONE socket/one shared reflector state (confirmed by hand on macOS:
//! `IPV6_V6ONLY` defaults off, so a `[::]`-bound socket also receives
//! v4-mapped traffic sent to `127.0.0.1`) — a genuine v6 client and a v4
//! client both land in the same client table and can hear each other.
//! Windows defaults `IPV6_V6ONLY` ON, so this dual-stack trick is a
//! macOS/Linux-only assumption; this is a dev-tool-grade example (not a
//! production daemon — see the module docs), so that's accepted rather than
//! plumbed through a second socket for now.
//!
//! Run: `cargo run -p astar-m17 --example m17_parrot -- --port <p> [--module A]`
//! (or `just m17-parrot <port> [module]`). Then point an M17 client (e.g.
//! astar) at `127.0.0.1:<port>/<module>` OR `localhost:<port>/<module>` and
//! hear yourself echoed back after keying and unkeying.
//!
//! `--module` is informational only — it's printed in the banner as a
//! reminder of which module to dial; [`Reflector`] itself doesn't restrict
//! which module a client links onto, and parrot mode echoes a sender's
//! transmission back regardless of which module they picked.
//!
//! No Ctrl-C handling beyond the OS default: this is a dev-tool-grade
//! runnable (per the iax-91f4 brief), not a production daemon (see
//! iax-d3b6 for what a hardened standalone daemon would still need). The
//! process just parks the main thread forever; `^C` kills the whole process,
//! so [`ReflectorHandle`]'s `Drop` (which would join the run-loop thread) is
//! never reached — harmless here since there's nothing to flush or persist.

use std::net::SocketAddr;
use std::process::ExitCode;

use astar_m17::Reflector;

const USAGE: &str = "usage: m17_parrot --port <port> [--module <A-Z>]";

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
    let mut module: u8 = b'A';

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                let v = args.next().ok_or(USAGE)?;
                port = Some(v.parse::<u16>().map_err(|_| format!("bad --port {v:?}"))?);
            }
            "--module" => {
                let v = args.next().ok_or(USAGE)?;
                module = parse_module(&v)?;
            }
            flag => return Err(format!("unknown argument: {flag}\n{USAGE}").into()),
        }
    }
    let port = port.ok_or(USAGE)?;

    // Unspecified IPv6, not IPv4 — see the module doc's "Dual-stack bind"
    // section for why: this is what lets both `127.0.0.1` and `localhost`
    // (which resolves `[::1]`-first on macOS) reach the same reflector.
    let addr: SocketAddr = (std::net::Ipv6Addr::UNSPECIFIED, port).into();
    let reflector = Reflector::bind_parrot(addr)?;
    let bound = reflector.local_addr();
    println!(
        "M17 parrot on [::]:{} module {} — dial 127.0.0.1:{}/{} or localhost:{}/{}",
        bound.port(),
        module as char,
        bound.port(),
        module as char,
        bound.port(),
        module as char
    );
    let _handle = reflector.run();

    // Dev-tool-grade shutdown: park forever, let `^C` kill the process. See
    // the module doc comment for why this is fine here.
    loop {
        std::thread::park();
    }
}

/// Parses a single uppercase A-Z module letter from a 1-character CLI arg
/// (case-insensitive input, like [`astar_m17::ControlPacket::Conn`]'s
/// module field expects on the wire).
fn parse_module(s: &str) -> Result<u8, Box<dyn std::error::Error>> {
    let mut chars = s.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return Err(format!("--module must be a single letter A-Z, got {s:?}").into());
    };
    if !c.is_ascii_alphabetic() {
        return Err(format!("--module must be a single letter A-Z, got {s:?}").into());
    }
    Ok(c.to_ascii_uppercase() as u8)
}
