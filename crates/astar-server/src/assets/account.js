// The account page: your sign-in methods (add, rename, remove) and your
// signed-in browsers.
"use strict";

const A = window.astarAuth;
const el = A.el;
const $ = (id) => document.getElementById(id);

let me = null;

function say(text) {
  $("msg").textContent = text || "";
}

const KIND = {
  passkey: { group: "Passkeys", glyph: "\u{1F511}︎" },
  security_key: { group: "Security keys", glyph: "\u{1F50C}︎" },
  totp: { group: "Authenticator apps", glyph: "⏱︎" },
};

/// A short device name from a User-Agent: enough to tell browsers apart.
function device(ua) {
  if (!ua) return "Unknown browser";
  const os = /iPhone|iPad/.test(ua) ? "iPhone/iPad" : /Android/.test(ua) ? "Android" : /Mac OS X/.test(ua) ? "Mac" : /Windows/.test(ua) ? "Windows" : /Linux/.test(ua) ? "Linux" : "";
  const br = /Edg\//.test(ua) ? "Edge" : /Firefox\//.test(ua) ? "Firefox" : /Chrome\//.test(ua) ? "Chrome" : /Safari\//.test(ua) ? "Safari" : "";
  const both = [br, os].filter(Boolean).join(" on ");
  return both || ua.slice(0, 60);
}

function nickEditor(c, nickBtn) {
  const input = el("input", "nick-edit");
  input.value = c.nickname;
  input.maxLength = 64;
  input.setAttribute("aria-label", "Nickname");
  nickBtn.replaceWith(input);
  input.focus();
  input.select();
  let done = false;
  const finish = async (save) => {
    if (done) return;
    done = true;
    const v = input.value.trim();
    if (save && v && v !== c.nickname) {
      const r = await A.api("POST", "credentials/" + encodeURIComponent(c.id) + "/rename", { nickname: v });
      if (r.status !== 200) say(A.message(r));
    }
    load();
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") finish(true);
    if (e.key === "Escape") finish(false);
  });
  input.addEventListener("blur", () => finish(true));
}

async function remove(c, row, btn) {
  const actions = btn.parentElement;
  const keep = Array.from(actions.childNodes);
  const q = el("span", "confirm-q", "Remove “" + c.nickname + "”?");
  const yes = el("button", "btn danger", "Remove");
  const no = el("button", "btn", "Cancel");
  yes.type = no.type = "button";
  actions.classList.add("confirming");
  actions.replaceChildren(q, yes, no);
  no.focus();
  no.addEventListener("click", () => {
    actions.classList.remove("confirming");
    actions.replaceChildren(...keep);
    btn.focus();
  });
  yes.addEventListener("click", async () => {
    yes.disabled = true;
    const r = await A.api("DELETE", "credentials/" + encodeURIComponent(c.id), {}, { why: "Removing a sign-in method needs a fresh confirmation." });
    if (r.status !== 200) {
      say(A.message(r));
      load();
      return;
    }
    // Tell the browser's passkey store this credential is gone, where it
    // can be told (the WebAuthn Signal API); the hint below covers the rest.
    const gone = r.data.removed || {};
    if (gone.webauthn_id && gone.rp_id && window.PublicKeyCredential && typeof PublicKeyCredential.signalUnknownCredential === "function") {
      try {
        await PublicKeyCredential.signalUnknownCredential({ rpId: gone.rp_id, credentialId: gone.webauthn_id });
      } catch (_e) {
        /* best effort */
      }
    }
    load();
  });
}

function credRow(c) {
  const row = el("div", "cred");
  row.append(el("span", "cred-glyph", (KIND[c.kind] || KIND.passkey).glyph));
  const who = el("div", "who");
  const nick = el("button", "nick", c.nickname);
  nick.type = "button";
  nick.title = "Rename";
  nick.addEventListener("click", () => nickEditor(c, nick));
  const meta = el("div", "meta");
  if (c.kind === "passkey") meta.append(el("span", "tag", c.synced ? "synced" : "this device"));
  if (c.rp_id) meta.append(el("span", c.works_here ? "" : "tag", "works on " + c.rp_id));
  else meta.append(el("span", null, "any hostname"));
  meta.append(el("span", null, "added " + A.ago(c.created)), el("span", null, "used " + A.ago(c.last_used)));
  who.append(nick, meta);
  const actions = el("div", "row-actions");
  const del = el("button", "btn quiet-danger", "Remove");
  del.type = "button";
  if (c.last) {
    del.disabled = true;
    del.title = "Your only way in: add another first.";
  } else if (c.last_key) {
    del.disabled = true;
    del.title = "Admins sign in with a passkey or security key: add another first.";
  }
  del.addEventListener("click", () => remove(c, row, del));
  actions.append(del);
  row.append(who, actions);
  return row;
}

async function loadMethods() {
  const r = await A.api("GET", "credentials");
  if (r.status !== 200) return say(A.message(r));
  const list = r.data.credentials || [];
  $("method-count").textContent = list.length;
  const box = $("methods");
  box.replaceChildren();
  for (const kind of ["passkey", "security_key", "totp"]) {
    const some = list.filter((c) => c.kind === kind);
    if (!some.length) continue;
    box.append(el("p", "group-title", KIND[kind].group));
    for (const c of some) box.append(credRow(c));
  }
}

