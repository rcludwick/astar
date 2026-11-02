// The public listener page: status, graphs and the live player.
//
// Everything shown here came off the wire — node numbers, callsigns an M17
// reflector relayed, event text — so values reach the DOM only through
// createElement and textContent. The server's CSP (no inline script or
// style) is the second lock on that door.
//
// Data: /public/events (SSE) sends `status` on change and `levels` at 10 Hz.
// Player: Opus packets over /stream.ws → WebCodecs AudioDecoder → an
// AudioWorklet jitter buffer; without WebCodecs (or off HTTPS) it falls back
// to /stream.mp3 in an <audio> element. Both run through an AnalyserNode so
// the graphs show what is actually being heard.
"use strict";

const $ = (id) => document.getElementById(id);

// ---- constants ----------------------------------------------------------------

const BINS = 32;
const F_LO = 300;
const F_HI = 3400;
const FLOOR = -80;
const PEAK_DECAY_DB_S = 100; // the app's default spectrum decay
const HISTORY_S = 2;
const HISTORY_HZ = 20;
const HISTORY_N = HISTORY_S * HISTORY_HZ;
const VOLUME_KEY = "astar.listener.volume";

/// Centre of spectrum bin i: the same formula as the server's bin_centre_hz.
function binCentreHz(i) {
  return F_LO * Math.pow(F_HI / F_LO, i / (BINS - 1));
}

const reducedMotion = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;

// ---- small helpers --------------------------------------------------------------

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined && text !== null) e.textContent = text;
  return e;
}

function store(key, value) {
  try {
    if (value === undefined) return window.localStorage.getItem(key);
    window.localStorage.setItem(key, value);
  } catch (_) {
    // Private windows and blocked storage: the page works without it.
  }
  return null;
}

/// m:ss under an hour, then "h m", then "d h".
function fmtDuration(secs) {
  secs = Math.max(0, Math.floor(secs));
  if (secs < 3600) return Math.floor(secs / 60) + ":" + String(secs % 60).padStart(2, "0");
  if (secs < 86400) return Math.floor(secs / 3600) + " h " + Math.floor((secs % 3600) / 60) + " m";
  return Math.floor(secs / 86400) + " d " + Math.floor((secs % 86400) / 3600) + " h";
}

/// How long a link has been up: minutes, then hours and minutes, then days.
function fmtUp(secs) {
  if (secs < 60) return "just now";
  if (secs < 86400) return fmtTalk(secs);
  return fmtDuration(secs);
}

function fmtTalk(secs) {
  if (secs < 60) return secs + " s";
  if (secs < 3600) return Math.floor(secs / 60) + " m";
  return Math.floor(secs / 3600) + " h " + Math.floor((secs % 3600) / 60) + " m";
}

function fmtClock(unix) {
  const d = new Date(unix * 1000);
  return [d.getHours(), d.getMinutes(), d.getSeconds()].map((n) => String(n).padStart(2, "0")).join(":");
}

const MODE_TEXT = {
  bridge: "bridge",
  conference: "conference",
  parrot: "parrot test",
  handset: "handset",
};

// ---- state --------------------------------------------------------------------------

let status = null;
let lastAnnounced = null;
const spectrum = new Float32Array(BINS).fill(FLOOR);
const peaks = new Float32Array(BINS).fill(FLOOR);
const history = new Float32Array(HISTORY_N).fill(-60);
let serverLevels = null;
let player = null; // the active player, see startOpus / startMp3
const stats = { decoded: 0, underruns: 0, dropped: 0, mode: "idle", reconnects: 0 };

// ---- rendering status -----------------------------------------------------------------

function setBanner() {
  const b = $("banner");
  let text = null;
  if (status) {
    if (status.stream === "off") text = "Live audio is off on this node.";
    else if (status.stream === "unavailable") text = "No live audio in this mode.";
    else if (!status.secure && !window.isSecureContext) text = "Secure connection pending, so the low-delay player is off.";
  }
  b.hidden = !text;
  b.textContent = text || "";
}

