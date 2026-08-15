// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Shared server state and the `tiny_http` accept loop (iax-dd42 Phase 2).

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use astar_audio::AudioBackend;
use astar_console::ConsoleSession;
use astar_console::OperatingMode;
use astar_console::{DtmfShared, DtmfTester, LocalParrot, ParrotShared};
use astar_station::{Station, StationConfig, StationEvent};

use crate::http::handle_request;
use crate::sse::SseReader;

/// Form defaults sourced from the environment (`.env` via `run-harness.sh`).
/// Lets the operator preset the node/calling-node/name and keep the secret
/// server-side. The secret is NEVER sent to the browser; see `GET /config`.
#[derive(Default, Clone)]
pub struct HarnessDefaults {
    /// Destination node (`HARNESS_NODE`).
    pub node: Option<String>,
    /// Calling node — `CALLING_NUMBER` IE (`HARNESS_CALLING_NODE`).
    pub calling_node: Option<String>,
    /// Calling name (`HARNESS_NAME`).
    pub name: Option<String>,
    /// Preset input (capture) device name (`HARNESS_INPUT`), preselected in the
    /// device picker. Case-insensitive substring match against the device list.
    pub input: Option<String>,
    /// Preset output (playback) device name (`HARNESS_OUTPUT`). See `input`.
    pub output: Option<String>,
    /// Preset secret (`HARNESS_SECRET`), injected on `/connect` when the form
    /// secret is blank. Held server-side only.
    pub secret: Option<String>,
    /// Portal credentials for native WT token minting (`ASL_USER`/`ASL_PASS`/
    /// `ASL_NODE`). Held server-side only — never sent to the browser.
    pub portal: Option<astar_asl3::PortalCredentials>,
    /// Operator callsign (`ASL_USER`) for Web Transceiver mode.
    pub callsign: Option<String>,
    /// Web Transceiver mode: when `true`, `/connect` mints a token and uses it
    /// as `CALLING_NAME` with the destination node as `CALLING_NUMBER`.
    pub wt: bool,
    /// Default inbound-listener bind port for Node mode (`HARNESS_NODE_PORT`).
    /// `None` → the `/node/start` handler falls back to 4569.
    pub node_port: Option<u16>,
    /// Default answer policy for Node mode (`HARNESS_NODE_ANSWER`): `"auto"` or
    /// `"manual"`. `None` → the `/node/start` handler falls back to `"auto"`.
    pub node_answer: Option<String>,
    /// Default registrar address `host:port` for node registration
    /// (`HARNESS_REGISTRAR`), prefilled in the Register tab.
    pub node_registrar: Option<String>,
    /// Default node number / username to register AS (`HARNESS_NODE_USER`),
    /// prefilled in the Register tab.
    pub node_username: Option<String>,
    /// Preset registrar password (`HARNESS_REGISTER_SECRET`), used by
    /// `/node/register` when the form password is blank. Held server-side only —
    /// never sent to the browser, same contract as `secret`.
    pub register_secret: Option<String>,
}

/// Live node-mode status, updated by the background event pump (single owner of
/// `Station::next_event`) and read by the SSE stream. Secret-free.
#[derive(Default)]
pub struct NodeStatus {
    /// Bound listener address while in Node mode.
    pub listening: Mutex<Option<String>>,
    /// Manual-answer mode: the ringing caller id, set while a call is offered.
    pub incoming_from: Mutex<Option<String>>,
    /// Last node-registration status, folded from `Registered`/`RegisterFailed`
    /// edges by the event pump (and seeded to `"registering…"` by
    /// `/node/register`). Secret-free: never holds the registrar password.
    pub register: Mutex<Option<String>>,
}

