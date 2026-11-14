// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Testable wiring for the `serve` and `tui` entry-points.
//!
//! `run_serve` takes a control port already bound with
//! [`crate::server::bind_control`], so the binary can report the address it
//! got and integration tests can read an ephemeral port first. A restart
//! accepted by the controller (`NodeCommand::Restart`) is a normal teardown
//! followed by a self re-exec, with a watchdog behind it; see `reexec_self`.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use crate::{command::NodeCommand, controller::NodeController, server::ControlServer};

/// The net recordings pages over `[netrec]`'s archive. `sign_in` is whether
/// this node offers sign-in, so a signed-out reader is pointed at it; the
/// public bind withholds the link per request when it serves no sign-in
/// (`admin_on_public` off; see `public::http::nets_route`). The
/// folder may be missing (the list is then empty); nothing here creates it.
/// Deletes post to `/admin/nets/delete`, which the admin router answers
/// behind the gate and the same-origin JSON rule on both binds.
#[must_use]
pub fn nets_pages(cfg: &crate::config::NetrecCfg, sign_in: bool) -> astar_netarchive::Pages {
    astar_netarchive::Pages::new(
        cfg.archive.clone(),
        astar_netarchive::ViewOpts {
            delete_url: Some("/admin/nets/delete".into()),
            sign_in_url: sign_in.then(|| "/admin/signin?next=/nets/".into()),
            fallback_my_calls: vec![],
        },
    )
}

/// Pump-tick interval for the serve loop.
const PUMP_INTERVAL: Duration = Duration::from_millis(50);

/// How often the serve loop checks for a stop.
const SERVE_POLL: Duration = Duration::from_millis(200);

/// How long the teardown after an accepted `Restart` may take before the
/// watchdog execs over it. `Gateway::drop` joins its runtime thread with
/// no timeout, so a wedged Discord runtime would otherwise turn "Restart"
/// into a hung process with the HTTP server already closed.
pub const RESTART_WATCHDOG: Duration = Duration::from_secs(30);

/// Replace this process with a fresh copy of itself: `current_exe()` with
/// the original arguments, the environment inherited. Returns ONLY on
/// failure — on success this function never returns, because there is no
/// longer a caller.
///
/// PID is preserved, so podman's PID 1, systemd and `cargo run` under
/// `just node` all see one continuous process; Rust std sockets are
/// close-on-exec, so 4569 and 8730 rebind cleanly in the new image.
#[cfg(unix)]
#[must_use = "returns only when the exec failed; the caller logs why and falls back to exiting"]
pub fn reexec_self() -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return e,
    };
    std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .exec()
}

/// The non-unix stand-in: there is no `exec`, so a restart falls back to
/// the plain exit the caller already handles.
#[cfg(not(unix))]
#[must_use = "returns only when the exec failed; the caller logs why and falls back to exiting"]
pub fn reexec_self() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "re-exec is not supported on this platform",
    )
}

/// Arm the restart watchdog: a detached thread that sleeps `after`, says
/// so, and execs from wherever the teardown got stuck. `execve` from any
/// thread replaces the whole process, so an unfinished teardown is simply
/// abandoned — the best-effort unmute sweep with it, which is already
/// documented as best-effort. If even the exec fails the process exits
/// non-zero, so a supervisor restarts what this could not.
pub fn spawn_restart_watchdog(after: Duration) {
    std::thread::Builder::new()
        .name("astar-restart-watchdog".into())
        .spawn(move || {
            std::thread::sleep(after);
            eprintln!(
                "astar-server: restart watchdog fired after {}s; teardown did not finish, restarting anyway",
                after.as_secs()
            );
            let e = reexec_self();
            eprintln!("astar-server: restart exec failed, exiting instead: {e}");
            std::process::exit(1);
        })
        .expect("spawn restart watchdog");
}

