// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! The `astar-netrec` binary; the work lives in the library.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use astar_iax_core::VoiceFormat;
use astar_netrec::config::{self, Config, ConfigError, NetCfg, StreamKeys};
use astar_netrec::daemon::{self, Live, Made};
use astar_netrec::runner::{Clock, RealClock};
use astar_netrec::sstv_cmd;
use astar_netrec::web;
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
    /// Render (or re-render) a finished session's video.mp4.
    Render {
        /// The session's folder, the one holding session.json.
        session: PathBuf,
    },
    /// Decode the SSTV pictures in a WAV, FLAC or .pcap recording into PNGs.
    /// Talks to no network.
    Sstv {
        /// The recording: a 16-bit PCM WAV, a FLAC, or a picture's or packet
        /// log's .pcap.
        file: PathBuf,
        /// The folder for the pictures; the recording's own folder if not
        /// given.
        #[arg(long)]
        out: Option<PathBuf>,
        /// The codec of a .pcap holding only mini frames: ulaw, alaw, slin,
        /// slin16 or gsm. Without it, the .jitter.jsonl beside the capture
        /// or the first full voice frame names it.
        #[arg(long, value_parser = sstv_cmd::parse_codec)]
        codec: Option<VoiceFormat>,
        /// Our own address in a .pcap holding both directions, such as
        /// 192.0.2.20:4569; everything sent from it is left out. Without
        /// it, the .jitter.jsonl beside the capture names it.
        #[arg(long)]
        local: Option<SocketAddr>,
    },
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

/// Load a config for recording, logging its warnings (a live switch with
/// no stream key among them).
fn checked(cfg: Config, keys: &StreamKeys) -> Result<Config, String> {
    let warnings =
        config::validate_with_keys(&cfg, RealClock.now(), keys).map_err(|e| e.to_string())?;
    for warning in warnings {
        tracing::warn!("{warning}");
    }
    Ok(cfg)
}

/// The stream keys for `run` and `now`. A credentials file that will not
/// load costs the live streams, never the recording: it is logged and no
/// key is used. The error never holds a key.
fn recording_keys(config: &Path) -> StreamKeys {
    config::load_stream_keys(config).unwrap_or_else(|e| {
        tracing::error!("no live streams: {e}");
        StreamKeys::default()
    })
}

/// What `run` and `now` record and stream with: the `ffmpeg` (found once)
/// and the keys.
fn media(cfg: &Config, config: &Path) -> (Option<astar_netvideo::Ffmpeg>, Live) {
    let keys = recording_keys(config);
    let ff = daemon::find_ffmpeg(cfg);
    let live = Live::with_ffmpeg(keys, ff.clone());
    (ff, live)
}

/// The `ffmpeg` to look for and the options for `render`: from the config,
/// `[video] ffmpeg` and `[recorder] my_callsigns` (the fallback "my side",
/// as the daemon uses it), and nothing else.
fn render_setup(config: &Path) -> Result<(PathBuf, astar_netvideo::RenderOptions), String> {
    let cfg = config::render_only(config).map_err(|e| e.to_string())?;
    Ok((
        cfg.ffmpeg,
        astar_netvideo::RenderOptions {
            my_callsigns: cfg.my_callsigns,
            ..astar_netvideo::RenderOptions::default()
        },
    ))
}

/// `render <session>`: the folder must hold a finished session. Only
/// `[video] ffmpeg` and `[recorder] my_callsigns` are read from the config,
/// and only if there is one; the session need not be under the config's
/// root.
fn render(config: &Path, session: &Path, stop: &AtomicBool) -> Result<String, String> {
    let meta = astar_netarchive::read_meta(session).map_err(|e| {
        format!(
            "cannot read {}: {e}",
            session.join(astar_netarchive::META_FILE).display()
        )
    })?;
    if meta.ended.is_none() {
        return Err(format!(
            "refusing: {} is still recording",
            session.display()
        ));
    }
    let (program, opts) = render_setup(config)?;
    let ff = astar_netvideo::find(&program).map_err(|e| e.to_string())?;
    let r = astar_netvideo::render_session(session, &ff, &opts, stop).map_err(|e| e.to_string())?;
    Ok(format!(
        "wrote {} ({} frames, {} reused, {:.1} s)",
        session.join(astar_netvideo::VIDEO_FILE).display(),
        r.frames,
        r.reused,
        r.seconds
    ))
}

