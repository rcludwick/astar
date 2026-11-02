// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! `nodebench` — astar-server against a real ASL3 node in podman.
//!
//! Usage:
//!   nodebench run [--topology loopback] [--keep] [--reuse] [--json-out DIR] [scenario...]
//!   nodebench list
//!   nodebench soak --topology loopback|nat --hours N [--udp-timeout S] [--resume DIR]
//!   nodebench serve [--bind 127.0.0.1:8740] [--runs DIR]
//!
//! `run` brings the rig up, runs the scenarios, prints the table, writes
//! `report.json` with logs and captures, tears the rig down (unless `--keep`),
//! and exits 0 when nothing failed, 1 when a check failed, 2 when the rig
//! itself would not come up.

use std::io::IsTerminal;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};

use astar_nodebench::node::NodeDriver;
use astar_nodebench::report::{EnvInfo, Report, ScenarioResult};
use astar_nodebench::rig::{Rig, RigConfig, RigError, Topology};
use astar_nodebench::scenario::{self, Check, Ctx, Outcome, Probe, Tuning, wait_until};
use astar_nodebench::server::ServerClient;
use astar_nodebench::soak;
use astar_nodebench::time::{rfc3339_utc, run_id, unix_now};

#[derive(Parser)]
#[command(
    name = "nodebench",
    version,
    about = "astar-server regression bench against a real ASL3 node in podman (no radio, 127.0.0.1 only)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Bring the rig up, run scenarios, report, tear down.
    Run(RunArgs),
    /// List the scenarios.
    List,
    /// Hold a link for hours, sample both sides, and log every drop.
    Soak(SoakArgs),
    /// Serve the results page on 127.0.0.1.
    Serve(ServeArgs),
}

#[derive(Args)]
struct RigArgs {
    /// Topology to play: loopback | nat.
    #[arg(long, default_value = "loopback")]
    topology: Topology,
    /// Directory holding `pod.yaml` / `nat.yaml` [default: <repo>/harness/nodebench].
    #[arg(long)]
    rig_dir: Option<PathBuf>,
    /// podman connection; defaults to `$CONTAINER_CONNECTION`.
    #[arg(long)]
    connection: Option<String>,
    /// Set a kube-file placeholder, e.g. `--var CODECS=ulaw` (repeatable).
    #[arg(long = "var", value_name = "KEY=VALUE", value_parser = parse_var)]
    vars: Vec<(String, String)>,
}

fn parse_var(s: &str) -> Result<(String, String), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("expected KEY=VALUE, got {s:?}"))?;
    if k.is_empty()
        || !k
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(format!("placeholder names are UPPER_SNAKE: {k:?}"));
    }
    Ok((k.to_string(), v.to_string()))
}

#[derive(Args)]
struct RunArgs {
    #[command(flatten)]
    rig: RigArgs,
    /// Leave the rig up afterwards.
    #[arg(long)]
    keep: bool,
    /// Use a rig that is already up: no `kube play`, no teardown.
    #[arg(long, conflicts_with = "replace")]
    reuse: bool,
    /// Recreate the pod if it already exists (default: refuse).
    #[arg(long)]
    replace: bool,
    /// Base directory for run output [default: <repo>/target/nodebench].
    #[arg(long, value_name = "DIR")]
    json_out: Option<PathBuf>,
    /// Override the hold window of "holds" checks (the design says 60).
    #[arg(long, value_name = "SECS")]
    hold_secs: Option<u64>,
    /// Scenario ids to run, in order (default: all).
    scenarios: Vec<String>,
}