/// Serve the already-bound control port until shutdown, then tear down.
///
/// Spawns a pump-ticker thread that calls `ctrl.pump()` every 50 ms and exits
/// when `stop` is set. The server answers on its own threads; this thread
/// waits, checking every 200 ms, until `stop` is set or the controller
/// signals shutdown, and then stops the server (no new connections; an open
/// event stream ends at the controller's stop flag or its client leaving).
///
/// On exit: sends `Deregister`, DISCs the M17 monitor (`teardown_m17`), then
/// `Hangup` for graceful teardown, then — once the pump thread has been
/// joined — tears the Discord side down, and last of all hangs up on every
/// `EchoLink` station and logs off the directory (`teardown_echolink`,
/// bounded to 10 s).
///
/// **The Discord teardown is last, and it is on this thread.** Nothing else
/// in `serve` ever drops the controller: `main` wraps it in an `Arc` and the
/// signal handler holds a clone for the life of the process, so the gateway's
/// `Drop` — which is what makes the runtime thread wait for the server
/// unmutes it has been asked for — is never reached. Running it from the
/// signal handler instead would race `main` returning out from under it. See
/// [`NodeController::teardown_discord`].
///
/// This function is `pub` so integration tests can call it on a thread with a
/// pre-bound server at a known ephemeral port.
pub fn run_serve(ctrl: &Arc<NodeController>, server: ControlServer, stop: &Arc<AtomicBool>) {
    // Spawn pump-ticker so SSE subscribers receive events while this thread
    // waits for the stop below.
    let pump_ctrl = Arc::clone(ctrl);
    let pump_stop = Arc::clone(stop);
    let pump_handle = std::thread::Builder::new()
        .name("iax-node-pump".into())
        .spawn(move || {
            while !pump_stop.load(Ordering::Relaxed) && !pump_ctrl.should_stop() {
                pump_ctrl.pump();
                std::thread::sleep(PUMP_INTERVAL);
            }
        })
        .expect("spawn pump-ticker");

    // Serve until stop: the server runs on its own threads.
    while !stop.load(Ordering::Relaxed) && !ctrl.should_stop() {
        std::thread::sleep(SERVE_POLL);
    }
    server.stop();

    // A restart was asked for: from here the teardown below runs on a
    // clock. The watchdog is armed BEFORE Deregister/Hangup/join/
    // teardown_discord, because any of them can be what wedges.
    //
    // The flag was stored on an HTTP connection thread and this is the
    // serve thread. The `Acquire` load of the controller's own stop flag
    // pairs with the `Release` store in `shutdown()`, which the `Restart`
    // arm makes after its `Release` store of the restart flag, so once
    // stop reads true here `restart_requested()` (itself an `Acquire`
    // load) cannot read a stale false.
    if ctrl.stop_flag().load(Ordering::Acquire) && ctrl.restart_requested() {
        spawn_restart_watchdog(RESTART_WATCHDOG);
    }

    // Graceful teardown.
    let _ = ctrl.execute(NodeCommand::Deregister);
    // DISC the M17 reflector explicitly: `Hangup` leaves the monitor alone
    // by design, and this controller is never dropped in `serve`, so nothing
    // else would ever tell the reflector the node has gone.
    ctrl.teardown_m17();
    let _ = ctrl.execute(NodeCommand::Hangup);

    // Signal the pump to stop and wait for it.
    stop.store(true, Ordering::Relaxed);
    let _ = pump_handle.join();

    // AFTER the join, so no tick can hand the rule engine new work while its
    // mutes are being lifted, and on this thread, so the process cannot
    // return out from under a gateway that is still sending them. A no-op in
    // a build without the `discord` feature.
    ctrl.teardown_discord();

    // LAST, on purpose: hang up on every EchoLink station and log off the
    // directory. The logoff can block on a directory exchange or a DNS
    // lookup with no deadline, so it is bounded (10 s, then abandoned with a
    // warning). Run any earlier and a slow directory would hold up the
    // Hangup, the pump join and the Discord unmute sweep — and, on a
    // restart, the watchdog armed above could re-exec before they ran. A
    // `Shutdown` has already hung the stations up (`shutdown()`); this hangs
    // up any still there before the logoff.
    ctrl.teardown_echolink();
}

/// Install a SIGINT/SIGTERM handler that stores `true` into `stop` and
/// executes `Shutdown` on `ctrl`.
///
/// The FIRST signal requests graceful shutdown (set stop + execute `Shutdown`).
/// A SECOND signal escalates to an immediate hard exit (`process::exit(130)`),
/// so an operator who hits Ctrl-C twice never gets stuck waiting on teardown
/// (spec §Error handling).
///
/// Uses the `ctrlc` crate (`termination` feature covers both SIGINT and
/// SIGTERM on Unix).
pub fn install_signal_handler(ctrl: Arc<NodeController>, stop: Arc<AtomicBool>) {
    ctrlc::set_handler(move || {
        // `swap` returns the prior value: if it was already `true`, this is the
        // second signal — escalate to an immediate hard exit.
        if stop.swap(true, Ordering::Relaxed) {
            std::process::exit(130);
        }
        let _ = ctrl.execute(NodeCommand::Shutdown);
    })
    .expect("install ctrlc signal handler");
}