async function loadSessions() {
  const r = await A.api("GET", "sessions");
  if (r.status !== 200) return;
  const ul = $("sessions");
  ul.replaceChildren();
  for (const s of r.data.sessions || []) {
    const li = el("li", "person");
    const who = el("div", "who");
    const line = el("div", "name-line");
    line.append(el("span", "name", device(s.user_agent)));
    if (s.current) line.append(el("span", "tag live", "This device"));
    const meta = el("div", "meta");
    meta.append(el("span", "mono", s.ip || "—"), el("span", null, "seen " + A.ago(s.last_seen)), el("span", null, "since " + A.ago(s.created)));
    who.append(line, meta);
    const actions = el("div", "row-actions");
    const out = el("button", "btn quiet-danger", s.current ? "Sign out" : "Sign out");
    out.type = "button";
    out.addEventListener("click", async () => {
      if (s.current) return A.signOut();
      const x = await A.api("DELETE", "sessions/" + encodeURIComponent(s.id), {});
      if (x.status !== 200) say(A.message(x));
      loadSessions();
    });
    actions.append(out);
    li.append(who, actions);
    ul.append(li);
  }
}

function load() {
  loadMethods();
  loadSessions();
}

async function addWebauthn(btn, kind) {
  btn.disabled = true;
  say("");
  const r = await A.register("credentials/webauthn/begin", "credentials/webauthn/finish", { kind: kind });
  btn.disabled = false;
  if (r.status !== 200) return say(A.message(r, "That didn't register."));
  load();
}

$("add-passkey").addEventListener("click", () => addWebauthn($("add-passkey"), "passkey"));
$("add-key").addEventListener("click", () =>
  addWebauthn($("add-key"), $("nopin").checked ? "security_key" : "pin_key"),
);

$("add-app").addEventListener("click", async () => {
  say("");
  const r = await A.api("POST", "credentials/totp/begin", {}, { why: "Adding a sign-in method needs a fresh confirmation." });
  if (r.status !== 200) return say(A.message(r));
  $("app-qr").replaceChildren(A.qr(r.data.qr, "QR code for your authenticator app"));
  $("app-secret").textContent = r.data.secret;
  $("app-code").value = "";
  $("app-panel").hidden = false;
  $("app-code").focus();
});
$("app-copy").addEventListener("click", () => A.copy($("app-secret").textContent, $("app-copy")));
$("app-cancel").addEventListener("click", () => {
  $("app-panel").hidden = true;
  $("app-secret").textContent = "";
  $("app-qr").replaceChildren();
});
$("app-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  $("app-go").disabled = true;
  const r = await A.api("POST", "credentials/totp/finish", { code: $("app-code").value.trim() });
  $("app-go").disabled = false;
  if (r.status !== 200) {
    say(A.message(r));
    $("app-code").select();
    return;
  }
  $("app-panel").hidden = true;
  $("app-secret").textContent = "";
  $("app-qr").replaceChildren();
  load();
});

$("reenrol").addEventListener("click", async () => {
  say("");
  const r = await A.api("POST", "reenrol", {}, { why: "Making a link that adds a sign-in method needs a fresh confirmation." });
  if (r.status !== 200) return say(A.message(r));
  $("reenrol-panel").replaceChildren(
    A.onceLink(
      r.data.link,
      r.data.qr,
      "Open it on the other device, or change the hostname in it to the new one, within 15 minutes. It adds a sign-in method to your account there.",
    ),
  );
  $("reenrol-panel").hidden = false;
});

$("others").addEventListener("click", async () => {
  const r = await A.api("POST", "sessions/others/revoke", {});
  if (r.status !== 200) say(A.message(r));
  loadSessions();
});

async function init() {
  me = await A.api("GET", "me");
  if (me.status === 401) return window.location.replace("/admin/signin?next=/admin/account");
  if (me.status !== 200) return say(A.message(me, "Can't reach the node."));
  if (me.data.console) {
    say("The console has no account. Sign in as a user to manage sign-in methods.");
    return;
  }
  me = me.data;
  $("title").textContent = me.display_name && me.display_name !== me.name ? me.display_name + " (" + me.name + ")" : me.name;
  $("subtitle").textContent = me.role === "admin" ? "admin · your sign-in methods and browsers" : "your sign-in methods and browsers";
  if (me.role !== "admin") $("back").href = "/";
  if (!me.host_has_webauthn && me.webauthn_elsewhere && me.webauthn_elsewhere.length) {
    $("host-banner").textContent =
      "Your passkeys work on " + me.webauthn_elsewhere.join(", ") + "; add one for " + me.host + " so you can use it here too.";
    $("host-banner").hidden = false;
  }
  if (me.totp_only && me.role === "admin") {
    // An admin from before codes stopped signing admins in: they still do,
    // until a passkey or key is added.
    $("totp-nudge").textContent =
      "Admins sign in with a passkey or security key, since codes can be guessed. Yours still sign you in until you add one below; please do.";
  }
  $("totp-nudge").hidden = !me.totp_only;
  $("admin-app-note").hidden = me.role !== "admin";
  if (!A.webauthnAvailable()) {
    for (const id of ["add-passkey", "add-key"]) $(id).hidden = true;
    $("nopin").closest("label").hidden = true;
  }
  load();
}

init();
