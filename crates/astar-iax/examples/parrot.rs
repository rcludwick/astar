// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Minimal parrot/echo-test consumer for astar-iax (iax-612e acceptance example).
//! Dials an ASL3 parrot node, prints lifecycle, holds PTT for a few seconds, hangs up.
//! Run: cargo run -p astar-iax --example parrot -- [flags] <host:port> <dest-ext>
//!
//! Flags (iax-fd34):
//!   --list-devices      print audio devices and exit
//!   --input <substr>    pick the capture device by name substring
//!   --output <substr>   pick the playback device by name substring
//!
//! Migrated off the deleted high-level `Client` onto the iax-42e9 `Manager`
//! (iax-64b6 P3): `dial(DialSpec)` pools the call monitor-only, `route` binds the
//! mic, `key`/`unkey` drive PTT, and a single `take_events` consumer prints the
//! lifecycle exactly as the old `Client::dial` event stream did.

use std::env;
use std::net::ToSocketAddrs;
use std::process::ExitCode;
use std::time::Duration;

use astar_audio::{AudioBackend, CpalBackend, DeviceInfo, Direction, MicId, OutputId};
use astar_iax::manager::DialSpec;
use astar_iax::{CallEvent, CallId, CallMode, CodecPolicy, Manager};

const USAGE: &str = "usage: parrot [--list-devices] [--input <substr>] [--output <substr>] [<host:port>] [<dest-ext>]";

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
    let mut input_device: Option<String> = None;
    let mut output_device: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list-devices" => return list_devices(),
            "--input" => input_device = Some(args.next().ok_or(USAGE)?),
            "--output" => output_device = Some(args.next().ok_or(USAGE)?),
            flag if flag.starts_with("--") => {
                eprintln!("unknown flag: {flag}\n{USAGE}");
                return Ok(ExitCode::FAILURE);
            }
            _ => positional.push(arg),
        }
    }
    let mut positional = positional.into_iter();
    let host = positional
        .next()
        .unwrap_or_else(|| "127.0.0.1:4569".to_string());
    let dest = positional.next().unwrap_or_else(|| "55553".to_string());
    let peer = host.to_socket_addrs()?.next().ok_or("bad host")?;

    // The Manager takes explicit device ids. Resolve any configured name
    // substring to a device id against the enumerated device list (the
    // case-insensitive, unique-substring match the old Client builder did via
    // `find_device`); unset falls back to the system default device.
    let mut mgr = Manager::new(Box::new(CpalBackend::new()));
    let in_id = resolve_device(&mgr, input_device.as_deref(), Direction::Input)?;
    let out_id = resolve_device(&mgr, output_device.as_deref(), Direction::Output)?;

    let id = mgr.dial(DialSpec {
        id: CallId::from_raw(1),
        node: dest.clone(),
        peer,
        output: OutputId::new(&out_id),
        caller_id: "astar".to_string(),
        secret: String::new(),
        mode: CallMode::Standard,
        dest: dest.clone(),
        frame_observer: None,
        codec_policy: CodecPolicy::default(),
    })?;
    mgr.route(id, &MicId::new(&in_id))?;
    println!("dialing… (Ctrl-C to abort)");

    // Print events on a background thread until Hangup. The Manager pools the
    // call's event stream; take it once for this single consumer (mirrors the
    // old `Client::dial` returning a `Receiver<CallEvent>`).
    let events = mgr.take_events(id).ok_or("call has no event stream")?;
    let ev = std::thread::spawn(move || {
        for e in events {
            match e {
                CallEvent::Answered { .. } => println!("connected"),
                CallEvent::Hangup { reason } => {
                    println!("hangup: {reason:?}");
                    break;
                }
                other => println!("event: {other:?}"),
            }
        }
    });

    // Hold PTT for 3 seconds (mic audio flows to the parrot), then release.
    mgr.key(id)?;
    std::thread::sleep(Duration::from_secs(3));
    mgr.unkey(id)?;

    mgr.hangup(id, None)?;
    let _ = ev.join();
    Ok(ExitCode::SUCCESS)
}

/// Resolve an optional device-name substring to a concrete device id. `None`
/// falls back to the backend default for `dir`. A present substring is matched
/// case-insensitively and must hit exactly one usable device (`Duplex` counts
/// either way) — zero or several is a configuration error, exactly as the old
/// `Client` builder's `find_device` enforced (iax-fd34).
fn resolve_device(
    mgr: &Manager,
    query: Option<&str>,
    dir: Direction,
) -> Result<String, Box<dyn std::error::Error>> {
    let Some(q) = query else {
        let dev = match dir {
            Direction::Output => mgr.default_output(),
            _ => mgr.default_input(),
        }
        .ok_or("no default audio device")?;
        return Ok(dev.id.as_str().to_string());
    };

    let devices = mgr.devices()?;
    let usable = |d: &&DeviceInfo| d.direction == dir || d.direction == Direction::Duplex;
    let needle = q.to_lowercase();
    let matches: Vec<&DeviceInfo> = devices
        .iter()
        .filter(usable)
        .filter(|d| d.name.to_lowercase().contains(&needle))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.id.as_str().to_string()),
        [] => Err(format!("no {dir:?} device name contains {q:?}").into()),
        many => {
            let names: Vec<&str> = many.iter().map(|d| d.name.as_str()).collect();
            Err(format!("{q:?} is ambiguous; matches {names:?}").into())
        }
    }
}

/// `--list-devices`: enumerate and print every audio device, then exit.
fn list_devices() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let backend = CpalBackend::new();
    for d in backend.devices()? {
        let rates = d
            .native_sample_rates
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join("/");
        println!(
            "{:<40} [{:?}] ch={} rates={}",
            d.name,
            d.direction,
            d.channels,
            if rates.is_empty() {
                "?".to_string()
            } else {
                rates
            },
        );
    }
    Ok(ExitCode::SUCCESS)
}
