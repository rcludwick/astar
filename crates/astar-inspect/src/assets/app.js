"use strict";
const $ = (id) => document.getElementById(id);

// ---- transport-agnostic render + command layer (reused by Tauri later) ----
function dbToPct(db) {
  return Math.max(0, Math.min(100, ((db + 60) / 60) * 100));
}
function render(state) {
  const p = state.parrot || { running: false, phase: "stopped", tx_db: -60 };
  // TX meter shows the parrot record level while it runs, else the call's TX.
  const txDb = p.running ? p.tx_db : state.tx_level_db;
  $("tx-bar").style.width = dbToPct(txDb) + "%";
  $("rx-bar").style.width = dbToPct(state.rx_level_db) + "%";
  $("tx-db").textContent = Math.round(txDb) + " dB";
  $("rx-db").textContent = Math.round(state.rx_level_db) + " dB";
  const lit = state.ptt || p.phase === "recording";
  const light = $("ptt-light");
  if (light) light.className = "light " + (lit ? "tx" : "idle");
  let s = state.status.kind;
  if (state.status.reason) s += ": " + state.status.reason;
  if (state.rtt_ms != null) s += "  (rtt " + state.rtt_ms + " ms)";
  $("status").textContent = s;
  const ps = $("parrot-status");
  if (ps) ps.textContent = p.running ? p.phase : "Stopped";
  // Keep the two modes mutually exclusive in the UI. parrotRunning tracks the
  // SSE truth (not an optimistic local toggle) so the Start/Stop button can't
  // drift if the parrot stops server-side (restart, internal failure).
  parrotRunning = p.running;
  const inCall = state.status.kind !== "idle" && state.status.kind !== "hangup";
  $("connect").disabled = p.running;
  $("parrot-toggle").textContent = p.running ? "Stop Parrot" : "Start Parrot";
  $("parrot-toggle").disabled = inCall;
  renderNode(state);
}

// Reflect node-mode status (iax-64b6) from the SSE `node` sub-object: bound
// address, ringing caller (manual answer), and the bridged-call state.
function renderNode(state) {
  const n = state.node;
  if (!n) return;
  const inNode = n.mode === "node";
  const incoming = $("node-incoming");
  if (inNode && n.incoming_from) {
    incoming.hidden = false;
    $("node-incoming-from").textContent = n.incoming_from;
  } else {
    incoming.hidden = true;
  }
  const st = $("node-status");
  if (!inNode) {
    st.textContent = "Stopped.";
  } else if (state.status.kind === "answered") {
    st.textContent = "In call" + (n.listening ? " (listening on " + n.listening + ")" : "");
  } else if (n.incoming_from) {
    st.textContent = "Ringing — call from " + n.incoming_from;
  } else {
    st.textContent = "Listening on " + (n.listening || "?");
  }
  // Register tab mirrors the same node state, plus the registration status
  // (n.register: "registering…" / "registered" / "failed: …"). Same inbound
  // call + PTT surface so the operator can answer/talk from this tab too.
  const regIncoming = $("reg-incoming");
  if (regIncoming) {
    if (inNode && n.incoming_from) {
      regIncoming.hidden = false;
      $("reg-incoming-from").textContent = n.incoming_from;
    } else {
      regIncoming.hidden = true;
    }
  }
  // reg-status is PURELY SSE-driven (single source of truth) so it never races
  // the click handlers. Click-handler feedback (errors, e.g. a blank-password
  // 400 that never starts the node) goes to the separate #reg-msg line, which
  // SSE never touches.
  const rs = $("reg-status");
  if (rs) {
    if (!inNode) {
      rs.textContent = "Not registered.";
    } else {
      let base = n.register || "listening (not registered)";
      if (state.status.kind === "answered") base = "in call — " + base;
      else if (n.incoming_from) base = "ringing from " + n.incoming_from + " — " + base;
      rs.textContent = base + (n.listening ? "  [" + n.listening + "]" : "");
    }
  }
}