#[derive(Args)]
#[allow(clippy::struct_excessive_bools)] // CLI switches
struct SoakArgs {
    #[command(flatten)]
    rig: RigArgs,
    /// How long to hold the link (fractions allowed: 0.05 = 3 min). Required
    /// unless --resume, which carries on with what is left.
    #[arg(long)]
    hours: Option<f64>,
    /// conntrack UDP timeout (both `_udp_timeout` and `_stream`) on the nat
    /// router; nat topology only [default: 30].
    #[arg(long, value_name = "SECS")]
    udp_timeout: Option<u64>,
    /// Seconds between samples of both sides.
    #[arg(long, value_name = "SECS", default_value_t = 5.0)]
    sample_secs: f64,
    /// Who dials the link: node (`ilink 3`, the `ClearNode` case) | server.
    #[arg(long = "from", default_value = "node")]
    dialer: soak::Dialer,
    /// Stop at the first drop instead of re-linking and counting.
    #[arg(long)]
    no_relink: bool,
    /// Carry on a stopped soak in this run directory (appends to its
    /// soak.jsonl and runs what is left of its planned time; --hours is
    /// ignored).
    #[arg(long, value_name = "DIR")]
    resume: Option<PathBuf>,
    /// Leave the rig up afterwards.
    #[arg(long)]
    keep: bool,
    /// Use a rig that is already up (no bring-up, no teardown).
    #[arg(long, conflicts_with = "replace")]
    reuse: bool,
    /// Recreate the rig if it already exists.
    #[arg(long)]
    replace: bool,
    /// Base directory for run output [default: <repo>/target/nodebench].
    #[arg(long, value_name = "DIR")]
    json_out: Option<PathBuf>,
}

#[derive(Args)]
struct ServeArgs {
    /// Address for the page; loopback only.
    #[arg(long, default_value = "127.0.0.1:8740")]
    bind: std::net::SocketAddr,
    /// Directory of run directories [default: <repo>/target/nodebench].
    #[arg(long, value_name = "DIR")]
    runs: Option<PathBuf>,
}

fn rig_config(a: &RigArgs) -> RigConfig {
    let mut cfg = RigConfig::new(RigConfig::default_repo_root());
    cfg.topology = a.topology;
    if let Some(d) = &a.rig_dir {
        cfg.rig_dir.clone_from(d);
    }
    if let Some(c) = &a.connection {
        cfg.connection = Some(c.clone());
    }
    cfg.kube_vars.extend(a.vars.iter().cloned());
    cfg
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::List => {
            for s in scenario::registry() {
                println!("{:<28} {}", s.id(), s.describe());
            }
            ExitCode::SUCCESS
        }
        Cmd::Run(a) => run(&a),
        Cmd::Soak(a) => soak_cmd(&a),
        Cmd::Serve(a) => {
            let base = a.runs.clone().unwrap_or_else(|| {
                RigConfig::default_repo_root()
                    .join("target")
                    .join("nodebench")
            });
            match astar_nodebench::serve::serve(a.bind, &base) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("nodebench serve: {e}");
                    ExitCode::from(2)
                }
            }
        }
    }
}

/// Wait for `app_rpt` to load the node, AMI to answer, and astar-server to
/// report it is listening.
fn wait_ready(cfg: &RigConfig) -> Result<Duration, RigError> {
    let node = NodeDriver::new(cfg);
    let server = ServerClient::new(&cfg.server_url);
    let probe = || -> Probe {
        match node.node_loaded() {
            Ok(true) => {}
            Ok(false) => return Err(format!("app_rpt has not loaded node {}", cfg.node_number)),
            Err(e) => return Err(format!("node: {e}")),
        }
        node.ami(false, Duration::from_secs(3))
            .map(astar_nodebench::node::ami::AmiClient::logoff)
            .map_err(|e| format!("AMI: {e}"))?;
        match server.status() {
            Ok(s) if s.listening => Ok(()),
            Ok(_) => Err("astar-server is not listening".into()),
            Err(e) => Err(e.to_string()),
        }
    };
    wait_until(cfg.ready_timeout, cfg.ready_poll, probe).map_err(|e| RigError::NotReady {
        what: format!("rig (after {:.0?})", e.waited),
        last: e.last,
    })
}

