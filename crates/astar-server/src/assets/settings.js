// The node's settings page: every section of node.toml, and the credentials
// kept beside it.
//
// Nothing here builds markup. Every value on this page came off the wire —
// a device name, a template string, an error sentence — so values reach the
// DOM only through createElement and textContent, and no node in this file
// is ever given a string of HTML. The server's CSP is the second lock on
// that door; this rule is the first. Keep it that way.
"use strict";

const $ = (id) => document.getElementById(id);
// The light refresh: badges, tags, .was lines, chips, the checklist — never
// an input. A poll that rebuilt a field would eat the edit under the cursor.
const POLL_MS = 5000;
const OFFLINE_TEXT = "Can't reach the node — still trying.";
const CONFLICT_TEXT = "node.toml changed since the page loaded — reload to see the current file.";
const AUTH_BANNER_TEXT = 'auth = "off" with no allowed_nodes admits every caller on the internet.';
// What the Identity card offers for a registrar when node.toml has none. The
// daemon resolves hostnames and fails over across every address one has, so
// the name beats the pinned address the old template carried.
const DEFAULT_REGISTRAR = "register.allstarlink.org:4569";

// The last /settings/state body.
let state = null;
// The etag of node.toml as of the last FULL fetch or the last save. The light
// poll never touches it: an edit made outside this page must surface as a
// 409 on the next save, not be adopted silently.
let etag = null;
// Cards with unsaved edits. A full render leaves these alone, so saving one
// card never sweeps up another's half-typed values.
const dirty = new Set();
// Outstanding POSTs. No render and no poll while one is in flight.
let inFlight = 0;

// The eleven cards in page order. A card is shown when it has a renderer.
const CARD_NAMES = ["identity", "listener", "audio", "announce", "discord", "linking", "m17", "vocoder", "public", "recording", "advanced"];

// ---- small pure helpers ----------------------------------------------------

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined && text !== null) e.textContent = text;
  return e;
}

function announce(text) {
  $("live").textContent = text;
}

function setOffline(down) {
  const banner = $("offline");
  banner.hidden = !down;
  banner.textContent = OFFLINE_TEXT;
}

function disk(path) {
  return state && state.disk ? state.disk[path] : undefined;
}

function running(path) {
  return state && state.running ? state.running[path] : undefined;
}

/// Equal as JSON, with "absent" and null the same thing: an optional key
/// that is not in the file and an empty input agree.
function same(a, b) {
  const norm = (v) => JSON.stringify(v === undefined ? null : v);
  return norm(a) === norm(b);
}

function isLive(path) {
  return !!state && (state.live || []).some((p) => path.startsWith(p));
}

/// "apply" when the file has a live-tier value the node has not taken yet,
/// "restart" when a restart-tier value waits, else null.
function pendingKind(path) {
  const p = (state && state.pending) || {};
  if ((p.apply || []).includes(path)) return "apply";
  if ((p.restart || []).includes(path)) return "restart";
  return null;
}

/// The tag beside a field. Amber only when something actually waits: a
/// restart-tier field that agrees with the running node is merely "on
/// restart", in the dim base style.
function tagFor(path) {
  const kind = pendingKind(path);
  if (kind === "apply") return el("span", "apply-tag apply", "apply");
  if (kind === "restart") return el("span", "apply-tag restart", "restart");
  if (isLive(path)) return el("span", "apply-tag live", "live");
  return el("span", "apply-tag", "on restart");
}

/// The running node's value in words when it differs from the file, else
/// null. This is the honest half of the restart tag: the file says one
/// thing, the node is doing another, and the page shows both.
function wasText(path) {
  const now = running(path);
  if (same(now, disk(path))) return null;
  if (now === undefined || now === null) return "running: not set";
  return "running: " + (Array.isArray(now) ? now.join(", ") : String(now));
}

/// The button beside a live-tier field whose file value the node has not
/// taken — a `sudo -e` edit of bridge.mode, say. It sends the file's own
/// value back; the node treats a value already on disk but still pending
/// as a live apply. The file that put the path here was edited outside this
/// page, so the page's etag is stale by construction: the click adopts the
/// current file first (a dirty card is still left alone by that render),
/// then applies only if the path still waits.
function applyButton(path) {
  const b = el("button", "btn", "Apply");
  b.type = "button";
  b.dataset.apply = path;
  b.addEventListener("click", async () => {
    await fetchState();
    if (pendingKind(path) !== "apply") return;
    const patch = {};
    // A key removed by hand (audio.input deleted to fall back to the system
    // default) reads undefined here, and JSON.stringify would drop it,
    // sending an empty patch that "saves" nothing. null is the wire's
    // "remove this key", which is what the file already says.
    const v = disk(path);
    patch[path] = v === undefined ? null : v;
    await postPatch(patch, []);
  });
  return b;
}

// ---- finding things without building selectors -----------------------------

/// A field row by wire path. The path in a 400 came off the wire, so it is
/// matched, never spliced into a selector.
function fieldRow(path) {
  for (const row of document.querySelectorAll(".field")) {
    if (row.dataset.path === path) return row;
  }
  return null;
}

function controlOf(path) {
  const row = fieldRow(path);
  return row ? row.querySelector("[data-kind]") : null;
}

function bodyOf(card) {
  return $("body-" + card);
}

/// The card's "Advanced" disclosure, made on first use and always last in
/// the body — renderers add their advanced fields after the common ones.
function advancedOf(card) {
  const body = bodyOf(card);
  let d = body.querySelector("details.advanced");
  if (!d) {
    d = el("details", "advanced");
    d.append(el("summary", null, "Advanced"));
    body.append(d);
  }
  return d;
}

// ---- controls --------------------------------------------------------------

/// Options as {value, label}. A plain string is its own label, except the
/// empty string, which is the "unset" choice on a nullable picker and reads
/// "(default)"; collectPatch sends it as null so the key is removed.
function normaliseOptions(options) {
  return (options || []).map((o) =>
    typeof o === "string" ? { value: o, label: o === "" ? "(default)" : o } : o,
  );
}

/// The one place a switch's four faces are set: dataset, class, aria and
/// the On/Off text. Renderers that preset a switch call this rather than
/// touching the button, so the reader (valueOf) and the setter agree.
function setSwitch(b, on) {
  b.dataset.value = on ? "true" : "false";
  b.className = on ? "switch on" : "switch";
  b.setAttribute("aria-checked", on ? "true" : "false");
  b.lastChild.textContent = on ? "On" : "Off";
}

function switchButton(on, label, onChange) {
  const b = el("button", "switch");
  b.type = "button";
  b.setAttribute("role", "switch");
  b.setAttribute("aria-label", label);
  b.append(el("span", "track"), el("span", "switch-text", "Off"));
  setSwitch(b, on);
  b.addEventListener("click", () => {
    const now = b.dataset.value !== "true";
    setSwitch(b, now);
    onChange(now);
  });
  return b;
}

function segmented(options, value, onChange) {
  const seg = el("div", "seg");
  seg.setAttribute("role", "radiogroup");
  seg.dataset.value = value;
  for (const o of normaliseOptions(options)) {
    const on = o.value === value;
    const b = el("button", on ? "on" : null, o.label);
    b.type = "button";
    b.setAttribute("role", "radio");
    b.setAttribute("aria-checked", on ? "true" : "false");
    b.dataset.option = o.value;
    b.addEventListener("click", () => {
      seg.dataset.value = o.value;
      for (const other of seg.children) {
        const chosen = other === b;
        other.className = chosen ? "on" : "";
        other.setAttribute("aria-checked", chosen ? "true" : "false");
      }
      onChange(o.value);
    });
    seg.append(b);
  }
  return seg;
}

/// A list of removable chips with an add input, in the stylesheet's `.chips` idiom:
/// one flex container whose direct children are the `.chip` pills (each
/// with its own `.x` remove button), then the `.chip-input`, then Add. The
/// list is the DOM: each chip carries its item, and collectPatch reads them
/// back from there.
function chipList(items, spec, onChange) {
  const box = el("div", "chips");
  const input = el("input", "chip-input");
  const draw = () => {
    for (const old of Array.from(box.children)) {
      if (old.classList.contains("chip") || old.classList.contains("blank")) old.remove();
    }
    for (const item of items) {
      const chip = el("span", "chip", item);
      chip.dataset.item = item;
      const x = el("button", "x", "×");
      x.type = "button";
      x.setAttribute("aria-label", "Remove " + item);
      x.addEventListener("click", () => {
        items.splice(items.indexOf(item), 1);
        draw();
        onChange(items.slice());
        input.focus();
      });
      chip.append(x);
      box.insertBefore(chip, input);
    }
    if (!items.length) box.insertBefore(el("span", "blank", spec.empty || "none"), input);
  };
  input.type = "text";
  if (spec.digits) input.inputMode = "numeric";
  input.placeholder = spec.placeholder || "";
  input.setAttribute("aria-label", "Add to " + spec.label);
  // The add row's input is not a bound value: never part of a patch.
  input.dataset.noPatch = "1";
  const add = () => {
    const v = input.value.trim();
    input.value = "";
    if (!v || items.includes(v)) return;
    items.push(v);
    draw();
    onChange(items.slice());
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      add();
    }
  });
  const addBtn = el("button", "btn", "Add");
  addBtn.type = "button";
  addBtn.addEventListener("click", add);
  // The input and Add go in first, so draw() can place the chips before
  // the input with insertBefore on every redraw.
  box.append(input, addBtn);
  draw();
  return box;
}

