// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `dial` subcommand: place a call to `<number>` and stream audio with
//! keyboard PTT.

use crate::audio::list_devices;
use crate::call::{self, CallOptions};
use crate::cli::{self, DIAL_USAGE};

pub fn run(args: impl Iterator<Item = String>) -> Result<(), String> {
    let parsed = parse(args, DIAL_USAGE, None)?;
    match parsed {
        Parsed::Help => {
            print!("{DIAL_USAGE}");
            Ok(())
        }
        Parsed::ListDevices => list_devices(),
        Parsed::Call(opts) => call::run(&opts),
    }
}

pub enum Parsed {
    Help,
    ListDevices,
    Call(CallOptions),
}

/// Shared option parser for `dial` and `parrot`. `default_dest` makes the
/// destination optional (parrot) when `Some`; `None` requires it (dial).
pub fn parse(
    mut args: impl Iterator<Item = String>,
    usage: &str,
    default_dest: Option<(&str, &str)>,
) -> Result<Parsed, String> {
    let mut caller_id = "astar".to_string();
    let mut secret = String::new();
    let mut input: Option<String> = None;
    let mut output: Option<String> = None;
    let mut ptt_prompt = true;
    let mut positional: Vec<String> = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--list-devices" => return Ok(Parsed::ListDevices),
            "--caller-id" => caller_id = cli::flag_value(&mut args, "--caller-id")?,
            "--secret" => secret = cli::flag_value(&mut args, "--secret")?,
            "--input" => input = Some(cli::flag_value(&mut args, "--input")?),
            "--output" => output = Some(cli::flag_value(&mut args, "--output")?),
            "--no-ptt-prompt" => ptt_prompt = false,
            flag if flag.starts_with("--") => {
                return Err(format!("unknown flag: {flag}\n\n{usage}"));
            }
            _ => positional.push(arg),
        }
    }

    let mut pos = positional.into_iter();
    let (host, dest) = if let Some((default_host, default_dest)) = default_dest {
        // parrot: both positionals optional, with defaults.
        let host = pos.next().unwrap_or_else(|| default_host.to_string());
        let dest = pos.next().unwrap_or_else(|| default_dest.to_string());
        (host, dest)
    } else {
        // dial: host and number both required.
        let host = pos
            .next()
            .ok_or_else(|| format!("missing <host>\n\n{usage}"))?;
        let dest = pos
            .next()
            .ok_or_else(|| format!("missing <number>\n\n{usage}"))?;
        (host, dest)
    };

    let peer = cli::resolve_peer(&host)?;
    Ok(Parsed::Call(CallOptions {
        peer,
        dest,
        caller_id,
        secret,
        input,
        output,
        ptt_prompt,
    }))
}
