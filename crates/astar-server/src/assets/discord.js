// The node's Discord admin page: the master switch, the link, the mic, and
// the approval lists.
//
// Every string that reaches this page can be a Discord display name, which
// its owner chooses. So nothing here builds markup: values reach the DOM
// only through createElement and textContent, and no node in this file is
// ever given a string of HTML. Keep it that way.
"use strict";

const $ = (id) => document.getElementById(id);
const POLL_MS = 2000;
const BUSY_TEXT = "The node is busy with a dial — try again in a moment.";
const OFFLINE_TEXT = "Can't reach the node — still trying.";

// The last /discord/state body. Also holds the refusal shape
// ({ error, configured, commands, feature }) when there is nothing to show.
let state = null;
// user id -> display name, rebuilt from every list in the body above.
let names = new Map();
// The one open inline confirmation, or null. While it is open the page does
// not re-render and does not poll, so a row cannot vanish mid-decision.
let confirmOpen = null;
// Outstanding POSTs. Same reason: no re-render while the user is mid-action.
let inFlight = 0;
// data-key -> the reason the node gave for refusing that control's last POST.
// Survives a re-render on purpose, and is cleared by the next success.
const notes = new Map();
// Controls whose POST was answered "busy" and which are waiting for their
// one automatic retry. They stay disabled for that second: the row is
// re-rendered to show the note, and a fresh enabled button there would let
// one intent become two bans.
const retrying = new Set();
// The last /discord/state body, as text. A poll that changes nothing must
// not rebuild the page: a mousedown that spans a rebuild loses its target
// and never becomes a click, and these buttons ban people.
let lastBody = null;

// ---- small pure helpers ----------------------------------------------------

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined && text !== null) e.textContent = text;
  return e;
}

function keyOf(action, userId) {
  return action + ":" + (userId || "");
}

/// Seconds since `unix`, in words. Empty for a missing timestamp.
function relTime(unix) {
  if (!unix) return "";
  const secs = Math.max(0, Math.floor(Date.now() / 1000) - Number(unix));
  if (secs < 45) return "just now";
  if (secs < 90) return "a minute ago";
  const mins = Math.round(secs / 60);
  if (mins < 60) return mins + " min ago";
  const hours = Math.round(mins / 60);
  if (hours < 24) return hours === 1 ? "an hour ago" : hours + " hours ago";
  const days = Math.round(hours / 24);
  return days === 1 ? "a day ago" : days + " days ago";
}

function absTime(unix) {
  return unix ? new Date(Number(unix) * 1000).toLocaleString() : "";
}

/// A "3 min ago" that can be brought up to date without rebuilding the row
/// it lives in: the timestamp and its prefix are on the element.
function timeNode(prefix, at) {
  const s = el("span", "when", prefix + relTime(at));
  s.dataset.at = String(at || 0);
  s.dataset.prefix = prefix;
  if (at) s.title = absTime(at);
  return s;
}

/// Re-word every timestamp on the page in place. Touches textContent only,
/// so nothing under the pointer or the focus ring moves.
function refreshTimes() {
  for (const node of document.querySelectorAll("[data-at]")) {
    const at = Number(node.dataset.at);
    if (!at) continue;
    node.textContent = (node.dataset.prefix || "") + relTime(at);
  }
}

/// A whole number of seconds, in the shortest honest words.
function fmtSecs(n) {
  const s = Number(n) || 0;
  if (s >= 60 && s % 60 === 0) return s / 60 + " min";
  return s + " s";
}

/// Who took a decision. "admin" is the page itself; anything else is a
/// snowflake, shown by name when this page has seen one.
function whoActed(by) {
  if (!by) return "";
  if (by === "admin") return "the node admin";
  return names.get(by) || by;
}

function networkName(net) {
  if (net === "allstar") return "AllStar";
  if (net === "m17") return "M17";
  if (net === "dstar") return "D-Star";
  return String(net || "");
}

// ---- the action vocabulary -------------------------------------------------
//
// One row per action the node accepts. `confirm` present means destructive:
// the button asks inline before it posts.

