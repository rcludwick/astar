// The node's recordings: every over the node logger wrote, newest first,
// with play, download and delete.
//
// Nothing here builds markup. A talker's callsign came off the air, so every
// value reaches the DOM through createElement and textContent only, and no
// node is ever given a string of HTML. The server's CSP is the second lock.
"use strict";

const $ = (id) => document.getElementById(id);
const POLL_MS = 10000;
// The node caps how many downloads one address may have open; past it,
// it answers 429 and a browser would only show a broken download.
const BUSY_TEXT = "Too many downloads at once — finish or close one and try again.";
// A tar of nothing: what "Download all" asks first, to learn whether the
// node would take another download, without sending any recordings.
const TAR_PROBE = "recordings.tar?since=9999-12-31T23:59:59Z";
const GONE_TEXT = "That recording is gone.";
const PLAY_FAILED = "This recording could not be played.";
const NETWORKS = { allstar: "AllStar", m17: "M17", node: "node" };
const STATES = {
  recording: ["on", "recording"],
  idle: ["off", "idle"],
  no_conference: ["off", "no conference"],
  disk_low: ["err", "disk low"],
};

// The last rows rendered, by name, so a poll that changed nothing leaves
// the table (and the focus in it) alone.
let shown = "";
// The open delete confirmation, or null: { box, back }.
let confirmOpen = null;
let inFlight = 0;

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined && text !== null) e.textContent = text;
  return e;
}

function announce(text) {
  $("live").textContent = text;
}

function fileUrl(name, download) {
  return "recordings/file?name=" + encodeURIComponent(name) + (download ? "&download=1" : "");
}

function when(startedAt) {
  const d = new Date(startedAt);
  if (isNaN(d.getTime())) return startedAt;
  return d.toLocaleString([], { dateStyle: "medium", timeStyle: "medium" });
}

function length(ms) {
  const s = Math.round(ms / 1000);
  const m = Math.floor(s / 60);
  return m + ":" + String(s % 60).padStart(2, "0");
}

function lengthSpoken(ms) {
  const s = Math.round(ms / 1000);
  const m = Math.floor(s / 60);
  const rest = s % 60;
  const sec = rest + (rest === 1 ? " second" : " seconds");
  return m ? m + (m === 1 ? " minute " : " minutes ") + sec : sec;
}

function size(bytes) {
  if (bytes >= 1e9) return (bytes / 1e9).toFixed(1) + " GB";
  if (bytes >= 1e6) return (bytes / 1e6).toFixed(1) + " MB";
  if (bytes >= 1e3) return Math.round(bytes / 1e3) + " kB";
  return bytes + " B";
}

function showError(text) {
  $("error").textContent = text;
  $("error").hidden = false;
  announce(text);
}

/// Asks the node whether it would answer this download, for as little as
/// possible: one byte of a recording, or an empty tar. `null` when it would
/// (a 2xx); otherwise the sentence to show instead.
async function refusal(url, goneText) {
  let res;
  try {
    res = await fetch(url, { headers: { Range: "bytes=0-0" } });
  } catch (_e) {
    return "Can't reach the node — still trying.";
  }
  if (res.ok) return null;
  if (res.status === 429) return BUSY_TEXT;
  if (res.status === 404) return goneText;
  if (res.status === 401 || res.status === 403) return "Your session has ended — sign in again.";
  return "The node refused that (" + res.status + ").";
}

/// A download link that checks first, so a refusal reads as a sentence on
/// the page rather than a failed download, or a raw error page in the tab.
/// A click with a modifier or another button is the browser's own (a new
/// tab, a saved link) and is left alone. Only a 2xx probe starts the
/// download, through a throwaway <a download>, so the page never navigates.
function guardDownload(a, probeUrl, goneText) {
  a.addEventListener("click", async (e) => {
    if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
    e.preventDefault();
    const why = await refusal(probeUrl, goneText);
    if (why) {
      showError(why);
      return;
    }
    $("error").hidden = true;
    const go = document.createElement("a");
    go.href = a.href;
    go.download = a.getAttribute("download") || "";
    go.hidden = true;
    document.body.append(go);
    go.click();
    go.remove();
  });
}

// ---- playback ------------------------------------------------------------------

const player = $("player");
let playing = null;

function syncPlayButtons() {
  for (const b of document.querySelectorAll("button[data-play]")) {
    const on = b.dataset.play === playing && !player.paused;
    b.textContent = on ? "Pause" : "Play";
    b.setAttribute("aria-pressed", on ? "true" : "false");
  }
}

function togglePlay(name) {
  if (playing === name && !player.paused) {
    player.pause();
    return;
  }
  if (playing !== name) {
    playing = name;
    player.src = fileUrl(name, false);
  }
  player.play().catch((err) => {
    // A pause or a new source interrupts play(): that is not a failure.
    // A load failure is reported by the error event below.
    if (err && err.name === "AbortError") return;
    if (player.error) return;
    showError(PLAY_FAILED);
  });
}

player.addEventListener("error", async () => {
  if (!playing) return;
  showError((await refusal(fileUrl(playing, false), GONE_TEXT)) || PLAY_FAILED);
});

for (const ev of ["play", "pause", "ended", "emptied"]) {
  player.addEventListener(ev, syncPlayButtons);
}

// ---- delete ----------------------------------------------------------------------

function closeConfirm() {
  if (!confirmOpen) return;
  const open = confirmOpen;
  confirmOpen = null;
  open.box.remove();
  if (open.back && open.back.isConnected) open.back.focus({ preventScroll: true });
}

