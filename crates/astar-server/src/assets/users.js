// Users & invites (admins): roles, sign-outs, credentials, re-enrol links,
// invites, and the audit.
"use strict";

const A = window.astarAuth;
const el = A.el;
const $ = (id) => document.getElementById(id);

function say(text) {
  $("msg").textContent = text || "";
}

function seg(id) {
  const box = $(id);
  box.addEventListener("click", (e) => {
    const b = e.target.closest("button");
    if (!b) return;
    for (const x of box.querySelectorAll("button")) {
      x.classList.toggle("on", x === b);
      x.setAttribute("aria-checked", x === b ? "true" : "false");
    }
    if (id === "role") {
      // An admin invite defaults to an hour; a listener's to a day.
      const life = b.dataset.v === "admin" ? "1" : "24";
      $("role-note").hidden = b.dataset.v !== "admin";
      for (const x of $("life").querySelectorAll("button")) {
        x.classList.toggle("on", x.dataset.v === life);
        x.setAttribute("aria-checked", x.dataset.v === life ? "true" : "false");
      }
    }
  });
  return () => box.querySelector("button.on").dataset.v;
}
const role = seg("role");
const life = seg("life");

function button(text, cls, onClick) {
  const b = el("button", cls || "btn", text);
  b.type = "button";
  b.addEventListener("click", onClick);
  return b;
}

/// Replace a row's actions with "question? Yes / Cancel".
function confirmIn(actions, question, yesText, act) {
  const keep = Array.from(actions.childNodes);
  actions.classList.add("confirming");
  const no = button("Cancel", "btn", () => {
    actions.classList.remove("confirming");
    actions.replaceChildren(...keep);
  });
  const yes = button(yesText, "btn danger", async () => {
    yes.disabled = true;
    await act();
  });
  actions.replaceChildren(el("span", "confirm-q", question), yes, no);
  no.focus();
}

async function act(method, path, body, why) {
  const r = await A.api(method, path, body || {}, { why: why });
  if (r.status !== 200) say(A.message(r));
  else say("");
  await load();
  return r;
}

function userRow(u) {
  const li = el("li", "person");
  const who = el("div", "who");
  const line = el("div", "name-line");
  line.append(el("span", "name", u.name), el("span", "role-pill " + u.role, u.role === "admin" ? "admin" : "listener"));
  if (u.display_name && u.display_name !== u.name) line.append(el("span", "meta", u.display_name));
  if (u.codes_locked_until) line.append(el("span", "tag", "codes locked"));
  const meta = el("div", "meta");
  meta.append(
    el("span", null, u.passkeys + " passkey" + (u.passkeys === 1 ? "" : "s")),
    el("span", null, u.security_keys + " key" + (u.security_keys === 1 ? "" : "s")),
    el("span", null, u.apps + " app" + (u.apps === 1 ? "" : "s")),
    el("span", null, "signed in " + A.ago(u.last_sign_in)),
  );
  who.append(line, meta);

  const creds = el("details", "advanced");
  creds.append(el("summary", null, "Sign-in methods"));
  for (const c of u.credentials || []) {
    const row = el("div", "cred");
    const w = el("div", "who");
    w.append(el("span", "name", c.nickname), el("div", "meta", c.kind.replace("_", " ") + (c.rp_id ? " · " + c.rp_id : " · any hostname")));
    const a = el("div", "row-actions");
    const del = button("Remove", "btn quiet-danger", () =>
      confirmIn(a, "Remove " + u.name + "'s “" + c.nickname + "”?", "Remove", () =>
        act("DELETE", "users/" + u.id + "/credentials/" + encodeURIComponent(c.id), {}, "Removing someone's sign-in method needs a fresh confirmation."),
      ),
    );
    if ((u.credentials || []).length <= 1) {
      del.disabled = true;
      del.title = "Their only way in: send a re-enrol link, or delete the user.";
    } else if (u.role === "admin" && c.kind !== "totp" && u.passkeys + u.security_keys <= 1) {
      del.disabled = true;
      del.title = "An admin's last passkey or key: admins can't sign in with codes.";
    }
    a.append(del);
    row.append(w, a);
    creds.append(row);
  }

  const actions = el("div", "row-actions");
  const toggle = button(u.role === "admin" ? "Make listener" : "Make admin", "btn", () =>
    act("POST", "users/" + u.id + "/role", { role: u.role === "admin" ? "viewer" : "admin" }, "Changing a role needs a fresh confirmation."),
  );
  if (u.last_admin) {
    toggle.disabled = true;
    toggle.title = "The only admin: make someone else an admin first.";
  } else if (u.role !== "admin" && u.passkeys + u.security_keys === 0) {
    toggle.disabled = true;
    toggle.title = "Admins sign in with a passkey or security key, and " + u.name + " has neither: send a re-enrol link first.";
  }
  const reenrol = button("Re-enrol link", "btn", async () => {
    const r = await A.api("POST", "invites", { user_id: u.id }, { why: "Sending a re-enrol link needs a fresh confirmation." });
    if (r.status !== 200) return say(A.message(r));
    $("made").replaceChildren(A.onceLink(r.data.link, r.data.qr, "Send this to " + u.name + ". It adds a sign-in method to their account, once, within the hour."));
    $("made").hidden = false;
    loadInvites();
  });
  const out = button("Sign out everywhere", "btn", () => act("POST", "users/" + u.id + "/signout", {}));
  const del = button("Delete", "btn quiet-danger", () =>
    confirmIn(actions, "Delete " + u.name + " and everything they can sign in with?", "Delete", () =>
      act("DELETE", "users/" + u.id, {}, "Deleting a user needs a fresh confirmation."),
    ),
  );
  if (u.last_admin) {
    del.disabled = true;
    del.title = "The only admin can't be deleted.";
  }
  actions.append(toggle, reenrol, out, del);
  li.append(who, actions, creds);
  return li;
}