const ACTIONS = {
  approve: { label: "Approve", done: (n) => n + " approved" },
  revoke: {
    label: "Revoke",
    confirm: (n) => "Revoke " + n + "?",
    yes: "Yes, revoke",
    done: (n) => n + " revoked",
  },
  ban: {
    label: "Ban",
    confirm: (n) => "Ban " + n + "?",
    detail: "They can't request, be approved, or take the mic until they are unbanned.",
    yes: "Yes, ban",
    done: (n) => n + " banned",
  },
  unban: { label: "Unban", done: (n) => n + " unbanned" },
  make_admin: { label: "Make admin", done: (n) => n + " is now an admin" },
  remove_admin: {
    label: "Remove admin",
    confirm: (n) => "Remove " + n + " as an admin?",
    yes: "Yes, remove",
    done: (n) => n + " is no longer an admin",
  },
  release_mic: {
    label: "Take mic away",
    confirm: (n) => "Take the mic from " + n + "?",
    detail: "They are muted again and nobody holds the mic.",
    yes: "Yes, take it",
    done: (n) => "took the mic from " + n,
  },
  hangup: {
    label: "Hang up",
    confirm: () => "Hang up?",
    // Not just the Discord link: the engine hangs up the node's whole link
    // roster, including anything dialled by DTMF or POST /link.
    detail: "This hangs up every link on the node, not only the one dialled from Discord.",
    yes: "Yes, hang up",
    done: () => "hung up",
  },
  enable: { label: "On", done: () => "Discord commands are on" },
  disable: {
    label: "Off",
    confirm: () => "Turn Discord commands off?",
    detail:
      "Nobody will be able to dial or take the mic. Whoever has the mic is muted. " +
      "A link that is up stays up.",
    yes: "Yes, turn off",
    done: () => "Discord commands are off",
  },
};

function doneText(action, name) {
  const spec = ACTIONS[action];
  return spec && spec.done ? spec.done(name || "that user") : "done";
}

// ---- focus, notes, announcements -------------------------------------------

function activeKey() {
  const a = document.activeElement;
  return a && a.dataset ? a.dataset.key || null : null;
}

/// Find a control by its stable key without building a selector out of data
/// that came off the wire.
function findKey(key) {
  const all = document.querySelectorAll("[data-key]");
  for (const node of all) {
    if (node.dataset.key === key) return node;
  }
  return null;
}

function focusKey(key) {
  const node = key ? findKey(key) : null;
  if (node) node.focus({ preventScroll: true });
}

function noteFor(key, text) {
  notes.set(key, text);
}

function addNote(row, key) {
  const text = notes.get(key);
  if (text) row.append(el("p", "note err", text));
}

function announce(text) {
  $("live").textContent = text;
}

function setOffline(down) {
  const banner = $("offline");
  banner.hidden = !down;
  banner.textContent = OFFLINE_TEXT;
}

// ---- talking to the node ---------------------------------------------------

async function refresh() {
  let res;
  try {
    res = await fetch("state", { headers: { Accept: "application/json" } });
  } catch (_e) {
    setOffline(true);
    return;
  }
  setOffline(false);
  let text = "";
  try {
    text = await res.text();
  } catch (_e) {
    return;
  }
  if (text === lastBody) {
    // Byte-identical: nothing on the node moved. The only thing on screen
    // that still has to move is "3 min ago", and that is one textContent
    // per timestamp rather than a new DOM.
    refreshTimes();
    return;
  }
  let body = null;
  try {
    body = JSON.parse(text);
  } catch (_e) {
    body = null;
  }
  if (!body || typeof body !== "object") return;
  lastBody = text;
  state = body;
  render();
}

async function reasonOf(res) {
  try {
    const v = await res.json();
    if (v && typeof v.error === "string") return v.error;
  } catch (_e) {
    /* a refusal without a JSON body still gets a sentence below */
  }
  return "The node refused that (" + res.status + ").";
}

