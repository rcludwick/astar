// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//! Pure HTTP router for the operator console (iax-dd42 Phase 2). `handle_request`
//! maps method+path+body to an [`HttpResponse`] value over the shared
//! [`ServerState`], with no socket dependency so it is unit-testable.

use serde::Deserialize;

use astar_console::list_devices;
use astar_console::{DtmfTester, LocalParrot, OperatingMode, calibrate_mic};
use astar_iax::{
    CallMode, IncomingAuthPolicy, IncomingCallPolicy, ParrotConfig, dial_raw, run_parrot,
};
use astar_station::{AnswerPolicy, NodeConfig, RegisterConfig, StationError};
use serde_json::json;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::server::ServerState;

const INDEX_HTML: &str = include_str!("assets/index.html");
const APP_JS: &str = include_str!("assets/app.js");
const STYLE_CSS: &str = include_str!("assets/style.css");

fn asset(content_type: &'static str, body: &str) -> HttpResponse {
    HttpResponse {
        status: 200,
        content_type,
        body: body.as_bytes().to_vec(),
    }
}

/// A fully-formed HTTP response: status, content type, and body bytes.
pub struct HttpResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl HttpResponse {
    fn json(status: u16, value: &serde_json::Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }
    fn ok_json() -> Self {
        Self::json(200, &serde_json::json!({ "ok": true }))
    }
    fn error_json(status: u16, msg: &str) -> Self {
        Self::json(status, &serde_json::json!({ "error": msg }))
    }
}

#[derive(Deserialize)]
struct ConnectRequest {
    /// Destination node to dial (resolved to an address).
    node: String,
    /// Source node (`CALLING_NUMBER`) for non-WT calls; blank → reuse
    /// `node` (the parrot case). IGNORED in WT mode, where the protocol
    /// requires `CALLING_NUMBER` = destination (it's the node selector).
    #[serde(default)]
    calling: Option<String>,
    /// Secret. Blank → fall back to the server-side `HARNESS_SECRET` default.
    #[serde(default)]
    secret: String,
    /// `CALLING_NAME`. Omitted entirely by the WT-mode browser body (the server
    /// mints the token instead), so it must default rather than 400.
    #[serde(default)]
    name: String,
    /// Capture/playback device picks (blank → system default). Applied to this
    /// dial via `Station::set_devices` before connecting, so an operator who
    /// changes the picker is honored.
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    output: Option<String>,
}

#[derive(Deserialize)]
struct PttRequest {
    on: bool,
}

#[derive(Deserialize)]
struct ParrotStartRequest {
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    output: Option<String>,
}

#[derive(Deserialize)]
struct GainRequest {
    value: f32,
}

#[derive(Deserialize)]
struct DtmfStartRequest {
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    output: Option<String>,
}

#[derive(Deserialize)]
struct MonitorStartRequest {
    /// Capture device substring (blank → system default).
    #[serde(default)]
    input: Option<String>,
}

#[derive(Deserialize)]
struct CharacterizeRequest {
    /// Enable harmonic-comb notch detection (iax-5fb6). Default-off: a flat
    /// per-tone detector when `false`.
    #[serde(default)]
    harmonic_comb: bool,
}

#[derive(Deserialize)]
struct NodeStartRequest {
    /// Listener bind port (blank → `HARNESS_NODE_PORT` default → 4569).
    #[serde(default)]
    port: Option<u16>,
    /// Answer policy: `"auto"` or `"manual"` (blank → `HARNESS_NODE_ANSWER`
    /// default → `"auto"`).
    #[serde(default)]
    answer_policy: Option<String>,
}

#[derive(Deserialize)]
struct NodeRegisterRequest {
    /// Registrar address `host:port` the REGREQ is sent to.
    registrar: String,
    /// Node number / username to register AS (e.g. `"77777"`).
    username: String,
    /// Registrar password. Blank → fall back to the server-side
    /// `HARNESS_REGISTER_SECRET`. Moved straight into the resolver closure and
    /// never stored on the server, echoed in a response, or logged.
    #[serde(default)]
    password: String,
    /// Registration refresh interval, seconds (blank → 60).
    #[serde(default)]
    refresh_s: Option<u64>,
    /// Listener bind port (blank → `HARNESS_NODE_PORT` default → 4569).
    #[serde(default)]
    port: Option<u16>,
    /// Answer policy: `"auto"` or `"manual"` (blank → `HARNESS_NODE_ANSWER`
    /// default → `"auto"`).
    #[serde(default)]
    answer_policy: Option<String>,
}

#[derive(Deserialize)]
struct DtmfPlayRequest {
    /// The key to play (first char used): one of `0-9 A-D * #`.
    digit: String,
}

/// Parse a `since=<u64>` query parameter from a URL path (e.g. `/frames?since=5`).
/// Returns `0` if absent or unparseable.
fn query_since(path: &str) -> u64 {
    path.split_once('?')
        .and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix("since=")))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Parse the `source=<mic|rx|tx>` query parameter from a URL path (iax-2b09).
/// Returns `"mic"` if absent or unrecognized so the default stays the no-call
/// mic-monitor view (backward compatible).
fn query_source(path: &str) -> &'static str {
    match path
        .split_once('?')
        .and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix("source=")))
    {
        Some("rx") => "rx",
        Some("tx") => "tx",
        _ => "mic",
    }
}

/// Build a JSON view of a [`astar_console::TracedFrame`] suitable for the API response.
fn frame_view(tf: &astar_console::TracedFrame) -> serde_json::Value {
    use std::fmt::Write as _;
    let mut raw_hex = String::with_capacity(tf.raw.len() * 2);
    for b in &tf.raw {
        let _ = write!(raw_hex, "{b:02x}");
    }
    serde_json::json!({
        "seq": tf.seq,
        "dir": match tf.dir {
            astar_console::Direction::In => "in",
            astar_console::Direction::Out => "out",
        },
        "at_ms": tf.at_ms,
        "summary": tf.summary,
        "raw_hex": raw_hex,
    })
}

