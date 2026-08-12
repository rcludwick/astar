// iax-24e2 node status page: a read-only EventSource consumer of /events.
// Snapshots arrive ~30x/s; renders are coalesced to at most 4x/s.
"use strict";

const $ = (id) => document.getElementById(id);
let snap = null;
let dirty = false;
let logCount = 0;
const LOG_CAP = 200;

function badge(id, on, label) {
  const el = $(id);
  el.className = "badge " + (on ? "on" : "off");
  el.innerHTML = '<i class="dot"></i>' + label;
}

function fmtUp(secs) {
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  const mm = String(m).padStart(2, "0");
  const ss = String(s).padStart(2, "0");
  return h > 0 ? `${h}:${mm}:${ss}` : `${m}:${ss}`;
}

function esc(s) {
  const d = document.createElement("span");
  d.textContent = String(s);
  return d.innerHTML;
}

function render() {
  if (!snap) return;
  $("node-id").textContent = snap.node_id ? "node " + snap.node_id : "node";
  document.title = snap.node_id ? `node ${snap.node_id}` : "astar-server";
  badge("badge-listening", snap.listening, "listening");
  badge("badge-registered", snap.registered, "registered");
  const links = snap.links || [];
  $("link-count").textContent = links.length;
  $("links-empty").style.display = links.length ? "none" : "block";
  $("links-body").innerHTML = links
    .map((l) => {
      const up = l.state === "up";
      return `<tr>
        <td class="node">${esc(l.node)}</td>
        <td>${esc(l.mode)}</td>
        <td><span class="state ${up ? "up" : "connecting"}"><i class="dot"></i>${up ? "up" : "connecting"}</span></td>
        <td>${l.keyed ? '<span class="pill tx">TX</span>' : ""}</td>
        <td>${l.rx_active ? '<span class="pill rx">talking</span>' : ""}</td>
        <td class="up">${up ? fmtUp(l.up_secs) : "—"}</td>
        <td class="addr">${l.addr ? esc(l.addr) : "—"}</td>
      </tr>`;
    })
    .join("");
}

function addLog(cls, text) {
  const t = new Date().toLocaleTimeString();
  const li = document.createElement("li");
  li.innerHTML = `<span class="t">${t}</span><span class="${cls}">${esc(text)}</span>`;
  const log = $("log");
  log.prepend(li);
  while (log.children.length > LOG_CAP) log.removeChild(log.lastChild);
  logCount += 1;
  $("log-count").textContent = logCount;
  $("log-empty").style.display = "none";
}

function onEvent(ev) {
  switch (ev.event) {
    case "snapshot":
      snap = ev;
      dirty = true;
      break;
    case "link": {
      const what =
        ev.kind === "keyed"
          ? `link ${ev.node} ${ev.keyed ? "keyed" : "unkeyed"}`
          : `link ${ev.node} ${ev.kind}` + (ev.reason ? ` (${ev.reason})` : "");
      addLog(ev.kind, what);
      break;
    }
    case "dtmf":
      addLog("dtmf", `DTMF ${ev.digit}` + (ev.command ? ` → ${ev.command}` : ""));
      break;
    case "registered":
      addLog("connected", "registered");
      break;
    case "register_failed":
      addLog("disconnected", `register failed (${ev.reason})`);
      break;
    case "incoming_call":
      addLog("connected", `incoming call from ${ev.from}`);
      break;
    case "hangup":
      addLog("disconnected", `hangup (${ev.reason})`);
      break;
    default:
      break;
  }
}

const es = new EventSource("/events");
es.onopen = () => {
  badge("badge-stream", true, "stream");
  document.body.classList.remove("stale");
};
es.onerror = () => {
  // EventSource auto-reconnects; dim the page until the next snapshot lands.
  const el = $("badge-stream");
  el.className = "badge err";
  el.innerHTML = '<i class="dot"></i>stream';
  document.body.classList.add("stale");
};
es.onmessage = (m) => {
  try {
    onEvent(JSON.parse(m.data));
  } catch (_e) {
    /* tolerate unknown frames (additive-evolution rule) */
  }
};
setInterval(() => {
  if (dirty) {
    dirty = false;
    render();
  }
}, 250);