/// Run each scenario in its own artifact directory, turning a panic into a
/// failed check rather than a lost run.
fn run_scenarios(
    selected: Vec<Box<dyn scenario::Scenario>>,
    cfg: &RigConfig,
    tuning: &Tuning,
    run_dir: &std::path::Path,
    report: &mut Report,
) {
    for s in selected {
        let dir = run_dir.join(s.id());
        if let Err(e) = std::fs::create_dir_all(&dir) {
            report
                .rig_notes
                .push(format!("create {}: {e}", dir.display()));
        }
        let mut ctx = Ctx::new(Rig::new(cfg.clone()), tuning.clone(), dir);
        ctx.started_pod = astar_nodebench::pcap::pod_now(&ctx.rig).ok();
        eprintln!("nodebench: {} ...", s.id());
        let t0 = Instant::now();
        let mut outcome = catch_unwind(AssertUnwindSafe(|| s.run(&mut ctx))).unwrap_or_else(|p| {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_string()))
                .unwrap_or_else(|| "panic".into());
            let mut o = Outcome::new();
            o.check(Check::fail(
                "scenario completed",
                format!("panicked: {msg}"),
            ));
            o
        });
        if outcome.failed() {
            ctx.failure_artifacts(&mut outcome);
        }
        eprintln!(
            "nodebench: {} {}",
            s.id(),
            if outcome.failed() { "FAILED" } else { "ok" }
        );
        report.push(ScenarioResult {
            id: s.id().to_string(),
            describe: s.describe().to_string(),
            duration_secs: t0.elapsed().as_secs_f64(),
            outcome,
        });
    }
}

fn run(a: &RunArgs) -> ExitCode {
    let selected = match scenario::select(&a.scenarios) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("nodebench: {e}");
            return ExitCode::from(2);
        }
    };
    let mut cfg = rig_config(&a.rig);
    cfg.replace = a.replace;
    let started = unix_now();
    let id = run_id(started);
    let base = a
        .json_out
        .clone()
        .unwrap_or_else(|| cfg.repo_root.join("target").join("nodebench"));
    let run_dir = base.join(&id);
    // Captures land straight in the run directory (a hostPath; on the Mac
    // the podman VM mounts the home directory at the same path), unless the
    // caller pinned CAPTURES with --var.
    if !a.rig.vars.iter().any(|(k, _)| k == "CAPTURES") {
        cfg.kube_vars.insert(
            "CAPTURES".into(),
            run_dir.join("pcap").to_string_lossy().into_owned(),
        );
    }

    let mut env = EnvInfo::local(&cfg.repo_root, &cfg.topology.to_string());
    env.podman_connection.clone_from(&cfg.connection);
    let rig = Rig::new(cfg.clone());
    let mut report = Report::new(id.clone(), rfc3339_utc(started), env);

    let mut tuning = Tuning::default();
    if let Some(h) = a.hold_secs {
        tuning.hold = Duration::from_secs(h);
    }

    let color = std::io::stdout().is_terminal();
    eprintln!("nodebench: run {id} -> {}", run_dir.display());

    // ---- bring the rig up ----
    // `played`: this run started the pod, so it owns teardown. `live`: there
    // is a pod whose logs are worth collecting.
    let mut played = false;
    let mut live = false;
    let setup = (|| -> Result<(), RigError> {
        report.env.podman_version = Some(rig.preflight()?);
        if a.reuse {
            if !rig.is_up()? {
                return Err(RigError::NotReady {
                    what: format!("pod {}", cfg.pod),
                    last: "--reuse given but the pod does not exist".into(),
                });
            }
            live = true;
        } else {
            eprintln!("nodebench: podman kube play {}", cfg.kube_file().display());
            rig.up()?;
            played = true;
            live = true;
        }
        let t = wait_ready(&cfg)?;
        eprintln!("nodebench: rig ready after {t:.1?}");
        Ok(())
    })();
    report.env.images = rig.images();

    if let Err(e) = setup {
        report.rig_error = Some(e.to_string());
    } else {
        run_scenarios(selected, &cfg, &tuning, &run_dir, &mut report);
    }

    // ---- collect and tear down ----
    if live {
        let (_files, notes) = rig.collect(&run_dir);
        report.rig_notes.extend(notes);
    }
    if played && !a.keep {
        if let Err(e) = rig.down() {
            report.rig_notes.push(format!("teardown: {e}"));
        }
    } else if live {
        eprintln!("nodebench: leaving the rig up");
    }
    report.finished = rfc3339_utc(unix_now());

    print!("{}", report.table(color));
    match report.write_json(&run_dir) {
        Ok(p) => println!("report: {}", p.display()),
        Err(e) => {
            eprintln!("nodebench: cannot write report: {e}");
            return ExitCode::from(2);
        }
    }
    ExitCode::from(report.exit_code())
}