function linkRow(l) {
  const tr = document.createElement("tr");
  const badge = document.createElement("td");
  badge.append(el("span", "lp-net " + (l.network === "m17" ? "m17" : "asl"), l.network === "m17" ? "M17" : "ASL"));
  const name = el("td", "n", l.name);
  const dirText = { in: "Inbound", out: "Outbound", monitor: "Monitor", receive: "Receive only" }[l.direction] || l.direction;
  name.append(el("div", "d", dirText));
  tr.append(badge, name, el("td", "r", fmtUp(l.up_secs)));
  return tr;
}

function renderStatus() {
  const s = status;
  if (!s) return;
  const node = s.node ? "Node " + s.node : "astar node";
  $("node-name").textContent = node;
  document.title = s.node ? "Node " + s.node + " · live" : "astar node";
  $("node-mode").textContent = "astar-server · " + (MODE_TEXT[s.mode] || s.mode) + " mode";
  $("pill-online").className = "lp-pill on";
  $("online-text").textContent = "Online · " + fmtDuration(s.uptime_secs);
  $("pill-listeners").hidden = s.stream === "off";
  $("listener-count").textContent = String(s.listeners);

  const t = s.talker;
  $("talker").classList.toggle("idle", !t);
  $("pill-onair").hidden = !t;
  if (t) {
    $("talker-name").textContent = t.name;
    $("talker-meta").textContent = (t.via ? "via " + t.via + " · " : "") + fmtDuration(t.secs);
  } else {
    $("talker-name").textContent = "Quiet";
    $("talker-meta").textContent = s.last_talker ? "Last: " + s.last_talker : "Nobody is talking";
  }
  const say = t ? t.name + " talking" : "Quiet";
  if (say !== lastAnnounced) {
    $("live").textContent = say;
    lastAnnounced = say;
  }

  const rows = (s.links || []).map(linkRow);
  $("links-body").replaceChildren(...rows);
  $("link-count").textContent = String(rows.length);
  $("links-empty").hidden = rows.length > 0;

  const feed = (s.activity || []).slice(0, 8).map((e) => {
    const li = document.createElement("li");
    const time = el("time", null, fmtClock(e.at_unix));
    time.dateTime = new Date(e.at_unix * 1000).toISOString();
    li.append(time, el("span", null, e.text));
    return li;
  });
  $("feed").replaceChildren(...feed);
  $("feed-empty").hidden = feed.length > 0;

  $("today-talk").textContent = fmtTalk(s.today.talk_secs);
  $("today-tx").textContent = String(s.today.transmissions);
  $("today-stations").textContent = String(s.today.stations);

  $("mp3-url").textContent = window.location.origin + "/stream.mp3";
  $("mp3-note").textContent =
    s.stream === "off"
      ? "Live audio is off on this node."
      : !s.mp3
        ? "The MP3 stream isn't built into this server."
        : "Opens in VLC, a phone or a scanner app · about 6 s delay";
  $("copy").disabled = !s.mp3 || s.stream === "off";

  setBanner();
  updatePlayButton();
}

// ---- the event feed ---------------------------------------------------------------------

let source = null;
let feedRetry = 2000;

function openFeed() {
  source = new EventSource("/public/events");
  source.addEventListener("status", (e) => {
    feedRetry = 2000;
    try {
      status = JSON.parse(e.data);
    } catch (_) {
      return;
    }
    renderStatus();
  });
  source.addEventListener("levels", (e) => {
    try {
      serverLevels = JSON.parse(e.data);
    } catch (_) {
      return;
    }
    if (!player) takeLevels(serverLevels.dbfs, serverLevels.spectrum);
  });
  source.onerror = () => {
    source.close();
    $("pill-online").className = "lp-pill";
    $("online-text").textContent = "Reconnecting…";
    window.setTimeout(openFeed, feedRetry);
    feedRetry = Math.min(feedRetry * 2, 30000);
  };
}

// ---- graphs -----------------------------------------------------------------------------

function takeLevels(dbfs, bins) {
  for (let i = 0; i < BINS; i++) spectrum[i] = Math.max(FLOOR, Math.min(0, bins[i] === undefined ? FLOOR : bins[i]));
  history.copyWithin(0, 1);
  history[HISTORY_N - 1] = Math.max(-60, Math.min(0, dbfs));
}