async function send(btn, action, userId, name, isRetry) {
  const key = (btn && btn.dataset.key) || keyOf(action, userId);
  if (btn) btn.disabled = true;
  inFlight += 1;
  const body = { action: action };
  if (userId) body.user_id = String(userId);

  let res;
  try {
    res = await fetch("admin", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
  } catch (_e) {
    inFlight -= 1;
    setOffline(true);
    noteFor(key, "Can't reach the node.");
    render();
    return;
  }
  inFlight -= 1;
  setOffline(false);

  if (res.ok) {
    notes.clear();
    announce(doneText(action, name));
    await refreshAndRender();
    focusKey(key);
    return;
  }

  if (res.status === 503 && !isRetry) {
    // The rule engine was checked out by a dial in progress. One automatic
    // retry, and the note says so in the meantime. The control stays
    // disabled for that second — `render()` below rebuilds the row, and a
    // fresh enabled button would turn one intent into two POSTs.
    noteFor(key, BUSY_TEXT);
    retrying.add(key);
    render();
    window.setTimeout(() => {
      // Cleared first: `send` re-disables its own button synchronously, so
      // there is no moment in between where the page is interactive, and
      // whatever `send` does next — success, refusal, or a second busy —
      // ends with a render that re-enables it.
      retrying.delete(key);
      send(findKey(key), action, userId, name, true);
    }, 1000);
    return;
  }

  noteFor(key, await reasonOf(res));
  announce("that was refused");
  await refreshAndRender();
  focusKey(key);
}

/// Poll, then render whatever the answer was.
///
/// `refresh` alone skips the render when the node's answer has not changed
/// — but a POST has just disabled a button, and only a render brings it
/// back. Every path out of `send` goes through here.
async function refreshAndRender() {
  await refresh();
  render();
}

// ---- inline confirmation ---------------------------------------------------

function closeConfirm() {
  if (!confirmOpen) return;
  const key = confirmOpen.key;
  confirmOpen = null;
  render();
  focusKey(key);
}

/// Replace a control's row with "<question> [Yes, …] [Cancel]".
function askFirst(btn, action, userId, name) {
  const row = btn.closest(".row-actions");
  if (!row) {
    send(btn, action, userId, name);
    return;
  }
  if (confirmOpen) {
    // Only one open at a time, and cancelling the old one re-renders, which
    // would throw away the row this button lives in — so re-find it after.
    const key = btn.dataset.key;
    closeConfirm();
    const again = findKey(key);
    if (again) askFirst(again, action, userId, name);
    return;
  }

  const spec = ACTIONS[action];
  const who = name || "this user";
  row.replaceChildren();
  row.classList.add("confirming");
  const question = spec.confirm(who);
  const q = el("span", "confirm-q", question);
  const yes = el("button", "btn danger", spec.yes);
  yes.type = "button";
  yes.dataset.key = btn.dataset.key;
  // The visible text is "Yes, ban"; on its own that names no one, and this
  // is the button that does the irreversible thing.
  yes.setAttribute("aria-label", spec.yes + " — " + question);
  const no = el("button", "btn", "Cancel");
  no.type = "button";
  row.append(q, yes, no);
  if (spec.detail) row.append(el("p", "note", spec.detail));

  confirmOpen = { key: btn.dataset.key };
  announce(spec.detail ? question + " " + spec.detail : question);
  yes.addEventListener("click", () => {
    confirmOpen = null;
    row.classList.remove("confirming");
    send(yes, action, userId, name);
  });
  no.addEventListener("click", closeConfirm);
  yes.focus({ preventScroll: true });
}

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && confirmOpen) {
    e.preventDefault();
    closeConfirm();
  }
});

// ---- building the page -----------------------------------------------------

function actionButton(action, userId, name) {
  const spec = ACTIONS[action];
  const b = el("button", spec.confirm ? "btn quiet-danger" : "btn", spec.label);
  b.type = "button";
  b.dataset.key = keyOf(action, userId);
  b.disabled = retrying.has(b.dataset.key);
  b.setAttribute("aria-label", name ? spec.label + " " + name : spec.label);
  b.addEventListener("click", () => {
    if (spec.confirm) askFirst(b, action, userId, name);
    else send(b, action, userId, name);
  });
  return b;
}

/// The user id, small, monospace, and copyable.
function idButton(userId) {
  const b = el("button", "id mono", userId);
  b.type = "button";
  b.setAttribute("aria-label", "Copy user id " + userId);
  b.addEventListener("click", () => {
    const back = () => {
      b.textContent = userId;
      b.classList.remove("copied");
    };
    if (!navigator.clipboard) {
      b.textContent = "copy unavailable";
      window.setTimeout(back, 1200);
      return;
    }
    navigator.clipboard.writeText(userId).then(
      () => {
        b.textContent = "copied";
        b.classList.add("copied");
        window.setTimeout(back, 1200);
      },
      () => {
        b.textContent = "copy failed";
        window.setTimeout(back, 1200);
      },
    );
  });
  return b;
}

function blank(...parts) {
  const p = el("p", "blank");
  for (const part of parts) {
    if (typeof part === "string") p.append(document.createTextNode(part));
    else p.append(el("code", null, part.code));
  }
  return p;
}

function badgeFor(text, tone) {
  return el("span", "tag " + (tone || ""), text);
}

