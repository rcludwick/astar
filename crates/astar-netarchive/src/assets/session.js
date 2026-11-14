// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.
//
// One recorded session: the head card with the orb, the conversation of
// overs, and the full-screen replay. The audio is session.flac, read with
// byte ranges by the browser; seeking sets currentTime and never fetches
// the whole file.
//
// Callsigns come off the air and may carry bidi or format characters, so
// every string from data.json is set with textContent inside a <bdi>, and
// nothing here builds markup from text.
"use strict";

(function () {
  const $ = (id) => document.getElementById(id);
  const BINS = 48;
  const PAGE_C = 2 * Math.PI * 40; // the head orb's ring, r = 40
  const MINI_C = 2 * Math.PI * 30; // the replay orb's ring, r = 30
  const reduced = window.matchMedia("(prefers-reduced-motion: reduce)");

  const audio = $("audio");
  const page = $("page");
  const replay = $("replay");

  const st = {
    data: null,
    overs: [], // { seq, offset, end, dur, base, label, els: [{ node, kind, spans, filled }] }
    pos: 0, // ms on the session.flac timeline
    pending: null, // a seek (seconds) the element has not applied yet
    nowIdx: -1,
    doneCount: -1,
    started: false, // the user has pressed play at least once
    mySide: false,
    replayOpen: false,
    openedByHash: false,
    raf: 0,
  };

  // ---- small helpers ------------------------------------------------------

  function el(tag, cls, text) {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined) e.textContent = text;
    return e;
  }

  function clockLen(ms) {
    const s = Math.max(0, Math.floor(ms / 1000));
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const ss = String(s % 60).padStart(2, "0");
    return h > 0 ? h + ":" + String(m).padStart(2, "0") + ":" + ss : m + ":" + ss;
  }

  function overLen(ms) {
    return clockLen(Math.max(1000, Math.round(ms / 1000) * 1000));
  }

  // The wall clock `ms` into a session that started at `start` ("HH:MM:SS").
  function wallClock(start, ms) {
    const p = (start || "").split(":").map(Number);
    if (p.length !== 3 || p.some((n) => !Number.isFinite(n))) return "";
    let s = (p[0] * 3600 + p[1] * 60 + p[2] + Math.floor(ms / 1000)) % 86400;
    const two = (n) => String(n).padStart(2, "0");
    const h = Math.floor(s / 3600);
    s %= 3600;
    return two(h) + ":" + two(Math.floor(s / 60)) + ":" + two(s % 60);
  }

  function plural(n, one, many) {
    return n + " " + (n === 1 ? one : many);
  }

  function store(key, value) {
    try {
      if (value === undefined) return window.localStorage.getItem(key);
      window.localStorage.setItem(key, value);
    } catch (_) {
      /* private browsing: the switch still works for this visit */
    }
    return null;
  }

  // ---- building the conversation ------------------------------------------

  function callNode(o, cls) {
    const call = el("span", cls);
    if (o.call) {
      call.append(el("bdi", "base", o.call));
      if (o.suffix) call.append(el("bdi", "sfx", o.suffix));
    } else {
      call.append(el("span", "unknown", "Unknown station"));
    }
    const eq = el("span", "eq");
    eq.setAttribute("aria-hidden", "true");
    eq.append(el("span"), el("span"), el("span"));
    call.append(eq);
    return call;
  }

  function wave(peaks) {
    const w = el("span", "wave");
    w.setAttribute("aria-hidden", "true");
    const n = peaks.length;
    let max = 0;
    for (const p of peaks) max = Math.max(max, p);
    const spans = [];
    for (let k = 0; k < BINS; k++) {
      const s = el("span");
      const peak = n ? peaks[Math.floor((k * n) / BINS)] : 0;
      const pct = max > 0 ? (peak * 100) / max : 0;
      s.style.height = Math.max(12, Math.round(pct)) + "%";
      w.append(s);
      spans.push(s);
    }
    return { w, spans };
  }

  function play(o) {
    seek(o.offset);
    start();
  }

  function bubble(o, mine) {
    const row = el("div", mine ? "row me" : "row");
    const b = el("button", "b");
    b.type = "button";
    b.dataset.seq = String(o.seq);
    b.setAttribute("aria-label", o.label);
    const { w, spans } = wave(o.peaks);
    b.append(callNode(o, "call"), w, el("span", "meta", o.clock + " · " + overLen(o.dur)));
    b.addEventListener("click", () => play(o));
    row.append(b);
    return { row, entry: { node: b, kind: "b", spans, filled: -1 } };
  }

  function chip(o, mine) {
    const k = el("button", mine ? "k mine" : "k");
    k.type = "button";
    k.dataset.seq = String(o.seq);
    k.setAttribute("aria-label", o.label);
    k.title = o.clock + " · " + overLen(o.dur);
    k.append(callNode(o, "kcall"));
    k.addEventListener("click", () => play(o));
    return { node: k, kind: "k", spans: [], filled: -1 };
  }

  function divider(it) {
    const d = el("p", "div");
    const t = el("span", "dt");
    const at = el("span", "", it.clock);
    switch (it.kind) {
      case "dropped":
        t.append(el("b", "", "link dropped"), " ", at);
        if (it.back_clock) t.append(" → back " + it.back_clock);
        break;
      case "error":
        t.append(el("b", "err", "error"), " ", at);
        break;
      default:
        t.append(at, " · " + it.kind);
    }
    if (it.detail) t.append(" · ", el("bdi", "detail", it.detail));
    d.append(t);
    return d;
  }

  // Fill `box` with the timeline. `which` is 0 for the page, 1 for replay.
  function build(box, which) {
    const mine = (o) => st.mySide && o.base !== null && st.data.my_calls.includes(o.base);
    let keyups = null;
    let i = 0;
    for (const it of st.data.items) {
      if (it.type === "over") {
        const o = st.overs[i++];
        if (it.short) {
          if (!keyups) {
            keyups = el("div", "keyups");
            keyups.append(el("span", "label", "key-ups"));
            box.append(keyups);
          }
          const c = chip(o, mine(o));
          keyups.append(c.node);
          o.els[which] = c;
        } else {
          keyups = null;
          const { row, entry } = bubble(o, mine(o));
          box.append(row);
          o.els[which] = entry;
        }
      } else {
        keyups = null;
        box.append(divider(it));
      }
    }
  }

  function buildPage() {
    const box = $("convo");
    box.replaceChildren();
    for (const o of st.overs) o.els[0] = null;
    build(box, 0);
    if (st.overs.length === 0) {
      box.append(el("p", "note", "Nothing was heard in this session."));
    }
    paintStates(true);
  }

  // ---- position, classes and progress --------------------------------------

  function indexAt(t) {
    for (let i = 0; i < st.overs.length; i++) {
      const o = st.overs[i];
      if (o.offset <= t && t < o.end) return i;
    }
    return -1;
  }

  function paintEntry(o, e, isNow, isDone, t) {
    if (!e) return;
    e.node.classList.toggle("now", isNow);
    e.node.classList.toggle("done", isDone);
    const fill = isNow ? Math.ceil(((t - o.offset) / o.dur) * BINS) : -1;
    if (fill !== e.filled) {
      e.spans.forEach((s, k) => s.classList.toggle("p", k < fill));
      e.filled = fill;
    }
  }

  function paintStates(force) {
    const t = st.pos;
    const now = indexAt(t);
    // A seek between two gaps keeps `now` at -1 but changes which overs
    // are done, so the count of finished overs is part of what moved.
    const done = st.overs.filter((o) => o.end <= t).length;
    const moved = now !== st.nowIdx;
    const repaint = force || moved || done !== st.doneCount;
    st.doneCount = done;
    st.overs.forEach((o, i) => {
      if (!repaint && i !== now) return;
      const isNow = i === now;
      const isDone = !isNow && o.end <= t;
      for (const e of o.els) paintEntry(o, e, isNow, isDone, t);
    });
    st.nowIdx = now;
    if (moved && now >= 0) follow(st.overs[now]);
  }

  function follow(o) {
    const behavior = reduced.matches ? "auto" : "smooth";
    if (st.replayOpen) {
      centre(o, behavior);
    } else if (st.started && o.els[0]) {
      o.els[0].node.scrollIntoView({ block: "nearest", behavior });
    }
  }

  // Keep the playing over in the middle of the replay column.
  function centre(o, behavior) {
    const e = o && o.els[1];
    if (!e) return;
    const col = $("r-col");
    const node = e.kind === "k" ? e.node.parentElement : e.node;
    const top = node.offsetTop - col.clientHeight / 2 + node.offsetHeight / 2;
    col.scrollTo({ top: Math.max(0, top), behavior });
  }

  function paint() {
    const d = st.data;
    if (!d) return;
    const total = Math.max(1, d.duration_ms);
    const frac = Math.min(1, Math.max(0, st.pos / total));
    $("elapsed").textContent = clockLen(st.pos);
    $("ring").style.strokeDashoffset = String(PAGE_C * (1 - frac));
    if (st.replayOpen) {
      $("r-ring").style.strokeDashoffset = String(MINI_C * (1 - frac));
      $("r-fill").style.width = frac * 100 + "%";
      $("r-clock").textContent = wallClock(d.clock, st.pos);
    }
    paintStates(false);
  }

  function loop() {
    st.raf = 0;
    if (st.pending === null && !audio.seeking) st.pos = audio.currentTime * 1000;
    paint();
    if (!audio.paused) st.raf = requestAnimationFrame(loop);
  }

  function seek(ms) {
    const d = st.data;
    st.pos = Math.min(Math.max(0, ms), d ? d.duration_ms : ms);
    st.pending = st.pos / 1000;
    if (audio.readyState >= 1) {
      try {
        audio.currentTime = st.pending;
      } catch (_) {
        /* applied again at loadedmetadata */
      }
    }
    paint();
  }

  function start() {
    st.started = true;
    const p = audio.play();
    if (p && p.catch) {
      p.catch((e) => {
        if (e && e.name === "NotSupportedError") showAudioError();
      });
    }
  }

  function toggle() {
    if (audio.paused) start();
    else audio.pause();
  }

  // ←/→: the previous or next over, chips included, playing it.
  function step(dir) {
    if (st.overs.length === 0) return;
    let cur = -1;
    st.overs.forEach((o, i) => {
      if (o.offset <= st.pos) cur = i;
    });
    let target;
    if (dir > 0) target = cur + 1;
    else target = st.nowIdx >= 0 ? cur - 1 : cur;
    if (target < 0 || target >= st.overs.length) return;
    const o = st.overs[target];
    play(o);
    const e = o.els[st.replayOpen ? 1 : 0];
    if (e) e.node.focus({ preventScroll: true });
    follow(o);
  }

  function showAudioError() {
    $("audio-error").hidden = false;
  }

  audio.addEventListener("loadedmetadata", () => {
    if (st.pending !== null) {
      try {
        audio.currentTime = st.pending;
      } catch (_) {
        st.pending = null;
      }
    }
  });
  audio.addEventListener("seeked", () => {
    st.pending = null;
    st.pos = audio.currentTime * 1000;
    paint();
  });
  audio.addEventListener("timeupdate", () => {
    if (st.pending === null && !audio.seeking) st.pos = audio.currentTime * 1000;
    paint();
  });
  audio.addEventListener("play", () => {
    document.body.classList.add("playing");
    for (const id of ["orb", "r-orb"]) $(id).setAttribute("aria-label", "Pause");
    if (!st.raf) st.raf = requestAnimationFrame(loop);
  });
  audio.addEventListener("pause", () => {
    document.body.classList.remove("playing");
    for (const id of ["orb", "r-orb"]) $(id).setAttribute("aria-label", "Play");
  });
  audio.addEventListener("ended", () => {
    document.body.classList.remove("playing");
  });
  audio.addEventListener("error", showAudioError);

  // ---- the "My side" switch ----------------------------------------------

  function setMySide(on, save) {
    st.mySide = on;
    $("myside").setAttribute("aria-checked", on ? "true" : "false");
    if (save) store("nets.myside." + st.data.net_slug, on ? "1" : "0");
    buildPage();
    if (st.replayOpen) buildReplay();
  }

  $("myside").addEventListener("click", () => setMySide(!st.mySide, true));

  // ---- the replay view -----------------------------------------------------

  function buildReplay() {
    const col = $("r-col");
    col.replaceChildren();
    for (const o of st.overs) o.els[1] = null;
    col.append(el("div", "pad"));
    build(col, 1);
    col.append(el("div", "pad"));
    paintStates(true);
  }

  function openReplay() {
    if (st.replayOpen || !st.data) return;
    st.openedByHash = /(^|&)replay(&|$)/.test(location.hash.slice(1));
    st.replayOpen = true;
    replay.hidden = false;
    page.inert = true;
    document.body.classList.add("replaying");
    buildReplay();
    paint();
    let o = st.overs[st.nowIdx];
    if (!o) o = st.overs.filter((x) => x.offset <= st.pos).pop() || st.overs[0];
    centre(o, "auto");
    $("r-orb").focus({ preventScroll: true });
  }

  function closeReplay() {
    if (!st.replayOpen) return;
    st.replayOpen = false;
    st.openedByHash = false;
    replay.hidden = true;
    page.inert = false;
    document.body.classList.remove("replaying");
    $("r-col").replaceChildren();
    for (const o of st.overs) o.els[1] = null;
    if (/(^|&)replay(&|$)/.test(location.hash.slice(1))) {
      const rest = location.hash
        .slice(1)
        .split("&")
        .filter((p) => p && p !== "replay")
        .join("&");
      history.replaceState(null, "", location.pathname + location.search + (rest ? "#" + rest : ""));
    }
    $("replay-btn").focus();
  }

  $("replay-btn").addEventListener("click", openReplay);
  $("r-close").addEventListener("click", closeReplay);
  $("orb").addEventListener("click", toggle);
  $("r-orb").addEventListener("click", toggle);
  $("r-tl").addEventListener("click", (ev) => {
    const r = ev.currentTarget.getBoundingClientRect();
    const frac = Math.min(1, Math.max(0, (ev.clientX - r.left) / r.width));
    seek(frac * st.data.duration_ms);
  });

  // ---- the keyboard ---------------------------------------------------------

  // Controls whose own keys win: text fields, pickers, sliders.
  const OWN_KEYS = "input, textarea, select, [contenteditable], [role=slider], [role=listbox]";
  // Controls that space activates natively; elsewhere space plays or pauses.
  const OWN_SPACE = OWN_KEYS + ", a, .act, .sw, .r-close";

  function spaceIsOurs(ev) {
    return !(ev.target instanceof Element && ev.target.closest(OWN_SPACE));
  }
  // Space on a focused bubble or chip plays from it, like Enter.
  function overButton(ev) {
    return ev.target instanceof Element ? ev.target.closest(".b, .k") : null;
  }

  document.addEventListener("keydown", (ev) => {
    if (!st.data || ev.altKey || ev.ctrlKey || ev.metaKey) return;
    if (ev.key === "Escape" && st.replayOpen) {
      ev.preventDefault();
      closeReplay();
      return;
    }
    if (ev.key === " " && spaceIsOurs(ev)) {
      ev.preventDefault();
      const ob = overButton(ev);
      if (!ev.repeat) {
        if (ob) ob.click();
        else toggle();
      }
      return;
    }
    if (ev.key === "ArrowLeft" || ev.key === "ArrowRight") {
      if (ev.target instanceof Element && ev.target.closest(OWN_KEYS)) return;
      ev.preventDefault();
      step(ev.key === "ArrowRight" ? 1 : -1);
    }
  });
  // A button fires its click on space's keyup; ours was handled at keydown.
  document.addEventListener("keyup", (ev) => {
    if (ev.key === " " && spaceIsOurs(ev)) ev.preventDefault();
  });

  // ---- deep links: #t=<seconds>, #replay ------------------------------------

  function applyHash() {
    let deepLinked = false;
    const parts = location.hash.slice(1).split("&");
    for (const p of parts) {
      const m = /^t=(\d+(?:\.\d+)?)$/.exec(p);
      if (m) {
        audio.pause();
        seek(parseFloat(m[1]) * 1000);
        deepLinked = true;
      }
    }
    if (deepLinked && !st.replayOpen) {
      const o = st.overs[st.nowIdx];
      const e = o && o.els[0];
      if (e) e.node.scrollIntoView({ block: "center", behavior: "auto" });
    }
    if (parts.includes("replay")) openReplay();
    else if (st.replayOpen && st.openedByHash) closeReplay();
  }
  window.addEventListener("hashchange", applyHash);

  // ---- the head card ---------------------------------------------------------

  function renderHead(d) {
    document.title = d.net + " — " + d.date;
    $("net").textContent = d.net;
    $("total").textContent = clockLen(d.duration_ms);
    const sub = $("sub");
    sub.replaceChildren();
    sub.append(el("span", "chip", d.network_label));
    const facts = [];
    if (d.target) facts.push(d.target);
    if (d.date) facts.push(d.date);
    facts.push(plural(d.checkins, "check-in", "check-ins"));
    // Each fact carries the separator that follows it, so a wrapped line
    // never starts on a dot.
    facts.forEach((f, i) => {
      const s = el("span", "fact");
      s.append(el("bdi", "", f));
      if (i < facts.length - 1) s.append(el("span", "sep", "·"));
      sub.append(s);
    });
    if (!d.public) sub.append(el("span", "chip priv", "Private"));
    if (d.recording) sub.append(el("span", "chip rec", "Recording"));
    $("still").hidden = !d.recording;

    $("r-chip").textContent = d.network_label;
    $("r-sub").textContent = [d.target, d.date].filter(Boolean).join(" · ");
    $("r-title").textContent = d.net;
    $("r-total").textContent = clockLen(d.duration_ms);

    for (const [id, c] of [
      ["ring", PAGE_C],
      ["r-ring", MINI_C],
    ]) {
      $(id).style.strokeDasharray = String(c);
      $(id).style.strokeDashoffset = String(c);
    }

    if (d.my_calls.length > 0) {
      $("myside-box").hidden = false;
      const saved = store("nets.myside." + d.net_slug);
      st.mySide = saved === null ? d.my_side : saved === "1";
      $("myside").setAttribute("aria-checked", st.mySide ? "true" : "false");
    }

    if (d.can_delete && d.delete_url) {
      const del = $("delete-btn");
      del.hidden = false;
      del.addEventListener("click", () => remove(d));
    }
  }

  function remove(d) {
    if (!window.confirm("Delete this recording of “" + d.net + "”, " + d.date + "? This can't be undone.")) {
      return;
    }
    fetch(d.delete_url, {
      method: "POST",
      headers: { "Content-Type": "application/json", Accept: "application/json" },
      credentials: "same-origin",
      body: JSON.stringify({ net: d.net_slug, session: d.session_slug }),
    })
      .then((r) => {
        if (r.ok) {
          window.location.href = "/nets/";
          return null;
        }
        return r
          .json()
          .catch(() => ({}))
          .then((j) => {
            throw new Error(j.error || "HTTP " + r.status);
          });
      })
      .catch((e) => {
        const b = $("err");
        b.textContent = "The recording was not deleted: " + e.message + ".";
        b.hidden = false;
      });
  }

  function label(it) {
    const who = it.call ? "⁨" + it.call + (it.suffix ? " " + it.suffix : "") + "⁩" : "Unknown station";
    const n = Math.max(1, Math.round(it.duration_ms / 1000));
    return who + ", " + it.clock + ", " + plural(n, "second", "seconds") + ". Play from here.";
  }

  function render(d) {
    st.data = d;
    st.overs = d.items
      .filter((it) => it.type === "over")
      .map((it) => ({
        seq: it.seq,
        offset: it.offset_ms,
        dur: Math.max(1, it.duration_ms),
        end: it.offset_ms + Math.max(1, it.duration_ms),
        base: it.call,
        call: it.call,
        suffix: it.suffix,
        clock: it.clock,
        peaks: it.peaks || [],
        label: label(it),
        els: [null, null],
      }));
    renderHead(d);
    buildPage();
    $("head").hidden = false;
    $("convo").hidden = false;
    page.removeAttribute("aria-busy");
    $("download").href = d.audio.url;
    audio.src = d.audio.url;
    paint();
    applyHash();
  }

  function missing() {
    document.title = "No such recording — astar";
    $("missing").hidden = false;
    page.removeAttribute("aria-busy");
  }

  fetch("data.json", { cache: "no-store", headers: { Accept: "application/json" } })
    .then((r) => {
      if (r.status === 404) return null;
      if (!r.ok) throw new Error("HTTP " + r.status);
      return r.json();
    })
    .then((d) => (d ? render(d) : missing()))
    .catch(() => {
      const b = $("err");
      b.textContent = "This recording could not be loaded. Reload to try again.";
      b.hidden = false;
      page.removeAttribute("aria-busy");
    });
})();