function makeControl(card, path, spec, id) {
  const value = disk(path);
  const touched = () => markDirty(card);
  const text = value === undefined || value === null ? "" : String(value);
  let c;
  if (spec.kind === "text" || spec.kind === "number") {
    c = el("input");
    c.type = spec.kind;
    if (spec.kind === "number") {
      c.step = spec.step || "1";
      if (spec.min !== undefined) c.min = String(spec.min);
    }
    if (spec.digits) c.inputMode = "numeric";
    if (spec.placeholder) c.placeholder = spec.placeholder;
    c.value = text;
    c.addEventListener("input", touched);
  } else if (spec.kind === "select") {
    c = el("select");
    for (const o of normaliseOptions(spec.options)) {
      const opt = el("option", null, o.label);
      opt.value = o.value;
      c.append(opt);
    }
    c.value = text;
    c.addEventListener("change", touched);
  } else if (spec.kind === "bool") {
    c = switchButton(!!value, spec.label, touched);
  } else if (spec.kind === "seg") {
    c = segmented(spec.options, text, touched);
  } else {
    c = chipList(Array.isArray(value) ? value.slice() : [], spec, touched);
  }
  // Every kind carries the id, so the row's label can point at it where
  // the element is labelable (input, select, the switch button) and the
  // rest have a stable anchor.
  c.id = id;
  c.dataset.path = path;
  c.dataset.kind = spec.kind;
  if (spec.required) c.dataset.required = "1";
  if (spec.kind === "bool" || spec.kind === "seg" || spec.kind === "chips") {
    c.setAttribute("aria-labelledby", id + "-label");
  }
  // What the control SHOWS once built, read back through the same reader
  // collectPatch uses. A control is sent only when it moves away from this
  // — never because the file lacks a key the control renders as its
  // default (an unchecked switch over an absent bool, an empty chip list
  // over an absent array).
  c.dataset.baseline = JSON.stringify(valueOf(c));
  return c;
}

/// One row: label, control, tag — and the running value when it differs,
/// with an Apply button when the difference is live-tier.
/// `spec.advanced` puts the row under the card's disclosure.
///
/// Inside `.field-control` the order is: the control, then the Apply
/// button beside it, then the paragraphs (hint, running value), each of
/// which takes its own line under the control through the stylesheet's
/// `.field-control > p` rule. The label's `for` is set only where the
/// control is a labelable element — an input, a select, the switch button
/// — because `for` pointing at a `div` (a segmented control, a chip list)
/// is invalid and does nothing; those kinds are named by `aria-labelledby`.
function field(card, path, spec) {
  const row = el("div", "field");
  row.dataset.path = path;
  const id = "f-" + path.replace(/[^A-Za-z0-9_-]/g, "-");
  const label = el("label", "field-label", spec.label);
  label.id = id + "-label";
  if (spec.kind !== "seg" && spec.kind !== "chips") label.htmlFor = id;
  const box = el("div", "field-control");
  box.append(makeControl(card, path, spec, id));
  if (pendingKind(path) === "apply") box.append(applyButton(path));
  if (spec.hint) box.append(el("p", "hint", spec.hint));
  const was = wasText(path);
  if (was) box.append(el("p", "was", was));
  row.append(label, box, tagFor(path));
  (spec.advanced ? advancedOf(card) : bodyOf(card)).append(row);
  return row;
}

// ---- credentials: presence only ---------------------------------------------

function credStatus(name) {
  const c = (state && state.credentials) || {};
  if (name.startsWith("link:")) return (c.links || {})[name.slice(5)] || "unset";
  if (name.startsWith("peer:")) return (c.peers || {})[name.slice(5)] || "unset";
  return c[name] || "unset";
}

// ---- reading a card back ---------------------------------------------------

function valueOf(control) {
  const kind = control.dataset.kind;
  const required = control.dataset.required === "1";
  if (kind === "text" || kind === "number") {
    const v = control.value.trim();
    if (v === "") return required ? "" : null;
    return kind === "number" ? Number(v) : v;
  }
  if (kind === "select") return control.value === "" ? null : control.value;
  if (kind === "bool") return control.dataset.value === "true";
  if (kind === "seg") return control.dataset.value;
  return Array.from(control.querySelectorAll(".chip[data-item]")).map((c) => c.dataset.item);
}

/// Every bound control in the card whose value has moved away from what it
/// showed when it was built. Comparing against the baseline rather than the
/// file is what keeps an untouched card silent: an unchecked switch over an
/// absent key and an empty chip list over an absent array are both "as
/// rendered", not edits, so a first Save never writes a dozen defaults. A
/// required field left blank is sent blank so the node's own rule can say
/// what it wanted. Only `[data-path]` controls are read — a password input
/// or an add-row input has no path and is `data-no-patch` besides, so a
/// config write can never carry a credential.
function collectPatch(card) {
  const patch = {};
  for (const control of bodyOf(card).querySelectorAll("[data-path][data-kind]")) {
    if (control.disabled || control.dataset.noPatch) continue;
    const path = control.dataset.path;
    const v = valueOf(control);
    if (JSON.stringify(v) !== control.dataset.baseline) patch[path] = v;
  }
  const rule = CARD_PATCH_RULES[card];
  return rule ? rule(patch) : patch;
}

// A card whose patch is not just its controls. Identity: the switch is the
// presence of [register] — off removes the whole table, on with no table
// yet needs the registrar as well as the number.
const CARD_PATCH_RULES = {
  identity(patch) {
    const on = $("register-switch").dataset.value === "true";
    const had = disk("register.node_id") !== undefined;
    if (!on) return had ? { register: null } : {};
    if (!had && !("register.peer" in patch)) patch["register.peer"] = DEFAULT_REGISTRAR;
    return patch;
  },
  // [m17] is the opt-in; see m17Patch (a hoisted function declaration).
  m17: m17Patch,
};

// ---- dirty state, errors, saving -------------------------------------------

function writable() {
  return !state || !state.file || state.file.writable !== false;
}

function markDirty(card) {
  dirty.add(card);
  $("save-" + card).disabled = !writable();
  renderBadges();
}

function clearErrors(card) {
  for (const n of $("card-" + card).querySelectorAll(".note.err")) n.remove();
}

/// Pin a refusal under its field and put the focus on something that can
/// take it: the input itself, a segmented control's first button, a chip
/// list's add input. An advanced field lives in a closed `<details>`, and a
/// sentence pinned into a closed disclosure is invisible — so the
/// disclosure is opened first; focus() alone does not reliably open one.
/// `ackKey` is not used here: postPatch asks the acknowledgement question
/// itself, inline, after this has pinned the sentence.
function pinError(path, text, ackKey) {
  void ackKey;
  const row = fieldRow(path);
  if (!row) {
    announce(text);
    return;
  }
  const details = row.closest("details.advanced");
  if (details) details.open = true;
  const old = row.querySelector(".note.err");
  if (old) old.remove();
  (row.querySelector(".field-control") || row).append(el("p", "note err", text));
  const control = row.querySelector("[data-kind]");
  let target = control;
  if (control && control.dataset.kind === "seg") target = control.querySelector("button");
  else if (control && control.dataset.kind === "chips") target = control.querySelector(".chip-input");
  if (target) target.focus();
  announce(text);
}

function showConflict() {
  const b = $("conflict");
  b.hidden = false;
  b.replaceChildren(document.createTextNode(CONFLICT_TEXT + " "));
  const reload = el("button", "btn", "Reload");
  reload.type = "button";
  reload.addEventListener("click", () => window.location.reload());
  b.append(reload);
  announce(CONFLICT_TEXT);
}

/// A refusal of the whole state (503 with no store, a 500 from the node):
/// the banner says why and the cards collapse, because a card with an
/// empty body — or a stale one under a banner — reads as broken.
function showUnavailable(text) {
  const b = $("unavailable");
  b.hidden = !text;
  b.textContent = text || "";
  $("cards").hidden = !!text;
}

// ---- talking to the node ---------------------------------------------------

async function getState() {
  let res;
  try {
    res = await fetch("settings/state", { headers: { Accept: "application/json" } });
  } catch (_e) {
    setOffline(true);
    return null;
  }
  setOffline(false);
  let body = null;
  try {
    body = await res.json();
  } catch (_e) {
    body = null;
  }
  if (!res.ok) {
    const why = body && typeof body.error === "string" ? body.error : "The node refused the settings state (" + res.status + ").";
    showUnavailable(why);
    return null;
  }
  showUnavailable("");
  return body && typeof body === "object" ? body : null;
}

/// The full fetch: adopts the file's etag and rebuilds every clean card.
async function fetchState() {
  const body = await getState();
  if (!body) return;
  state = body;
  etag = body.etag;
  renderAll();
}

/// The light refresh: everything that can move under the operator without
/// taking an edit with it. The etag is left alone on purpose — except when
/// the page has never had one: a first fetch that failed (a 500 while
/// node.toml is mid-hand-edit, a dropped connection) left every card shell
/// hidden and no etag to save with, so the first answer that does arrive is
/// adopted as a full fetch. No card can be dirty then, because none was
/// ever rendered.
async function refreshLight() {
  const body = await getState();
  if (!body || inFlight > 0) return;
  if (etag === null) {
    state = body;
    etag = body.etag;
    renderAll();
    return;
  }
  state = body;
  renderBadges();
  renderFileBanner();
  renderAuthBanner();
  renderChecklist();
  renderRestartBar();
  refreshTags();
  renderCredChips();
}

function refreshTags() {
  for (const row of document.querySelectorAll(".field[data-path]")) {
    const path = row.dataset.path;
    const tag = row.querySelector(".apply-tag");
    if (tag) tag.replaceWith(tagFor(path === "register" ? "register.node_id" : path));
    const old = row.querySelector(".was");
    if (old) old.remove();
    const box = row.querySelector(".field-control") || row;
    const was = wasText(path);
    if (was) box.append(el("p", "was", was));
    // Add or remove the Apply button only when its state changes: replacing
    // it on every 5 s poll would take the keyboard focus off it.
    // Beside the control, as field() places it — not after the paragraphs.
    const want = pendingKind(path) === "apply";
    const have = row.querySelector("button[data-apply]");
    const control = row.querySelector("[data-kind]");
    if (want && !have) {
      if (control) control.after(applyButton(path));
      else box.append(applyButton(path));
    } else if (!want && have) {
      have.remove();
    }
  }
}