/// One row: who they are, what the node knows about them, and what can be
/// done to them.
function personRow(entry, actions, tags) {
  const li = el("li", "person");
  const who = el("div", "who");
  const line = el("div", "name-line");
  line.append(el("span", "name", entry.name || "unknown"));
  // The callsign they registered with, when the node knows one. It is the
  // identity that will go on the air for them, so it sits with the name.
  const callsign = (state && state.callsigns && state.callsigns[entry.user_id]) || "";
  if (callsign) line.append(badgeFor(callsign, "call"));
  for (const t of tags || []) line.append(t);
  who.append(line);

  const meta = el("div", "meta");
  meta.append(idButton(entry.user_id));
  const by = whoActed(entry.by);
  if (entry.at_unix) {
    meta.append(timeNode(by ? "by " + by + " · " : "", entry.at_unix));
  } else if (by) {
    meta.append(el("span", "when", "by " + by));
  }
  who.append(meta);
  li.append(who);

  const row = el("div", "row-actions");
  for (const a of actions) {
    row.append(actionButton(a, entry.user_id, entry.name || entry.user_id));
  }
  for (const a of actions) addNote(row, keyOf(a, entry.user_id));
  li.append(row);
  return li;
}

function renderList(id, countId, entries, actions, emptyParts, tagsFor) {
  const list = $(id);
  list.replaceChildren();
  $(countId).textContent = entries.length;
  if (!entries.length) {
    list.append(blank(...emptyParts));
    return;
  }
  for (const e of entries) {
    list.append(personRow(e, actions, tagsFor ? tagsFor(e) : []));
  }
}

function renderBadges() {
  const set = (id, on, text) => {
    const b = $(id);
    b.className = "badge " + (on ? "on" : "off");
    b.replaceChildren(el("i", "dot"), document.createTextNode(text));
  };
  set("badge-bridge", !!state.connected, state.connected ? "in the channel" : "not in the channel");
  set("badge-gateway", !!state.commands, state.commands ? "bot online" : "bot offline");
  set("badge-commands", !!state.enabled, state.enabled ? "commands on" : "commands off");

  const err = $("last-error");
  if (state.last_error) {
    err.hidden = false;
    err.textContent = "Last error: " + state.last_error;
  } else {
    err.hidden = true;
    err.textContent = "";
  }
}

function renderSwitch() {
  const slot = $("slot-switch");
  // Reset the class too: this slot is also the row a confirmation replaces,
  // and "confirming" would otherwise survive the cancel that rebuilt it.
  slot.className = "slot row-actions";
  slot.replaceChildren();
  const on = !!state.enabled;
  const action = on ? "disable" : "enable";
  const b = el("button", on ? "switch on" : "switch");
  b.type = "button";
  b.dataset.key = keyOf(action, "");
  b.setAttribute("role", "switch");
  b.setAttribute("aria-checked", on ? "true" : "false");
  b.setAttribute("aria-label", "Discord commands");
  b.disabled = retrying.has(b.dataset.key);
  b.append(el("span", "track"), el("span", "switch-text", on ? "On" : "Off"));
  b.addEventListener("click", () => {
    if (on) askFirst(b, "disable", null, null);
    else send(b, "enable", null, null);
  });
  slot.append(b);
  addNote(slot, b.dataset.key);
  $("note-switch").textContent = on
    ? "Approved users can dial the node and take the mic from Discord."
    : "Off: nobody can dial or take the mic. People are still muted when they join, and a link that is up stays up.";
}

function renderLink() {
  const box = $("link-body");
  box.replaceChildren();
  const link = state.link;
  if (!link) {
    box.append(blank("Not linked. Approved users can ", { code: "/dial" }, " from Discord."));
    return;
  }
  const head = el("div", "link-head");
  const target = el("div", "link-target");
  target.append(el("span", "link-net", networkName(link.network)));
  target.append(el("span", "link-node mono", link.target));
  const up = !!link.up;
  const st = el("span", "state " + (up ? "up" : "connecting"));
  st.append(el("i", "dot"), document.createTextNode(up ? "up" : "connecting"));
  target.append(st);
  head.append(target);

  const row = el("div", "row-actions");
  row.append(actionButton("hangup", "", link.target));
  addNote(row, keyOf("hangup", ""));
  head.append(row);
  box.append(head);

  const meta = el("div", "meta");
  const by = link.by ? "dialled by " + link.by : "dialled from Discord";
  if (link.since_unix) {
    meta.append(timeNode(by + " · ", link.since_unix));
  } else {
    meta.append(el("span", "when", by));
  }
  box.append(meta);
}

