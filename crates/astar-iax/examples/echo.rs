// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Headless parrot test client (iax-64b6): dials an IAX2 node, RECORDS the
//! audio the far end sends while keyed, waits a few seconds after they unkey,
//! then PLAYS IT BACK through the node — the classic parrot. Lets a SOLO
//! operator test their Node-mode handset: start the node, run this pointed at
//! it, key the node handset and talk, release PTT, and a moment later you hear
//! your own recording played back.
//!
//! Run: cargo run -p astar-iax --example echo -- <host:port> [dest-ext] [`codec_policy`]
//!   <host:port>   the node's listener address (e.g. 127.0.0.1:4569)
//!   [dest-ext]    `CALLED_NUMBER` (default `"s"`)
//!   [`codec_policy`] `ulaw_only|allow_slin|prefer_slin|prefer_slin16` (default `"ulaw_only"`)

use std::env;
use std::net::ToSocketAddrs;
use std::process::ExitCode;
use std::str::FromStr;

use astar_iax::{CallMode, CodecPolicy, ParrotConfig, dial_raw_with_policy, run_parrot};

const USAGE: &str = "usage: echo <host:port> [dest-ext] [codec_policy: ulaw_only|allow_slin|prefer_slin|prefer_slin16]";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let Some(host) = args.next() else {
        eprintln!("{USAGE}");
        return Ok(ExitCode::FAILURE);
    };
    let dest = args.next().unwrap_or_else(|| "s".to_string());
    let codec_policy_str = args.next().unwrap_or_else(|| "ulaw_only".to_string());
    let codec_policy = CodecPolicy::from_str(&codec_policy_str)?;
    let peer = host.to_socket_addrs()?.next().ok_or("bad host")?;

    // Headless raw dial: empty secret (the node should run auth=Off for this
    // test). Standard mode sends CALLED_NUMBER=<dest>, a plain IAX2 node dial.
    let raw = dial_raw_with_policy(
        peer,
        "echo-test",
        dest,
        "",
        CallMode::Standard,
        codec_policy,
    )?;
    println!(
        "parrot connected to {peer} with codec policy {codec_policy:?}; key your node handset, talk, then release \
         PTT — a few seconds later you'll hear it played back. (Ctrl-C to quit)"
    );

    // The example runs until the call hangs up / Ctrl-C; never set stop.
    let stop = std::sync::atomic::AtomicBool::new(false);
    run_parrot(raw, &ParrotConfig::default(), &stop, |line| {
        println!("{line}");
    });
    Ok(ExitCode::SUCCESS)
}