/// Bring the soak's topology up (unless reusing one), returning whether this
/// process played it (and so owns teardown).
fn soak_up(rig: &Rig, reuse: bool) -> Result<bool, RigError> {
    let nat = rig.cfg.topology == Topology::Nat;
    let exists = if nat {
        soak::nat_exists(rig)?
    } else {
        rig.is_up()?
    };
    if reuse || (exists && !rig.cfg.replace) {
        if !exists {
            return Err(RigError::NotReady {
                what: format!("{} rig", rig.cfg.topology),
                last: "--reuse given but it is not up".into(),
            });
        }
        if !reuse {
            return Err(RigError::NotReady {
                what: format!("{} rig", rig.cfg.topology),
                last: "already up; pass --reuse to use it or --replace to recreate it".into(),
            });
        }
        return Ok(false);
    }
    eprintln!(
        "nodebench soak: bringing up {}",
        rig.cfg.kube_file().display()
    );
    if nat {
        soak::nat_up(rig, rig.cfg.replace)?;
    } else {
        rig.up()?;
    }
    Ok(true)
}

fn soak_down(rig: &Rig) -> Result<(), RigError> {
    if rig.cfg.topology == Topology::Nat {
        soak::nat_down(rig)
    } else {
        rig.down()
    }
}

/// The soak's run directory and options from the command line.
fn soak_plan(a: &SoakArgs, cfg: &RigConfig) -> Result<soak::SoakOptions, String> {
    let (dir, run_id, duration, resume) = if let Some(d) = &a.resume {
        let jsonl = d.join("soak.jsonl");
        let ev = soak::read_events(&jsonl).map_err(|e| format!("{}: {e}", jsonl.display()))?;
        let s = soak::summarize(&ev);
        if s.topology != cfg.topology.to_string() {
            return Err(format!(
                "{} is a {} soak; pass --topology {}",
                d.display(),
                s.topology,
                s.topology
            ));
        }
        (
            d.clone(),
            s.run_id.clone(),
            Duration::from_secs_f64(s.planned_secs.max(0.0)),
            true,
        )
    } else {
        let hours = a.hours.ok_or("--hours is required (or --resume DIR)")?;
        if !(hours.is_finite() && hours > 0.0) {
            return Err(format!("--hours must be positive, got {hours}"));
        }
        let id = run_id(unix_now());
        let base = a
            .json_out
            .clone()
            .unwrap_or_else(|| cfg.repo_root.join("target").join("nodebench"));
        (
            base.join(&id),
            id,
            Duration::from_secs_f64(hours * 3600.0),
            false,
        )
    };
    if !(a.sample_secs.is_finite() && a.sample_secs >= 1.0) {
        return Err("--sample-secs must be at least 1".into());
    }
    Ok(soak::SoakOptions {
        duration,
        sample: Duration::from_secs_f64(a.sample_secs),
        dialer: a.dialer,
        udp_timeout: a.udp_timeout,
        relink: !a.no_relink,
        dir,
        run_id,
        resume,
    })
}

