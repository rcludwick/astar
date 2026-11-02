// iax-24e2 node status page: a read-only EventSource consumer of /events.
// Snapshots arrive ~30x/s; renders are coalesced to at most 4x/s.
//
// Node ids, link addresses, Discord display names and M17 reflector names,
// callsigns and end reasons all arrive off the wire, so every value reaches
// the DOM through createElement and textContent — no node in this file is
// ever given a string of HTML.
"use strict";

const $ = (id) => document.getElementById(id);
let snap = null;
let dirty = false;
let logCount = 0;
const LOG_CAP = 200;

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined && text !== null) e.textContent = text;
  return e;
}

function badge(id, on, label) {
  const e = $(id);
  e.className = "badge " + (on ? "on" : "off");
  e.replaceChildren(el("i", "dot"), document.createTextNode(label));
}

function fmtUp(secs) {
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  const mm = String(m).padStart(2, "0");
  const ss = String(s).padStart(2, "0");
  return h > 0 ? `${h}:${mm}:${ss}` : `${m}:${ss}`;
}

function cell(cls, text) {
  return el("td", cls, text);
}

function linkRow(l) {
  const up = l.state === "up";
  const tr = document.createElement("tr");
  tr.append(cell("node", l.node), cell(null, l.mode));

  const st = el("span", "state " + (up ? "up" : "connecting"));
  st.append(el("i", "dot"), document.createTextNode(up ? "up" : "connecting"));
  const state = document.createElement("td");
  state.append(st);
  tr.append(state);

  const tx = document.createElement("td");
  if (l.keyed) tx.append(el("span", "pill tx", "TX"));
  const rx = document.createElement("td");
  if (l.rx_active) rx.append(el("span", "pill rx", "talking"));
  tr.append(tx, rx);

  tr.append(cell("up", up ? fmtUp(l.up_secs) : "—"), cell("addr", l.addr ? l.addr : "—"));
  return tr;
}

// The node's M17 monitor leg. It is receive-only, so it never gets a TX
// pill; "talking" means a far station is being heard through it. The addr
// cell shows the identity the node presented to the reflector, not an
// address: the reflector's address is the operator's config, not news.
function m17Row(m) {
  const up = m.state === "linked";
  const tr = document.createElement("tr");
  tr.append(cell("node", "M17 " + m.target), cell(null, "monitor"));

  const st = el("span", "state " + (up ? "up" : "connecting"));
  st.append(el("i", "dot"), document.createTextNode(up ? "up" : "connecting"));
  const state = document.createElement("td");
  state.append(st);
  tr.append(state);

  const tx = document.createElement("td");
  const rx = document.createElement("td");
  if (m.receiving) rx.append(el("span", "pill rx", "talking"));
  tr.append(tx, rx);

  const upText = up && typeof m.up_secs === "number" ? fmtUp(m.up_secs) : "—";
  tr.append(cell("up", upText), cell("addr", "as " + m.callsign));
  return tr;
}

function kv(key, value, mono) {
  const row = document.createElement("div");
  row.className = "kv-row";
  row.append(el("span", "k", key), el("span", mono ? "v mono" : "v", value));
  return row;
}

// The Discord card is hidden outright on a node with no [discord] section:
// an empty card would read as "configured and idle", which is a different
// thing from "not set up".
function renderDiscord(d) {
  const card = $("discord-card");
  if (!d) {
    card.hidden = true;
    return;
  }
  card.hidden = false;
  const box = $("discord-kv");
  box.replaceChildren();
  box.append(kv("bridge", d.connected ? "in the channel" : "not in the channel"));
  let commands = "off";
  if (d.enabled && d.commands) commands = "on";
  else if (d.enabled) commands = "on (bot offline)";
  box.append(kv("commands", commands));
  box.append(kv("mic", d.mic_holder ? d.mic_holder : "nobody", !!d.mic_holder));
  const link = d.link;
  box.append(
    link
      ? kv("link", link.network + " " + link.target + (link.up ? " (up)" : " (connecting)"), true)
      : kv("link", "none"),
  );
}

// The EchoLink card is hidden on a node with no [echolink] section. Station
// callsigns and the directory's refusal text come off the wire, so they go
// in through textContent only. The Dial box is static markup that is never
// rebuilt, so a snapshot arriving mid-typing cannot eat what is typed.
const EL_LOGIN = {
  logged_in: (c) => "Logged in as " + c,
  logging_in: (c) => "Logging in as " + c + "…",
  logged_out: (c) => "Not logged in (" + c + ")",
  failed: (c) => "Login failed for " + c,
};

function echoLinkRow(p) {
  const up = p.state === "connected";
  const tr = document.createElement("tr");
  tr.append(cell("node", p.call), cell("node", String(p.node)));
  const st = el("span", "state " + (up ? "up" : "connecting"));
  st.append(el("i", "dot"), document.createTextNode(p.state));
  const state = document.createElement("td");
  state.append(st);
  const rx = document.createElement("td");
  if (p.talking) rx.append(el("span", "pill rx", "talking"));
  // The transmit cap stopped the net reaching this station; the sentence
  // carries a directory callsign, so it goes in as text only.
  if (p.notice) {
    const capped = el("span", "pill", "stopped at 10 min");
    capped.title = p.notice;
    rx.append(capped);
  }
  tr.append(state, rx);
  tr.append(cell("up", up && typeof p.up_secs === "number" ? fmtUp(p.up_secs) : "—"));
  const act = document.createElement("td");
  const b = el("button", "btn quiet-danger", "Disconnect");
  b.type = "button";
  b.setAttribute("aria-label", "Disconnect " + p.call);
  b.addEventListener("click", () => sendEchoLink({ action: "disconnect", target: p.call }, b));
  act.append(b);
  tr.append(act);
  return tr;
}