// ---- the header, banners, checklist, restart bar ----------------------------

function renderBadges() {
  const set = (id, cls, text) => {
    const b = $(id);
    b.className = "badge " + cls;
    b.replaceChildren(el("i", "dot"), document.createTextNode(text));
  };
  set("badge-saved", dirty.size ? "off" : "on", "saved");
  const n = state && state.pending ? (state.pending.restart || []).length : 0;
  set("badge-restart", n ? "warn" : "off", n ? n + " restart" : "restart");
  set("badge-file", writable() ? "on" : "err", "file");
  $("badge-file").title = state && state.file ? state.file.path : "";
}

function renderFileBanner() {
  const b = $("file-banner");
  const bad = !writable();
  b.hidden = !bad;
  b.textContent = bad ? "node.toml is not writable by this node: " + state.file.path : "";
  if (bad) {
    for (const name of CARD_NAMES) $("save-" + name).disabled = true;
  }
}

function renderAuthBanner() {
  const b = $("auth-banner");
  const open = disk("listener.auth") === "off" && !(disk("listener.allowed_nodes") || []).length;
  b.hidden = !open;
  b.textContent = AUTH_BANNER_TEXT;
}

/// Four steps for a node that is not registered yet; once it is, the card
/// collapses to the header's `registered` chip.
/// Each step is two flex children — the dot, then the link holding only
/// the text — so the `.step` gap spaces them, `.step.done` dims the
/// row, and `.step a` keeps the link blue.
/// The 5 s poll calls this too, so it rebuilds the card only when a step's
/// state or target changed: a rebuild would take the keyboard focus off
/// the link it replaced.
function renderChecklist() {
  const card = $("setup");
  const done = !!(state && state.node_id && state.registered);
  card.hidden = done;
  const chip = $("badge-registered");
  chip.hidden = !done;
  chip.className = done ? "badge on" : "badge off";
  const c = (state && state.credentials) || {};
  const steps = [
    { text: "Node number", done: disk("register.node_id") !== undefined, href: "#identity" },
    { text: "Node password", done: c.node === "set", href: "#identity" },
    { text: "Restart to register", done: !!(state && state.registered), href: "#restart-bar" },
    {
      text: "Portal and Discord (optional)",
      done: disk("portal.user") !== undefined || disk("discord.channel_id") !== undefined,
      href: "#linking",
    },
  ];
  const sig = JSON.stringify([done, steps.map((s) => [s.done, s.href])]);
  if (card.dataset.sig === sig) return;
  card.dataset.sig = sig;
  card.replaceChildren();
  if (done) return;
  const head = el("div", "card-head");
  head.append(el("h2", null, "Set up this node"));
  card.append(head);
  const list = el("ol", "checklist");
  for (const s of steps) {
    const li = el("li", s.done ? "step done" : "step");
    const a = el("a", null, s.text);
    a.href = s.href;
    li.append(el("i", "dot"), a);
    list.append(li);
  }
  card.append(list);
}

// ---- the cards -------------------------------------------------------------

function renderIdentity() {
  const card = "identity";
  const body = bodyOf(card);
  body.replaceChildren();
  const has = disk("register.node_id") !== undefined;

  // The switch is presence: [register] in the file is registration on.
  const row = el("div", "field");
  row.dataset.path = "register";
  const label = el("span", "field-label", "Register with AllStarLink");
  label.id = "f-register-label";
  const box = el("div", "field-control");
  const sw = switchButton(has, "Register with AllStarLink", (on) => {
    for (const input of body.querySelectorAll("input[data-path]")) input.disabled = !on;
    markDirty(card);
  });
  sw.id = "register-switch";
  box.append(sw);
  box.append(
    el(
      "p",
      "hint",
      has
        ? "Registration is on. Turning it off removes [register] from node.toml."
        : "Off: this node answers calls but is not listed with a registrar.",
    ),
  );
  row.append(label, box, tagFor("register.node_id"));
  body.append(row);

  field(card, "register.node_id", {
    label: "Node number",
    kind: "text",
    required: true,
    digits: true,
    placeholder: "12345",
  });
  field(card, "register.peer", {
    label: "Registrar",
    kind: "text",
    required: true,
    advanced: true,
    placeholder: DEFAULT_REGISTRAR,
    hint: "host:port — a hostname is resolved at startup, every address it has.",
  });
  if (!has) {
    controlOf("register.peer").value = DEFAULT_REGISTRAR;
    for (const input of body.querySelectorAll("input[data-path]")) input.disabled = true;
  }
}

function renderListener() {
  const card = "listener";
  bodyOf(card).replaceChildren();
  field(card, "listener.auth", {
    label: "Authentication",
    kind: "seg",
    required: true,
    options: [
      { value: "required", label: "Required" },
      { value: "optional", label: "Optional" },
      { value: "off", label: "Off" },
    ],
  });
  field(card, "listener.allowed_nodes", {
    label: "Allowed nodes",
    kind: "chips",
    digits: true,
    placeholder: "node number",
    empty: "any node",
  });
  field(card, "listener.answer", {
    label: "Answer",
    kind: "seg",
    required: true,
    options: [
      { value: "auto", label: "Auto" },
      { value: "manual", label: "Manual" },
    ],
  });
  field(card, "listener.bind", {
    label: "Bind",
    kind: "text",
    required: true,
    advanced: true,
    placeholder: "0.0.0.0:4569",
  });
  field(card, "listener.max_calls", {
    label: "Max calls",
    kind: "number",
    required: true,
    advanced: true,
    min: 0,
  });
  renderAuthBanner();
}

function renderAudio() {
  const card = "audio";
  bodyOf(card).replaceChildren();
  field(card, "audio.input", { label: "Input device", kind: "text", placeholder: "system default" });
  field(card, "audio.output", { label: "Output device", kind: "text", placeholder: "system default" });
  field(card, "bridge.mode", {
    label: "Bridge mode",
    kind: "seg",
    required: true,
    options: [
      { value: "handset", label: "Handset" },
      { value: "bridge", label: "Bridge" },
      { value: "conference", label: "Conference" },
      { value: "parrot", label: "Parrot" },
    ],
  });
  field(card, "bridge.mix_minus", { label: "Mix-minus", kind: "bool" });
  field(card, "bridge.include_local_radio", {
    label: "Local radio",
    kind: "bool",
    hint: "Puts the local microphone on the air.",
  });
  field(card, "audio.backend", {
    label: "Backend",
    kind: "select",
    advanced: true,
    options: [
      { value: "", label: "default (cpal)" },
      { value: "cpal", label: "cpal" },
      { value: "none", label: "none — no sound hardware" },
    ],
  });
  field(card, "parrot.playback_delay_ms", {
    label: "Parrot playback delay (ms)",
    kind: "number",
    advanced: true,
    min: 0,
  });
  field(card, "parrot.silence_gap_ms", {
    label: "Parrot silence gap (ms)",
    kind: "number",
    advanced: true,
    min: 0,
  });
  field(card, "parrot.vox_threshold_db", {
    label: "Parrot VOX threshold (dB)",
    kind: "number",
    advanced: true,
    step: "any",
  });
}

const CARD_RENDERERS = {
  identity: renderIdentity,
  listener: renderListener,
  audio: renderAudio,
  announce: renderCardAnnounce,
  discord: renderCardDiscord,
  linking: renderCardLinking,
  m17: renderCardM17,
  vocoder: renderCardVocoder,
  public: renderCardPublic,
  recording: renderCardRecording,
  advanced: renderCardAdvanced,
};

function renderAll() {
  renderBadges();
  renderFileBanner();
  renderChecklist();
  for (const name of CARD_NAMES) {
    const draw = CARD_RENDERERS[name];
    $("card-" + name).hidden = !draw;
    if (!draw || dirty.has(name)) continue;
    draw();
    $("save-" + name).disabled = true;
  }
  renderIdentityCredential();
  refreshTags();
  renderCredChips();
  renderRestartBar();
  if (!writable()) renderFileBanner();
}

// ---- cards 4–7, credentials, acknowledgements, restart ----------------------
//
// Same rule as the rest of the file: node numbers, paths, Discord ids and the
// node's own error sentences all arrive off the wire, and every one of them
// reaches the DOM through createElement and textContent. Credential values go
// the other way only — from a password input straight into one POST body —
// and are never kept in a variable that outlives that request. No input this
// section creates without a data-path ever reaches collectPatch: each one is
// marked data-no-patch as well, so a config write cannot carry a secret.

/// Put a block into a card's body: inside its Advanced details, or before
/// them, or at the end of the body when there are none yet. Always inside
/// #body-<card>, because that is the only element collectPatch reads — a
/// block parked beside the body (before the .card-foot, say) would show
/// its fields and never save them. Everything mounted here is tagged so a
/// re-render can clear it without touching the details element field()
/// created, which advancedOf() finds again on the next render.
function mount(card, node, advanced) {
  const body = bodyOf(card);
  const det = body.querySelector("details.advanced");
  node.dataset.mounted = card;
  if (advanced && det) det.append(node);
  else if (det) det.before(node);
  else body.append(node);
  return node;
}

function clearMounted(card) {
  for (const n of bodyOf(card).querySelectorAll("[data-mounted]")) n.remove();
}

/// A .field row holding something other than a bound control — a
/// credential input, a button, a read-only value. Same three columns.
function customRow(label, control, tag) {
  const row = el("div", "field");
  row.append(el("span", "field-label", label));
  const c = el("div", "field-control");
  c.append(control);
  row.append(c);
  if (tag) row.append(tag);
  return row;
}

