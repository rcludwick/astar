// The invite page: read the token from the fragment (it is never sent in a
// URL, so it reaches no log and no Referer), show what it is for, and enrol
// a passkey, a security key or (not for an admin) an authenticator app.
"use strict";

const A = window.astarAuth;
const $ = (id) => document.getElementById(id);

const token = window.location.hash.slice(1);
// Out of the address bar and the history entry as soon as it is read.
if (token) window.history.replaceState(null, "", window.location.pathname);

let info = null;

function say(text) {
  $("msg").textContent = text || "";
}

function dead(text) {
  $("enrol").hidden = true;
  $("dead-text").textContent = text;
  $("dead").hidden = false;
  $("title").textContent = "This invitation can't be used";
  $("who").textContent = "";
}

function names() {
  return {
    token: token,
    name: $("name").value.trim(),
    display_name: $("display").value.trim() || undefined,
  };
}

function choose(tile) {
  for (const id of ["t-passkey", "t-key", "t-app"]) $(id).classList.toggle("on", id === tile);
  $("key-panel").hidden = tile !== "t-key";
  $("app-panel").hidden = tile !== "t-app";
  say("");
}

function needName() {
  if ($("name").value.trim()) return false;
  say("Choose a name first: your callsign is ideal.");
  $("name").focus();
  return true;
}

function finished(r) {
  $("enrol").hidden = true;
  const admin = r.data.role === "admin";
  $("title").textContent = "Welcome, " + r.data.name;
  $("done-text").textContent = admin
    ? "You're signed in as an admin of this node."
    : "You're signed in. You can listen to this node.";
  const go = $("done-go");
  go.href = admin ? "/admin/" : "/";
  $("done").hidden = false;
  go.focus();
}

async function enrolWebauthn(kind) {
  if (needName()) return;
  const r = await A.register("invite/begin", "invite/finish", Object.assign({ kind: kind }, names()));
  if (r.status === 200) return finished(r);
  say(A.message(r, "That didn't register."));
}

$("t-passkey").addEventListener("click", async () => {
  choose("t-passkey");
  $("t-passkey").disabled = true;
  await enrolWebauthn("passkey");
  $("t-passkey").disabled = false;
});

$("t-key").addEventListener("click", () => choose("t-key"));
$("key-go").addEventListener("click", async () => {
  $("key-go").disabled = true;
  // A key with a PIN registers as a passkey that stays off the key's
  // limited resident slots (the PIN is its user verification); one without
  // is a possession-only security key.
  await enrolWebauthn($("nopin").checked ? "security_key" : "pin_key");
  $("key-go").disabled = false;
});

$("t-app").addEventListener("click", async () => {
  choose("t-app");
  if (needName()) {
    $("app-panel").hidden = true;
    return;
  }
  $("t-app").disabled = true;
  const r = await A.api("POST", "invite/totp/begin", names());
  $("t-app").disabled = false;
  if (r.status !== 200) {
    $("app-panel").hidden = true;
    say(A.message(r));
    return;
  }
  $("app-qr").replaceChildren(A.qr(r.data.qr, "QR code for your authenticator app"));
  $("app-secret").textContent = r.data.secret;
  $("app-code").value = "";
  $("app-code").focus();
});

$("app-copy").addEventListener("click", () => A.copy($("app-secret").textContent, $("app-copy")));

$("app-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  $("app-go").disabled = true;
  const r = await A.api("POST", "invite/totp/finish", { code: $("app-code").value.trim() });
  $("app-go").disabled = false;
  if (r.status === 200) return finished(r);
  say(A.message(r, "That code didn't match."));
  $("app-code").select();
});

async function init() {
  if (!token) return dead("This link is incomplete: the part after # is missing. Copy the whole link.");
  const r = await A.api("POST", "invite/info", { token: token });
  if (r.status !== 200) return dead(A.message(r, "This invitation isn't valid."));
  info = r.data;
  const node = info.node ? "node " + info.node : "this node";
  // An invite from the command line was made by whoever runs the node.
  const inviter = info.inviter === "console" ? "The node's operator" : info.inviter;
  if (info.for_user) {
    $("title").textContent = "Add a sign-in method";
    $("who").textContent = inviter + " invited you to add a way to sign in to " + info.for_user + " on " + node + ". Expires " + A.until(info.expires) + ".";
    $("name").value = info.for_user;
    $("name").readOnly = true;
    $("display").closest("form").hidden = true;
  } else {
    const role = info.role === "admin" ? "an admin" : "a listener";
    $("who").textContent = inviter + " invited you as " + role + " of " + node + ". Expires " + A.until(info.expires) + ".";
    if (info.name) $("name").value = info.name;
  }
  if (info.app_allowed === false) {
    // Codes are only rate-limited, which is not enough to guard an admin.
    $("t-app").hidden = true;
    $("app-note").textContent =
      "Admins sign in with a passkey or security key, which can't be guessed; you can add an authenticator app later to confirm it's you.";
    $("app-note").hidden = false;
  }
  if (!A.webauthnAvailable()) {
    $("t-passkey").hidden = true;
    $("t-key").hidden = true;
    if (info.app_allowed === false) say("This browser can't make a passkey. Open the link in one that can.");
  }
  $("enrol").hidden = false;
  (info.for_user || info.name ? $("t-passkey") : $("name")).focus();
}

init();
