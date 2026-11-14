// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//
// The list of recorded nets: one card per net, its sessions newest first.
// Every string from the archive is set with textContent; nothing here
// builds markup from text.
"use strict";

(function () {
  const $ = (id) => document.getElementById(id);

  function el(tag, cls, text) {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined) e.textContent = text;
    return e;
  }

  function length(ms) {
    const s = Math.round(ms / 1000);
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    if (h > 0) return h + " h " + String(m).padStart(2, "0") + " min";
    const r = s % 60;
    if (m > 0) return r > 0 ? m + " min " + r + " s" : m + " min";
    return s + " s";
  }

  function plural(n, one, many) {
    return n + " " + (n === 1 ? one : many);
  }

  function sessionRow(s) {
    const li = el("li");
    const a = el("a", "srow");
    a.href = s.url;
    const when = el("span", "when");
    when.append(el("span", "date", s.date), el("span", "start", s.clock));
    const facts = el("span", "facts");
    facts.append(
      el("span", "", length(s.duration_ms)),
      el("span", "", "·"),
      el("span", "", plural(s.checkins, "check-in", "check-ins")),
    );
    if (!s.public) facts.append(el("span", "chip priv", "Private"));
    if (s.recording) facts.append(el("span", "chip rec", "Recording"));
    const chev = el("span", "chev");
    chev.setAttribute("aria-hidden", "true");
    a.append(when, facts, chev);
    li.append(a);
    return li;
  }

  function netCard(n) {
    const card = el("section", "card net");
    const head = el("div", "net-head");
    const h = el("h2");
    h.append(el("bdi", "", n.name));
    head.append(h, el("span", "chip", n.network_label));
    head.append(el("span", "count", plural(n.sessions.length, "session", "sessions")));
    card.setAttribute("aria-label", n.name);
    const ul = el("ul", "sessions");
    for (const s of n.sessions) ul.append(sessionRow(s));
    card.append(head, ul);
    return card;
  }

  function render(v) {
    const box = $("nets");
    box.replaceChildren();
    if (v.nets.length === 0) {
      box.append(el("section", "card empty-card", "No recordings yet."));
    } else {
      for (const n of v.nets) box.append(netCard(n));
    }
    if (!v.signed_in && v.sign_in_url) {
      const a = $("signin");
      a.href = v.sign_in_url;
      a.hidden = false;
    }
    box.removeAttribute("aria-busy");
  }

  fetch("/nets/index.json", { cache: "no-store", headers: { Accept: "application/json" } })
    .then((r) => {
      if (!r.ok) throw new Error("HTTP " + r.status);
      return r.json();
    })
    .then(render)
    .catch(() => {
      const e = $("err");
      e.textContent = "The recordings could not be loaded. Reload to try again.";
      e.hidden = false;
      $("nets").removeAttribute("aria-busy");
    });
})();