/// Shared, thread-safe server state. `make_backend` builds a fresh audio
/// backend per call (device enumeration and each `connect`). `stop` is the
/// shutdown flag: the accept loop polls it, and the `POST /shutdown` handler
/// (au-ef39) sets it so the browser's Shutdown button can stop the harness.
/// `defaults` holds env-sourced form defaults (au-3365 sibling work).
/// `lifecycle` serializes connect/disconnect so their slow audio work (cpal
/// stream open/teardown, which can take ~1-2 s) never overlaps and never runs
/// under the `session` lock — that keeps the SSE snapshot loop responsive and
/// avoids `CoreAudio` device contention on rapid reconnects.
pub struct ServerState {
    /// The single console session. Shared (same `Arc`) with [`Self::station`]:
    /// the harness's own diagnostic handlers (parrot/DTMF/calibration, gains,
    /// timeline/frames, shutdown) read/write it directly, while the network
    /// call, PTT, disconnect, and SSE snapshot flow through `station` — which
    /// holds a clone of this exact `Arc`. One session, no divergence.
    pub session: Arc<Mutex<ConsoleSession>>,
    /// Embeds the shared session and dogfoods the WT connect/PTT/disconnect
    /// recipe. Built via [`Station::with_shared_session`] over `session`.
    pub station: Station,
    pub make_backend: Box<dyn Fn() -> Box<dyn AudioBackend> + Send + Sync>,
    pub stop: AtomicBool,
    pub defaults: HarnessDefaults,
    pub lifecycle: Mutex<()>,
    /// Lock-free key/phase/tx signals shared with the local parrot, the serial
    /// bridge, and `/ptt`. Present even when no parrot runs.
    pub parrot_shared: ParrotShared,
    /// The running local parrot, if any (holds its audio streams alive).
    /// Mutually exclusive with a live `session` call.
    pub parrot: Mutex<Option<LocalParrot>>,
    /// Log of detected DTMF digits (mic + loopback), polled by the DTMF tab.
    /// Present even when the tester is stopped.
    pub dtmf_shared: DtmfShared,
    /// The running DTMF tester, if any (holds its mic/playback streams alive).
    /// Mutually exclusive with a live `session` call or a running parrot.
    pub dtmf: Mutex<Option<DtmfTester>>,
    /// Stop flag for the PTT runner thread (`astar-ptt::spawn` needs an
    /// Arc<AtomicBool> of its own; `stop` is a plain field).
    pub ptt_stop: Arc<std::sync::atomic::AtomicBool>,
    /// Read-only serial RX monitor (iax-b38e): buffered timestamped data bytes,
    /// polled by the Serial tab. Present even when no reader thread runs.
    pub serial_monitor: Arc<crate::serial_monitor::SerialMonitor>,
    /// Live Node-mode status (iax-64b6), updated solely by the background event
    /// pump (the single consumer of `Station::next_event`) and read by the SSE
    /// stream. Secret-free.
    pub node_status: Arc<NodeStatus>,
    /// Stop flag for the in-process NETWORK parrot that dials the running node
    /// on loopback (iax-64b6). Distinct from the local-audio `parrot` field:
    /// this drives the node-as-handset test path. Set true to tear it down.
    pub node_parrot_stop: Arc<AtomicBool>,
    /// True while the in-process network parrot thread is alive; gates a second
    /// `/node/parrot/start` and is cleared by the parrot thread on exit.
    pub node_parrot_running: Arc<AtomicBool>,
}

impl ServerState {
    #[must_use]
    pub fn new(make_backend: Box<dyn Fn() -> Box<dyn AudioBackend> + Send + Sync>) -> Arc<Self> {
        Self::with_defaults(make_backend, HarnessDefaults::default())
    }

    #[must_use]
    pub fn with_defaults(
        make_backend: Box<dyn Fn() -> Box<dyn AudioBackend> + Send + Sync>,
        defaults: HarnessDefaults,
    ) -> Arc<Self> {
        // One shared session: the harness keeps a clone for its diagnostic
        // handlers; the Station gets the same Arc for the connect/PTT recipe.
        let session = Arc::new(Mutex::new(ConsoleSession::new()));

        // The backend factory is shared too — both the harness (parrot/DTMF/
        // device enumeration) and the Station (connect) build fresh backends
        // from it, so wrap it in an Arc and hand each side a thin clone.
        let factory: Arc<dyn Fn() -> Box<dyn AudioBackend> + Send + Sync> = Arc::from(make_backend);
        let station_factory = Arc::clone(&factory);
        let make_backend: Box<dyn Fn() -> Box<dyn AudioBackend> + Send + Sync> = {
            let f = Arc::clone(&factory);
            Box::new(move || f())
        };

        // StationConfig mirrors the env-sourced defaults: device picks, the WT
        // portal credentials, and the guest secret (HARNESS_SECRET when preset,
        // otherwise the "allstar" default).
        let station = Station::with_shared_session(
            StationConfig {
                input: defaults.input.clone(),
                output: defaults.output.clone(),
                portal: defaults.portal.clone(),
                secret: defaults
                    .secret
                    .clone()
                    .unwrap_or_else(|| "allstar".to_string()),
                ..StationConfig::default()
            },
            Arc::clone(&session),
            Box::new(move || station_factory()),
        );

        Arc::new(Self {
            session,
            station,
            make_backend,
            stop: AtomicBool::new(false),
            defaults,
            lifecycle: Mutex::new(()),
            parrot_shared: ParrotShared::new(),
            parrot: Mutex::new(None),
            dtmf_shared: DtmfShared::new(),
            dtmf: Mutex::new(None),
            ptt_stop: Arc::new(AtomicBool::new(false)),
            serial_monitor: Arc::new(crate::serial_monitor::SerialMonitor::new(2_000, 9600)),
            node_status: Arc::new(NodeStatus::default()),
            node_parrot_stop: Arc::new(AtomicBool::new(false)),
            node_parrot_running: Arc::new(AtomicBool::new(false)),
        })
    }
}