function renderPresent() {
  const list = $("list-present");
  const present = state.present || [];
  const holder = state.mic_holder;
  list.replaceChildren();
  $("count-present").textContent = present.length;
  if (!present.length) {
    list.append(
      blank("Nobody is in the bridged voice channel. People appear here when they join it in Discord."),
    );
  }
  for (const p of present) {
    // The mic is the node's own state, not Discord's: `server_muted` can be a
    // moment stale, `mic_holder` cannot.
    const isHolder = !!holder && p.user_id === holder;
    let tag;
    if (isHolder && state.talking) tag = badgeFor("\u{1F534} talking", "live");
    else if (isHolder) tag = badgeFor("\u{1F399}\u{FE0F} has the mic", "mic");
    else tag = badgeFor("muted", "muted");
    list.append(personRow(p, isHolder ? ["release_mic"] : [], [tag]));
  }

  const timeout = Number(state.mic_timeout_secs) || 0;
  const silence = Number(state.mic_silence_secs) || 0;
  const bits = [];
  if (silence > 0) bits.push("unkeys after " + fmtSecs(silence) + " of silence");
  if (timeout > 0) bits.push(fmtSecs(timeout) + " limit");
  $("mic-timers").textContent = bits.length ? bits.join(" \u00B7 ") : "No mic timers are set.";
}

function renderSetup() {
  $("admin").hidden = true;
  const card = $("setup");
  card.hidden = false;
  card.replaceChildren();
  const head = el("div", "card-head");
  const noFeature = state.feature === false;
  head.append(el("h2", null, noFeature ? "Not in this build" : "Slash commands are off"));
  card.append(head);

  if (noFeature) {
    card.append(
      el(
        "p",
        "lead",
        "This astar-server was built without its Discord support, so there are no slash commands to manage.",
      ),
    );
    card.append(blank("Rebuild it with ", { code: "cargo build -p astar-server --features discord" }, "."));
    return;
  }

  card.append(
    el(
      "p",
      "lead",
      "Nobody can run a slash command until this node knows whose word it takes. Add your Discord user id as an approver in Settings, then restart the node:",
    ),
  );
  const go = el("a", "manage", "Open the Discord settings →");
  go.href = "../settings#discord";
  card.append(go);
  card.append(
    blank(
      "Approvers are the top tier: they can approve people, make admins, and act on an admin. Then this page manages the rest.",
    ),
  );
}

function renderAdmin() {
  $("setup").hidden = true;
  $("admin").hidden = false;
  renderSwitch();
  renderLink();
  renderPresent();
  renderList("list-pending", "count-pending", state.pending || [], ["approve", "ban"], [
    "Nobody is waiting. People ask with ",
    { code: "/register callsign:" },
    " in Discord.",
  ]);
  renderList("list-admins", "count-admins", state.admins || [], ["remove_admin"], [
    "No admins yet. Promote someone from Approved below, or an approver can use ",
    { code: "/admin add" },
    " in Discord.",
  ]);
  renderList(
    "list-approved",
    "count-approved",
    state.approved || [],
    ["revoke", "ban", "make_admin"],
    [
      "Nobody is approved. Approve a pending request and they can ",
      { code: "/dial" },
      " and take the mic.",
    ],
  );
  renderList("list-banned", "count-banned", state.banned || [], ["unban"], [
    "Nobody is banned. A banned user can't request access, be approved, or take the mic.",
  ]);
  renderList(
    "list-approvers",
    "count-approvers",
    (state.approvers || []).map((id) => ({ user_id: id, name: names.get(id) || "approver" })),
    [],
    ["No approvers. Slash commands need at least one."],
  );
}

function rebuildNames() {
  names = new Map();
  const lists = [state.admins, state.pending, state.approved, state.banned, state.present];
  for (const list of lists) {
    for (const e of list || []) {
      if (e && e.user_id && e.name) names.set(e.user_id, e.name);
    }
  }
}

function render() {
  // A re-render throws away every row, so it must not happen underneath
  // someone who is deciding something or waiting for an answer.
  if (confirmOpen || inFlight > 0 || !state) return;
  const key = activeKey();
  rebuildNames();
  renderBadges();
  if (state.configured) renderAdmin();
  else renderSetup();
  if (key) focusKey(key);
}

// ---- the poll --------------------------------------------------------------

function tick() {
  if (confirmOpen || inFlight > 0 || document.hidden) return;
  refresh();
}

refresh();
window.setInterval(tick, POLL_MS);