/// One POST with a JSON body. Resolves to { status, body }: body is the
/// parsed object or {} when there was none; status 0 means the node could
/// not be reached at all. The offline banner follows the outcome through
/// setOffline — the one place that touches it — and the next
/// successful fetch of any kind clears it. Counts as in flight, so the
/// light poll stays away while it runs.
async function postJson(path, payload, retried) {
  inFlight += 1;
  let res;
  try {
    res = await fetch(path, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
    });
  } catch (_e) {
    inFlight -= 1;
    setOffline(true);
    return { status: 0, body: {} };
  }
  inFlight -= 1;
  setOffline(false);
  let body = {};
  try {
    body = await res.json();
  } catch (_e) {
    body = {};
  }
  if (!body || typeof body !== "object") body = {};
  // Signed in on the public page and the last credential use is too old for
  // a save: confirm it's you, then send the same request once more.
  if (res.status === 401 && body.error === "reauth_required" && !retried && window.astarAuth) {
    if (await window.astarAuth.stepUp("Saving settings needs a fresh confirmation.")) {
      return postJson(path, payload, true);
    }
  }
  return { status: res.status, body: body };
}

function errorText(r) {
  if (r.status === 0) return "Can't reach the node — still trying.";
  // Sign-in refusals carry a code in `error` and the sentence in `message`.
  if (typeof r.body.message === "string") return r.body.message;
  if (typeof r.body.error === "string") return r.body.error;
  return "The node refused that (" + r.status + ").";
}

// The one open inline confirmation, or null: { box, back } where `back` is
// the element that had focus, so Cancel and Esc put it back.
let confirmOpen = null;

function closeConfirm() {
  if (!confirmOpen) return;
  const open = confirmOpen;
  confirmOpen = null;
  open.box.remove();
  if (open.back && open.back.isConnected) open.back.focus({ preventScroll: true });
}

/// Ask before doing: a .confirm block inserted after `anchor`, Yes and
/// Cancel, Esc cancels, focus returns to where it was. One at a time —
/// opening another cancels the first. An anchor inside a closed Advanced
/// disclosure is opened first: a question appended into a closed <details>
/// is not rendered, and focus() on a button that is not rendered is a
/// no-op — the operator would see a Save button come back on and nothing
/// else. The one confirm that gates keying the transmitter for an idle CW
/// ID (announce.cw_keys_when_idle) sits in exactly such a disclosure.
function openConfirm(anchor, question, onYes) {
  closeConfirm();
  const det = anchor.closest("details.advanced");
  if (det) det.open = true;
  const box = el("div", "confirm");
  box.setAttribute("role", "group");
  box.setAttribute("aria-label", question);
  const acts = el("div", "row-actions");
  const yes = el("button", "btn danger", "Yes");
  yes.type = "button";
  // "Yes" on its own names nothing, and this is the button that does the
  // irreversible thing.
  yes.setAttribute("aria-label", "Yes — " + question);
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
  yes.focus({ preventScroll: true });
}

// ---- credentials -------------------------------------------------------------

const CRED_NOTE = {
  env: "Applied until restart — this node reads credentials from the environment.",
  config: "Applied until restart — this node reads credentials from node.toml.",
  control: "Applied until restart — this node reads credentials from the control port.",
};

/// Presence only: the node never says what a credential is, only whether
/// it has one. `name` is the wire name: node, portal, discord, link:<n>,
/// peer:<user>. The lookup is credStatus; this is its boolean.
function credentialPresence(name) {
  return credStatus(name) === "set";
}

/// What a successful Set means on this node. The portal password is read
/// once at startup, and a non-file source forgets everything at
/// restart — both are said, because a chip reading "set" would otherwise
/// promise more than the node does.
function credNote(name) {
  const src = state.credential_source;
  const parts = [];
  if (name === "portal") parts.push("The node reads the portal password at startup — restart, then test.");
  if (name.startsWith("porkbun_")) parts.push("The node reads Porkbun's keys at startup — restart to use them.");
  if (src !== "file" && CRED_NOTE[src]) parts.push(CRED_NOTE[src]);
  if (!parts.length) parts.push("Saved.");
  return parts.join(" ");
}

async function postCredential(name, value, migrate) {
  const body = { name: name, value: value };
  if (migrate) body.migrate = true;
  return postJson("settings/credential", body);
}

async function forgetCredential(name) {
  return postJson("settings/credential", { name: name, forget: true });
}

/// One credential: a password input that is created empty and never filled,
/// a chip, and the buttons. The value leaves the input in exactly one place
/// — the POST in the click handler — and the input is cleared before the
/// answer is looked at. The input has no data-path and is marked
/// data-no-patch, so collectPatch cannot carry it into a config write.
function credentialRow(name, label) {
  const box = el("div");
  box.dataset.cred = name;
  const tpl = document.getElementById("tpl-credential");
  const input = tpl.content.firstElementChild.cloneNode(true);
  input.setAttribute("aria-label", label);
  input.dataset.noPatch = "1";
  const chip = el("span", "chip unset", "not set");
  box.append(customRow(label, input, chip));

  const acts = el("div", "row-actions");
  const set = el("button", "btn", "Set");
  set.type = "button";
  set.disabled = true;
  set.dataset.act = "set";
  const migrate = el("button", "btn", "Move credentials to credentials.toml");
  migrate.type = "button";
  migrate.disabled = true;
  migrate.hidden = true;
  migrate.dataset.act = "migrate";
  const forget = el("button", "btn quiet-danger", "Forget");
  forget.type = "button";
  forget.hidden = true;
  forget.dataset.act = "forget";
  const note = el("p", "note");
  acts.append(set, migrate, forget, note);
  box.append(acts);

  const say = (text, bad) => {
    note.className = bad ? "note err" : "note";
    note.textContent = text;
    announce(bad ? "that was refused" : text);
  };
  input.addEventListener("input", (e) => {
    // A password input is not a config edit: it never dirties the card.
    e.stopPropagation();
    set.disabled = input.value === "";
    migrate.disabled = input.value === "";
  });
  const submit = async (doMigrate) => {
    set.disabled = true;
    migrate.disabled = true;
    const r = await postCredential(name, input.value, doMigrate);
    input.value = "";
    if (r.status !== 200) {
      say(errorText(r), true);
      return;
    }
    if (doMigrate) {
      // node.toml changed under the page (source flipped to "file"), so the
      // etag it holds is stale: a full fetch, not the light one.
      say("Moved to credentials.toml — this node now reads credentials from the file.", false);
      await fetchState();
      return;
    }
    say(credNote(name), false);
    await refreshLight();
  };
  set.addEventListener("click", () => submit(false));
  migrate.addEventListener("click", () => submit(true));
  forget.addEventListener("click", () => {
    openConfirm(acts, "Forget the " + label.toLowerCase() + "? The node stops using it now.", async () => {
      const r = await forgetCredential(name);
      if (r.status !== 200) {
        say(errorText(r), true);
        return;
      }
      say(label + " forgotten.", false);
      await refreshLight();
    });
  });
  return box;
}

/// Chips and button labels from the current state. Light-poll safe: it
/// never touches an input.
function renderCredChips() {
  const file = !!state && state.credential_source === "file";
  for (const row of document.querySelectorAll("[data-cred]")) {
    const has = credentialPresence(row.dataset.cred);
    const chip = row.querySelector(".chip");
    chip.className = "chip " + (has ? "set" : "unset");
    chip.textContent = has ? "set" : "not set";
    row.querySelector("[data-act=set]").textContent = has ? "Replace" : "Set";
    row.querySelector("[data-act=forget]").hidden = !has;
    row.querySelector("[data-act=migrate]").hidden = file;
  }
}

/// The node password row in the Identity card. Mounted below the card's
/// other rows, above Advanced.
function renderIdentityCredential() {
  if (dirty.has("identity")) return;
  clearMounted("identity");
  mount("identity", credentialRow("node", "Node password"), false);
}

// ---- card 4: Announcements ---------------------------------------------------

const EVENT_NAMES = ["incoming_call", "hangup", "registered", "register_failed", "answered"];

/// A boolean whose default when the key is absent is `true` — two of them:
/// announce.cw_keys_when_idle (an Option<bool> read with unwrap_or(true))
/// and discord.announce_on_join (serde default_true). An absent key must
/// show as on, or the page would say the opposite of what the node does
/// and the operator would be unable to turn it off from here (nothing to
/// untick). Set through the same setter the switch itself uses,
/// then the baseline is re-stamped to match, so the tick is what the page
/// shows, not an edit — a first Save writes nothing for it.
function presetOn(row) {
  const c = row.querySelector("[data-path]");
  if (!c || c.dataset.kind !== "bool") return;
  setSwitch(c, true);
  c.dataset.baseline = JSON.stringify(valueOf(c));
}

function renderCardAnnounce() {
  const card = "announce";
  if (dirty.has(card)) return;
  clearMounted(card);

  const main = el("div");
  main.append(
    field(card, "announce.enabled", { label: "Announcements", kind: "bool" }),
    field(card, "announce.id_mode", { label: "ID mode", kind: "seg", options: ["off", "cw", "tts"] }),
    field(card, "announce.id_interval_secs", { label: "ID interval (seconds)", kind: "number", min: 0 }),
    field(card, "announce.join_template", { label: "Join greeting", kind: "text" }),
    el("p", "hint", "{server-node-number} in the greeting becomes this node's number, read digit by digit."),
  );
  mount(card, main, false);

  const adv = el("div");
  adv.append(
    field(card, "announce.tts.enabled", { label: "TTS", kind: "bool", advanced: true }),
    field(card, "announce.tts.binary", { label: "TTS binary", kind: "text", advanced: true }),
    field(card, "announce.tts.voice", { label: "TTS voice", kind: "text", advanced: true }),
    field(card, "announce.tts.timeout_ms", { label: "TTS time-out (ms)", kind: "number", advanced: true, min: 0 }),
    field(card, "announce.tts.gain_db", { label: "TTS gain (dB)", kind: "number", advanced: true, step: "any" }),
    field(card, "announce.cw_wpm", { label: "CW speed (wpm)", kind: "number", advanced: true, min: 0 }),
    field(card, "announce.cw_tone_hz", { label: "CW tone (Hz)", kind: "number", advanced: true, step: "any" }),
  );
  const idle = field(card, "announce.cw_keys_when_idle", {
    label: "CW keys when idle",
    kind: "bool",
    advanced: true,
  });
  if (state.disk["announce.cw_keys_when_idle"] === undefined) presetOn(idle);
  adv.append(idle);

  // The per-event table: five events, each with an on/off and a destination.
  // The destination is nullable, so "" ((default)) is offered and maps to
  // null — picking it removes the key rather than pinning a value. An h3:
  // the card's own title in the shell is the h2, and a sub-heading at the
  // same level would read in the heading rotor as a peer of the cards.
  adv.append(el("h3", null, "Per event"));
  for (const name of EVENT_NAMES) {
    adv.append(
      field(card, "announce.events." + name + ".enabled", { label: name, kind: "bool", advanced: true }),
      field(card, "announce.events." + name + ".destination", {
        label: name + " → destination",
        kind: "select",
        options: ["", "to_air", "to_monitor"],
        advanced: true,
      }),
    );
  }
  adv.append(
    field(card, "announce.mixunder_default_gain_db", {
      label: "Mix-under gain (dB)",
      kind: "number",
      advanced: true,
      step: "any",
    }),
    field(card, "announce.link_connect_template", { label: "Link connect phrase", kind: "text", advanced: true }),
    field(card, "announce.link_disconnect_template", { label: "Link disconnect phrase", kind: "text", advanced: true }),
    field(card, "announce.sounds_dir", { label: "Sounds directory", kind: "text", advanced: true }),
  );
  mount(card, adv, true);
}