/// `astar-netrec sstv`: exit 0 with at least one picture saved, 2 with none,
/// 1 on an error, as `slowrx-cli` does. A recording damaged part way
/// through exits 0 when a picture was saved before the damage, with a
/// warning, and 1 when none was.
fn sstv(file: &Path, out: Option<&Path>, options: &sstv_cmd::Options) -> ExitCode {
    let folder = out.map_or_else(
        || match file.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        },
        Path::to_path_buf,
    );
    match sstv_cmd::decode_file_with(file, &folder, options) {
        Ok(decoded) if decoded.saved.is_empty() => {
            eprintln!("astar-netrec: no SSTV picture found in {}", file.display());
            ExitCode::from(2)
        }
        Ok(decoded) => {
            for picture in &decoded.saved {
                println!("{}", picture.summary());
            }
            if let Some(damage) = decoded.damage {
                eprintln!(
                    "astar-netrec: warning: {}: {damage}; only the pictures heard before it were saved",
                    file.display()
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("astar-netrec: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        // Handled in `main`, for its own exit codes.
        Command::Sstv { .. } => Ok(()),
        Command::Run => {
            let cfg = config::load_or_bootstrap(&cli.config).map_err(|e| e.to_string())?;
            let (ff, live) = media(&cfg, &cli.config);
            let cfg = checked(cfg, &live.keys)?;
            let stop = stop_on_signal()?;
            // Held until `run` returns. A port that will not bind costs the
            // pages, not the recording.
            let _pages = match web::serve_local(&cfg) {
                Ok(server) => server,
                Err(e) => {
                    tracing::error!("cannot serve the recordings pages on {}: {e}", cfg.web.bind);
                    None
                }
            };
            let make = |net: &NetCfg| -> Made { daemon::station_adapter(&cfg, net) };
            daemon::run(&cfg, ff, &live, &RealClock, &stop, &make).map_err(|e| e.to_string())
        }
        Command::Check => {
            let cfg = config::load_or_bootstrap(&cli.config).map_err(|e| e.to_string())?;
            let keys = config::load_stream_keys(&cli.config).map_err(|e| e.to_string())?;
            let (warnings, lines) =
                daemon::check_report(&cfg, RealClock.now(), &keys).map_err(|e| e.to_string())?;
            for warning in warnings {
                eprintln!("warning: {warning}");
            }
            let ff = astar_netvideo::find(&cfg.video.ffmpeg);
            for line in daemon::check_media(&ff, &keys).into_iter().chain(lines) {
                println!("{line}");
            }
            Ok(())
        }
        Command::Now { net, length } => {
            let cfg = load_existing(&cli.config).map_err(|e| e.to_string())?;
            let (_, live) = media(&cfg, &cli.config);
            let cfg = checked(cfg, &live.keys)?;
            let length = config::parse_duration(&length)?;
            if length.is_zero() {
                return Err("--for must be longer than zero".to_string());
            }
            let stop = stop_on_signal()?;
            let make = |n: &NetCfg| -> Made { daemon::station_adapter(&cfg, n) };
            daemon::record_now(&cfg, &live, &net, length, &RealClock, &stop, &make)
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Command::Status => {
            let cfg = load_existing(&cli.config).map_err(|e| e.to_string())?;
            println!("{}", daemon::status_line(&cfg.recorder.root));
            Ok(())
        }
        Command::Render { session } => {
            let stop = stop_on_signal()?;
            println!("{}", render(&cli.config, &session, &stop)?);
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    // A usage error exits 1, not clap's 2: `sstv` gives 2 its own meaning,
    // "no picture found".
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    init_logging();
    if let Command::Sstv {
        file,
        out,
        codec,
        local,
    } = &cli.command
    {
        let options = sstv_cmd::Options {
            codec: *codec,
            local: *local,
        };
        return sstv(file, out.as_deref(), &options);
    }
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("astar-netrec: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("netrec-render-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch");
        d
    }

    /// The fixture's still-recording session, under `root`.
    fn open_session(root: &Path) -> PathBuf {
        let _ = astar_netarchive::fixture::write_fixture(root);
        let net = root.join(astar_netarchive::net_dir_name("Thursday D-Star net"));
        std::fs::read_dir(net)
            .expect("net folder")
            .map(|e| e.expect("entry").path())
            .find(|p| astar_netarchive::read_meta(p).is_ok_and(|m| m.ended.is_none()))
            .expect("the open session")
    }

    #[test]
    fn render_refuses_a_session_still_recording() {
        let root = scratch("open");
        let dir = open_session(&root);
        let err = render(&root.join("none.toml"), &dir, &AtomicBool::new(false)).unwrap_err();
        assert_eq!(
            err,
            format!("refusing: {} is still recording", dir.display())
        );
        assert!(!dir.join(astar_netvideo::VIDEO_FILE).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn render_needs_a_session_folder() {
        let root = scratch("empty");
        let err = render(&root.join("none.toml"), &root, &AtomicBool::new(false)).unwrap_err();
        assert!(err.starts_with("cannot read "), "{err}");
        assert!(err.contains("session.json"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn render_takes_my_callsigns_from_the_config() {
        let root = scratch("mycalls");
        let cfg = root.join("netrec.toml");
        std::fs::write(&cfg, "[recorder]\nmy_callsigns = [\"N0CALL\"]\n").expect("config");
        let (program, opts) = render_setup(&cfg).expect("setup");
        assert_eq!(program, Path::new("ffmpeg"));
        assert_eq!(opts.my_callsigns, vec!["N0CALL".to_owned()]);
        // No config at all: no callsigns, as before.
        let (_, opts) = render_setup(&root.join("none.toml")).expect("setup");
        assert_eq!(opts.my_callsigns, Vec::<String>::new());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn render_names_a_missing_ffmpeg() {
        let root = scratch("noff");
        let _ = astar_netarchive::fixture::write_fixture(&root);
        let net = root.join(astar_netarchive::net_dir_name("Tuesday M17 net"));
        let dir = std::fs::read_dir(net)
            .expect("net folder")
            .map(|e| e.expect("entry").path())
            .find(|p| p.join("session.json").is_file())
            .expect("the session");
        let cfg = root.join("netrec.toml");
        let gone = root.join("no/such/ffmpeg");
        std::fs::write(
            &cfg,
            format!("[video]\nffmpeg = {:?}\n", gone.display().to_string()),
        )
        .expect("config");
        let err = render(&cfg, &dir, &AtomicBool::new(false)).unwrap_err();
        assert!(err.contains(&gone.display().to_string()), "{err}");
        assert!(err.contains("brew install ffmpeg"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