function echoLinkNote(text) {
  const n = $("echolink-note");
  n.hidden = !text;
  n.textContent = text || "";
}

async function sendEchoLink(body, btn) {
  if (btn) btn.disabled = true;
  echoLinkNote("");
  try {
    const res = await fetch("echolink", {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    if (!res.ok) {
      let msg = "The node refused that (" + res.status + ").";
      try {
        const data = await res.json();
        if (data && typeof data.error === "string" && data.error) msg = data.error;
      } catch (_e) {
        /* keep the status sentence */
      }
      echoLinkNote(msg);
    } else if (body.action === "dial") {
      $("echolink-target").value = "";
    }
  } catch (_e) {
    echoLinkNote("Can't reach the node.");
  }
  if (btn) btn.disabled = false;
}

function renderEchoLink(e) {
  const card = $("echolink");
  if (!snap || !snap.echolink_enabled) {
    card.hidden = true;
    return;
  }
  card.hidden = false;
  const login = $("echolink-login");
  if (!e) {
    login.textContent = "";
    $("echolink-body").replaceChildren();
    $("echolink-empty").style.display = "block";
    return;
  }
  const fmt = EL_LOGIN[e.login];
  login.textContent = (fmt ? fmt(e.callsign) : e.callsign) + (e.error ? ": " + e.error : "");
  const rows = (e.peers || []).map(echoLinkRow);
  $("echolink-body").replaceChildren(...rows);
  $("echolink-empty").style.display = rows.length ? "none" : "block";
}

$("echolink-dial").addEventListener("submit", (ev) => {
  ev.preventDefault();
  const target = $("echolink-target").value.trim();
  if (!target) {
    echoLinkNote("Type a node number or callsign first.");
    return;
  }
  sendEchoLink({ action: "dial", target: target }, $("echolink-go"));
});

function render() {
  if (!snap) return;
  $("node-id").textContent = snap.node_id ? "node " + snap.node_id : "node";
  document.title = snap.node_id ? `node ${snap.node_id}` : "astar-server";
  badge("badge-listening", snap.listening, "listening");
  badge("badge-registered", snap.registered, "registered");
  const links = snap.links || [];
  const rows = links.map(linkRow);
  if (snap.m17) rows.push(m17Row(snap.m17));
  $("link-count").textContent = rows.length;
  $("links-empty").style.display = rows.length ? "none" : "block";
  $("links-body").replaceChildren(...rows);
  // Only an operator who turned M17 on needs telling that it cannot decode.
  $("badge-m17codec").hidden = !(snap.m17_enabled && !snap.m17_available);
  renderDiscord(snap.discord);
  renderEchoLink(snap.echolink);
  renderPublic(snap.public);
}

// The public page's certificate and DNS, for the operator only: the public
// status never carries any of it. Shown when something needs doing — an
// issue or renewal that failed, a dynamic-DNS pass that failed, or a
// certificate inside 14 days of expiry.
const PUBLIC_WARN_DAYS = 14;

function publicProblem(p) {
  if (!p) return null;
  const host = (p.hosts || []).join(", ") || "the public page";
  if (p.last_error) {
    const tail = p.serving_tls ? " The current certificate keeps serving." : " The page is on plain HTTP until it succeeds.";
    return "Certificate for " + host + ": " + p.last_error + "." + tail;
  }
  if (p.dns_error) return "Dynamic DNS for " + host + ": " + p.dns_error;
  if (p.not_after_unix) {
    const days = Math.floor((p.not_after_unix * 1000 - Date.now()) / 86400000);
    if (days < PUBLIC_WARN_DAYS) return "The certificate for " + host + " expires in " + Math.max(days, 0) + " days and has not renewed.";
  }
  return null;
}

function renderPublic(p) {
  const text = publicProblem(p);
  const banner = $("public-banner");
  banner.hidden = !text;
  banner.textContent = text || "";
}

function addLog(cls, text) {
  const t = new Date().toLocaleTimeString();
  const li = document.createElement("li");
  li.append(el("span", "t", t), el("span", cls, text));
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
    case "m17":
      addLog(
        ev.kind === "disconnected" ? "disconnected" : "connected",
        "M17 " + ev.reflector + " " + ev.module + " " + ev.kind +
          (ev.reason ? " (" + ev.reason + ")" : ""),
      );
      break;
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

const es = new EventSource("events");
es.onopen = () => {
  badge("badge-stream", true, "stream");
  document.body.classList.remove("stale");
};
es.onerror = () => {
  // EventSource auto-reconnects; dim the page until the next snapshot lands.
  const e = $("badge-stream");
  e.className = "badge err";
  e.replaceChildren(el("i", "dot"), document.createTextNode("stream"));
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