// ---- card 5: Discord ---------------------------------------------------------

function renderCardDiscord() {
  const card = "discord";
  if (dirty.has(card)) return;
  clearMounted(card);

  const main = el("div");
  if (state.feature && state.feature.discord === false) {
    main.append(
      el(
        "p",
        "banner warn",
        "This astar-server was built without its Discord support. What is saved here waits for a build that has it.",
      ),
    );
  }
  const manage = el("a", "manage", "Approvals and the mic →");
  manage.href = "discord/admin";
  // announce_on_join defaults to true when the key is absent (serde
  // default_true), so an absent key shows on — see presetOn.
  const onJoin = field(card, "discord.announce_on_join", { label: "Announce when joining", kind: "bool" });
  if (state.disk["discord.announce_on_join"] === undefined) presetOn(onJoin);
  main.append(
    manage,
    field(card, "discord.guild_id", { label: "Guild id", kind: "text" }),
    field(card, "discord.channel_id", { label: "Voice channel id", kind: "text" }),
    credentialRow("discord", "Bot token"),
    field(card, "discord.join_at_start", { label: "Join the channel at start", kind: "bool" }),
    onJoin,
    field(card, "discord.approvers", { label: "Approvers", kind: "chips" }),
    el("p", "hint", "Approvers are Discord user ids. An empty list turns slash commands off."),
  );
  mount(card, main, false);

  const adv = el("div");
  adv.append(
    field(card, "discord.state_file", { label: "Approvals file", kind: "text", advanced: true }),
    field(card, "discord.mic_timeout_secs", {
      label: "Mic time-out (seconds, 0 = none)",
      kind: "number",
      advanced: true,
      min: 0,
    }),
    field(card, "discord.mic_silence_secs", { label: "Mic silence unkey (seconds)", kind: "number", advanced: true, min: 0 }),
  );
  mount(card, adv, true);
}

// ---- card 6: Linking ---------------------------------------------------------

async function testPortal() {
  return postJson("settings/portal/test", {});
}

/// "Test the running credentials": the node mints and drops a WT token with
/// what it loaded at startup. A password set a moment ago is not what is
/// being tested — the button's label says so.
function portalTestRow() {
  const acts = el("div", "row-actions");
  const btn = el("button", "btn", "Test the running credentials");
  btn.type = "button";
  const note = el("p", "note");
  acts.append(btn, note);
  btn.addEventListener("click", async () => {
    btn.disabled = true;
    note.className = "note";
    note.textContent = "Testing…";
    const r = await testPortal();
    btn.disabled = false;
    if (r.status === 200) {
      note.textContent = "Portal credentials work.";
      announce(note.textContent);
      return;
    }
    note.className = "note err";
    note.textContent = errorText(r);
    announce("that was refused");
  });
  return acts;
}

/// Adding a link saves at once, together with whatever else is edited in
/// the Linking card: the add is posted as this card's save, so a 400 lands
/// in this card, the dirty mark clears on 200 because the edits rode along,
/// and the re-render shows the new row. The node number is checked here
/// because the node's own sentence would name a field that does not exist
/// on the page yet. Neither input is bound, and both are data-no-patch.
function linkAddRow() {
  const acts = el("div", "row-actions");
  const num = el("input");
  num.type = "text";
  num.inputMode = "numeric";
  num.placeholder = "node number";
  num.setAttribute("aria-label", "Node number to add");
  num.dataset.noPatch = "1";
  const shape = el("select");
  shape.setAttribute("aria-label", "Link shape");
  shape.dataset.noPatch = "1";
  for (const o of ["standard", "wt-guest"]) {
    const opt = el("option", null, o);
    opt.value = o;
    shape.append(opt);
  }
  const add = el("button", "btn", "Add");
  add.type = "button";
  const note = el("p", "note");
  acts.append(num, shape, add, note);
  for (const c of [num, shape]) {
    // Typing a node number is not an edit of a bound field.
    c.addEventListener("input", (e) => e.stopPropagation());
    c.addEventListener("change", (e) => e.stopPropagation());
  }
  add.addEventListener("click", async () => {
    const n = num.value.trim();
    if (!/^[0-9]+$/.test(n)) {
      note.className = "note err";
      note.textContent = '"' + n + '" is not a node number (digits only)';
      num.focus();
      return;
    }
    const patch = {};
    patch['links."' + n + '".shape'] = shape.value;
    // Off while the POST is out: a second click would send the same patch
    // with the same etag, and its 409 would raise a false conflict banner
    // over a save that succeeded.
    add.disabled = true;
    await postPatch(Object.assign(collectPatch("linking"), patch), [], "linking");
    add.disabled = false;
  });
  return acts;
}

/// One block per node: shape (a bound field), its dial secret, Remove. The
/// node list comes from state.credentials.links, which the node builds from
/// both node.toml and the provider, so a secret without a table still shows
/// — with its shape reading (default) until one is chosen.
function linksTable(card) {
  const box = el("div");
  const nodes = Object.keys((state.credentials && state.credentials.links) || {}).sort(
    (a, b) => Number(a) - Number(b),
  );
  if (!nodes.length) {
    box.append(el("p", "blank", "No links are configured. Add a node number to set its dial shape or secret."));
  }
  for (const n of nodes) {
    box.append(
      field(card, 'links."' + n + '".shape', { label: n, kind: "select", options: ["", "standard", "wt-guest"] }),
      credentialRow("link:" + n, "Link secret for " + n),
    );
    const acts = el("div", "row-actions");
    const rm = el("button", "btn quiet-danger", "Remove " + n);
    rm.type = "button";
    acts.append(rm);
    rm.addEventListener("click", () => {
      openConfirm(acts, "Remove node " + n + " from the link table? Its secret is kept until you forget it.", async () => {
        // One POST at a time, for the same reason as Add.
        if (inFlight > 0) return;
        // The node applies a patch in sorted key order, and links."<n>"
        // sorts before links."<n>".shape: a pending shape edit riding along
        // would recreate the table the removal just dropped. So every key
        // under the removed node is dropped from the card's edits first.
        const patch = collectPatch("linking");
        const prefix = 'links."' + n + '".';
        for (const k of Object.keys(patch)) if (k.startsWith(prefix)) delete patch[k];
        patch['links."' + n + '"'] = null;
        rm.disabled = true;
        await postPatch(patch, [], "linking");
        rm.disabled = false;
      });
    });
    box.append(acts);
  }
  box.append(linkAddRow());
  return box;
}

function renderCardLinking() {
  const card = "linking";
  if (dirty.has(card)) return;
  clearMounted(card);

  const main = el("div");
  main.append(
    field(card, "portal.user", { label: "Portal user", kind: "text" }),
    field(card, "portal.node", { label: "Portal node", kind: "text" }),
    credentialRow("portal", "Portal password"),
    portalTestRow(),
    el("h3", null, "Links"),
    linksTable(card),
    el("h3", null, "DTMF"),
    field(card, "dtmf.enabled", { label: "DTMF commands", kind: "bool" }),
  );
  mount(card, main, false);

  const adv = el("div");
  adv.append(
    field(card, "dtmf.inter_digit_timeout_ms", {
      label: "DTMF inter-digit time-out (ms)",
      kind: "number",
      advanced: true,
      min: 0,
    }),
    field(card, "portal.credential_env", {
      label: "Portal password variable (env, control and config sources)",
      kind: "text",
      advanced: true,
    }),
  );
  mount(card, adv, true);
}

// ---- card 7: Advanced --------------------------------------------------------

// Real values only; the select offers "" ((default)) in front of them, which
// collectPatch sends as null — the key is removed and the library default
// (prefer_slin16) is back.
const CODEC_POLICIES = ["ulaw_only", "allow_slin", "prefer_slin", "prefer_slin16"];

/// Read-only: the WireGuard section is edited by hand and its private key
/// never comes near this page. A hand edit that the running node has not
/// loaded carries the same restart tag as any other pending path.
function wireguardSummary() {
  const box = el("div", "kv");
  const disk = state.disk || {};
  const keys = Object.keys(disk)
    .filter((k) => k.startsWith("wireguard."))
    .sort();
  if (!keys.length) {
    box.append(el("p", "blank", "No [wireguard] section. Add one to node.toml by hand; the private key stays in the environment."));
    return box;
  }
  const pending = (state.pending && state.pending.restart) || [];
  for (const k of keys) {
    const row = el("div", "kv-row");
    row.append(el("span", "k", k.slice("wireguard.".length)));
    const v = disk[k];
    row.append(el("span", "v mono", Array.isArray(v) ? v.join(", ") : String(v)));
    if (pending.includes(k)) row.append(tagFor(k));
    box.append(row);
  }
  box.append(el("p", "hint", "Read-only here — edit node.toml by hand and restart."));
  return box;
}

