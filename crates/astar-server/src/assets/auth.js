// astar admin sign-in: what the sign-in, invite, account and users pages
// share, and what the node, settings and Discord pages borrow — the user
// chip, the "Confirm it's you" sheet, and the console's first-admin banner.
//
// Every string that came from the node (a name, a nickname, a user agent)
// is put on the page with textContent. Nothing here, or in any page script,
// parses HTML.
"use strict";

(function () {
  const API = "/auth/";

  function el(tag, cls, text) {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined && text !== null) n.textContent = String(text);
    return n;
  }

  // ---- WebAuthn's binary fields, to and from the node's Base64url JSON -----

  function b64uToBuf(s) {
    const b64 = String(s).replace(/-/g, "+").replace(/_/g, "/");
    const padded = b64 + "===".slice((b64.length + 3) % 4);
    const bin = atob(padded);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out.buffer;
  }

  function bufToB64u(buf) {
    const bytes = new Uint8Array(buf);
    let bin = "";
    for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
    return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  }

  function creationOptions(json) {
    const pk = Object.assign({}, json.publicKey);
    pk.challenge = b64uToBuf(pk.challenge);
    pk.user = Object.assign({}, pk.user, { id: b64uToBuf(pk.user.id) });
    if (Array.isArray(pk.excludeCredentials)) {
      pk.excludeCredentials = pk.excludeCredentials.map((c) => Object.assign({}, c, { id: b64uToBuf(c.id) }));
    }
    return { publicKey: pk };
  }

  function requestOptions(json, mediation, signal) {
    const pk = Object.assign({}, json.publicKey);
    pk.challenge = b64uToBuf(pk.challenge);
    if (Array.isArray(pk.allowCredentials)) {
      pk.allowCredentials = pk.allowCredentials.map((c) => Object.assign({}, c, { id: b64uToBuf(c.id) }));
    }
    const opts = { publicKey: pk };
    if (mediation) opts.mediation = mediation;
    if (signal) opts.signal = signal;
    return opts;
  }

  function credentialJSON(cred) {
    const r = cred.response;
    const out = {
      id: cred.id,
      rawId: bufToB64u(cred.rawId),
      type: cred.type,
      extensions: typeof cred.getClientExtensionResults === "function" ? cred.getClientExtensionResults() : {},
      response: { clientDataJSON: bufToB64u(r.clientDataJSON) },
    };
    if (r.attestationObject) {
      out.response.attestationObject = bufToB64u(r.attestationObject);
      if (typeof r.getTransports === "function") out.response.transports = r.getTransports();
    } else {
      out.response.authenticatorData = bufToB64u(r.authenticatorData);
      out.response.signature = bufToB64u(r.signature);
      out.response.userHandle = r.userHandle ? bufToB64u(r.userHandle) : null;
    }
    return out;
  }

  function webauthnAvailable() {
    return !!(window.PublicKeyCredential && navigator.credentials);
  }

  /// What a browser's refusal means to a person.
  function ceremonyError(e) {
    const name = e && e.name;
    if (name === "NotAllowedError") return "Cancelled, or the authenticator didn't answer in time.";
    if (name === "InvalidStateError") return "That authenticator is already registered here.";
    if (name === "SecurityError") return "This page's address can't use passkeys; open it by name over HTTPS, or localhost.";
    if (name === "AbortError") return "";
    return "The browser couldn't finish that.";
  }

  // ---- the API --------------------------------------------------------------

  /// Call /auth/<path>. A 401 reauth_required opens the step-up sheet and
  /// retries once. Resolves to { status, data }; status 0 is "unreachable".
  async function api(method, path, body, opts) {
    const init = { method: method, headers: { Accept: "application/json" }, credentials: "same-origin", cache: "no-store" };
    if (method !== "GET") {
      init.headers["Content-Type"] = "application/json";
      init.body = JSON.stringify(body || {});
    }
    let res;
    try {
      res = await fetch(API + path, init);
    } catch (_e) {
      return { status: 0, data: { message: "Can't reach the node." } };
    }
    let data = {};
    try {
      data = await res.json();
    } catch (_e) {
      data = {};
    }
    if (!data || typeof data !== "object") data = {};
    if (res.status === 401 && data.error === "reauth_required" && !(opts && opts.noStepUp)) {
      if (await stepUp(opts && opts.why)) return api(method, path, body, { noStepUp: true });
    }
    return { status: res.status, data: data };
  }

  function message(r, fallback) {
    if (r.data && typeof r.data.message === "string" && r.data.message) return r.data.message;
    if (r.data && typeof r.data.error === "string") return r.data.error;
    return fallback || "The node refused that (" + r.status + ").";
  }

  /// begin → navigator.credentials.get → finish.
  async function assert(beginPath, finishPath, beginBody, mediation, signal) {
    const b = await api("POST", beginPath, beginBody || {});
    if (b.status !== 200) return b;
    let cred;
    try {
      cred = await navigator.credentials.get(requestOptions(b.data.options, mediation, signal));
    } catch (e) {
      return { status: -1, data: { message: ceremonyError(e) }, begin: b.data };
    }
    const f = await api("POST", finishPath, { credential: credentialJSON(cred) });
    f.begin = b.data;
    return f;
  }

  /// begin → navigator.credentials.create → finish.
  async function register(beginPath, finishPath, beginBody, finishExtra) {
    const b = await api("POST", beginPath, beginBody || {});
    if (b.status !== 200) return b;
    let cred;
    try {
      cred = await navigator.credentials.create(creationOptions(b.data.options));
    } catch (e) {
      return { status: -1, data: { message: ceremonyError(e) } };
    }
    return api("POST", finishPath, Object.assign({ credential: credentialJSON(cred) }, finishExtra || {}));
  }

  // ---- QR codes, as DOM cells ------------------------------------------------

  function qr(rows, label) {
    const box = el("div", "qr");
    box.setAttribute("role", "img");
    box.setAttribute("aria-label", label || "QR code");
    for (const row of rows || []) {
      const r = el("div", "qr-row");
      for (const ch of row) r.append(el("span", ch === "1" ? "qr-cell on" : "qr-cell"));
      box.append(r);
    }
    return box;
  }

  async function copy(text, button) {
    try {
      await navigator.clipboard.writeText(text);
      if (button) {
        const was = button.textContent;
        button.textContent = "Copied";
        window.setTimeout(() => {
          button.textContent = was;
        }, 1400);
      }
    } catch (_e) {
      /* the field is user-select: all; a manual copy still works */
    }
  }

  // ---- "Confirm it's you" -----------------------------------------------------

  let sheetOpen = null;

  /// Open the step-up sheet. Resolves true once a credential was used.
  function stepUp(why) {
    if (sheetOpen) return sheetOpen;
    sheetOpen = new Promise((resolve) => {
      const back = document.activeElement;
      const scrim = el("div", "sheet-backdrop");
      const sheet = el("section", "card sheet");
      sheet.setAttribute("role", "dialog");
      sheet.setAttribute("aria-modal", "true");
      const title = el("h2", null, "Confirm it's you");
      title.id = "stepup-title";
      sheet.setAttribute("aria-labelledby", title.id);
      const lead = el("p", "lead", why || "This needs a sign-in method used in the last few minutes.");
      const msg = el("p", "auth-msg err");
      msg.setAttribute("role", "alert");

      const done = (ok) => {
        document.removeEventListener("keydown", onKey, true);
        scrim.remove();
        sheetOpen = null;
        if (back && back.isConnected) back.focus({ preventScroll: true });
        resolve(ok);
      };
      const onKey = (e) => {
        if (e.key === "Escape") {
          e.preventDefault();
          done(false);
        }
      };
      document.addEventListener("keydown", onKey, true);

      const keyBtn = el("button", "btn primary wide", "Use a passkey or security key");
      keyBtn.type = "button";
      const noPin = el("label", "check");
      const noPinBox = el("input");
      noPinBox.type = "checkbox";
      noPin.append(noPinBox, document.createTextNode("My key has no PIN"));
      keyBtn.addEventListener("click", async () => {
        keyBtn.disabled = true;
        msg.textContent = "";
        const r = await assert("stepup/begin", "stepup/finish", noPinBox.checked ? { pin: false } : {});
        keyBtn.disabled = false;
        if (r.status === 200) return done(true);
        msg.textContent = message(r, "That didn't work.");
      });

      const form = el("form", "auth-form");
      const label = el("label", null, "Or a code from your authenticator app");
      const codeIn = el("input", "code");
      codeIn.id = "stepup-code";
      label.htmlFor = codeIn.id;
      codeIn.autocomplete = "one-time-code";
      codeIn.inputMode = "numeric";
      codeIn.maxLength = 6;
      codeIn.placeholder = "123456";
      const codeBtn = el("button", "btn", "Confirm");
      codeBtn.type = "submit";
      const row = el("div", "auth-row");
      row.append(codeIn, codeBtn);
      form.append(label, row);
      form.addEventListener("submit", async (e) => {
        e.preventDefault();
        codeBtn.disabled = true;
        msg.textContent = "";
        const r = await api("POST", "stepup/totp", { code: codeIn.value }, { noStepUp: true });
        codeBtn.disabled = false;
        if (r.status === 200) return done(true);
        msg.textContent = message(r, "That code didn't work.");
        codeIn.select();
      });

      const cancel = el("button", "btn", "Cancel");
      cancel.type = "button";
      cancel.addEventListener("click", () => done(false));
      const actions = el("div", "row-actions");
      actions.append(cancel);

      if (webauthnAvailable()) sheet.append(title, lead, keyBtn, noPin, el("div", "divider", "or"), form, msg, actions);
      else sheet.append(title, lead, form, msg, actions);
      scrim.append(sheet);
      scrim.addEventListener("click", (e) => {
        if (e.target === scrim) done(false);
      });
      document.body.append(scrim);
      (webauthnAvailable() ? keyBtn : codeIn).focus();
    });
    return sheetOpen;
  }

  // ---- the user chip ------------------------------------------------------------

  async function signOut() {
    await api("POST", "signout", {});
    window.location.assign("/admin/signin");
  }

  /// Fill #auth-chip with "NAME · role ▾" and its menu. Resolves to /auth/me
  /// (or null: not signed in, or a build without sign-in).
  async function mountChip() {
    const slot = document.getElementById("auth-chip");
    const me = await api("GET", "me");
    if (me.status !== 200) return null;
    if (!slot) return me.data;
    const d = el("details", "auth-chip");
    const s = el("summary");
    if (me.data.console) {
      s.append(el("span", null, "Console"), el("span", "role", "this machine"));
    } else {
      s.append(el("span", null, me.data.name), el("span", "role", "· " + me.data.role));
    }
    const nav = el("nav");
    const link = (href, text) => {
      const a = el("a", null, text);
      a.href = href;
      return a;
    };
    if (!me.data.console) nav.append(link("/admin/account", "Account"));
    if (me.data.role === "admin") nav.append(link("/admin/users", "Users & invites"));
    if (!me.data.console) {
      const out = el("button", null, "Sign out");
      out.type = "button";
      out.addEventListener("click", signOut);
      nav.append(out);
    }
    d.append(s, nav);
    document.addEventListener("click", (e) => {
      if (d.open && !d.contains(e.target)) d.open = false;
    });
    slot.replaceChildren(d);
    return me.data;
  }

  // ---- the console's first-admin banner (the node's own page, on loopback) ----

  async function mountBootstrap(me) {
    const slot = document.getElementById("auth-bootstrap");
    if (!slot || !me || !me.console) return;
    const st = await api("GET", "status");
    if (st.status !== 200) return;
    if (me.signin_disabled) {
      const b = el("p", "banner err", "Sign-in is off: " + me.signin_disabled);
      slot.replaceChildren(b);
      slot.hidden = false;
      return;
    }
    if (st.data.admin_exists) return;
    const card = el("section", "card");
    const head = el("div", "card-head");
    head.append(el("h2", null, "No admin yet"));
    const lead = el(
      "p",
      "lead",
      "Nobody can sign in to this node from the web until someone has an admin invite. You're on the node itself, so you can make one.",
    );
    const btn = el("button", "btn primary", "Create an admin invite");
    btn.type = "button";
    const out = el("div", "panel");
    const msg = el("p", "auth-msg err");
    btn.addEventListener("click", async () => {
      btn.disabled = true;
      const r = await api("POST", "invites", { role: "admin" });
      btn.disabled = false;
      if (r.status !== 200) {
        msg.textContent = message(r);
        return;
      }
      out.replaceChildren(onceLink(r.data.link, r.data.qr, "Open this on the device you'll sign in with. It works once, for an hour."));
      btn.hidden = true;
    });
    const hint = el("p", "hint");
    hint.append(document.createTextNode("Or from a shell on the node: "), el("code", null, "astar-server admin invite --role admin"));
    card.append(head, lead, btn, out, msg, hint);
    slot.replaceChildren(card);
    slot.hidden = false;
  }

  /// A link shown once: the text, Copy, and its QR code.
  function onceLink(link, rows, note) {
    const box = el("div", "once");
    box.append(el("strong", null, "Shown once — copy it now"));
    const l = el("div", "link", link);
    const actions = el("div", "auth-row");
    const c = el("button", "btn", "Copy link");
    c.type = "button";
    c.addEventListener("click", () => copy(link, c));
    actions.append(c);
    box.append(l, actions);
    if (rows && rows.length) box.append(qr(rows, "QR code of the invite link"));
    if (note) box.append(el("p", "hint", note));
    return box;
  }

  // ---- times ----------------------------------------------------------------------

  function ago(unix) {
    if (!unix) return "never";
    const s = Math.max(0, Math.round(Date.now() / 1000 - unix));
    if (s < 60) return "just now";
    if (s < 3600) return Math.round(s / 60) + " min ago";
    if (s < 86400) return Math.round(s / 3600) + " h ago";
    if (s < 86400 * 14) return Math.round(s / 86400) + " days ago";
    return new Date(unix * 1000).toLocaleDateString();
  }

  function until(unix) {
    const s = Math.round(unix - Date.now() / 1000);
    if (s <= 0) return "expired";
    if (s < 3600) return "in " + Math.max(1, Math.round(s / 60)) + " min";
    if (s < 86400 * 2) return "in " + Math.round(s / 3600) + " h";
    return "in " + Math.round(s / 86400) + " days";
  }

  window.astarAuth = {
    el: el,
    api: api,
    message: message,
    assert: assert,
    register: register,
    requestOptions: requestOptions,
    credentialJSON: credentialJSON,
    ceremonyError: ceremonyError,
    webauthnAvailable: webauthnAvailable,
    stepUp: stepUp,
    qr: qr,
    copy: copy,
    onceLink: onceLink,
    mountChip: mountChip,
    signOut: signOut,
    ago: ago,
    until: until,
  };

  // Pages that carry the slots get them filled; nothing else happens on a
  // page without them, or on a build without sign-in (/auth/me is a 404).
  if (document.getElementById("auth-chip") || document.getElementById("auth-bootstrap")) {
    mountChip().then(mountBootstrap);
  }
})();