/// Route a request to a response. SSE (`GET /events`) and static assets are
/// handled elsewhere; this covers the JSON control API and a 404 fallback.
#[must_use]
#[allow(clippy::too_many_lines)] // flat route match — splitting hurts readability
pub fn handle_request(state: &ServerState, method: &str, path: &str, body: &[u8]) -> HttpResponse {
    // Strip query string for route matching; `path` (with query) is still
    // available for handlers that need to parse parameters like `since=`.
    let route = path.split('?').next().unwrap_or(path);
    match (method, route) {
        // Env-sourced form defaults. The secret is NOT included — only whether
        // one is preset — so HARNESS_SECRET never reaches the browser.
        // callsign and wt flag are reported for WT mode; no token or password.
        ("GET", "/config") => HttpResponse::json(
            200,
            &serde_json::json!({
                "node": state.defaults.node,
                "calling": state.defaults.calling_node,
                "name": state.defaults.name,
                "input": state.defaults.input,
                "output": state.defaults.output,
                "secret_preset": state.defaults.secret.is_some(),
                "callsign": state.defaults.callsign,
                "wt": state.defaults.wt,
                "node_port": state.defaults.node_port,
                "node_answer": state.defaults.node_answer,
                "node_registrar": state.defaults.node_registrar,
                "node_username": state.defaults.node_username,
                "register_secret_preset": state.defaults.register_secret.is_some(),
            }),
        ),
        ("GET", "/devices") => {
            let backend = (state.make_backend)();
            match list_devices(&*backend) {
                Ok((inputs, outputs)) => HttpResponse::json(
                    200,
                    &serde_json::json!({ "inputs": inputs, "outputs": outputs }),
                ),
                Err(e) => HttpResponse::error_json(500, &e.to_string()),
            }
        }
        ("POST", "/connect") => {
            // Serialize against disconnect teardown so audio open never races a
            // still-running cpal teardown (CoreAudio device contention).
            let _life = state.lifecycle.lock().unwrap();
            if state.parrot.lock().unwrap().is_some() {
                return HttpResponse::error_json(409, "stop the local parrot before connecting");
            }
            let req: ConnectRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            // Dogfood the Station: the WT mint→resolve→dial recipe and the
            // generic resolve→dial recipe both now live in `Station`, over the
            // SAME shared session the diagnostic handlers use. The harness keeps
            // only its policy glue (the parrot-running guard above, the WT
            // no-portal precheck and the note_error side-effect below).
            //
            // WT mode mints a fresh token → CALLING_NAME=token,
            // CALLING_NUMBER=DESTINATION node. In the web-transceiver dialect
            // CALLING_NUMBER is the NODE SELECTOR — "connect me to this node" —
            // not the caller's identity (verified live 2026-06-12). Non-WT mode
            // takes the source node from the form, defaulting to the
            // destination when blank (the parrot case).
            // Apply the per-request device picks (blank = system default) so the
            // operator's selection in the UI is honored on this dial.
            state.station.set_devices(
                req.input
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                req.output
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            );
            let result = if state.defaults.wt {
                // Preserve the exact 500 message/code the harness returned: the
                // Station would surface a generic Portal error here instead.
                if state.defaults.portal.is_none() {
                    return HttpResponse::error_json(500, "WT mode without portal credentials");
                }
                state.station.connect_wt(&req.node)
            } else {
                // Secret falls back to the server-side HARNESS_SECRET default so
                // the real password never has to be typed into / sent from the UI.
                let secret = if req.secret.is_empty() {
                    state.defaults.secret.clone().unwrap_or_default()
                } else {
                    req.secret.clone()
                };
                let calling = req
                    .calling
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&req.node);
                state
                    .station
                    .connect(&req.node, calling, &secret, &req.name)
            };
            match result {
                Ok(()) => HttpResponse::ok_json(),
                // Token mint failure: note it on the shared session (as today)
                // and report 502 with the same message shape.
                Err(StationError::Portal(e)) => {
                    state
                        .session
                        .lock()
                        .unwrap()
                        .note_error(format!("WT token mint failed: {e}"));
                    HttpResponse::error_json(502, &format!("WT token mint failed: {e}"))
                }
                // Node resolution failure: 502, same as the inline recipe.
                Err(StationError::Resolve(e)) => HttpResponse::error_json(502, &e.to_string()),
                // Any other connect failure (already-connected, audio, iax): 409.
                Err(e) => HttpResponse::error_json(409, &e.to_string()),
            }
        }
        ("POST", "/ptt") => {
            let req: PttRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            // Single key signal: drives the network call (set_ptt) AND the local
            // parrot (parrot_shared.key). NotConnected just means "no call" —
            // still 200, because the parrot may be the consumer.
            state.parrot_shared.key.store(req.on, Ordering::Relaxed);
            match state.station.set_ptt(req.on) {
                Ok(()) | Err(StationError::NotConnected) => HttpResponse::ok_json(),
                Err(e) => HttpResponse::error_json(409, &e.to_string()),
            }
        }
        ("POST", "/parrot/compress") => {
            // Voice-compression toggle for the parrot capture path (iax-32cf).
            let req: PttRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            // One toggle drives both the local parrot and the network call
            // (iax-d50d): the parrot reads parrot_shared, the call reads the
            // session's flag in the metering decorator.
            state
                .parrot_shared
                .compress
                .store(req.on, Ordering::Relaxed);
            state.session.lock().unwrap().set_compress(req.on);
            HttpResponse::ok_json()
        }
        ("POST", "/parrot/denoise") => {
            // Noise-reduction toggle (hum filter + gate) for both the parrot
            // and the network call (iax-a9d7 / iax-d50d).
            let req: PttRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            state.parrot_shared.denoise.store(req.on, Ordering::Relaxed);
            state.session.lock().unwrap().set_denoise(req.on);
            HttpResponse::ok_json()
        }
        ("POST", "/parrot/start") => {
            // Serialize with connect/disconnect (shared audio devices).
            let _life = state.lifecycle.lock().unwrap();
            let req: ParrotStartRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            if state.session.lock().unwrap().is_active() {
                return HttpResponse::error_json(
                    409,
                    "disconnect the call before starting the parrot",
                );
            }
            if state.parrot.lock().unwrap().is_some() {
                return HttpResponse::error_json(409, "the local parrot is already running");
            }
            let (tx_gain, rx_gain) = {
                let s = state.session.lock().unwrap();
                (s.input_gain_cell(), s.output_gain_cell())
            };
            let backend = (state.make_backend)();
            match LocalParrot::start(
                backend,
                req.input.as_deref().filter(|s| !s.is_empty()),
                req.output.as_deref().filter(|s| !s.is_empty()),
                state.parrot_shared.clone(),
                tx_gain,
                rx_gain,
            ) {
                Ok(p) => {
                    *state.parrot.lock().unwrap() = Some(p);
                    HttpResponse::ok_json()
                }
                Err(e) => HttpResponse::error_json(500, &e.to_string()),
            }
        }
        ("POST", "/parrot/stop") => {
            let _life = state.lifecycle.lock().unwrap();
            // Drop stops the streams + marks the phase Stopped.
            state.parrot.lock().unwrap().take();
            HttpResponse::ok_json()
        }
        ("POST", "/parrot/calibrate") => {
            // Records ~2.5 s of mic silence and characterizes its noise into a
            // per-mic profile (iax-fb8d). Needs the device, so guard like start.
            let _life = state.lifecycle.lock().unwrap();
            let req: ParrotStartRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            if state.session.lock().unwrap().is_active() {
                return HttpResponse::error_json(409, "disconnect the call before calibrating");
            }
            if state.parrot.lock().unwrap().is_some() {
                return HttpResponse::error_json(409, "stop the local parrot before calibrating");
            }
            let backend = (state.make_backend)();
            match calibrate_mic(
                &*backend,
                req.input.as_deref().filter(|s| !s.is_empty()),
                10.0,
            ) {
                Ok(profile) => {
                    let notches: Vec<serde_json::Value> = profile
                        .notches
                        .iter()
                        .map(|n| serde_json::json!({ "freq_hz": n.freq_hz, "q": n.q }))
                        .collect();
                    *state.parrot_shared.calibrated.lock().unwrap() = Some(profile.clone());
                    // Share the profile with the network-call DSP too, so a real
                    // call gets the same per-mic whine removal as the parrot.
                    state
                        .session
                        .lock()
                        .unwrap()
                        .set_calibrated(Some(profile.clone()));
                    HttpResponse::json(
                        200,
                        &serde_json::json!({
                            "noise_floor_dbfs": profile.noise_floor_dbfs,
                            "gate_threshold_db": profile.gate_threshold_db,
                            "notches": notches,
                        }),
                    )
                }
                Err(e) => HttpResponse::error_json(500, &e.to_string()),
            }
        }
        // ---- Monitor mode + live mic spectrum (iax-2377 / iax-e73e) --------
        // Open the mic WITHOUT a call so the operator can preview / characterize
        // it. Dogfoods Station::monitor_start, which uses the SAME shared backend
        // factory the harness's calibrate/parrot paths use (no divergent device
        // open). Serialize on the lifecycle lock and refuse while the parrot/DTMF
        // hold the mic — those own the capture device.
        ("POST", "/monitor/start") => {
            let _life = state.lifecycle.lock().unwrap();
            let req: MonitorStartRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            if state.parrot.lock().unwrap().is_some() {
                return HttpResponse::error_json(409, "stop the local parrot before monitoring");
            }
            if state.dtmf.lock().unwrap().is_some() {
                return HttpResponse::error_json(409, "stop DTMF mic decode before monitoring");
            }
            // monitor_start is a no-op while a call is live (the call's mic lane
            // already feeds the spectrum), so this is safe during a call too.
            match state
                .station
                .monitor_start(req.input.as_deref().filter(|s| !s.is_empty()))
            {
                Ok(()) => HttpResponse::ok_json(),
                Err(e) => HttpResponse::error_json(500, &e.to_string()),
            }
        }
        ("POST", "/monitor/stop") => {
            let _life = state.lifecycle.lock().unwrap();
            // Drop releases the device; off the session lock (Station does it).
            state.station.monitor_stop();
            HttpResponse::ok_json()
        }
        // Poll ~20 Hz to draw the live spectrum. Returns the peak-held log-binned
        // dBFS values (0 bins when not monitoring — the UI then shows the floor).
        ("GET", "/spectrum") => {
            // iax-2b09: a `?source=` selector picks which stream's spectrum to
            // return. Default `mic` keeps the original no-call mic-monitor view
            // (backward compatible). `rx` taps the active call's decoded received
            // audio; `tx` taps the active call's pre-encode outgoing audio.
            let source = query_source(path);
            let mut bins = [0.0_f32; astar_audio::SPECTRUM_BINS];
            let count = match source {
                "rx" => state.station.rx_spectrum(&mut bins),
                "tx" => state.station.tx_spectrum(&mut bins),
                _ => state.station.mic_spectrum(&mut bins),
            };
            HttpResponse::json(
                200,
                &serde_json::json!({
                    "bins": &bins[..count],
                    "count": count,
                    "source": source,
                    "monitoring": state.station.is_monitoring(),
                }),
            )
        }
        // Characterize the monitored mic (iax-5fb6). Needs an active monitor; the
        // harmonic_comb flag toggles harmonic-aware notch detection (default off).
        // Shares the resulting profile with the parrot + network-call DSP, same
        // as /parrot/calibrate, so the per-mic whine removal carries over.
        ("POST", "/characterize") => {
            let req: CharacterizeRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            match state.station.characterize(req.harmonic_comb) {
                Some(profile) => {
                    let notches: Vec<serde_json::Value> = profile
                        .notches
                        .iter()
                        .map(|n| serde_json::json!({ "freq_hz": n.freq_hz, "q": n.q }))
                        .collect();
                    *state.parrot_shared.calibrated.lock().unwrap() = Some(profile.clone());
                    state
                        .session
                        .lock()
                        .unwrap()
                        .set_calibrated(Some(profile.clone()));
                    HttpResponse::json(
                        200,
                        &serde_json::json!({
                            "noise_floor_dbfs": profile.noise_floor_dbfs,
                            "gate_threshold_db": profile.gate_threshold_db,
                            "harmonic_comb": req.harmonic_comb,
                            "notches": notches,
                        }),
                    )
                }
                None => HttpResponse::error_json(
                    409,
                    "start monitor mode (and let it run a few seconds) before characterizing",
                ),
            }
        }
        ("POST", "/dtmf/start") => {
            // Shares the audio devices with the call + parrot, so serialize.
            let _life = state.lifecycle.lock().unwrap();
            let req: DtmfStartRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            if state.session.lock().unwrap().is_active() {
                return HttpResponse::error_json(409, "disconnect the call before DTMF mic decode");
            }
            if state.parrot.lock().unwrap().is_some() {
                return HttpResponse::error_json(
                    409,
                    "stop the local parrot before DTMF mic decode",
                );
            }
            if state.dtmf.lock().unwrap().is_some() {
                return HttpResponse::error_json(409, "DTMF mic decode is already running");
            }
            let backend = (state.make_backend)();
            match DtmfTester::start(
                backend,
                req.input.as_deref().filter(|s| !s.is_empty()),
                req.output.as_deref().filter(|s| !s.is_empty()),
                state.dtmf_shared.clone(),
            ) {
                Ok(t) => {
                    *state.dtmf.lock().unwrap() = Some(t);
                    HttpResponse::ok_json()
                }
                Err(e) => HttpResponse::error_json(500, &e.to_string()),
            }
        }
        ("POST", "/dtmf/stop") => {
            let _life = state.lifecycle.lock().unwrap();
            state.dtmf.lock().unwrap().take(); // drop stops the streams
            HttpResponse::ok_json()
        }
        ("POST", "/dtmf/play") => {
            let req: DtmfPlayRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            let Some(digit) = req.digit.chars().next() else {
                return HttpResponse::error_json(400, "empty digit");
            };
            // Always loopback-decode (proves the round-trip); play the sidetone
            // too when the tester's output stream is running.
            match state.dtmf_shared.record_loopback(digit) {
                Some(tone) => {
                    if let Some(t) = state.dtmf.lock().unwrap().as_ref() {
                        t.play_tone(&tone);
                    }
                    HttpResponse::ok_json()
                }
                None => HttpResponse::error_json(400, "not a DTMF digit"),
            }
        }
        ("GET", "/dtmf/detected") => {
            let since = query_since(path);
            let digits = serde_json::to_value(state.dtmf_shared.since(since)).unwrap_or_default();
            HttpResponse::json(200, &digits)
        }
        ("POST", "/disconnect") => {
            // Serialize against connect (lifecycle lock). Detach under the session
            // lock (instant: state → idle so the SSE reports idle immediately),
            // then hang up + tear down audio OFF the session lock. The blocking
            // cpal teardown must not run under the session lock (the SSE snapshot
            // loop would freeze and the UI would stick on "answered"), and it runs
            // synchronously here so a subsequent connect can't race it.
            let _life = state.lifecycle.lock().unwrap();
            // Station::disconnect does the same detach-under-lock then
            // hangup-off-lock dance over the shared session.
            state.station.disconnect();
            HttpResponse::ok_json()
        }
        ("POST", "/input-gain") => {
            let req: GainRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            state.session.lock().unwrap().set_input_gain(req.value);
            HttpResponse::ok_json()
        }
        ("POST", "/output-gain") => {
            let req: GainRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            state.session.lock().unwrap().set_output_gain(req.value);
            HttpResponse::ok_json()
        }
        ("GET", "/gains") => {
            let session = state.session.lock().unwrap();
            HttpResponse::json(
                200,
                &serde_json::json!({
                    "input": session.input_gain(),
                    "output": session.output_gain(),
                }),
            )
        }
        ("POST", "/shutdown") => {
            // au-ef39: gracefully hang up any active call, then signal the
            // accept loop to stop so the process exits. Disconnect errors are
            // ignored — we're tearing down regardless.
            let _ = state.session.lock().unwrap().disconnect();
            state.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            state
                .ptt_stop
                .store(true, std::sync::atomic::Ordering::Relaxed);
            // Stop the serial monitor reader thread too (iax-b38e).
            state
                .serial_monitor
                .stop
                .store(true, std::sync::atomic::Ordering::Relaxed);
            HttpResponse::ok_json()
        }
        ("GET", "/timeline") => {
            let since = query_since(path);
            let events = state.session.lock().unwrap().timeline_since(since);
            HttpResponse::json(
                200,
                &serde_json::to_value(events).unwrap_or_else(|_| serde_json::json!([])),
            )
        }
        ("GET", "/frames") => {
            let since = query_since(path);
            let frames = state.session.lock().unwrap().frames_since(since);
            let view: Vec<_> = frames.iter().map(frame_view).collect();
            HttpResponse::json(200, &serde_json::json!(view))
        }
        // Read-only serial RX monitor (iax-b38e). Reports the open port + baud
        // plus new records since the cursor, mirroring `/frames?since=N`.
        ("GET", "/serial/monitor") => {
            let since = query_since(path);
            let mon = &state.serial_monitor;
            let records: Vec<_> = mon
                .buffer
                .since(since)
                .into_iter()
                .map(|r| {
                    serde_json::json!({
                        "seq": r.seq,
                        "at_ms": r.at_ms,
                        "hex": r.hex,
                        "ascii": r.ascii,
                    })
                })
                .collect();
            HttpResponse::json(
                200,
                &serde_json::json!({
                    "port": mon.port_label(),
                    "baud": mon.baud(),
                    "records": records,
                }),
            )
        }
        // ---- Node mode (node-as-handset, iax-64b6) ------------------------
        // v1: direct inbound dial-in only. Bind a listener, accept calls, bridge
        // to the local mic/speaker. NO registration (deferred).
        ("POST", "/node/start") => {
            // Opening the listener + audio shares the same slow CoreAudio path
            // as connect/parrot, so serialize on the lifecycle lock.
            let _life = state.lifecycle.lock().unwrap();
            if state.parrot.lock().unwrap().is_some() {
                return HttpResponse::error_json(
                    409,
                    "stop the local parrot before starting a node",
                );
            }
            let req: NodeStartRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            let port = req.port.or(state.defaults.node_port).unwrap_or(4569);
            let answer = match req
                .answer_policy
                .as_deref()
                .or(state.defaults.node_answer.as_deref())
            {
                Some("manual") => AnswerPolicy::Manual,
                _ => AnswerPolicy::Auto,
            };
            let Ok(bind) = format!("0.0.0.0:{port}").parse::<SocketAddr>() else {
                return HttpResponse::error_json(400, "invalid bind port");
            };
            // auth: Off + 0.0.0.0 bind is PERMISSIVE — fine for a dev dial-in
            // test (the user's "direct inbound first" choice). A real deployment
            // would require auth and/or a narrower bind.
            let policy = IncomingCallPolicy {
                auth: IncomingAuthPolicy::Off,
                ..IncomingCallPolicy::default()
            };
            state.station.set_node_config(NodeConfig {
                bind,
                policy,
                answer,
                ..NodeConfig::default()
            });
            match state.station.set_mode(OperatingMode::Node) {
                Ok(()) => HttpResponse::json(
                    200,
                    &json!({
                        "ok": true,
                        "listening": state.station.node_bind_addr().map(|a| a.to_string()),
                    }),
                ),
                Err(e) => HttpResponse::error_json(409, &e.to_string()),
            }
        }
        ("POST", "/node/stop") => {
            let _life = state.lifecycle.lock().unwrap();
            // Tear down any lingering in-process parrot with the node — it dialed
            // the node on loopback and is meaningless once the listener is gone.
            state.node_parrot_stop.store(true, Ordering::Relaxed);
            match state.station.set_mode(OperatingMode::Wt) {
                Ok(()) => {
                    // The pump's ModeChanged handler also clears these, but clear
                    // here too so the next SSE frame reflects "stopped" instantly.
                    *state.node_status.listening.lock().unwrap() = None;
                    *state.node_status.incoming_from.lock().unwrap() = None;
                    // Dropping the engine deregisters (Registration's Drop); clear
                    // the cached register status so the UI reflects it instantly.
                    *state.node_status.register.lock().unwrap() = None;
                    HttpResponse::ok_json()
                }
                Err(e) => HttpResponse::error_json(409, &e.to_string()),
            }
        }
        ("POST", "/node/register") => {
            let _life = state.lifecycle.lock().unwrap();
            if state.parrot.lock().unwrap().is_some() {
                return HttpResponse::error_json(
                    409,
                    "stop the local parrot before registering a node",
                );
            }
            let req: NodeRegisterRequest = match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(e) => return HttpResponse::error_json(400, &format!("bad request: {e}")),
            };
            // Resolve the registrar address (host:port). Numeric or DNS.
            let Some(peer) = req
                .registrar
                .to_socket_addrs()
                .ok()
                .and_then(|mut it| it.next())
            else {
                return HttpResponse::error_json(
                    400,
                    "registrar must be host:port (e.g. register.example.org:4569)",
                );
            };
            if req.username.trim().is_empty() {
                return HttpResponse::error_json(400, "node number / username is required");
            }
            // Password: form value, else the server-side HARNESS_REGISTER_SECRET.
            // Empty → refuse rather than send a blank credential.
            let password = if req.password.is_empty() {
                state.defaults.register_secret.clone().unwrap_or_default()
            } else {
                req.password.clone()
            };
            if password.is_empty() {
                return HttpResponse::error_json(
                    400,
                    "no registrar password (enter one or set HARNESS_REGISTER_SECRET)",
                );
            }
            // The secret lives ONLY inside this resolver closure — never in
            // config, ServerState, a snapshot/event, a response, or a log.
            state
                .station
                .set_secret_resolver(Box::new(move |_user| password.clone()));

            let port = req.port.or(state.defaults.node_port).unwrap_or(4569);
            let answer = match req
                .answer_policy
                .as_deref()
                .or(state.defaults.node_answer.as_deref())
            {
                Some("manual") => AnswerPolicy::Manual,
                _ => AnswerPolicy::Auto,
            };
            let Ok(bind) = format!("0.0.0.0:{port}").parse::<SocketAddr>() else {
                return HttpResponse::error_json(400, "invalid bind port");
            };
            let refresh = Duration::from_secs(req.refresh_s.unwrap_or(60).clamp(10, 3600));
            let policy = IncomingCallPolicy {
                auth: IncomingAuthPolicy::Off,
                ..IncomingCallPolicy::default()
            };
            state.station.set_node_config(NodeConfig {
                bind,
                policy,
                answer,
                register: Some(RegisterConfig {
                    peer,
                    username: req.username.clone(),
                    refresh,
                }),
                max_calls: 20,
                ..NodeConfig::default()
            });
            // Seed a pending status; the event pump folds Registered/Failed.
            *state.node_status.register.lock().unwrap() = Some("registering…".into());
            match state.station.set_mode(OperatingMode::Node) {
                Ok(()) => HttpResponse::json(
                    200,
                    &json!({
                        "ok": true,
                        "listening": state.station.node_bind_addr().map(|a| a.to_string()),
                        "registrar": peer.to_string(),
                        "username": req.username,
                    }),
                ),
                Err(e) => {
                    *state.node_status.register.lock().unwrap() = None;
                    HttpResponse::error_json(409, &e.to_string())
                }
            }
        }
        ("POST", "/node/deregister") => {
            // Deregister = stop the node: dropping the engine fires Registration's
            // Drop (REGREL). Same teardown as /node/stop.
            let _life = state.lifecycle.lock().unwrap();
            state.node_parrot_stop.store(true, Ordering::Relaxed);
            match state.station.set_mode(OperatingMode::Wt) {
                Ok(()) => {
                    *state.node_status.listening.lock().unwrap() = None;
                    *state.node_status.incoming_from.lock().unwrap() = None;
                    *state.node_status.register.lock().unwrap() = None;
                    HttpResponse::ok_json()
                }
                Err(e) => HttpResponse::error_json(409, &e.to_string()),
            }
        }
        ("POST", "/node/parrot/start") => {
            if state.station.mode() != OperatingMode::Node {
                return HttpResponse::error_json(409, "start the node first");
            }
            if state.node_parrot_running.load(Ordering::Relaxed) {
                return HttpResponse::error_json(409, "parrot already running");
            }
            let Some(addr) = state.station.node_bind_addr() else {
                return HttpResponse::error_json(409, "node not listening");
            };
            let port = addr.port();
            state.node_parrot_stop.store(false, Ordering::Relaxed);
            state.node_parrot_running.store(true, Ordering::Relaxed);
            // The parrot dials the node on loopback — the node binds 0.0.0.0:port
            // so 127.0.0.1:port reaches it. Device-free: the node owns the real
            // mic/speaker, so there is no audio-device contention.
            let stop = std::sync::Arc::clone(&state.node_parrot_stop);
            let running = std::sync::Arc::clone(&state.node_parrot_running);
            std::thread::spawn(move || {
                match dial_raw(
                    ([127, 0, 0, 1], port).into(),
                    "harness-parrot",
                    "s",
                    "",
                    CallMode::Standard,
                ) {
                    Ok(raw) => run_parrot(raw, &ParrotConfig::default(), &stop, |_l| {}),
                    Err(_e) => {}
                }
                running.store(false, Ordering::Relaxed);
            });
            HttpResponse::ok_json()
        }
        ("POST", "/node/parrot/stop") => {
            state.node_parrot_stop.store(true, Ordering::Relaxed);
            HttpResponse::ok_json()
        }
        ("POST", "/node/answer") => match state.station.answer() {
            Ok(()) => HttpResponse::ok_json(),
            Err(e) => HttpResponse::error_json(409, &e.to_string()),
        },
        ("POST", "/node/reject") => match state.station.reject() {
            Ok(()) => HttpResponse::ok_json(),
            Err(e) => HttpResponse::error_json(409, &e.to_string()),
        },
        ("GET", "/") => asset("text/html; charset=utf-8", INDEX_HTML),
        ("GET", "/app.js") => asset("text/javascript; charset=utf-8", APP_JS),
        ("GET", "/style.css") => asset("text/css; charset=utf-8", STYLE_CSS),
        _ => HttpResponse::error_json(404, "not found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astar_audio::{
        AudioBackend, AudioError, DeviceId, DeviceInfo, Direction, InputSink, OutputSource,
        StreamConfig, StreamHandle,
    };

    fn dev(name: &str, dir: Direction) -> DeviceInfo {
        DeviceInfo {
            id: DeviceId::new(name.to_string()),
            name: name.to_string(),
            direction: dir,
            channels: 1,
            native_sample_rates: vec![8_000],
        }
    }

    struct NullHandle;
    impl StreamHandle for NullHandle {
        fn stop(self: Box<Self>) {}
        fn pause(&self) -> Result<(), AudioError> {
            Ok(())
        }
        fn resume(&self) -> Result<(), AudioError> {
            Ok(())
        }
    }

    struct StubBackend;
    impl AudioBackend for StubBackend {
        fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
            Ok(vec![
                dev("Mic A", Direction::Input),
                dev("Speaker B", Direction::Output),
            ])
        }
        fn default_input(&self) -> Option<DeviceInfo> {
            Some(dev("Mic A", Direction::Input))
        }
        fn default_output(&self) -> Option<DeviceInfo> {
            Some(dev("Speaker B", Direction::Output))
        }
        fn open_input(
            &self,
            _d: &DeviceInfo,
            _c: StreamConfig,
            _s: Box<dyn InputSink>,
            _overruns: std::sync::Arc<std::sync::atomic::AtomicU64>,
        ) -> Result<Box<dyn StreamHandle>, AudioError> {
            Ok(Box::new(NullHandle))
        }
        fn open_output(
            &self,
            _d: &DeviceInfo,
            _c: StreamConfig,
            _s: Box<dyn OutputSource>,
        ) -> Result<Box<dyn StreamHandle>, AudioError> {
            Ok(Box::new(NullHandle))
        }
    }

    fn stub_state() -> std::sync::Arc<ServerState> {
        ServerState::new(Box::new(|| Box::new(StubBackend) as Box<dyn AudioBackend>))
    }

    #[test]
    fn devices_returns_inputs_and_outputs() {
        let st = stub_state();
        let resp = handle_request(&st, "GET", "/devices", b"");
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["inputs"], serde_json::json!(["Mic A"]));
        assert_eq!(v["outputs"], serde_json::json!(["Speaker B"]));
    }

    #[test]
    fn connect_with_malformed_body_is_400() {
        let st = stub_state();
        let resp = handle_request(&st, "POST", "/connect", b"not json");
        assert_eq!(resp.status, 400);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert!(v["error"].as_str().unwrap().contains("bad request"));
    }

    #[test]
    fn connect_request_parses_wt_body_without_name() {
        // WT-mode browser sends only {node, input, output}; name/secret/calling
        // are omitted (the server mints the token). This must parse, not 400.
        let body = br#"{"node":"55553","input":null,"output":null}"#;
        let req: ConnectRequest = serde_json::from_slice(body).expect("WT body parses");
        assert_eq!(req.node, "55553");
        assert_eq!(req.name, "", "name defaults when omitted");
    }

    #[test]
    fn ptt_without_active_call_sets_key_and_is_ok() {
        use std::sync::atomic::Ordering;
        let st = stub_state();
        let resp = handle_request(&st, "POST", "/ptt", br#"{"on":true}"#);
        assert_eq!(
            resp.status, 200,
            "no call → key still accepted for the parrot"
        );
        assert!(st.parrot_shared.key.load(Ordering::Relaxed), "key flag set");
    }

    #[test]
    fn parrot_compress_toggle_sets_the_shared_flag() {
        use std::sync::atomic::Ordering;
        let st = stub_state();
        assert!(!st.parrot_shared.compress.load(Ordering::Relaxed));
        let on = handle_request(&st, "POST", "/parrot/compress", br#"{"on":true}"#);
        assert_eq!(on.status, 200);
        assert!(
            st.parrot_shared.compress.load(Ordering::Relaxed),
            "flag set"
        );
        let _ = handle_request(&st, "POST", "/parrot/compress", br#"{"on":false}"#);
        assert!(
            !st.parrot_shared.compress.load(Ordering::Relaxed),
            "flag cleared"
        );
    }

    // ---- monitor mode + live mic spectrum (iax-2377 / iax-e73e) ------------

    #[test]
    fn spectrum_is_floor_shaped_when_not_monitoring() {
        // With no monitor (and the stub backend has no real audio), /spectrum
        // returns 0 bins and monitoring=false — assert the SHAPE, not live tone.
        let st = stub_state();
        let resp = handle_request(&st, "GET", "/spectrum", b"");
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert!(v["bins"].is_array(), "bins is a JSON array");
        assert_eq!(v["count"], 0, "no bins until monitoring");
        assert_eq!(v["monitoring"], false);
    }

    #[test]
    fn spectrum_defaults_to_mic_source() {
        // iax-2b09: with no `?source=` the endpoint must behave exactly as before
        // (mic-monitor view) and echo source="mic" for the UI.
        let st = stub_state();
        let resp = handle_request(&st, "GET", "/spectrum", b"");
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["source"], "mic");
        assert_eq!(v["count"], 0, "no monitor → 0 mic bins");
    }

    #[test]
    fn spectrum_source_rx_routes_to_rx_tap() {
        // iax-2b09: `?source=rx` selects the RX tap. With no live call the stub
        // returns 0 bins, but the source must echo "rx" (the UI keys off it).
        let st = stub_state();
        let resp = handle_request(&st, "GET", "/spectrum?source=rx", b"");
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["source"], "rx");
        assert_eq!(v["count"], 0, "no active call → 0 rx bins");
        assert!(v["bins"].is_array());
    }

    #[test]
    fn spectrum_source_tx_routes_to_tx_tap() {
        // iax-2b09: `?source=tx` selects the TX tap.
        let st = stub_state();
        let resp = handle_request(&st, "GET", "/spectrum?source=tx", b"");
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["source"], "tx");
        assert_eq!(v["count"], 0, "no active call → 0 tx bins");
    }

    #[test]
    fn spectrum_unknown_source_falls_back_to_mic() {
        // iax-2b09: an unrecognized source is treated as the default mic view.
        let st = stub_state();
        let resp = handle_request(&st, "GET", "/spectrum?source=bogus", b"");
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["source"], "mic");
    }

    #[test]
    fn monitor_start_is_refused_while_parrot_runs() {
        let st = stub_state();
        assert_eq!(
            handle_request(&st, "POST", "/parrot/start", b"{}").status,
            200
        );
        let resp = handle_request(&st, "POST", "/monitor/start", b"{}");
        assert_eq!(
            resp.status, 409,
            "can't grab the mic while the parrot holds it"
        );
    }

    #[test]
    fn monitor_start_malformed_body_is_400() {
        let st = stub_state();
        let resp = handle_request(&st, "POST", "/monitor/start", b"not json");
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn monitor_stop_when_idle_is_ok() {
        let st = stub_state();
        let resp = handle_request(&st, "POST", "/monitor/stop", b"");
        assert_eq!(resp.status, 200);
    }

    #[test]
    fn characterize_without_monitor_is_409() {
        // No monitor running → nothing to characterize → 409 (not a panic).
        let st = stub_state();
        let resp = handle_request(&st, "POST", "/characterize", br#"{"harmonic_comb":true}"#);
        assert_eq!(resp.status, 409);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert!(v["error"].as_str().unwrap().contains("monitor"));
    }

    #[test]
    fn characterize_malformed_body_is_400() {
        let st = stub_state();
        let resp = handle_request(&st, "POST", "/characterize", b"not json");
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn parrot_calibrate_is_refused_while_running() {
        let st = stub_state();
        assert_eq!(
            handle_request(&st, "POST", "/parrot/start", b"{}").status,
            200
        );
        let resp = handle_request(&st, "POST", "/parrot/calibrate", b"{}");
        assert_eq!(
            resp.status, 409,
            "can't grab the mic while the parrot holds it"
        );
    }

    #[test]
    fn parrot_denoise_toggle_sets_the_shared_flag() {
        use std::sync::atomic::Ordering;
        let st = stub_state();
        assert!(!st.parrot_shared.denoise.load(Ordering::Relaxed));
        let on = handle_request(&st, "POST", "/parrot/denoise", br#"{"on":true}"#);
        assert_eq!(on.status, 200);
        assert!(st.parrot_shared.denoise.load(Ordering::Relaxed), "flag set");
        let _ = handle_request(&st, "POST", "/parrot/denoise", br#"{"on":false}"#);
        assert!(
            !st.parrot_shared.denoise.load(Ordering::Relaxed),
            "flag cleared"
        );
    }

    #[test]
    fn parrot_start_then_connect_is_409() {
        let st = stub_state();
        let start = handle_request(&st, "POST", "/parrot/start", b"{}");
        assert_eq!(start.status, 200, "parrot starts on default devices");
        let conn = handle_request(&st, "POST", "/connect", br#"{"node":"55553"}"#);
        assert_eq!(conn.status, 409);
        let v: serde_json::Value = serde_json::from_slice(&conn.body).unwrap();
        assert!(v["error"].as_str().unwrap().contains("local parrot"));
    }

    #[test]
    fn parrot_double_start_is_409_and_stop_clears() {
        let st = stub_state();
        assert_eq!(
            handle_request(&st, "POST", "/parrot/start", b"{}").status,
            200
        );
        assert_eq!(
            handle_request(&st, "POST", "/parrot/start", b"{}").status,
            409,
            "second start while running is rejected"
        );
        assert_eq!(handle_request(&st, "POST", "/parrot/stop", b"").status, 200);
        assert_eq!(
            handle_request(&st, "POST", "/parrot/start", b"{}").status,
            200,
            "start succeeds again after stop"
        );
    }

    #[test]
    fn dtmf_play_loopback_decodes_and_is_pollable() {
        let st = stub_state();
        let resp = handle_request(&st, "POST", "/dtmf/play", br#"{"digit":"5"}"#);
        assert_eq!(resp.status, 200, "loopback play works without starting mic");
        let polled = handle_request(&st, "GET", "/dtmf/detected?since=0", b"");
        let v: serde_json::Value = serde_json::from_slice(&polled.body).unwrap();
        let digits = v.as_array().unwrap();
        assert_eq!(digits.len(), 1, "the loopback digit is logged");
        assert_eq!(digits[0]["digit"], "5");
        assert_eq!(digits[0]["source"], "loopback");
    }

    #[test]
    fn dtmf_play_rejects_non_dtmf_digit() {
        let st = stub_state();
        let resp = handle_request(&st, "POST", "/dtmf/play", br#"{"digit":"E"}"#);
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn dtmf_double_start_is_409_and_stop_clears() {
        let st = stub_state();
        assert_eq!(
            handle_request(&st, "POST", "/dtmf/start", b"{}").status,
            200
        );
        assert_eq!(
            handle_request(&st, "POST", "/dtmf/start", b"{}").status,
            409,
            "second start while running is rejected"
        );
        assert_eq!(handle_request(&st, "POST", "/dtmf/stop", b"").status, 200);
        assert_eq!(
            handle_request(&st, "POST", "/dtmf/start", b"{}").status,
            200,
            "start succeeds again after stop"
        );
    }

    #[test]
    fn dtmf_start_conflicts_with_a_running_parrot() {
        let st = stub_state();
        assert_eq!(
            handle_request(&st, "POST", "/parrot/start", b"{}").status,
            200
        );
        let resp = handle_request(&st, "POST", "/dtmf/start", b"{}");
        assert_eq!(resp.status, 409);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert!(v["error"].as_str().unwrap().contains("parrot"));
    }

    #[test]
    fn disconnect_when_idle_is_ok() {
        let st = stub_state();
        let resp = handle_request(&st, "POST", "/disconnect", b"");
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["ok"], serde_json::json!(true));
    }

    #[test]
    fn unknown_route_is_404() {
        let st = stub_state();
        let resp = handle_request(&st, "GET", "/nope", b"");
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn shutdown_sets_stop_flag_and_is_ok() {
        use std::sync::atomic::Ordering;
        let st = stub_state();
        assert!(!st.stop.load(Ordering::Relaxed), "stop starts clear");
        let resp = handle_request(&st, "POST", "/shutdown", b"");
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["ok"], serde_json::json!(true));
        assert!(
            st.stop.load(Ordering::Relaxed),
            "shutdown must set the stop flag"
        );
        assert!(
            st.ptt_stop.load(Ordering::Relaxed),
            "shutdown must also stop the PTT runner"
        );
    }

    #[test]
    fn config_reports_defaults_without_exposing_secret() {
        let st = crate::server::ServerState::with_defaults(
            Box::new(|| Box::new(StubBackend) as Box<dyn AudioBackend>),
            crate::server::HarnessDefaults {
                node: Some("77777".into()),
                calling_node: Some("1234".into()),
                name: None,
                input: Some("USB Audio Device".into()),
                output: Some("Mac mini Speakers".into()),
                secret: Some("test-secret-xyz".into()),
                portal: None,
                callsign: None,
                wt: false,
                node_port: None,
                node_answer: None,
                node_registrar: None,
                node_username: None,
                register_secret: None,
            },
        );
        let resp = handle_request(&st, "GET", "/config", b"");
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["node"], "77777");
        assert_eq!(v["calling"], "1234");
        assert_eq!(v["name"], serde_json::Value::Null);
        assert_eq!(v["input"], "USB Audio Device");
        assert_eq!(v["output"], "Mac mini Speakers");
        assert_eq!(v["secret_preset"], true);
        // The secret VALUE must never appear in the /config response.
        let body = String::from_utf8(resp.body).unwrap();
        assert!(
            !body.contains("test-secret-xyz"),
            "secret must not leak to the browser: {body}"
        );
    }

    #[test]
    fn config_reports_wt_identity_without_token_or_password() {
        let st = crate::server::ServerState::with_defaults(
            Box::new(|| Box::new(StubBackend) as Box<dyn astar_audio::AudioBackend>),
            crate::server::HarnessDefaults {
                node: Some("55553".into()),
                calling_node: None,
                name: None,
                input: None,
                output: None,
                secret: Some("allstar".into()),
                // wt is derived from portal presence in main.rs — keep the
                // test data consistent with that invariant.
                portal: Some(astar_asl3::PortalCredentials {
                    user: "AJ7HR".into(),
                    password: "portal-password-x".into(),
                    node: "55553".into(),
                }),
                callsign: Some("AJ7HR".into()),
                wt: true,
                node_port: None,
                node_answer: None,
                node_registrar: None,
                node_username: None,
                register_secret: None,
            },
        );
        let resp = handle_request(&st, "GET", "/config", b"");
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["callsign"], "AJ7HR");
        assert_eq!(v["wt"], true);
        let body = String::from_utf8(resp.body).unwrap();
        assert!(
            !body.contains("allstar"),
            "secret/token must not leak to the browser"
        );
        assert!(
            !body.contains("portal-password-x"),
            "portal password must not leak to the browser"
        );
    }

    #[test]
    fn node_register_starts_listening_and_never_echoes_password() {
        let st = stub_state();
        // port 0 → ephemeral bind; numeric registrar → no DNS, REGREQ goes
        // nowhere but the node still starts listening.
        let body =
            br#"{"registrar":"127.0.0.1:14599","username":"77777","password":"topsecret-pw","port":0}"#;
        let resp = handle_request(&st, "POST", "/node/register", body);
        assert_eq!(resp.status, 200, "register should start the node");
        let raw = String::from_utf8(resp.body.clone()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(v["ok"], serde_json::json!(true));
        assert_eq!(v["username"], "77777");
        assert!(v["listening"].as_str().is_some(), "listening addr present");
        // Secret-free: the registrar password must never appear in the response.
        assert!(
            !raw.contains("topsecret-pw"),
            "password leaked into response: {raw}"
        );
        assert_eq!(st.station.mode(), OperatingMode::Node);
        // Pending status seeded; secret-free.
        let reg = st.node_status.register.lock().unwrap().clone();
        assert_eq!(reg.as_deref(), Some("registering…"));
        // Deregister tears the node down and clears the status.
        let off = handle_request(&st, "POST", "/node/deregister", b"");
        assert_eq!(off.status, 200);
        assert_eq!(st.station.mode(), OperatingMode::Wt);
        assert!(st.node_status.register.lock().unwrap().is_none());
    }

    #[test]
    fn node_register_requires_a_password() {
        let st = stub_state();
        // No form password and no HARNESS_REGISTER_SECRET default → 400.
        let body = br#"{"registrar":"127.0.0.1:14599","username":"77777","port":0}"#;
        let resp = handle_request(&st, "POST", "/node/register", body);
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn node_register_rejects_a_bad_registrar() {
        let st = stub_state();
        let body = br#"{"registrar":"not-a-host-port","username":"77777","password":"x","port":0}"#;
        let resp = handle_request(&st, "POST", "/node/register", body);
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn index_is_served_as_html() {
        let st = stub_state();
        let resp = handle_request(&st, "GET", "/", b"");
        assert_eq!(resp.status, 200);
        assert!(resp.content_type.starts_with("text/html"));
        let body = String::from_utf8(resp.body).unwrap();
        assert!(body.contains("astar-inspect"), "index names the app");
        assert!(body.contains("id=\"ptt-btn\""), "index has the PTT button");
    }

    #[test]
    fn app_js_and_css_are_served() {
        let st = stub_state();
        let js = handle_request(&st, "GET", "/app.js", b"");
        assert_eq!(js.status, 200);
        assert!(js.content_type.contains("javascript"));
        assert!(String::from_utf8(js.body).unwrap().contains("EventSource"));
        let css = handle_request(&st, "GET", "/style.css", b"");
        assert_eq!(css.status, 200);
        assert!(css.content_type.contains("css"));
    }

    // ---- gain API (au-3365) ------------------------------------------------

    #[test]
    fn gains_defaults_to_unity() {
        let st = stub_state();
        let resp = handle_request(&st, "GET", "/gains", b"");
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        let input = v["input"].as_f64().unwrap();
        let output = v["output"].as_f64().unwrap();
        assert!((input - 1.0).abs() < 1e-6, "input gain defaults to 1.0");
        assert!((output - 1.0).abs() < 1e-6, "output gain defaults to 1.0");
    }

    #[test]
    fn post_input_gain_changes_reported_gain() {
        let st = stub_state();
        let resp = handle_request(&st, "POST", "/input-gain", br#"{"value":0.5}"#);
        assert_eq!(resp.status, 200);
        let ok: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(ok["ok"], serde_json::json!(true));
        // GET /gains must now reflect the new value.
        let g = handle_request(&st, "GET", "/gains", b"");
        let v: serde_json::Value = serde_json::from_slice(&g.body).unwrap();
        let input = v["input"].as_f64().unwrap();
        assert!((input - 0.5).abs() < 1e-6, "input gain updated to 0.5");
        // output should remain at unity.
        let output = v["output"].as_f64().unwrap();
        assert!((output - 1.0).abs() < 1e-6, "output gain still 1.0");
    }

    #[test]
    fn post_output_gain_changes_reported_gain() {
        let st = stub_state();
        let resp = handle_request(&st, "POST", "/output-gain", br#"{"value":1.5}"#);
        assert_eq!(resp.status, 200);
        let g = handle_request(&st, "GET", "/gains", b"");
        let v: serde_json::Value = serde_json::from_slice(&g.body).unwrap();
        let output = v["output"].as_f64().unwrap();
        assert!((output - 1.5).abs() < 1e-6, "output gain updated to 1.5");
        let input = v["input"].as_f64().unwrap();
        assert!((input - 1.0).abs() < 1e-6, "input gain still 1.0");
    }

    #[test]
    fn post_input_gain_malformed_is_400() {
        let st = stub_state();
        let resp = handle_request(&st, "POST", "/input-gain", b"not json");
        assert_eq!(resp.status, 400);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert!(v["error"].as_str().unwrap().contains("bad request"));
    }

    #[test]
    fn post_output_gain_malformed_is_400() {
        let st = stub_state();
        let resp = handle_request(&st, "POST", "/output-gain", b"not json");
        assert_eq!(resp.status, 400);
        let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert!(v["error"].as_str().unwrap().contains("bad request"));
    }

    #[test]
    fn timeline_and_frames_endpoints_return_arrays() {
        let st = stub_state();
        let t = handle_request(&st, "GET", "/timeline", b"");
        assert_eq!(t.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&t.body).unwrap();
        assert!(v.is_array(), "timeline is a JSON array");

        let f = handle_request(&st, "GET", "/frames?since=0", b"");
        assert_eq!(f.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&f.body).unwrap();
        assert!(v.is_array(), "frames is a JSON array");
    }

    // ---- serial monitor (iax-b38e) -----------------------------------------

    #[test]
    fn serial_monitor_reports_port_baud_and_records_since() {
        let st = stub_state();
        // Empty to start: no reader, port unset, no records.
        let empty = handle_request(&st, "GET", "/serial/monitor?since=0", b"");
        assert_eq!(empty.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&empty.body).unwrap();
        assert_eq!(v["port"], serde_json::Value::Null, "no port until opened");
        assert_eq!(v["baud"], 9600, "default display baud");
        assert_eq!(v["records"].as_array().unwrap().len(), 0);

        // Push two chunks directly into the shared buffer (no hardware needed).
        st.serial_monitor.buffer.push(b"Hi", 10);
        st.serial_monitor.buffer.push(b"\n", 20);

        let all = handle_request(&st, "GET", "/serial/monitor?since=0", b"");
        let v: serde_json::Value = serde_json::from_slice(&all.body).unwrap();
        let recs = v["records"].as_array().unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0]["seq"], 1);
        assert_eq!(recs[0]["at_ms"], 10);
        assert_eq!(recs[0]["hex"], "48 69");
        assert_eq!(recs[0]["ascii"], "Hi");
        assert_eq!(recs[1]["hex"], "0a");
        assert_eq!(recs[1]["ascii"], ".", "newline is non-printable");

        // since cursor returns only newer records.
        let tail = handle_request(&st, "GET", "/serial/monitor?since=1", b"");
        let v: serde_json::Value = serde_json::from_slice(&tail.body).unwrap();
        let recs = v["records"].as_array().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0]["seq"], 2);
    }
}