async function loadUsers() {
  const r = await A.api("GET", "users");
  if (r.status === 401) return window.location.replace("/admin/signin?next=/admin/users");
  if (r.status !== 200) return say(A.message(r));
  const users = r.data.users || [];
  $("user-count").textContent = users.length;
  $("users").replaceChildren(...users.map(userRow));
}

async function loadInvites() {
  const r = await A.api("GET", "invites");
  if (r.status !== 200) return;
  const list = r.data.invites || [];
  $("invite-count").textContent = list.filter((i) => i.state === "pending").length;
  $("invites").replaceChildren(
    ...list.map((i) => {
      const li = el("li", "person");
      const who = el("div", "who");
      const line = el("div", "name-line");
      const what = i.for_user ? "re-enrol " + i.for_user : (i.name || "anyone") + " as " + (i.role === "admin" ? "admin" : "listener");
      line.append(el("span", "name", what), el("span", "tag", i.state));
      const meta = el("div", "meta");
      meta.append(el("span", null, "by " + i.created_by));
      if (i.state === "pending") meta.append(el("span", null, "expires " + A.until(i.expires)));
      if (i.state === "used") meta.append(el("span", null, "used by " + i.used_by + " " + A.ago(i.used_at)));
      who.append(line, meta);
      const actions = el("div", "row-actions");
      if (i.state === "pending") actions.append(button("Revoke", "btn quiet-danger", () => act("DELETE", "invites/" + encodeURIComponent(i.id), {})));
      li.append(who, actions);
      return li;
    }),
  );
}

let auditTimer = 0;
async function loadAudit() {
  const f = $("audit-filter").value.trim();
  const r = await A.api("GET", "audit" + (f ? "?user=" + encodeURIComponent(f) : ""));
  if (r.status !== 200) return;
  $("audit").replaceChildren(
    ...(r.data.entries || []).map((e) => {
      const li = el("li");
      li.append(
        el("span", "t", new Date(e.at * 1000).toLocaleString([], { dateStyle: "medium", timeStyle: "short" })),
        el("span", "e", e.event.replace(/_/g, " ") + (e.user ? " · " + e.user : "")),
      );
      const times = e.times > 1 ? "×" + e.times + " since " + new Date(e.since * 1000).toLocaleTimeString([], { timeStyle: "short" }) : "";
      const d = [e.detail, e.ip, times].filter(Boolean).join(" · ");
      if (d) li.append(el("span", "d", d));
      return li;
    }),
  );
}
$("audit-filter").addEventListener("input", () => {
  window.clearTimeout(auditTimer);
  auditTimer = window.setTimeout(loadAudit, 250);
});

$("new-invite").addEventListener("submit", async (e) => {
  e.preventDefault();
  $("make").disabled = true;
  const body = { role: role(), hours: Number(life()) };
  const name = $("inv-name").value.trim();
  if (name) body.name = name;
  const r = await A.api("POST", "invites", body, { why: "Making an invite needs a fresh confirmation." });
  $("make").disabled = false;
  if (r.status !== 200) return say(A.message(r));
  say("");
  $("inv-name").value = "";
  $("made").replaceChildren(
    A.onceLink(r.data.link, r.data.qr, "Send it to them however you like; it works once, until it expires, and you can revoke it below."),
  );
  $("made").hidden = false;
  loadInvites();
  loadAudit();
});

async function load() {
  await Promise.all([loadUsers(), loadInvites(), loadAudit()]);
}

load();