function cssColor(name, fallback) {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

function fit(canvas) {
  const r = canvas.getBoundingClientRect();
  const d = window.devicePixelRatio || 1;
  const w = Math.max(1, Math.round(r.width * d));
  const h = Math.max(1, Math.round(r.height * d));
  if (canvas.width !== w || canvas.height !== h) {
    canvas.width = w;
    canvas.height = h;
  }
  const x = canvas.getContext("2d");
  x.setTransform(d, 0, 0, d, 0, 0);
  return [x, r.width, r.height];
}

function drawSpectrum(colors) {
  const [x, w, h] = fit($("spectrum"));
  x.clearRect(0, 0, w, h);
  x.strokeStyle = colors.grid;
  x.lineWidth = 1;
  for (let g = 1; g < 4; g++) {
    x.beginPath();
    x.moveTo(0, (h * g) / 4);
    x.lineTo(w, (h * g) / 4);
    x.stroke();
  }
  const plotH = h - 14;
  const y = (v) => plotH - ((v - FLOOR) / -FLOOR) * plotH;
  const px = (i) => (i / (BINS - 1)) * w;
  const grad = x.createLinearGradient(0, 0, 0, plotH);
  grad.addColorStop(0, colors.greenA);
  grad.addColorStop(1, colors.blueA);
  x.beginPath();
  x.moveTo(0, plotH);
  for (let i = 0; i < BINS; i++) x.lineTo(px(i), y(spectrum[i]));
  x.lineTo(w, plotH);
  x.closePath();
  x.fillStyle = grad;
  x.fill();
  x.beginPath();
  for (let i = 0; i < BINS; i++) (i ? x.lineTo : x.moveTo).call(x, px(i), y(spectrum[i]));
  x.strokeStyle = colors.green;
  x.lineWidth = 1.5;
  x.shadowColor = colors.green;
  x.shadowBlur = 8;
  x.stroke();
  x.shadowBlur = 0;
  x.beginPath();
  for (let i = 0; i < BINS; i++) (i ? x.lineTo : x.moveTo).call(x, px(i), y(peaks[i]));
  x.strokeStyle = colors.amber;
  x.lineWidth = 1;
  x.setLineDash([2, 3]);
  x.stroke();
  x.setLineDash([]);
  x.fillStyle = colors.dim;
  x.font = "10px -apple-system, system-ui, sans-serif";
  const span = Math.log(F_HI / F_LO);
  for (const [f, label] of [[300, "300 Hz"], [1000, "1 kHz"], [2000, "2 kHz"], [3000, "3 kHz"]]) {
    const at = (Math.log(f / F_LO) / span) * w;
    x.fillText(label, Math.min(Math.max(2, at - 12), w - 34), h - 3);
  }
}

function drawHistory(colors) {
  const [x, w, h] = fit($("history"));
  x.clearRect(0, 0, w, h);
  const y = (v) => h - ((v + 60) / 60) * h;
  x.strokeStyle = colors.grid;
  x.lineWidth = 1;
  for (const d of [-12, -48]) {
    x.beginPath();
    x.moveTo(0, y(d));
    x.lineTo(w, y(d));
    x.stroke();
  }
  const grad = x.createLinearGradient(0, 0, 0, h);
  grad.addColorStop(0, colors.greenA);
  grad.addColorStop(1, colors.clear);
  const px = (i) => (i / (HISTORY_N - 1)) * w;
  x.beginPath();
  x.moveTo(0, h);
  for (let i = 0; i < HISTORY_N; i++) x.lineTo(px(i), y(history[i]));
  x.lineTo(w, h);
  x.closePath();
  x.fillStyle = grad;
  x.fill();
  x.beginPath();
  for (let i = 0; i < HISTORY_N; i++) (i ? x.lineTo : x.moveTo).call(x, px(i), y(history[i]));
  x.strokeStyle = colors.green;
  x.lineWidth = 1.5;
  x.stroke();
  x.fillStyle = colors.dim;
  x.font = "10px -apple-system, system-ui, sans-serif";
  x.fillText("−12", 4, y(-12) - 3);
  x.fillText("−48", 4, y(-48) - 3);
}

function colors() {
  const rgb = (n) => cssColor(n, "48, 209, 88");
  return {
    green: cssColor("--green", "#30d158"),
    amber: cssColor("--amber", "#ffd60a"),
    dim: cssColor("--text-dim", "rgba(255,255,255,.45)"),
    grid: cssColor("--row-edge", "rgba(255,255,255,.06)"),
    greenA: "rgba(" + rgb("--green-rgb") + ", 0.5)",
    blueA: "rgba(" + cssColor("--blue-rgb", "10, 132, 255") + ", 0.03)",
    clear: "rgba(" + rgb("--green-rgb") + ", 0)",
  };
}

let lastFrame = 0;
let lastHistory = 0;
function frame(now) {
  const interval = reducedMotion ? 250 : 0;
  if (now - lastFrame >= interval) {
    const dt = lastFrame ? Math.min(0.25, (now - lastFrame) / 1000) : 0;
    lastFrame = now;
    if (player && player.analyser) sampleAnalyser(now);
    for (let i = 0; i < BINS; i++) peaks[i] = Math.max(spectrum[i], peaks[i] - PEAK_DECAY_DB_S * dt);
    const c = colors();
    drawSpectrum(c);
    drawHistory(c);
  }
  window.requestAnimationFrame(frame);
}

// ---- the analyser -------------------------------------------------------------------------

let freqData = null;
let timeData = null;

function sampleAnalyser(now) {
  const a = player.analyser;
  if (!freqData || freqData.length !== a.frequencyBinCount) {
    freqData = new Float32Array(a.frequencyBinCount);
    timeData = new Float32Array(a.fftSize);
  }
  a.getFloatFrequencyData(freqData);
  const nyquist = a.context.sampleRate / 2;
  const bins = [];
  for (let i = 0; i < BINS; i++) {
    const idx = Math.min(freqData.length - 1, Math.round((binCentreHz(i) / nyquist) * freqData.length));
    bins.push(Number.isFinite(freqData[idx]) ? freqData[idx] : FLOOR);
  }
  // The history advances at a steady 20 Hz whatever the frame rate.
  if (now - lastHistory >= 1000 / HISTORY_HZ) {
    lastHistory = now;
    a.getFloatTimeDomainData(timeData);
    let sum = 0;
    for (let i = 0; i < timeData.length; i++) sum += timeData[i] * timeData[i];
    const rms = Math.sqrt(sum / timeData.length);
    takeLevels(rms > 0 ? 20 * Math.log10(rms) : FLOOR, bins);
  } else {
    for (let i = 0; i < BINS; i++) spectrum[i] = Math.max(FLOOR, Math.min(0, bins[i]));
  }
}

// ---- volume -----------------------------------------------------------------------------

let volume = 0.8;
let muted = false;
(function loadVolume() {
  const saved = store(VOLUME_KEY);
  if (!saved) return;
  try {
    const v = JSON.parse(saved);
    if (typeof v.volume === "number") volume = Math.max(0, Math.min(1, v.volume));
    muted = !!v.muted;
  } catch (_) {
    // A damaged value is ignored.
  }
})();

function applyVolume() {
  $("volume").value = String(Math.round(volume * 100));
  $("mute").setAttribute("aria-pressed", muted ? "true" : "false");
  $("mute").setAttribute("aria-label", muted ? "Unmute" : "Mute");
  if (player && player.gain) player.gain.gain.value = muted ? 0 : volume;
  store(VOLUME_KEY, JSON.stringify({ volume: volume, muted: muted }));
}

$("volume").addEventListener("input", () => {
  volume = Number($("volume").value) / 100;
  muted = false;
  applyVolume();
});
$("mute").addEventListener("click", () => {
  muted = !muted;
  applyVolume();
});

// ---- the player -------------------------------------------------------------------------

function canOpus() {
  return window.isSecureContext && "AudioDecoder" in window && "AudioWorkletNode" in window;
}

/// Which player this browser and this server can run, or null and a reason.
function choosePlayer() {
  if (!status || status.stream !== "live") return { kind: null, why: null };
  if (canOpus()) return { kind: "opus", why: null };
  if (status.mp3) return { kind: "mp3", why: null };
  return { kind: null, why: "Live audio needs a newer browser." };
}

function updatePlayButton() {
  const b = $("play");
  const choice = choosePlayer();
  b.disabled = !player && !choice.kind;
  b.setAttribute("aria-pressed", player ? "true" : "false");
  b.setAttribute("aria-label", player ? "Stop listening" : "Listen live");
  const delay = $("pill-delay");
  const kind = player ? player.kind : choice.kind;
  delay.hidden = !kind;
  delay.textContent = kind === "opus" ? "Opus · ~0.4 s" : kind === "mp3" ? "MP3 · ~6 s" : "";
  if (!player) setPlayerStatus(choice.why || "");
}

function setPlayerStatus(text) {
  $("player-status").textContent = text;
}

function audioGraph(ctx) {
  const analyser = ctx.createAnalyser();
  analyser.fftSize = 1024;
  analyser.smoothingTimeConstant = 0.5;
  const gain = ctx.createGain();
  gain.gain.value = muted ? 0 : volume;
  analyser.connect(gain).connect(ctx.destination);
  return { analyser: analyser, gain: gain };
}

async function startOpus() {
  const p = { kind: "opus", stopped: false, retry: 1000 };
  player = p;
  stats.mode = "opus";
  p.ctx = new AudioContext({ latencyHint: "interactive" });
  Object.assign(p, audioGraph(p.ctx));
  await p.ctx.audioWorklet.addModule("/listener-worklet.js");
  p.node = new AudioWorkletNode(p.ctx, "astar-jitter", { outputChannelCount: [1] });
  p.node.port.onmessage = (e) => {
    stats.underruns = e.data.underruns;
    stats.dropped = e.data.dropped;
  };
  p.node.connect(p.analyser);
  connectOpus(p);
}

/// Resample mono PCM to the context's rate, linearly. Opus runs at the
/// pipeline rate (8 or 16 kHz) and an AudioContext at the device's.
function resample(input, from, to) {
  if (from === to) return input;
  const n = Math.round((input.length * to) / from);
  const out = new Float32Array(n);
  const step = from / to;
  for (let i = 0; i < n; i++) {
    const pos = i * step;
    const j = Math.floor(pos);
    const f = pos - j;
    const a = input[j] || 0;
    const b = input[j + 1] === undefined ? a : input[j + 1];
    out[i] = a + (b - a) * f;
  }
  return out;
}

function connectOpus(p) {
  if (p.stopped) return;
  const scheme = window.location.protocol === "https:" ? "wss://" : "ws://";
  const ws = new WebSocket(scheme + window.location.host + "/stream.ws");
  ws.binaryType = "arraybuffer";
  p.ws = ws;
  let rate = 16000;
  let t = 0;
  let decoder = null;
  ws.onmessage = (e) => {
    if (typeof e.data === "string") {
      let h;
      try {
        h = JSON.parse(e.data);
      } catch (_) {
        return;
      }
      rate = h.rate || 16000;
      decoder = new AudioDecoder({
        output: (data) => {
          const pcm = new Float32Array(data.numberOfFrames);
          data.copyTo(pcm, { planeIndex: 0, format: "f32-planar" });
          data.close();
          stats.decoded += 1;
          p.node.port.postMessage(resample(pcm, rate, p.ctx.sampleRate));
        },
        error: () => ws.close(),
      });
      decoder.configure({ codec: "opus", sampleRate: rate, numberOfChannels: 1 });
      p.node.port.postMessage("reset");
      p.retry = 1000;
      setPlayerStatus("Playing live");
      return;
    }
    if (!decoder || decoder.state !== "configured") return;
    decoder.decode(new EncodedAudioChunk({ type: "key", timestamp: t, data: e.data }));
    t += 20000;
  };
  ws.onclose = (e) => {
    if (decoder && decoder.state !== "closed") decoder.close();
    if (p.stopped) return;
    if (e.code === 1006 && !decoder) {
      // Refused before the upgrade (503): ask why, and say so.
      fetch("/stream.ws", { cache: "no-store" })
        .then((r) => r.text())
        .then((text) => setPlayerStatus(text.trim() || "Reconnecting…"))
        .catch(() => setPlayerStatus("Reconnecting…"));
    } else {
      setPlayerStatus("Reconnecting…");
    }
    stats.reconnects += 1;
    window.setTimeout(() => connectOpus(p), p.retry);
    p.retry = Math.min(p.retry * 2, 10000);
  };
}

function startMp3() {
  const p = { kind: "mp3", stopped: false, retry: 1000 };
  player = p;
  stats.mode = "mp3";
  const audio = $("mp3-audio");
  p.audio = audio;
  p.ctx = new AudioContext();
  Object.assign(p, audioGraph(p.ctx));
  if (!p.src) p.src = p.ctx.createMediaElementSource(audio);
  p.src.connect(p.analyser);
  const play = () => {
    if (p.stopped) return;
    audio.src = "/stream.mp3?t=" + Date.now();
    audio.play().then(
      () => setPlayerStatus("Playing live"),
      () => setPlayerStatus("Press play again to start audio."),
    );
  };
  audio.onerror = () => {
    if (p.stopped) return;
    // Ask why, without holding a listener slot: a refusal is a short text
    // body; anything else is the stream itself, aborted at once.
    const ask = new AbortController();
    fetch("/stream.mp3", { cache: "no-store", signal: ask.signal })
      .then((r) => (r.status === 503 ? r.text() : (ask.abort(), null)))
      .catch(() => null)
      .then((why) => setPlayerStatus(why ? why.trim() : "Reconnecting…"));
    window.setTimeout(play, p.retry);
    p.retry = Math.min(p.retry * 2, 10000);
  };
  audio.onplaying = () => {
    p.retry = 1000;
    stats.decoded += 1;
  };
  play();
}

function stopPlayer() {
  const p = player;
  if (!p) return;
  p.stopped = true;
  if (p.ws) p.ws.close();
  if (p.audio) {
    p.audio.pause();
    p.audio.removeAttribute("src");
    p.audio.load();
  }
  if (p.ctx) p.ctx.close();
  player = null;
  stats.mode = "idle";
  $("volume-box").hidden = true;
  updatePlayButton();
}

$("play").addEventListener("click", async () => {
  if (player) {
    stopPlayer();
    return;
  }
  const choice = choosePlayer();
  if (!choice.kind) return;
  $("volume-box").hidden = false;
  applyVolume();
  setPlayerStatus("Connecting…");
  try {
    if (choice.kind === "opus") await startOpus();
    else startMp3();
  } catch (_) {
    stopPlayer();
    if (status && status.mp3 && choice.kind === "opus") {
      $("volume-box").hidden = false;
      startMp3();
    } else {
      setPlayerStatus("Live audio needs a newer browser.");
    }
  }
  updatePlayButton();
});

// ---- copy -------------------------------------------------------------------------------

$("copy").addEventListener("click", () => {
  const url = $("mp3-url").textContent;
  const done = () => {
    $("copy").textContent = "Copied";
    window.setTimeout(() => {
      $("copy").textContent = "Copy";
    }, 1600);
  };
  if (navigator.clipboard && window.isSecureContext) {
    navigator.clipboard.writeText(url).then(done, () => selectUrl());
  } else {
    selectUrl();
  }
});

function selectUrl() {
  const range = document.createRange();
  range.selectNodeContents($("mp3-url"));
  const sel = window.getSelection();
  sel.removeAllRanges();
  sel.addRange(range);
}

// ---- debug hook (read-only) ------------------------------------------------------------

window.__astarListener = Object.freeze({
  stats: () => Object.assign({}, stats),
  choice: () => choosePlayer(),
});

// ---- start ------------------------------------------------------------------------------

applyVolume();
openFeed();
fetch("/public/status", { cache: "no-store" })
  .then((r) => r.json())
  .then((s) => {
    if (!status) {
      status = s;
      renderStatus();
    }
  })
  .catch(() => {});
window.requestAnimationFrame(frame);