/// Run the blocking `tiny_http` accept loop until `state.stop` is set (by the
/// `POST /shutdown` handler — au-ef39). Spawns a thread per request (SSE
/// responses block their connection). If `bound_tx` is given, the actually-
/// bound address is sent once the server is listening (for tests that bind an
/// ephemeral `:0` port).
///
/// # Errors
/// Returns an error if the address cannot be bound.
pub fn serve(
    addr: SocketAddr,
    state: &Arc<ServerState>,
    bound_tx: Option<Sender<SocketAddr>>,
) -> io::Result<()> {
    let server = tiny_http::Server::http(addr)
        .map_err(|e| io::Error::new(io::ErrorKind::AddrInUse, e.to_string()))?;
    // Start the single consumer of `Station::next_event` (iax-64b6). Draining
    // the edge stream in exactly one place keeps the node answering inbound
    // calls regardless of how many (or zero) browsers are watching `/events`.
    spawn_node_event_pump(Arc::clone(state));
    if let Some(tx) = bound_tx
        && let Some(a) = server.server_addr().to_ip()
    {
        let _ = tx.send(a);
    }
    while !state.stop.load(Ordering::Relaxed) {
        // recv_timeout returns None on a 200 ms tick (no request); re-check stop.
        if let Some(request) = server.recv_timeout(Duration::from_millis(200))? {
            let st = Arc::clone(state);
            std::thread::spawn(move || dispatch(request, &st));
        }
    }
    Ok(())
}

/// Spawn the background event pump: the SINGLE consumer of
/// [`Station::next_event`] (an edge stream — each event returns once). It owns
/// `next_event` and folds the discrete lifecycle edges into the shared
/// [`NodeStatus`]; the SSE stream and any status route only READ that. Draining
/// the edges here (not in `SseReader`) means multiple `/events` connections
/// never split events, and the node keeps answering inbound calls even with no
/// browser attached. The loop exits when `state.stop` is set (au-ef39 shutdown).
fn spawn_node_event_pump(state: Arc<ServerState>) {
    std::thread::spawn(move || {
        loop {
            if state.stop.load(Ordering::Relaxed) {
                break;
            }
            while let Some(ev) = state.station.next_event() {
                match ev {
                    StationEvent::IncomingCall { from } => {
                        *state.node_status.incoming_from.lock().unwrap() = Some(from);
                    }
                    StationEvent::Answered | StationEvent::Hangup { .. } => {
                        *state.node_status.incoming_from.lock().unwrap() = None;
                    }
                    StationEvent::ModeChanged(m) => {
                        if m == OperatingMode::Wt {
                            *state.node_status.listening.lock().unwrap() = None;
                            *state.node_status.incoming_from.lock().unwrap() = None;
                        }
                    }
                    StationEvent::Registered => {
                        *state.node_status.register.lock().unwrap() = Some("registered".into());
                    }
                    StationEvent::RegisterFailed { reason } => {
                        *state.node_status.register.lock().unwrap() =
                            Some(format!("failed: {reason}"));
                    }
                    StationEvent::RemotePtt(_) => {}
                }
            }
            // Refresh the listening addr each tick — covers an Auto-mode start
            // that produced no discrete event before the first poll.
            *state.node_status.listening.lock().unwrap() =
                state.station.node_bind_addr().map(|a| a.to_string());
            std::thread::sleep(Duration::from_millis(50));
        }
    });
}

fn dispatch(mut request: tiny_http::Request, state: &Arc<ServerState>) {
    let method = request.method().as_str().to_string();
    let url = request.url().to_string();
    // Strip query string only for the SSE special-case check; the full URL
    // (with query) is forwarded to handle_request so handlers can parse `since=`.
    let path_no_query = url.split('?').next().unwrap_or(&url).to_string();

    // SSE stream is a long-lived response handled specially. Use the generic
    // `Response::new` so the body is a streaming `Read` (SseReader) with an
    // unknown length (None) — the connection stays open until the client drops.
    if method == "GET" && path_no_query == "/events" {
        let reader = SseReader::new(Arc::clone(state));
        let response = tiny_http::Response::new(
            tiny_http::StatusCode(200),
            vec![
                header("Content-Type", "text/event-stream"),
                header("Cache-Control", "no-cache"),
            ],
            reader,
            None,
            None,
        );
        let _ = request.respond(response);
        return;
    }

    let mut body = Vec::new();
    let _ = request.as_reader().read_to_end(&mut body);
    // Pass the full URL (with query) so handle_request can parse `since=` etc.
    let resp = handle_request(state, &method, &url, &body);
    let response = tiny_http::Response::from_data(resp.body)
        .with_status_code(resp.status)
        .with_header(header("Content-Type", resp.content_type));
    let _ = request.respond(response);
}

fn header(key: &str, value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(key.as_bytes(), value.as_bytes()).expect("static header is valid")
}