function renderCardAdvanced() {
  const card = "advanced";
  if (dirty.has(card)) return;
  clearMounted(card);

  const main = el("div");
  main.append(
    field(card, "control.bind", { label: "Control port bind", kind: "text" }),
    el(
      "p",
      "banner warn",
      "The control port is this node's root of trust — it can POST /key. Keep it on loopback. " +
        "A wildcard is refused; any other address needs a confirmation, and only from a page opened on loopback.",
    ),
    field(card, "control.loopback_login", {
      label: "Sign-in on loopback too",
      kind: "bool",
      hint: "Off: anyone on this machine is trusted as the console admin, which is how curl and recovery work. On: for shared hosts; recovery is then the astar-server admin command only.",
    }),
    field(card, "codec_policy", { label: "Codec policy", kind: "select", options: [""].concat(CODEC_POLICIES) }),
    field(card, "credentials.source", { label: "Credential source", kind: "select", options: ["file", "env", "control", "config"] }),
    field(card, "credentials.file", { label: "Credentials file", kind: "text" }),
    el("p", "hint", "Changing the source here moves no credential. The Move button on a credential row does that, and flips the source for you."),
    el("h3", null, "WireGuard"),
    wireguardSummary(),
  );
  mount(card, main, false);
}

// ---- POST /settings, with the acknowledgements ------------------------------

// Acknowledgement key → the question, given the patch that provoked it.
const ACK_QUESTIONS = {
  // From the sign-in guard: the node's sentence names how many passkeys and
  // whose; this is the question that goes with it.
  "web.hostnames.strand": () =>
    "Save anyway? Those passkeys and keys stop working on this node until their owners add new ones (an authenticator app still works).",
  "control.bind.public": (patch) =>
    "The control port is this node's root of trust — it can POST /key. " +
    "It must not be reached by binding [control] to a public address. Bind to " +
    String(patch["control.bind"]) +
    " anyway?",
  "listener.auth.off": () =>
    'auth = "off" with no allowed nodes admits every caller on the internet. Turn authentication off anyway?',
  "dtmf.enabled": () => "DTMF commands let any connected member link and unlink this node. Enable them?",
  "bridge.include_local_radio": () => "This puts the local microphone on the air. Include the local radio?",
  "announce.cw_keys_when_idle": () =>
    "The transmitter will key to send a CW ID even when nothing is connected. Allow that?",
  "discord.mic_timeout.off": () =>
    "Without a hard time-out an open mic stays open until the holder unkeys. Remove the time-out?",
  "discord.approvers.remove": () =>
    "Removing an approver leaves their slash commands registered in Discord until you remove them there. Remove anyway?",
  "m17.codec_dirs": (patch) =>
    "At its next start the node will load and run a libcodec2 it finds in " +
    (patch["m17.codec_dirs"] || []).join(", ") +
    ". List only directories nobody else can write to. Load code from there?",
  "vocoder.trusted_serials": (patch) =>
    "At its next start the node may open the USB device with serial " +
    newSerials(patch).join(", ") +
    " as a vocoder. Opening a radio's CAT/PTT cable keys its transmitter, and this page cannot check — astar-server vocoder add can. Trust it anyway?",
  "announce.tts.binary": (patch) =>
    "The node will run " +
    String(patch["announce.tts.binary"]) +
    " for every spoken announcement. Name only a program nobody else can write to. Run it?",
};

/// The serials a patch adds to [vocoder] trusted_serials, for the question.
function newSerials(patch) {
  const had = disk("vocoder.trusted_serials") || [];
  return (patch["vocoder.trusted_serials"] || []).filter((s) => !had.includes(s));
}

/// A sentence with no field to sit under — the oracle's, a 413, a 500, the
/// node being away — goes at the card's foot, replacing only the foot's own
/// previous note (the notes pinned under fields are left alone). With no
/// card (applyButton posts without a card) it goes directly after the
/// #conflict banner at the top of the page, replacing only the previous
/// note put there by this function — never a sweep of <main>, which would
/// delete every card's pinned field errors.
function footNote(card, text) {
  const save = card ? document.getElementById("save-" + card) : null;
  const p = el("p", "note err", text);
  if (save) {
    const foot = save.closest(".card-foot") || save.parentElement;
    for (const old of foot.querySelectorAll(":scope > .note.err")) old.remove();
    foot.append(p);
  } else {
    const conflict = document.getElementById("conflict");
    const prev = conflict.nextElementSibling;
    if (prev && prev.dataset.footNote) prev.remove();
    p.dataset.footNote = "1";
    conflict.after(p);
  }
  announce(text);
}

function saveButton(card) {
  return card ? document.getElementById("save-" + card) : null;
}

async function saveCard(card, acks) {
  // First, before the empty-patch return: a
  // pinned sentence describes the last edit, and this Save is the operator
  // moving past it — including by reverting the refused control, which is
  // exactly the empty patch.
  clearErrors(card);
  const patch = collectPatch(card);
  const btn = saveButton(card);
  if (!Object.keys(patch).length) {
    dirty.delete(card);
    if (btn) btn.disabled = true;
    renderBadges();
    announce("Nothing to save");
    return;
  }
  if (btn) btn.disabled = true;
  await postPatch(patch, acks || [], card);
}

/// POST /settings. 200: the node wrote the file — take its etag, clear the
/// card's dirty mark and its pinned notes, and refetch (the redraw leaves
/// the Save button disabled). 400 with a field: the card stays dirty and
/// its button comes back; the sentence is pinned under that field and,
/// when the node also names an acknowledgement, asked inline and re-posted
/// with the key appended (a second key asks again, with both). 409: the
/// file moved under the page; the banner says to reload, nothing is
/// retried, and the edits stay. Anything else lands at the card's foot.
async function postPatch(patch, acks, card) {
  const keys = acks || [];
  const btn = saveButton(card);
  if (card) clearErrors(card);
  // Off for the whole POST, whoever called: saveCard has already done it,
  // but the acknowledged re-post arrives here from the confirm's Yes with
  // the button re-enabled by the 400. The 200 and failure branches below
  // set its final state.
  if (btn) btn.disabled = true;
  const r = await postJson("settings", { etag: etag, patch: patch, acknowledge: keys });
  if (r.status === 200) {
    if (typeof r.body.etag === "string") etag = r.body.etag;
    if (card) dirty.delete(card);
    if (btn) btn.disabled = true;
    const applied = (r.body.applied || []).length;
    const later = (r.body.on_restart || []).length;
    let said = "Saved";
    if (applied) said += " — " + applied + " applied now";
    if (later) said += " — " + later + " waiting for a restart";
    announce(said);
    await fetchState();
    if (typeof r.body.apply_error === "string") {
      footNote(card, "Saved, but the running node refused it: " + r.body.apply_error);
    }
    return;
  }
  // Every failure path: the edits are still the operator's, so the card
  // stays dirty and the button is usable again — unless the file is not
  // writable, in which case Save stays off exactly as markDirty leaves it.
  if (card) dirty.add(card);
  if (btn) btn.disabled = !writable();
  renderBadges();
  if (r.status === 409 && r.body.conflict === true) {
    // showConflict: the banner's text, its Reload button and the
    // announcement — one spelling of the conflict, not two.
    showConflict();
    return;
  }
  if (r.status === 400 && typeof r.body.field === "string") {
    // Two of the seven acknowledged paths (announce.cw_keys_when_idle,
    // discord.mic_timeout_secs) live in a closed Advanced disclosure. A
    // sentence pinned into a closed <details> is not rendered and the
    // focus() pinError ends with is a no-op there, so the disclosure is
    // opened before the pin — whatever pinError itself does about it.
    const row = fieldRow(r.body.field);
    const det = row ? row.closest("details.advanced") : null;
    if (det) det.open = true;
    pinError(r.body.field, r.body.error, r.body.acknowledge);
    const key = r.body.acknowledge;
    if (typeof key === "string" && ACK_QUESTIONS[key]) {
      const anchor = row || saveButton(card || "advanced");
      openConfirm(anchor, ACK_QUESTIONS[key](patch), () => postPatch(patch, keys.concat([key]), card));
    }
    return;
  }
  footNote(card, errorText(r));
}

// ---- restart -----------------------------------------------------------------

const BACK_POLL_MS = 1000;
const BACK_TIMEOUT_MS = 60000;
const RESTART_CONFIRM = "Restart now? This node is on a net — the calls and links will drop.";
const NOT_BACK = "The node did not come back — a config that fails at startup exits non-zero; check the log";

/// Light-poll safe: text only. The bar is always shown, so the restart
/// button is reachable whether or not anything waits.
function renderRestartBar() {
  const n = ((state.pending && state.pending.restart) || []).length;
  const summary = document.getElementById("restart-summary");
  if (n === 0) summary.textContent = "Nothing is waiting for a restart.";
  else if (n === 1) summary.textContent = "1 change waiting for a restart";
  else summary.textContent = n + " changes waiting for a restart";
  const why = state.restart && state.restart.why;
  document.getElementById("restart-why").textContent = why ? "On a net now: " + why + "." : "";
}

async function requestRestart(disconnect) {
  const btn = document.getElementById("btn-restart");
  const status = document.getElementById("restart-status");
  btn.disabled = true;
  status.className = "note";
  status.textContent = "";
  const r = await postJson("settings/restart", disconnect ? { disconnect: true } : {});
  if (r.status === 200) {
    pollBack();
    return;
  }
  btn.disabled = false;
  if (r.status === 409 && r.body.busy === true) {
    // The button was disabled for the POST, and a disabled button drops
    // focus to <body>. Focus it again first: openConfirm records the focused
    // element as the place Esc and Cancel return to.
    btn.focus({ preventScroll: true });
    openConfirm(btn.closest(".row-actions"), RESTART_CONFIRM, () => requestRestart(true));
    return;
  }
  status.className = "note err";
  status.textContent = errorText(r);
  announce(status.textContent);
}