function openConfirm(anchor, question, onYes) {
  closeConfirm();
  const box = el("div", "confirm");
  box.setAttribute("role", "group");
  box.setAttribute("aria-label", question);
  const acts = el("div", "row-actions");
  const yes = el("button", "btn danger", "Delete");
  yes.type = "button";
  yes.setAttribute("aria-label", "Delete — " + question);
  const no = el("button", "btn", "Cancel");
  no.type = "button";
  acts.append(yes, no);
  box.append(el("p", "confirm-q", question), acts);
  anchor.after(box);
  confirmOpen = { box: box, back: document.activeElement };
  announce(question);
  yes.addEventListener("click", () => {
    closeConfirm();
    onYes();
  });
  no.addEventListener("click", closeConfirm);
  box.addEventListener("keydown", (e) => {
    if (e.key === "Escape") closeConfirm();
  });
  yes.focus({ preventScroll: true });
}

async function remove(row) {
  inFlight += 1;
  let res;
  try {
    res = await fetch("recordings/delete", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name: row.name }),
    });
  } catch (_e) {
    inFlight -= 1;
    $("offline").hidden = false;
    return;
  }
  inFlight -= 1;
  let body = {};
  try {
    body = await res.json();
  } catch (_e) {
    body = {};
  }
  if (res.ok) {
    if (playing === row.name) {
      player.removeAttribute("src");
      player.load();
      playing = null;
    }
    announce("Deleted the recording from " + when(row.started_at) + ".");
    $("error").hidden = true;
  } else {
    const msg = (body && (body.message || body.error)) || "The node refused that (" + res.status + ").";
    $("error").textContent = "Could not delete: " + msg;
    $("error").hidden = false;
  }
  refresh();
}

// ---- rendering --------------------------------------------------------------------

function renderRow(row) {
  const tr = el("tr");
  const t = when(row.started_at);
  const time = el("td", null, t);
  const timeEl = el("time", null, t);
  timeEl.dateTime = row.started_at;
  time.replaceChildren(timeEl);
  const len = el("td", "mono", length(row.duration_ms));
  len.setAttribute("aria-label", lengthSpoken(row.duration_ms));
  const talker = el("td", "mono", row.talkers && row.talkers.length ? row.talkers.join(", ") : "—");
  const network = el("td", null, NETWORKS[row.network] || row.network);
  const sz = el("td", "mono", size(row.bytes));

  const actions = el("td");
  const acts = el("div", "row-actions");
  const play = el("button", "btn", "Play");
  play.type = "button";
  play.dataset.play = row.name;
  play.setAttribute("aria-label", "Play the recording from " + t);
  play.addEventListener("click", () => togglePlay(row.name));
  const down = el("a", "btn", "Download");
  down.href = fileUrl(row.name, true);
  down.setAttribute("download", row.name);
  down.setAttribute("aria-label", "Download the recording from " + t);
  guardDownload(down, fileUrl(row.name, false), GONE_TEXT);
  const del = el("button", "btn quiet-danger", "Delete");
  del.type = "button";
  del.setAttribute("aria-label", "Delete the recording from " + t);
  del.addEventListener("click", () =>
    openConfirm(acts, "Delete the recording from " + t + "? This cannot be undone.", () => remove(row)),
  );
  acts.append(play, down, del);
  actions.append(acts);
  tr.append(time, len, talker, network, sz, actions);
  return tr;
}

function renderStatus(status, rows) {
  const [cls, text] = STATES[status.state] || ["off", status.state];
  $("badge-state").className = "badge " + cls;
  $("badge-text").textContent = text;
  $("disk-low").hidden = status.state !== "disk_low";
  const parts = [rows.length + (rows.length === 1 ? " recording" : " recordings"), size(status.total_bytes)];
  if (typeof status.free_bytes === "number") parts.push(size(status.free_bytes) + " free");
  $("summary").textContent = parts.join(" · ");
}

function render(body) {
  const rows = Array.isArray(body.rows) ? body.rows : [];
  renderStatus(body.status || {}, rows);
  $("card-list").hidden = false;
  $("download-all").hidden = rows.length === 0;
  $("count").textContent = String(rows.length);
  $("empty").style.display = rows.length ? "none" : "block";
  const key = JSON.stringify(rows.map((r) => [r.name, r.bytes]));
  if (key === shown || confirmOpen) return;
  shown = key;
  $("rows").replaceChildren(...rows.map(renderRow));
  syncPlayButtons();
}

async function refresh() {
  if (inFlight) return;
  let res;
  try {
    res = await fetch("recordings/state", { headers: { Accept: "application/json" } });
  } catch (_e) {
    $("offline").hidden = false;
    return;
  }
  $("offline").hidden = true;
  let body = {};
  try {
    body = await res.json();
  } catch (_e) {
    body = {};
  }
  if (res.status === 404 && body.error === "recording is off") {
    $("off").hidden = false;
    $("card-list").hidden = true;
    $("download-all").hidden = true;
    $("badge-state").className = "badge off";
    $("badge-text").textContent = "off";
    return;
  }
  $("off").hidden = true;
  if (!res.ok) {
    $("error").textContent = (body && (body.message || body.error)) || "The node refused that (" + res.status + ").";
    $("error").hidden = false;
    return;
  }
  render(body);
}

guardDownload($("download-all"), TAR_PROBE, "Recording is off on this node.");
refresh();
setInterval(refresh, POLL_MS);
