// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `astar-cli` — a thin command-line front-end over the `astar-iax`
//! engine crates. Three always-on subcommands: `register`, `dial`, and
//! `parrot`, with push-to-talk driven from stdin (see [`ptt`] / `--help`);
//! plus `dstar-listen` (D-Star `DExtra` decode, with manual stdin PTT for
//! TX — iax-2f6b) behind the `dstar` feature — off by default, see the
//! `dstar_listen` module's doc (only compiled in with that feature, hence
//! no intra-doc link here).
//!
//! This binary owns no protocol or audio logic: registration goes through
//! `astar_iax::Registrar`, calls through `astar_iax::Manager`, D-Star
//! through `astar_station::Station`, and audio through
//! `astar_audio::CpalBackend`.

mod audio;
mod call;
mod cli;
mod dial;
#[cfg(feature = "dstar")]
mod dstar_listen;
mod parrot;
mod ptt;
mod register;

use std::process::ExitCode;

use cli::USAGE;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };

    let result = match command.as_str() {
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        "register" => register::run(args),
        "dial" => dial::run(args),
        "parrot" => parrot::run(args),
        #[cfg(feature = "dstar")]
        "dstar-listen" => dstar_listen::run(args),
        #[cfg(not(feature = "dstar"))]
        "dstar-listen" => Err("dstar-listen requires building with `--features dstar`".to_string()),
        other => {
            eprintln!("unknown command: {other}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