/// The node is going away and coming back with the same pid. Ask /status
/// once a second, each ask bounded so a socket the old process is still
/// holding cannot stall the loop; the first 200 is the new process — the
/// old one stops answering within its serve tick, long before the first
/// poll. Sixty seconds without one means it exited instead, which is what
/// a config that fails at startup does.
function pollBack() {
  const btn = document.getElementById("btn-restart");
  const status = document.getElementById("restart-status");
  status.className = "note";
  status.textContent = "Restarting…";
  announce("Restarting");
  const started = Date.now();
  const tick = async () => {
    let ok = false;
    try {
      const res = await fetch("status", {
        cache: "no-store",
        signal: AbortSignal.timeout(BACK_POLL_MS - 100),
      });
      ok = res.ok;
    } catch (_e) {
      ok = false;
    }
    if (ok) {
      status.textContent = "Back";
      announce("Back");
      btn.disabled = false;
      await fetchState();
      return;
    }
    if (Date.now() - started >= BACK_TIMEOUT_MS) {
      status.className = "note err";
      status.textContent = NOT_BACK;
      announce(NOT_BACK);
      btn.disabled = false;
      return;
    }
    window.setTimeout(tick, BACK_POLL_MS);
  };
  window.setTimeout(tick, BACK_POLL_MS);
}

document.getElementById("btn-restart").addEventListener("click", () => requestRestart(false));

// The checklist's "Restart to register" step points at the bar; the jump
// lands focus on the button so the next key press is the restart.
document.addEventListener("click", (e) => {
  const a = e.target.closest ? e.target.closest('a[href="#restart-bar"]') : null;
  if (!a) return;
  e.preventDefault();
  const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  document.getElementById("restart-bar").scrollIntoView({ behavior: reduce ? "auto" : "smooth", block: "center" });
  document.getElementById("btn-restart").focus({ preventScroll: true });
});

// ---- card: M17 ---------------------------------------------------------------
//
// [m17] as the operator sees it. The switch and the reflector table are read
// at the next dial, so they apply when saved; the directory and the Codec 2
// directories wait for a restart, and adding a Codec 2 directory asks first,
// because the node loads code from it. There is no callsign here and there
// never will be: the reflector sees whoever dials. Reflector names and
// addresses arrive off the wire like everything else and reach the DOM only
// through createElement and textContent.

const M17_HINT_OFF =
  "Off: nobody can dial M17 from Discord or POST /m17. A dial presents the dialler's callsign to someone else's reflector.";
const M17_HINT_ON = "Applies to the next dial. A monitor that is already up stays up until it is hung up.";
// The monitor never sends voice to a reflector, but it contributes to the
// conference: whatever the reflector carries is relayed to every AllStar node
// and EchoLink station on this node, and a linked radio transmits it. The same
// sentence as M17_RELAY_WARNING in m17_relay.rs (a test pins that). The count
// of who is connected right now comes from /status, which writes it.
const M17_RELAY_WARNING =
  "The node never sends voice to an M17 reflector, but it relays what it hears there to every AllStar node and EchoLink station connected to it, and a connected node with a radio puts that on the air. Link only reflectors whose traffic you would rebroadcast, and mind your licence's rules on retransmission.";

function m17RelayWarning() {
  const p = el("p", "banner warn", M17_RELAY_WARNING);
  p.id = "m17-relay-warning";
  p.setAttribute("role", "note");
  fetch("status", { cache: "no-store", headers: { Accept: "application/json" } })
    .then((res) => (res.ok ? res.json() : null))
    .then((st) => {
      if (st && st.m17_relay && st.m17_relay.warning) p.textContent = st.m17_relay.warning;
    })
    .catch(() => {});
  return p;
}
// A [m17.reflectors] entry's wire path is m17.reflectors."<NAME>".
const REFLECTOR_PREFIX = 'm17.reflectors."';

/// [m17] is the opt-in, and the section is on by default once it exists.
/// So a save that would create it — a reflector or a directory added on a
/// node without one — carries enabled = false unless the switch itself is
/// part of the save: only the switch ever turns M17 on. The node refuses a
/// save that creates the section without saying (field m17.enabled); this
/// keeps the page from ever meeting that refusal.
function m17Patch(patch) {
  if (disk("m17.enabled") === undefined && Object.keys(patch).length > 0 && !("m17.enabled" in patch)) {
    patch["m17.enabled"] = false;
  }
  return patch;
}

/// The node's own rule for a reflector name, checked beside the Add button
/// because the node's sentence would name a row that does not exist yet.
function reflectorNameProblem(name) {
  if (!/^[A-Z0-9-]*$/.test(name)) return "A reflector name is letters, digits and '-' only.";
  if (name.length < 3 || name.length > 8) return "A reflector name is 3 to 8 characters.";
  return null;
}

/// The node's parse_host_port, in its words, for the same reason. The node
/// checks the address again when it saves.
function addressProblem(text) {
  const i = text.indexOf(":");
  const host = i < 0 ? text : text.slice(0, i);
  const port = i < 0 ? null : text.slice(i + 1);
  if (host === "") return "host is empty";
  if (port !== null && port.includes(":")) return "IPv6 literals are not supported";
  if (/\s/.test(host)) return "host contains whitespace";
  if (port !== null && !(/^[0-9]+$/.test(port) && Number(port) >= 1 && Number(port) <= 65535)) {
    return "bad port " + JSON.stringify(port);
  }
  return null;
}

/// Add a reflector: saved at once with the card's other edits, like a link.
/// The name is uppercased so the file stays uniform and two spellings of
/// one name never meet. Neither input is bound; both are data-no-patch.
function reflectorAddRow() {
  const acts = el("div", "row-actions");
  const name = el("input");
  name.type = "text";
  name.placeholder = "M17-XYZ";
  name.autocapitalize = "characters";
  name.spellcheck = false;
  name.setAttribute("aria-label", "Reflector name to add");
  name.dataset.noPatch = "1";
  const addr = el("input");
  addr.type = "text";
  addr.placeholder = "host:port";
  addr.spellcheck = false;
  addr.setAttribute("aria-label", "Reflector address, host or host:port");
  addr.dataset.noPatch = "1";
  const add = el("button", "btn", "Add");
  add.type = "button";
  const note = el("p", "note");
  acts.append(name, addr, add, note);
  for (const c of [name, addr]) {
    // Typing a new reflector is not an edit of a bound field.
    c.addEventListener("input", (e) => e.stopPropagation());
    c.addEventListener("change", (e) => e.stopPropagation());
  }
  const refuse = (text, focus) => {
    note.className = "note err";
    note.textContent = text;
    announce(text);
    focus.focus();
  };
  add.addEventListener("click", async () => {
    const n = name.value.trim().toUpperCase();
    const bad = reflectorNameProblem(n);
    if (bad) return refuse(bad, name);
    const a = addr.value.trim();
    const wrong = addressProblem(a);
    if (wrong) return refuse(REFLECTOR_PREFIX + n + '": ' + wrong, addr);
    const patch = collectPatch("m17");
    patch[REFLECTOR_PREFIX + n + '"'] = a;
    // Off while the POST is out, as Add on a link is: a second click would
    // re-send the same etag and raise a false conflict banner.
    add.disabled = true;
    await postPatch(m17Patch(patch), [], "m17");
    add.disabled = false;
  });
  return acts;
}

/// One row per reflector in node.toml: its address (a bound field) and
/// Remove. Names are shown as the file spells them.
function reflectorsTable(card) {
  const box = el("div");
  const names = Object.keys((state && state.disk) || {})
    .filter((k) => k.startsWith(REFLECTOR_PREFIX))
    .map((k) => k.slice(REFLECTOR_PREFIX.length, -1))
    .sort();
  if (!names.length) {
    box.append(el("p", "blank", "No reflectors of this node's own. Add a name and its address, or use the bundled directory."));
  }
  for (const n of names) {
    const path = REFLECTOR_PREFIX + n + '"';
    box.append(field(card, path, { label: n, kind: "text", required: true, placeholder: "host:port" }));
    const acts = el("div", "row-actions");
    const rm = el("button", "btn quiet-danger", "Remove " + n);
    rm.type = "button";
    acts.append(rm);
    rm.addEventListener("click", () => {
      openConfirm(acts, "Remove " + n + " from this node's reflectors? A monitor already listening to it stays up.", async () => {
        if (inFlight > 0) return;
        // Same path as the row's own address edit, so null simply wins.
        const patch = collectPatch(card);
        patch[path] = null;
        rm.disabled = true;
        await postPatch(m17Patch(patch), [], card);
        rm.disabled = false;
      });
    });
    box.append(acts);
  }
  box.append(reflectorAddRow());
  return box;
}

function renderCardM17() {
  const card = "m17";
  if (dirty.has(card)) return;
  clearMounted(card);

  const main = el("div");
  main.append(
    m17RelayWarning(),
    field(card, "m17.enabled", {
      label: "Listen to M17 reflectors",
      kind: "bool",
      hint: disk("m17.enabled") === true ? M17_HINT_ON : M17_HINT_OFF,
    }),
    el("h3", null, "Reflectors"),
    reflectorsTable(card),
    field(card, "m17.directory", {
      label: "Directory",
      kind: "text",
      placeholder: "bundled",
      hint: "bundled — the dated reflector list built into this node — or an absolute path to a list of your own. Empty: only the reflectors above.",
    }),
  );
  mount(card, main, false);

  const adv = el("div");
  adv.append(
    field(card, "m17.codec_dirs", {
      label: "Codec 2 directories",
      kind: "chips",
      advanced: true,
      placeholder: "/opt/astar/lib",
      empty: "the system's own",
      hint: "The node loads libcodec2 from these directories at its next start. Absolute paths only, and only directories nobody else can write to.",
    }),
  );
  mount(card, adv, true);
}