fn soak_cmd(a: &SoakArgs) -> ExitCode {
    let mut cfg = rig_config(&a.rig);
    if cfg.topology == Topology::Nat {
        if a.dialer == soak::Dialer::Server {
            eprintln!(
                "nodebench soak: --from server cannot reach a node behind the NAT (no port forward); use --from node"
            );
            return ExitCode::from(2);
        }
        // A resumed soak that has to bring the rig up again keeps the
        // timeout it started with.
        let udp = a.udp_timeout.or_else(|| {
            let d = a.resume.as_ref()?;
            soak::summarize(&soak::read_events(&d.join("soak.jsonl")).ok()?).udp_timeout
        });
        cfg = soak::nat_rig_config(cfg, udp);
    } else if a.udp_timeout.is_some() {
        eprintln!("nodebench soak: --udp-timeout needs --topology nat");
        return ExitCode::from(2);
    }
    cfg.replace = a.replace;
    let opts = match soak_plan(a, &cfg) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("nodebench soak: {e}");
            return ExitCode::from(2);
        }
    };
    if !a.rig.vars.iter().any(|(k, _)| k == "CAPTURES") {
        cfg.kube_vars.insert(
            "CAPTURES".into(),
            opts.dir.join("pcap").to_string_lossy().into_owned(),
        );
    }

    // Ctrl-C stops the soak cleanly (summary written); a second one exits.
    let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
    if let Err(e) = ctrlc::set_handler(move || {
        if stop.swap(true, Ordering::SeqCst) {
            eprintln!("\nnodebench soak: second interrupt, exiting without a summary");
            std::process::exit(130);
        }
        eprintln!("\nnodebench soak: stopping (Ctrl-C again to abandon)...");
    }) {
        eprintln!("nodebench soak: no Ctrl-C handler ({e}); stopping early will lose the summary");
    }

    let rig = Rig::new(cfg.clone());
    eprintln!("nodebench soak: {} -> {}", opts.run_id, opts.dir.display());
    let mut played = false;
    let setup = (|| -> Result<(), RigError> {
        rig.preflight()?;
        played = soak_up(&rig, a.reuse || (opts.resume && rig_exists(&rig)))?;
        let t = wait_ready(&cfg)?;
        if cfg.topology == Topology::Nat {
            wait_until(cfg.ready_timeout, cfg.ready_poll, || soak::nat_ready(&rig)).map_err(
                |e| RigError::NotReady {
                    what: "nat router".into(),
                    last: e.last,
                },
            )?;
        }
        eprintln!("nodebench soak: rig ready after {t:.1?}");
        Ok(())
    })();
    let result = match setup {
        Ok(()) => {
            let sha = astar_nodebench::report::git_sha(&cfg.repo_root);
            soak::soak(&cfg, &opts, stop, sha).map_err(|e| e.to_string())
        }
        Err(e) => Err(format!("rig: {e}")),
    };

    let (_files, notes) = rig.collect(&opts.dir);
    for n in notes {
        eprintln!("nodebench soak: {n}");
    }
    if cfg.topology == Topology::Nat {
        save_router_state(&rig, &opts.dir.join("logs").join("router.txt"));
    }
    if played && !a.keep {
        if let Err(e) = soak_down(&rig) {
            eprintln!("nodebench soak: teardown: {e}");
        }
    } else {
        eprintln!("nodebench soak: leaving the rig up");
    }

    match result {
        Ok(s) => {
            print!("{}", soak::summary_text(&s));
            println!("soak log: {}", opts.dir.join("soak.jsonl").display());
            println!("summary:  {}", opts.dir.join("summary.json").display());
            if s.drops == 0 && s.setup_failures == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("nodebench soak: {e}");
            let _ = soak::write_summary(&opts.dir);
            ExitCode::from(2)
        }
    }
}

fn rig_exists(rig: &Rig) -> bool {
    if rig.cfg.topology == Topology::Nat {
        soak::nat_exists(rig).unwrap_or(false)
    } else {
        rig.is_up().unwrap_or(false)
    }
}

/// The nat router's timeouts, flows and rules, for the soak's logs.
fn save_router_state(rig: &Rig, path: &std::path::Path) {
    use std::fmt::Write as _;
    let mut text = String::new();
    for (what, r) in [
        ("sysctl", soak::nat_timeouts(rig)),
        ("conntrack -L -p udp", soak::nat_conntrack(rig)),
        (
            "iptables -S",
            rig.podman()
                .exec(soak::NAT_ROUTER_CONTAINER, &["iptables", "-S"]),
        ),
        (
            "iptables -t nat -S",
            rig.podman()
                .exec(soak::NAT_ROUTER_CONTAINER, &["iptables", "-t", "nat", "-S"]),
        ),
    ] {
        let _ = writeln!(text, "$ {what}\n{}", r.unwrap_or_else(|e| e.to_string()));
    }
    let _ = std::fs::write(path, text);
}
