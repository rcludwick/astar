// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The `astar-netrec` binary; the work lives in the library.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use astar_netrec::config::{self, Config, ConfigError, NetCfg};
use astar_netrec::daemon::{self, Made};
use astar_netrec::runner::{Clock, RealClock};
use clap::{Parser, Subcommand};

/// Receive-only recorder of amateur-radio nets.
#[derive(Parser)]
#[command(name = "astar-netrec", version, about)]
struct Cli {
    /// The config file.
    #[arg(long, global = true, default_value = "./netrec.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon: record every net on the schedule.
    Run,
    /// Validate the config and list the next 10 sessions.
    Check,
    /// Record one net right now.
    Now {
        /// The net's name, in any case.
        net: String,
        /// How long to record, such as "30m" or "1h30m".
        #[arg(long = "for", default_value = "30m")]
        length: String,
    },
    /// Say what the daemon is doing.
    Status,
}

/// Logs go to stderr, and `RUST_LOG` overrides the default of `info`.
fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();
}

/// SIGINT and SIGTERM set the flag that is returned, as in astar-server. A
/// second signal gives up on a clean close.
fn stop_on_signal() -> Result<Arc<AtomicBool>, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        if flag.swap(true, Ordering::AcqRel) {
            std::process::exit(130);
        }
    })
    .map_err(|e| format!("cannot install the signal handler: {e}"))?;
    Ok(stop)
}

/// Read a config that must already exist. `status` and `now` act on an
/// archive that is running, so a template written here would be no use, and
/// `status` would describe the template's archive rather than the real one.
fn load_existing(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    config::from_toml_str(&text)
}

/// Load a config for recording, logging its warnings.
fn checked(cfg: Config) -> Result<Config, String> {
    let warnings = config::validate(&cfg, RealClock.now()).map_err(|e| e.to_string())?;
    for warning in warnings {
        tracing::warn!("{warning}");
    }
    Ok(cfg)
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Run => {
            let cfg = config::load_or_bootstrap(&cli.config).map_err(|e| e.to_string())?;
            let cfg = checked(cfg)?;
            let stop = stop_on_signal()?;
            let make = |net: &NetCfg| -> Made { daemon::station_adapter(&cfg, net) };
            daemon::run(&cfg, &RealClock, &stop, &make).map_err(|e| e.to_string())
        }
        Command::Check => {
            let cfg = config::load_or_bootstrap(&cli.config).map_err(|e| e.to_string())?;
            let (warnings, lines) =
                daemon::check_report(&cfg, RealClock.now()).map_err(|e| e.to_string())?;
            for warning in warnings {
                eprintln!("warning: {warning}");
            }
            for line in lines {
                println!("{line}");
            }
            Ok(())
        }
        Command::Now { net, length } => {
            let cfg = load_existing(&cli.config).map_err(|e| e.to_string())?;
            let cfg = checked(cfg)?;
            let length = config::parse_duration(&length)?;
            if length.is_zero() {
                return Err("--for must be longer than zero".to_string());
            }
            let stop = stop_on_signal()?;
            let make = |n: &NetCfg| -> Made { daemon::station_adapter(&cfg, n) };
            daemon::record_now(&cfg, &net, length, &RealClock, &stop, &make)
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Command::Status => {
            let cfg = load_existing(&cli.config).map_err(|e| e.to_string())?;
            println!("{}", daemon::status_line(&cfg.recorder.root));
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("astar-netrec: {e}");
            ExitCode::FAILURE
        }
    }
}