// ---- card: Vocoders ----------------------------------------------------------
//
// [vocoder] trusted_serials: the only AMBE sticks (DVstick / ThumbDV) the node
// may open, by USB serial number. A stick and a radio's CAT/PTT cable look
// the same on USB, so the node never scans; adding a serial asks first, and
// the safe way to add one is the CLI, which probes it. Serials arrive off the
// wire and reach the DOM only through createElement and textContent.

const VOCODER_HINT =
  "USB serial numbers of the sticks this node may open. Empty: the node opens no vocoder at all. Takes effect at the next start.";
const VOCODER_HOWTO =
  "To add a stick safely, run astar-server vocoder add <serial> --config <node.toml> on the node: it probes the stick with DTR/RTS held low, and writes the serial here only if an AMBE-3000 answered. astar-server vocoder list shows the candidates without opening anything. A serial typed here is trusted without that check.";

// [vocoder] dstar (astar-dstar-perens): D-Star only. System Fusion, NXDN and
// DMR have no such key, because their AMBE+2 vocoder is still patented.
const VOCODER_DSTAR_HINT =
  "Which vocoder D-Star uses. Software needs no stick but only exists in an astar-server built with the dstar-soft-ambe feature; the node refuses to start with it otherwise. System Fusion, NXDN and DMR always use the dongle: their AMBE+2 vocoder is covered by US patent 8,359,197 until 2028-05-20. Takes effect at the next start.";

function renderCardVocoder() {
  const card = "vocoder";
  if (dirty.has(card)) return;
  clearMounted(card);

  const main = el("div");
  main.append(
    field(card, "vocoder.trusted_serials", {
      label: "Trusted sticks",
      kind: "chips",
      placeholder: "USB serial",
      empty: "none — no vocoder is opened",
      hint: VOCODER_HINT,
    }),
    el("p", "hint", VOCODER_HOWTO),
    field(card, "vocoder.dstar", {
      label: "D-Star vocoder",
      kind: "select",
      options: [
        { value: "", label: "default (dongle)" },
        { value: "dongle", label: "Dongle" },
        { value: "software", label: "Software (Bruce Perens' implementation)" },
      ],
      hint: VOCODER_DSTAR_HINT,
    }),
  );
  mount(card, main, false);
}

// ---- the public page: [web] and [stream] -------------------------------------

function renderCardPublic() {
  const card = "public";
  if (dirty.has(card)) return;
  clearMounted(card);

  const main = el("div");
  main.append(
    field(card, "web.public_bind", {
      label: "Public bind",
      kind: "text",
      placeholder: "0.0.0.0:443",
      hint: "Where the listener page listens. Empty: no public page at all. Admin pages are served under /admin only to people who sign in.",
    }),
    field(card, "web.public_hostnames", {
      label: "Hostnames",
      kind: "chips",
      placeholder: "{node}.example.net",
      empty: "from [web.dns]: {node}.<domain>",
      hint: "The names the page answers to and its certificate covers. {node} becomes this node's number.",
    }),
    field(card, "web.acme_email", {
      label: "Certificate email",
      kind: "text",
      placeholder: "optional",
      hint: "Let's Encrypt writes here before a certificate expires.",
    }),
    el("h3", null, "DNS provider"),
    field(card, "web.dns.provider", { label: "Provider", kind: "select", options: ["porkbun"] }),
    field(card, "web.dns.domain", {
      label: "Domain",
      kind: "text",
      placeholder: "example.net",
      hint: "The zone registered at the provider, with API access turned on for it.",
    }),
    field(card, "web.dns.host", {
      label: "Host",
      kind: "text",
      placeholder: "{node}",
      hint: "The subdomain when Hostnames is empty. {node} is the node number; empty text means the domain itself.",
    }),
    field(card, "web.dns.ddns", {
      label: "Dynamic DNS",
      kind: "bool",
      hint: "Keep an A record for each hostname pointed at this box's public IPv4 address.",
    }),
    credentialRow("porkbun_pk", "Porkbun API key"),
    credentialRow("porkbun_sk", "Porkbun secret API key"),
    el("p", "hint", "From porkbun.com/account/api (pk1_… and sk1_…). Turn on API Access for the domain too. Read at start-up."),
    el("h3", null, "Sign-in"),
    field(card, "web.admin_on_public", {
      label: "Sign-in on this page",
      kind: "bool",
      hint: "Serve /admin and sign-in on the public page. Needs a build with the auth feature, and HTTPS.",
    }),
    field(card, "web.anonymous_status", {
      label: "Open to everyone",
      kind: "bool",
      hint: "On: anyone can listen. Off: the listener page and its streams need a signed-in viewer or admin.",
    }),
    el("h3", null, "Live audio"),
    field(card, "stream.enabled", {
      label: "Stream link audio",
      kind: "bool",
      hint: "Off by default. Never in parrot mode, whatever this says. Announcements and IDs are never in the stream.",
    }),
    field(card, "stream.max_listeners", { label: "Most listeners", kind: "number", min: 1 }),
  );
  mount(card, main, false);

  const adv = el("div");
  adv.append(
    field(card, "web.acme_challenge", {
      label: "ACME challenge",
      kind: "select",
      options: ["", "http-01", "dns-01"],
      advanced: true,
      hint: "Empty: dns-01 with a DNS provider, else http-01 (which needs port 80).",
    }),
    field(card, "web.acme_http_bind", {
      label: "Port-80 bind",
      kind: "text",
      advanced: true,
      placeholder: "0.0.0.0:80",
      hint: "http-01 answers and the redirect to HTTPS. Under dns-01, empty means no port-80 listener.",
    }),
    field(card, "web.acme_staging", {
      label: "Staging CA",
      kind: "bool",
      advanced: true,
      hint: "Let's Encrypt's test CA: untrusted certificates, generous limits. For trying things out.",
    }),
    field(card, "web.dns.ddns_interval_secs", { label: "DNS check interval (s)", kind: "number", min: 60, advanced: true }),
    field(card, "web.dns.ttl", { label: "Record TTL (s)", kind: "number", min: 600, advanced: true }),
    field(card, "web.rp_id", {
      label: "Passkey domain",
      kind: "text",
      advanced: true,
      placeholder: "each hostname",
      hint: "Empty: a passkey works on the hostname it was made on. A parent domain you own (example.org) lets passkeys work on every hostname under it.",
    }),
    field(card, "web.session_idle_days", { label: "Sign-in idle days", kind: "number", min: 1, advanced: true, hint: "1–30. A browser unused this long signs in again." }),
    field(card, "web.session_max_days", { label: "Sign-in most days", kind: "number", min: 1, advanced: true, hint: "Up to 90. Every browser signs in again after this long." }),
    field(card, "web.step_up_minutes", { label: "Confirm-it's-you minutes", kind: "number", min: 2, advanced: true, hint: "2–60. Secrets, settings saves, restarts and user changes need a credential used this recently." }),
    field(card, "stream.mp3_kbps", { label: "MP3 kbit/s", kind: "number", min: 8, advanced: true, hint: "8, 16, 24, 32, 40, 48 or 64." }),
    field(card, "stream.opus_kbps", { label: "Opus kbit/s", kind: "number", min: 6, advanced: true }),
  );
  mount(card, adv, true);
}

// ---- recordings: [recording] -----------------------------------------------

function renderCardRecording() {
  const card = "recording";
  if (dirty.has(card)) return;
  clearMounted(card);

  const main = el("div");
  main.append(
    field(card, "recording.enabled", {
      label: "Record every transmission",
      kind: "bool",
      hint: "Off by default. Recordings include everything the conference carries, Discord too.",
    }),
    field(card, "recording.dir", {
      label: "Folder",
      kind: "text",
      placeholder: "/var/lib/astar/recordings",
    }),
    field(card, "recording.max_bytes", {
      label: "Keep at most",
      kind: "text",
      placeholder: "2 GB",
      hint: "Oldest recordings are deleted first. For example 2 GB.",
    }),
    field(card, "recording.max_age_days", { label: "Keep for days", kind: "number", min: 1, placeholder: "30" }),
  );
  mount(card, main, false);

  const adv = el("div");
  adv.append(
    field(card, "recording.frames", {
      label: "Save codec frames for unamblify",
      kind: "bool",
      advanced: true,
    }),
  );
  mount(card, adv, true);
}

// ---- keyboard and the unload guard ------------------------------------------

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && confirmOpen) {
    e.preventDefault();
    closeConfirm();
    return;
  }
  if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && (e.key === "s" || e.key === "S")) {
    // Always swallowed: the browser's Save Page dialog over a settings form
    // is never what was meant. The card is found by its id prefix — the
    // ids are the contract, the element type is not.
    e.preventDefault();
    const active = document.activeElement;
    const sec = active && active.closest ? active.closest("[id^='card-']") : null;
    if (!sec) return;
    const card = sec.id.slice("card-".length);
    // The same gate markDirty puts on the Save button: the shortcut bypasses
    // the button, and on an unwritable node.toml it must not post a save the
    // button itself refuses to offer.
    // And the button's own in-flight state: saveCard disables it before the
    // POST and dirty stays set until the 200, so without this a quick second
    // press would re-send the same patch with the same etag and its 409
    // would raise a false conflict banner over a save that succeeded.
    const btn = saveButton(card);
    if (dirty.has(card) && writable() && inFlight === 0 && btn && !btn.disabled) saveCard(card, []);
  }
});

window.addEventListener("beforeunload", (e) => {
  if (dirty.size === 0) return;
  e.preventDefault();
  // The browser's own sentence; a custom one is ignored by every current
  // browser, and this page does not pretend otherwise.
  e.returnValue = "";
});

// ---- wiring and the poll ---------------------------------------------------

for (const name of CARD_NAMES) {
  $("save-" + name).addEventListener("click", () => saveCard(name));
}

function tick() {
  if (document.hidden || inFlight > 0) return;
  refreshLight();
}

fetchState();
window.setInterval(tick, POLL_MS);
