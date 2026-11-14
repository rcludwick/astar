// The sign-in page: the passkey button, autofill, and identifier-first
// sign-in with a security key or an authenticator code.
"use strict";

const A = window.astarAuth;
const $ = (id) => document.getElementById(id);

/// Same rule as the node's: this origin's "/" or a path under /admin/.
function safeNext(n) {
  return (
    typeof n === "string" &&
    (n === "/" || n.startsWith("/admin/") || n.startsWith("/nets/")) &&
    !n.includes("//") &&
    !n.includes("\\") &&
    !n.includes("/../")
  );
}

const asked = new URLSearchParams(window.location.search).get("next");
const next = safeNext(asked) ? asked : "/admin/";
let autofill = null; // the pending conditional request's AbortController
let autofillDone = Promise.resolve(); // settles when that request has

function say(text) {
  $("msg").textContent = text || "";
}

/// Signed in: go on, unless there is something to say first.
function signedIn(r) {
  const failed = r.data && r.data.failed_codes;
  if (failed > 0) {
    $("signin").hidden = true;
    $("after-text").textContent =
      failed + " failed code attempt" + (failed === 1 ? "" : "s") + " since your last sign-in. If that wasn't you, review your sign-in methods in Account.";
    $("after").hidden = false;
    $("continue").focus();
    return;
  }
  window.location.replace(next);
}

/// Abort the autofill request and wait for the browser to let go of it: a
/// new request started while it is still pending is refused.
async function stopAutofill() {
  if (autofill) {
    autofill.abort();
    autofill = null;
  }
  await autofillDone;
}

async function startAutofill() {
  if (!A.webauthnAvailable() || typeof PublicKeyCredential.isConditionalMediationAvailable !== "function") return;
  let ok = false;
  try {
    ok = await PublicKeyCredential.isConditionalMediationAvailable();
  } catch (_e) {
    ok = false;
  }
  if (!ok) return;
  const ctl = new AbortController();
  autofill = ctl;
  const pending = A.assert("passkey/begin", "passkey/finish", {}, "conditional", ctl.signal);
  autofillDone = pending.then(
    () => undefined,
    () => undefined,
  );
  const r = await pending;
  if (autofill !== ctl) return; // replaced by a button press
  autofill = null;
  if (r.status === 200) return signedIn(r);
  if (r.status > 0) say(A.message(r));
}

async function withButton(btn, fn) {
  btn.disabled = true;
  say("");
  try {
    await fn();
  } finally {
    btn.disabled = false;
  }
}

$("passkey").addEventListener("click", () =>
  withButton($("passkey"), async () => {
    await stopAutofill();
    const r = await A.assert("passkey/begin", "passkey/finish", {});
    if (r.status === 200) return signedIn(r);
    say(A.message(r, "That passkey didn't work."));
    startAutofill();
  }),
);

$("name-form").addEventListener("submit", (e) => {
  e.preventDefault();
  withButton($("key"), async () => {
    await stopAutofill();
    const body = { name: $("name").value.trim() };
    if ($("nopin").checked) body.pin = false;
    const r = await A.assert("key/begin", "key/finish", body);
    if (r.status === 200) return signedIn(r);
    say(A.message(r, "That key didn't work."));
  });
});

async function codeSignIn() {
  const name = $("name").value.trim();
  if (!name) {
    say("Enter your name first.");
    $("name").focus();
    return;
  }
  await stopAutofill();
  const r = await A.api("POST", "totp", { name: name, code: $("code").value.trim() });
  if (r.status === 200) return signedIn(r);
  say(A.message(r, "That code didn't work."));
  $("code").select();
}

$("code-go").addEventListener("click", () => withButton($("code-go"), codeSignIn));
$("code").addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    e.preventDefault();
    withButton($("code-go"), codeSignIn);
  }
});
$("continue").addEventListener("click", () => window.location.replace(next));

async function init() {
  // A link from elsewhere arrives without the SameSite=Strict cookie; this
  // same-origin fetch carries it, so a signed-in browser goes straight on.
  const me = await A.api("GET", "me");
  if (me.status === 200 && !me.data.console) return window.location.replace(next);
  const st = await A.api("GET", "status");
  if (st.status !== 200) {
    $("unavailable").textContent =
      st.status === 404 ? "This node doesn't offer sign-in here." : A.message(st, "Can't reach the node.");
    $("unavailable").hidden = false;
    return;
  }
  const where = st.data.node ? "Node " + st.data.node + " · " + st.data.host : st.data.host;
  $("host").textContent = where;
  if (!st.data.admin_exists) {
    $("no-admin").hidden = false;
    return;
  }
  if (!st.data.signin) {
    $("unavailable").textContent = st.data.message || "Sign-in isn't available here.";
    $("unavailable").hidden = false;
    return;
  }
  $("signin").hidden = false;
  if (!A.webauthnAvailable()) {
    $("passkey").hidden = true;
    $("key-row").hidden = true;
    $("more").open = true;
    $("help").textContent = "This browser can't use passkeys here; use your authenticator app.";
    return;
  }
  $("passkey").focus();
  startAutofill();
}

init();
