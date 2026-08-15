// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Testable wiring for the `serve` and `tui` entry-points.
//!
//! `run_serve` accepts a pre-bound `tiny_http::Server` so integration tests can
//! inspect the ephemeral port before spawning the accept loop.  The real binary
//! calls `server::serve_http` which binds internally; `run_serve` is for tests
//! (and for the binary if it wants to inspect the addr first).

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use crate::{command::NodeCommand, controller::NodeController, server::serve_http_on};

/// Pump-tick interval for the serve loop.
const PUMP_INTERVAL: Duration = Duration::from_millis(50);

/// Run the HTTP accept loop on an already-bound `tiny_http::Server`.
///
/// Spawns a pump-ticker thread that calls `ctrl.pump()` every 50 ms and exits
/// when `stop` is set.  Then drives the `tiny_http` accept loop on the current
/// thread until `stop` is set or the controller signals shutdown.
///
/// On exit: sends `Deregister` + `Hangup` for graceful teardown.
///
/// This function is `pub` so integration tests can call it on a thread with a
/// pre-bound server at a known ephemeral port.
pub fn run_serve(ctrl: &Arc<NodeController>, server: &tiny_http::Server, stop: &Arc<AtomicBool>) {
    // Spawn pump-ticker so SSE subscribers receive events while the accept
    // loop is blocked in recv_timeout.
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

    // Accept loop — blocks until stop is set (recv_timeout polls every 200 ms).
    serve_http_on(server, ctrl, stop).expect("serve_http_on");

    // Graceful teardown.
    let _ = ctrl.execute(NodeCommand::Deregister);
    let _ = ctrl.execute(NodeCommand::Hangup);

    // Signal the pump to stop and wait for it.
    stop.store(true, Ordering::Relaxed);
    let _ = pump_handle.join();
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
