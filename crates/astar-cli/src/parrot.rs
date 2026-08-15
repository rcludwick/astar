// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `parrot` subcommand: call a parrot/echo extension and loop audio back. This
//! is the `dial` path with parrot-friendly defaults — host `127.0.0.1:4569`
//! and the ASL3 parrot extension `55553`. Key PTT, talk, unkey; the remote
//! parrot echoes your audio back through the selected output device.

use crate::audio::list_devices;
use crate::call;
use crate::cli::PARROT_USAGE;
use crate::dial::{self, Parsed};

/// Default peer when none is supplied on the command line.
const DEFAULT_HOST: &str = "127.0.0.1:4569";
/// ASL3 public parrot/echo extension (see project ASL3 dry-run notes).
const DEFAULT_DEST: &str = "55553";

pub fn run(args: impl Iterator<Item = String>) -> Result<(), String> {
    match dial::parse(args, PARROT_USAGE, Some((DEFAULT_HOST, DEFAULT_DEST)))? {
        Parsed::Help => {
            print!("{PARROT_USAGE}");
            Ok(())
        }
        Parsed::ListDevices => list_devices(),
        Parsed::Call(opts) => {
            println!("parrot mode: talk while keyed; the far end echoes it back.");
            call::run(&opts)
        }
    }
}