// ---- transport adapter (swap for Tauri invoke/listen) ---------------------
async function sendCommand(path, body) {
  const r = await fetch(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return r.json();
}
let eventSource = null;
function startEvents() {
  eventSource = new EventSource("/events");
  eventSource.onmessage = (e) => render(JSON.parse(e.data));
}
// au-ef39: stop the harness. The server hangs up any call, then exits; close
// the live stream so the browser stops trying to reconnect to a dead server.
function shutdown() {
  if (eventSource) eventSource.close();
  sendCommand("/shutdown", {}).catch(() => {});
  $("status").textContent = "Server stopped.";
  for (const id of ["connect", "disconnect", "ptt-btn", "shutdown"]) {
    $(id).disabled = true;
  }
}
// Preset device names from .env (HARNESS_INPUT / HARNESS_OUTPUT), applied once
// the device list is loaded. Case-insensitive: exact match first, else substring.
let defaultInput = null,
  defaultOutput = null;
function pickDefault(sel, want) {
  if (!want) return;
  const lc = want.toLowerCase();
  const opts = [...sel.options].filter((o) => o.value);
  const hit =
    opts.find((o) => o.value.toLowerCase() === lc) ||
    opts.find((o) => o.value.toLowerCase().includes(lc));
  if (hit) sel.value = hit.value;
}
function selectDefaultDevices() {
  pickDefault($("input"), defaultInput);
  pickDefault($("output"), defaultOutput);
}
async function loadDevices() {
  const d = await (await fetch("/devices")).json();
  fillSelect($("input"), d.inputs || []);
  fillSelect($("output"), d.outputs || []);
  selectDefaultDevices(); // re-apply once options exist (order-independent)
}
// Prefill the form from env-sourced defaults (.env via run-harness.sh). The
// secret VALUE is never sent here — only whether one is preset server-side.
async function loadConfig() {
  const c = await (await fetch("/config")).json();
  if (c.node) $("node").value = c.node;
  if (c.calling) $("calling").value = c.calling;
  if (c.name) $("name").value = c.name;
  defaultInput = c.input || null;
  defaultOutput = c.output || null;
  selectDefaultDevices(); // re-apply in case devices already loaded
  if (c.secret_preset) {
    $("secret").value = "";
    $("secret").placeholder = "(using HARNESS_SECRET from .env)";
  }
  // Node-mode prefills (iax-64b6): bind port + answer policy.
  if (c.node_port != null) $("node-port").value = c.node_port;
  if (c.node_answer) $("node-answer").value = c.node_answer;
  // Register-tab prefills (iax-64b6 P7): registrar + node number from .env. The
  // password VALUE is never sent — only whether one is preset server-side.
  if (c.node_registrar) $("reg-registrar").value = c.node_registrar;
  if (c.node_username) $("reg-username").value = c.node_username;
  if (c.node_port != null) $("reg-port").value = c.node_port;
  if (c.node_answer) $("reg-answer").value = c.node_answer;
  if (c.register_secret_preset) {
    $("reg-password").value = "";
    $("reg-password").placeholder = "(using HARNESS_REGISTER_SECRET from .env)";
  }
  if (c.wt) {
    wtMode = true;
    $("wt-id").textContent = "Web Transceiver as " + (c.callsign || "(unknown callsign)");
    $("wt-id").hidden = false;
    // Source node is hidden in WT mode: a web-transceiver session is a
    // GUEST identified by the token→callsign, and on the wire
    // CALLING_NUMBER must be the destination (it's the node selector —
    // putting anything else there makes the server dial that node).
    // Connecting AS your own node is node-to-node linking (node mode).
    for (const id of ["row-calling", "row-secret", "row-name"]) {
      const el = $(id); if (el) el.hidden = true;
    }
  }
}

// Fetch current gain values and sync sliders + readouts on page load (au-3365).
async function loadGains() {
  const g = await (await fetch("/gains")).json();
  const inSlider = $("input-gain");
  const outSlider = $("output-gain");
  inSlider.value = Math.round((g.input || 1.0) * 100);
  outSlider.value = Math.round((g.output || 1.0) * 100);
  $("input-gain-val").textContent = inSlider.value + "%";
  $("output-gain-val").textContent = outSlider.value + "%";
}

let wtMode = false;
let timelineSeq = 0, frameSeq = 0;
async function pollTimeline() {
  try {
    const evs = await (await fetch("/timeline?since=" + timelineSeq)).json();
    for (const e of evs) {
      timelineSeq = e.seq + 1;
      const li = document.createElement("li");
      li.textContent = e.kind + (e.detail ? " — " + e.detail : "");
      $("timeline-list").appendChild(li);
    }
  } catch {}
}
async function pollWire() {
  const det = document.querySelector("details.wire");
  if (!det || !det.open) return; // only when expanded — never fetch frames otherwise
  const showVoice = $("wire-voice").checked;
  try {
    const frames = await (await fetch("/frames?since=" + frameSeq)).json();
    for (const f of frames) {
      frameSeq = f.seq + 1;
      if (f.summary.is_voice && !showVoice) continue;
      const li = document.createElement("li");
      li.textContent = `${f.seq} ${f.dir} ${f.summary.kind} oseq=${f.summary.oseqno}` +
        (f.summary.highlights.length ? " [" + f.summary.highlights.join(",") + "]" : "");
      $("wire-list").appendChild(li);
    }
  } catch {}
}

// ---- wiring ---------------------------------------------------------------
function fillSelect(sel, names) {
  sel.innerHTML = "";
  const def = document.createElement("option");
  def.value = "";
  def.textContent = "(default)";
  sel.appendChild(def);
  for (const n of names) {
    const o = document.createElement("option");
    o.value = n;
    o.textContent = n;
    sel.appendChild(o);
  }
}
function showResult(r) {
  if (r && r.error) $("status").textContent = "Error: " + r.error;
}
function connect() {
  const body = wtMode
    ? {
        node: $("node").value,
        input: $("input").value || null,
        output: $("output").value || null,
      }
    : {
        node: $("node").value,
        calling: $("calling").value || null,
        secret: $("secret").value,
        name: $("name").value,
        input: $("input").value || null,
        output: $("output").value || null,
      };
  sendCommand("/connect", body).then(showResult);
}
function ptt(on) {
  sendCommand("/ptt", { on });
}
const TABS = ["network", "parrot", "dtmf", "spectrum", "serial", "node", "register"];
function showTab(which) {
  // Leaving the Spectrum tab releases the mic: monitor mode holds the input
  // device open, and the parrot/DTMF/connect paths refuse while it's running.
  if (which !== "spectrum" && monitorOn) {
    sendCommand("/monitor/stop", {}).catch(() => {});
    monitorOn = false;
  }
  for (const t of TABS) {
    $("panel-" + t).hidden = t !== which;
    $("tab-" + t).classList.toggle("active", t === which);
  }
}

// --- DTMF tab ---
let dtmfMicOn = false;
let dtmfSeq = 0;
function toggleDtmfMic() {
  const path = dtmfMicOn ? "/dtmf/stop" : "/dtmf/start";
  const body = dtmfMicOn
    ? {}
    : { input: $("input").value || null, output: $("output").value || null };
  sendCommand(path, body).then((r) => {
    if (!r.error) {
      dtmfMicOn = !dtmfMicOn;
      $("dtmf-mic-toggle").textContent = "Decode mic: " + (dtmfMicOn ? "on" : "off");
    }
    showResult(r);
  });
}
async function pollDtmf() {
  if ($("panel-dtmf").hidden) return; // only while the tab is visible
  try {
    const digits = await (await fetch("/dtmf/detected?since=" + dtmfSeq)).json();
    for (const d of digits) {
      dtmfSeq = d.seq;
      const li = document.createElement("li");
      li.textContent = `${d.digit}  (${d.source})`;
      $("dtmf-list").prepend(li);
    }
  } catch {}
}
// --- Serial monitor tab (iax-b38e) ---
let serialSeq = 0;
function fmtMs(ms) {
  const s = (ms / 1000).toFixed(3);
  return s.padStart(9, " ");
}
async function pollSerial() {
  if ($("panel-serial").hidden) return; // only while the tab is visible
  try {
    const r = await (await fetch("/serial/monitor?since=" + serialSeq)).json();
    $("serial-port").textContent =
      "Port: " + (r.port || "(none — set HARNESS_MONITOR_SERIAL)") +
      "   baud " + (r.baud || "?");
    const list = $("serial-list");
    for (const rec of r.records || []) {
      serialSeq = rec.seq;
      const li = document.createElement("li");
      const t = document.createElement("span");
      t.className = "serial-t";
      t.textContent = fmtMs(rec.at_ms);
      const h = document.createElement("span");
      h.className = "serial-hex";
      h.textContent = rec.hex;
      const a = document.createElement("span");
      a.className = "serial-ascii";
      a.textContent = rec.ascii;
      li.append(t, h, a);
      list.appendChild(li);
    }
    if (r.records && r.records.length) {
      list.scrollTop = list.scrollHeight; // follow the tail
    }
  } catch {}
}

// --- Spectrum / mic characterize tab (iax-2377 / iax-e73e / iax-5fb6) ---
// monitorOn tracks the SSE-confirmed monitoring flag from /spectrum (not an
// optimistic local toggle) so the Start/Stop label can't drift if the server
// drops monitor mode (e.g. a call opening the mic lane instead).
let monitorOn = false;
const SPEC_FLOOR_DB = -120; // bins below this clamp to the canvas baseline
function drawSpectrum(bins) {
  const c = $("spectrum-canvas");
  if (!c) return;
  const ctx = c.getContext("2d");
  const W = c.width, H = c.height;
  ctx.clearRect(0, 0, W, H);
  // Background + dBFS gridlines every 20 dB (0 at top, floor at bottom).
  ctx.fillStyle = "#16171a";
  ctx.fillRect(0, 0, W, H);
  ctx.strokeStyle = "#26282c";
  ctx.fillStyle = "#6b7177";
  ctx.font = "10px ui-monospace, monospace";
  ctx.lineWidth = 1;
  for (let db = 0; db >= SPEC_FLOOR_DB; db -= 20) {
    const y = ((0 - db) / -SPEC_FLOOR_DB) * H;
    ctx.beginPath();
    ctx.moveTo(0, y + 0.5);
    ctx.lineTo(W, y + 0.5);
    ctx.stroke();
    ctx.fillText(db + " dB", 2, Math.min(H - 2, y + 10));
  }
  const n = bins.length;
  if (!n) return;
  // The bins are ALREADY log-spaced over the voice band, so draw them evenly
  // across X; each bin is one bar. Height maps dBFS (0 top → floor bottom).
  const bw = W / n;
  for (let i = 0; i < n; i++) {
    const db = Math.max(SPEC_FLOOR_DB, bins[i]);
    const h = ((db - SPEC_FLOOR_DB) / -SPEC_FLOOR_DB) * H;
    // Green→amber→red as the bar approaches 0 dBFS, mirroring the TX/RX meters.
    const frac = (db - SPEC_FLOOR_DB) / -SPEC_FLOOR_DB;
    ctx.fillStyle = frac > 0.85 ? "#f85149" : frac > 0.6 ? "#d29922" : "#3fb950";
    ctx.fillRect(i * bw + 0.5, H - h, Math.max(1, bw - 1), h);
  }
}
// iax-2b09: which stream's spectrum to poll — "mic" (monitor, default), "rx"
// (active call's received audio), or "tx" (active call's outgoing audio).
function spectrumSource() {
  const sel = document.querySelector('input[name="spectrum-source"]:checked');
  return sel ? sel.value : "mic";
}
async function pollSpectrum() {
  if ($("panel-spectrum").hidden) return; // only while the tab is visible
  const source = spectrumSource();
  try {
    const r = await (await fetch("/spectrum?source=" + source)).json();
    monitorOn = !!r.monitoring;
    $("spectrum-toggle").textContent = monitorOn ? "Stop monitor" : "Start monitor";
    // Mic + characterize controls only apply to the monitor (mic) source.
    const micMode = source === "mic";
    $("spectrum-toggle").disabled = !micMode;
    $("spectrum-characterize").disabled = !micMode;
    if (micMode) {
      $("spectrum-status").textContent = monitorOn
        ? "Monitoring — live mic spectrum (no call)."
        : "Monitor stopped.";
    } else {
      const label = source === "rx" ? "received" : "outgoing";
      $("spectrum-status").textContent = (r.count || 0) > 0
        ? `Live call ${source.toUpperCase()} spectrum (${label} audio).`
        : `No active call — ${source.toUpperCase()} (${label}) spectrum shows the floor.`;
    }
    drawSpectrum(r.bins || []);
  } catch {}
}
function toggleMonitor() {
  const path = monitorOn ? "/monitor/stop" : "/monitor/start";
  const body = monitorOn ? {} : { input: $("input").value || null };
  sendCommand(path, body).then(showResult);
}
function characterizeMic() {
  const out = $("spectrum-result");
  out.hidden = false;
  out.textContent = "Characterizing the live monitor…";
  sendCommand("/characterize", { harmonic_comb: $("spectrum-comb").checked }).then((r) => {
    if (r.error) { out.textContent = "Characterize failed: " + r.error; return; }
    const tones = (r.notches || [])
      .map((n) => Math.round(n.freq_hz) + " Hz (Q " + n.q.toFixed(1) + ")")
      .join(", ") || "none";
    out.textContent =
      `Noise floor ${r.noise_floor_dbfs.toFixed(1)} dBFS; ` +
      (r.harmonic_comb ? "harmonic-comb " : "") +
      `notching: ${tones}. Profile shared with the parrot + next call's noise reduction.`;
  });
}

// Mirrors the SSE parrot.running flag (updated in render); picks which endpoint
// the toggle hits. Not optimistically flipped here — render reconciles it.
let parrotRunning = false;
function toggleParrot() {
  const path = parrotRunning ? "/parrot/stop" : "/parrot/start";
  const body = parrotRunning
    ? {}
    : { input: $("input").value || null, output: $("output").value || null };
  sendCommand(path, body).then(showResult);
}
window.addEventListener("DOMContentLoaded", () => {
  loadConfig();
  loadDevices();
  loadGains();
  startEvents();
  // Volume sliders (au-3365): update readout and POST new gain on every drag.
  const inSlider = $("input-gain");
  inSlider.addEventListener("input", () => {
    $("input-gain-val").textContent = inSlider.value + "%";
    sendCommand("/input-gain", { value: inSlider.value / 100 });
  });
  const outSlider = $("output-gain");
  outSlider.addEventListener("input", () => {
    $("output-gain-val").textContent = outSlider.value + "%";
    sendCommand("/output-gain", { value: outSlider.value / 100 });
  });
  setInterval(pollTimeline, 500);
  setInterval(pollWire, 500);
  setInterval(pollDtmf, 300);
  setInterval(pollSerial, 300);
  setInterval(pollSpectrum, 50); // ~20 Hz live spectrum, mirrors the meter cadence
  $("connect").onclick = connect;
  $("disconnect").onclick = () => sendCommand("/disconnect", {});
  $("shutdown").onclick = shutdown;
  $("tab-network").onclick = () => showTab("network");
  $("tab-parrot").onclick = () => showTab("parrot");
  $("tab-dtmf").onclick = () => showTab("dtmf");
  $("tab-spectrum").onclick = () => showTab("spectrum");
  $("tab-serial").onclick = () => showTab("serial");
  $("spectrum-toggle").onclick = toggleMonitor;
  $("spectrum-characterize").onclick = characterizeMic;
  // iax-2b09: re-poll immediately when the spectrum source changes so the view
  // (and the mic/characterize control enablement) updates without a frame lag.
  document.querySelectorAll('input[name="spectrum-source"]').forEach((el) => {
    el.addEventListener("change", () => { pollSpectrum().catch(() => {}); });
  });
  $("tab-node").onclick = () => showTab("node");
  $("tab-register").onclick = () => showTab("register");
  $("serial-clear").onclick = () => { $("serial-list").innerHTML = ""; };
  // --- Start Node tab (iax-64b6) ---
  $("node-start").onclick = () => {
    const port = parseInt($("node-port").value || "4569", 10);
    sendCommand("/node/start", { port, answer_policy: $("node-answer").value }).then((r) => {
      if (r.error) { showResult(r); return; }
      $("node-status").textContent = "Listening on " + (r.listening || "?");
    });
  };
  $("node-stop").onclick = () => sendCommand("/node/stop", {}).then((r) => {
    if (r.error) { showResult(r); return; }
    $("node-status").textContent = "Stopped.";
    $("node-incoming").hidden = true;
  });
  $("node-answer-btn").onclick = () => sendCommand("/node/answer", {}).then(showResult);
  $("node-reject-btn").onclick = () => sendCommand("/node/reject", {}).then(showResult);
  $("node-parrot-start").onclick = () => sendCommand("/node/parrot/start", {}).then(showResult);
  $("node-parrot-stop").onclick  = () => sendCommand("/node/parrot/stop", {}).then(showResult);
  // Hold-to-talk, same semantics as the Network PTT (mode-aware /ptt).
  const nptt = $("node-ptt");
  const nptt_on  = (e) => { e.preventDefault(); sendCommand("/ptt", { on: true }); nptt.classList.add("keyed"); };
  const nptt_off = (e) => { e.preventDefault(); sendCommand("/ptt", { on: false }); nptt.classList.remove("keyed"); };
  nptt.addEventListener("mousedown", nptt_on);
  nptt.addEventListener("mouseup", nptt_off);
  nptt.addEventListener("mouseleave", nptt_off);
  nptt.addEventListener("touchstart", nptt_on);
  nptt.addEventListener("touchend", nptt_off);
  // --- Register tab (register-as-node, iax-64b6 P7) ---
  $("reg-register").onclick = () => {
    const body = {
      registrar: $("reg-registrar").value.trim(),
      username: $("reg-username").value.trim(),
      password: $("reg-password").value,
      refresh_s: parseInt($("reg-refresh").value || "60", 10),
      port: parseInt($("reg-port").value || "4569", 10),
      answer_policy: $("reg-answer").value,
    };
    $("reg-msg").textContent = "";
    sendCommand("/node/register", body).then((r) => {
      // On success the live state shows up in #reg-status via SSE; only surface
      // errors here so the two lines don't race.
      $("reg-msg").textContent = r.error ? "Error: " + r.error : "";
    });
  };
  $("reg-deregister").onclick = () => sendCommand("/node/deregister", {}).then((r) => {
    $("reg-msg").textContent = r.error ? "Error: " + r.error : "";
    $("reg-incoming").hidden = true;
  });
  $("reg-answer-btn").onclick = () => sendCommand("/node/answer", {}).then(showResult);
  $("reg-reject-btn").onclick = () => sendCommand("/node/reject", {}).then(showResult);
  const rptt = $("reg-ptt");
  const rptt_on  = (e) => { e.preventDefault(); sendCommand("/ptt", { on: true });  rptt.classList.add("keyed"); };
  const rptt_off = (e) => { e.preventDefault(); sendCommand("/ptt", { on: false }); rptt.classList.remove("keyed"); };
  rptt.addEventListener("mousedown", rptt_on);
  rptt.addEventListener("mouseup", rptt_off);
  rptt.addEventListener("mouseleave", rptt_off);
  rptt.addEventListener("touchstart", rptt_on);
  rptt.addEventListener("touchend", rptt_off);
  $("parrot-toggle").onclick = toggleParrot;
  $("parrot-denoise").addEventListener("change", (e) => {
    sendCommand("/parrot/denoise", { on: e.target.checked }).then(showResult);
  });
  $("parrot-compress").addEventListener("change", (e) => {
    sendCommand("/parrot/compress", { on: e.target.checked }).then(showResult);
  });
  // Network-tab DSP toggles drive the same flags (parrot + network call share them).
  $("net-denoise").addEventListener("change", (e) => {
    sendCommand("/parrot/denoise", { on: e.target.checked }).then(showResult);
  });
  $("net-compress").addEventListener("change", (e) => {
    sendCommand("/parrot/compress", { on: e.target.checked }).then(showResult);
  });
  $("parrot-calibrate").onclick = () => {
    const out = $("calib-result");
    out.hidden = false;
    out.textContent = "Calibrating… stay quiet for ~10 seconds.";
    sendCommand("/parrot/calibrate", { input: $("input").value || null }).then((r) => {
      if (r.error) { out.textContent = "Calibrate failed: " + r.error; return; }
      const tones = (r.notches || []).map((n) => Math.round(n.freq_hz) + " Hz").join(", ") || "none";
      out.textContent =
        `Noise floor ${r.noise_floor_dbfs.toFixed(1)} dBFS; notching: ${tones}. ` +
        "Start the parrot and enable Noise reduction to hear the per-mic filter.";
    });
  };
  $("dtmf-mic-toggle").onclick = toggleDtmfMic;
  $("dtmf-pad").addEventListener("click", (e) => {
    const digit = e.target && e.target.dataset ? e.target.dataset.digit : null;
    if (digit) sendCommand("/dtmf/play", { digit });
  });
  const rec = $("parrot-rec");
  rec.addEventListener("mousedown", () => ptt(true));
  rec.addEventListener("mouseup", () => ptt(false));
  rec.addEventListener("mouseleave", () => ptt(false));
  const pb = $("ptt-btn");
  pb.addEventListener("mousedown", () => ptt(true));
  pb.addEventListener("mouseup", () => ptt(false));
  pb.addEventListener("mouseleave", () => ptt(false));
  document.addEventListener("keydown", (e) => {
    if (e.code === "Space" && !e.repeat) { e.preventDefault(); ptt(true); }
  });
  document.addEventListener("keyup", (e) => {
    if (e.code === "Space") { e.preventDefault(); ptt(false); }
  });
});
