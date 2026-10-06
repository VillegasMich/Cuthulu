// cuthulu client: live service table, detail view, log viewer, keyboard.
"use strict";

const $ = (sel, root = document) => root.querySelector(sel);
const page = document.body.dataset.page;
const readOnly = document.body.dataset.readOnly === "true";

const RANK = { running: 0, restarting: 1, paused: 2, dead: 3, stopped: 4, created: 5, unknown: 6 };
const UP = new Set(["running", "restarting", "paused"]);
// Exit codes produced by a normal `docker stop` (SIGINT / SIGKILL / SIGTERM).
const CLEAN_EXIT = new Set([0, 130, 137, 143]);

// ── helpers ────────────────────────────────────────────────

function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k === "class") node.className = v;
    else if (k.startsWith("on")) node.addEventListener(k.slice(2), v);
    else node.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children) if (c != null) node.append(c);
  return node;
}

/** Replace a node's children, skipping null/undefined. */
const fill = (node, ...kids) => node.replaceChildren(...kids.filter((k) => k != null));

function fmtDur(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ${m % 60}m`;
  const d = Math.floor(h / 24);
  return `${d}d ${h % 24}h`;
}

const ago = (ts) => (ts ? Date.now() - Date.parse(ts) : null);

/** Coarse age, largest unit only: "40s ago", "5h ago", "170d ago". */
function fmtAgo(ts) {
  const s = Math.max(0, Math.floor(ago(ts) / 1000));
  const [n, u] = s < 60 ? [s, "s"] : s < 3600 ? [s / 60, "m"] : s < 86400 ? [s / 3600, "h"] : [s / 86400, "d"];
  return `${Math.floor(n)}${u} ago`;
}

function stateLabel(s) {
  if (s.state === "running") {
    if (s.health === "unhealthy") return { cls: "unhealthy", text: "unhealthy" };
    if (s.health === "starting") return { cls: "starting", text: "starting" };
    return { cls: "running", text: "running" };
  }
  if (s.state === "stopped" && s.exit_code != null) {
    return CLEAN_EXIT.has(s.exit_code)
      ? { cls: "stopped", text: `exited ${s.exit_code}` }
      : { cls: "failed", text: `exit ${s.exit_code}` };
  }
  return { cls: s.state, text: s.state };
}

const isAlarming = (s) => ["unhealthy", "failed", "dead"].includes(stateLabel(s).cls);

function uptime(s) {
  if (UP.has(s.state) && s.started_at) return fmtDur(ago(s.started_at));
  if (s.finished_at) return fmtAgo(s.finished_at);
  return "—";
}

function portsText(ports) {
  return ports
    .filter((p) => p.host_port != null)
    .map((p) => `${p.host_port}->${p.container_port}`)
    .join(", ");
}

/** Image reference as [repo, tag span] (`:0.2.0`, `@sha256:…`); mirrors split_image in web.rs. */
function imageParts(image) {
  if (!image) return [""];
  const nameStart = image.lastIndexOf("/") + 1;
  const colon = image.indexOf(":", nameStart);
  const cut = Math.min(...[image.indexOf("@"), colon].filter((i) => i > 0), image.length);
  if (cut === image.length) return [image];
  return [image.slice(0, cut), el("span", { class: "img-tag" }, image.slice(cut))];
}

const svcUrl = (id) => `/services/${encodeURIComponent(id)}`;

function typing(e) {
  const t = e.target;
  if (t instanceof HTMLInputElement) return t.type !== "checkbox" && t.type !== "radio";
  return t instanceof HTMLSelectElement || t instanceof HTMLTextAreaElement;
}

let flashTimer;
function flash(msg, bad = false) {
  const f = $("#flash");
  f.textContent = msg;
  f.classList.toggle("bad", bad);
  f.hidden = false;
  clearTimeout(flashTimer);
  flashTimer = setTimeout(() => (f.hidden = true), bad ? 6000 : 2500);
}

function toggleTheme() {
  const root = document.documentElement;
  const current =
    root.dataset.theme || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
  const next = current === "dark" ? "light" : "dark";
  root.dataset.theme = next;
  try {
    localStorage.setItem("cuthulu.theme", next);
  } catch (_) {
    /* storage blocked: the choice lasts for this page only */
  }
}

// ── back navigation ────────────────────────────────────────

/** True when the previous history entry is a cuthulu page (Navigation API only
 * lists same-origin entries; `Referrer-Policy: no-referrer` rules out referrer). */
const canGoBack = () => window.navigation?.canGoBack === true;

function goBack() {
  if (canGoBack()) history.back();
  else location.href = "/";
}

function initBack() {
  const sync = () => document.body.classList.toggle("can-back", canGoBack());
  sync();
  window.addEventListener("pageshow", sync);
  $("#back").addEventListener("click", goBack);
}

function pref(key, fallback) {
  try {
    const v = localStorage.getItem(`cuthulu.${key}`);
    return v == null ? fallback : v === "1";
  } catch (_) {
    return fallback;
  }
}

function setPref(key, on) {
  try {
    localStorage.setItem(`cuthulu.${key}`, on ? "1" : "0");
  } catch (_) {
    /* ignore */
  }
}

// ── splitters ──────────────────────────────────────────────

const rootCss = document.documentElement.style;

/** Stored pane size `cuthulu.split.<key>` (theme.js applies it before first paint). */
function splitPref(key, fallback = null) {
  try {
    return JSON.parse(localStorage.getItem(`cuthulu.split.${key}`)) ?? fallback;
  } catch (_) {
    return fallback;
  }
}

function setSplitPref(key, value) {
  try {
    if (value == null) localStorage.removeItem(`cuthulu.split.${key}`);
    else localStorage.setItem(`cuthulu.split.${key}`, JSON.stringify(value));
  } catch (_) {
    /* storage blocked: the size lasts for this page only */
  }
}

/**
 * Make `handle` (a `role="separator"`) a pane splitter: drag it with mouse,
 * touch or pen; arrow keys step 16px or `o.step` (4× with shift); Home / End
 * or a double-click restore the default. `o.value()` is the controlled size in px,
 * `o.range()` its [min, max]; `o.set(px)` applies a size, `o.save(px)` stores
 * it, `o.reset()` drops it. `o.start()`, if given, measures what `range()`
 * needs; it runs on focus and before each drag or key press. `o.snap(px)`,
 * if given, rounds a size to the pane's grid.
 */
function splitter(handle, o) {
  const vertical = handle.getAttribute("aria-orientation") === "vertical";
  const sync = () => {
    o.start?.();
    const [min, max] = o.range();
    handle.setAttribute("aria-valuemin", String(Math.round(min)));
    handle.setAttribute("aria-valuemax", String(Math.round(max)));
    handle.setAttribute("aria-valuenow", String(Math.round(o.value())));
  };
  const resize = (px) => {
    const [min, max] = o.range();
    o.set(Math.round(Math.min(max, Math.max(min, o.snap ? o.snap(px) : px))));
    handle.setAttribute("aria-valuenow", String(Math.round(o.value())));
  };
  const reset = () => {
    o.reset();
    sync();
  };

  let drag = null;
  const pos = (e) => (vertical ? e.clientX : e.clientY);
  handle.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    e.preventDefault();
    handle.setPointerCapture(e.pointerId);
    sync();
    drag = { at: pos(e), from: o.value(), moved: false };
    handle.classList.add("drag");
    document.body.dataset.resizing = vertical ? "col" : "row";
  });
  handle.addEventListener("pointermove", (e) => {
    if (!drag || (!drag.moved && Math.abs(pos(e) - drag.at) < 2)) return;
    drag.moved = true;
    resize(drag.from + pos(e) - drag.at);
  });
  const stop = () => {
    if (!drag) return;
    if (drag.moved) o.save(o.value());
    drag = null;
    handle.classList.remove("drag");
    delete document.body.dataset.resizing;
  };
  handle.addEventListener("pointerup", stop);
  handle.addEventListener("lostpointercapture", stop);
  handle.addEventListener("dblclick", reset);
  handle.addEventListener("focus", sync);
  handle.addEventListener("keydown", (e) => {
    const [less, more] = vertical ? ["ArrowLeft", "ArrowRight"] : ["ArrowUp", "ArrowDown"];
    const step = (o.step?.() ?? 16) * (e.shiftKey ? 4 : 1);
    if (e.key === "Home" || e.key === "End") reset();
    else if (e.key === less || e.key === more) {
      sync();
      resize(o.value() + (e.key === more ? step : -step));
      o.save(o.value());
    } else return;
    sync();
    // Keep the page's own arrow-key handling (row selection) out of it.
    e.preventDefault();
    e.stopPropagation();
  });
  sync();
}

/** Detail view: width of the info column (`--split-meta`). */
function initMetaSplit() {
  const handle = $("#split-meta");
  const meta = $("#meta");
  // Same bounds as the CSS clamp() on .detail.
  splitter(handle, {
    value: () => meta.getBoundingClientRect().width,
    range: () => [220, Math.max(220, $("#detail").clientWidth * 0.7)],
    set: (px) => rootCss.setProperty("--split-meta", `${px}px`),
    save: (px) => setSplitPref("meta", px),
    reset: () => {
      rootCss.removeProperty("--split-meta");
      setSplitPref("meta", null);
    },
  });
}

/** Dashboard: height of the host panel's body (`--split-sys`, a max-height). */
function initSysSplit() {
  const handle = $("#split-sys");
  const body = $("#sys-body");
  let natural = 0;
  // Whole text lines below the top padding, so no row is cut in half.
  const line = () => parseFloat(getComputedStyle(body).lineHeight) || 19;
  const pad = () => parseFloat(getComputedStyle(body).paddingTop) || 0;
  const measure = () => {
    // Height with no limit: shrinking is all a limit can do.
    const limit = rootCss.getPropertyValue("--split-sys");
    rootCss.removeProperty("--split-sys");
    natural = body.getBoundingClientRect().height;
    if (limit) rootCss.setProperty("--split-sys", limit);
  };
  const reset = () => {
    rootCss.removeProperty("--split-sys");
    setSplitPref("sys", null);
  };
  splitter(handle, {
    start: measure,
    value: () => body.getBoundingClientRect().height,
    range: () => [Math.min(pad() + 2 * line(), natural), natural],
    step: line,
    snap: (px) => pad() + Math.round((px - pad()) / line()) * line(),
    set: (px) => rootCss.setProperty("--split-sys", `${px}px`),
    // At full height, store nothing, so a taller panel later is not cut off.
    save: (px) => (px >= natural - 1 ? reset() : setSplitPref("sys", px)),
    reset,
  });
}

/**
 * Dashboard: a splitter on the right edge of each column header from state
 * to ports. Dragging the border between two columns trades width between
 * just those two, so nothing else moves; `name` has no width of its own and
 * takes what the others leave. Widths are stored as % of the table.
 */
function initColumnSplits() {
  const table = $(".services");
  const cols = ["state", "name", "group", "image", "ports", "up"];
  const th = (c) => table.querySelector(`th.c-${c}`);
  const MIN = 56, MIN_NAME = 120;
  const stored = () => {
    const v = splitPref("cols", {});
    return v && typeof v === "object" ? v : {};
  };

  cols.slice(0, -1).forEach((left, i) => {
    const right = cols[i + 1];
    const handle = el("span", {
      class: "split col-split",
      role: "separator",
      "aria-orientation": "vertical",
      "aria-label": `resize ${th(left).textContent} column`,
      title: "drag to resize · double-click to reset",
      tabindex: "0",
    });
    th(left).append(handle);
    let total = 0, width = 0;
    const setCol = (c, px) => {
      if (c !== "name") rootCss.setProperty(`--col-${c}`, `${((px / width) * 100).toFixed(2)}%`);
    };
    const pct = (c) => Number(((th(c).getBoundingClientRect().width / width) * 100).toFixed(2));
    splitter(handle, {
      start: () => {
        width = table.getBoundingClientRect().width;
        total = th(left).getBoundingClientRect().width + th(right).getBoundingClientRect().width;
      },
      value: () => th(left).getBoundingClientRect().width,
      range: () => [left === "name" ? MIN_NAME : MIN, total - (right === "name" ? MIN_NAME : MIN)],
      set: (px) => {
        setCol(left, px);
        setCol(right, total - px);
      },
      save: () => {
        const v = stored();
        for (const c of [left, right]) if (c !== "name") v[c] = pct(c);
        setSplitPref("cols", v);
      },
      reset: () => {
        const v = stored();
        for (const c of [left, right]) {
          delete v[c];
          rootCss.removeProperty(`--col-${c}`);
        }
        setSplitPref("cols", Object.keys(v).length ? v : null);
      },
    });
  });
}

// ── live store (fed by /api/events) ────────────────────────

const store = {
  services: new Map(),
  status: new Map(),
  loaded: false,
  pending: new Set(),
};

let renderQueued = false;
let render = () => {};
function changed() {
  if (renderQueued) return;
  renderQueued = true;
  requestAnimationFrame(() => {
    renderQueued = false;
    document.body.classList.toggle("alert", [...store.services.values()].some(isAlarming));
    render();
  });
}

function setConn() {
  const conn = $("#conn");
  const down = [...store.status.values()].filter((s) => !s.connected);
  if (down.length) {
    conn.dataset.state = "down";
    conn.textContent = `${down[0].provider}: disconnected`;
    conn.title = down[0].error || "";
  } else if (store.status.size) {
    conn.dataset.state = "ok";
    conn.textContent = [...store.status.keys()].join(" ");
    conn.title = "connected";
  }
}

function connect() {
  const es = new EventSource("/api/events");
  const on = (name, fn) => es.addEventListener(name, (e) => fn(JSON.parse(e.data)));

  on("snapshot", (d) => {
    store.services = new Map(d.services.map((s) => [s.id, s]));
    store.status = new Map(d.status.map((s) => [s.provider, s]));
    store.loaded = true;
    setConn();
    changed();
  });
  on("upsert", (s) => {
    store.services.set(s.id, s);
    changed();
  });
  on("remove", (id) => {
    store.services.delete(id);
    changed();
  });
  on("status", (s) => {
    store.status.set(s.provider, s);
    setConn();
  });
  es.addEventListener("resync", () => {
    es.close();
    connect();
  });
  es.onerror = () => {
    // The browser retries on its own; the next snapshot restores everything.
    const conn = $("#conn");
    conn.dataset.state = "down";
    conn.textContent = "cuthulu: offline";
    conn.title = "lost connection to the cuthulu server, retrying";
  };
}

// ── actions ────────────────────────────────────────────────

async function act(s, action) {
  if (readOnly || store.pending.has(s.id)) return;
  if (action === "stop" && !confirm(`stop ${s.name}?`)) return;
  if (action === "restart" && s.is_self && !confirm("restart cuthulu itself? the page will reconnect.")) return;

  store.pending.add(s.id);
  changed();
  try {
    const res = await fetch(`/api/services/${encodeURIComponent(s.id)}/${action}`, {
      method: "POST",
      headers: { "X-Cuthulu": "1" },
    });
    const body = await res.json().catch(() => ({}));
    if (!res.ok) throw new Error(body.error || res.statusText);
    store.services.set(body.id, body);
    flash(`${action} ${s.name}: ok`);
  } catch (e) {
    flash(`${action} ${s.name}: ${e.message}`, true);
  } finally {
    store.pending.delete(s.id);
    changed();
  }
}

/** Buttons valid for the service's current state. */
function actionButtons(s, { all = false } = {}) {
  if (readOnly) return [];
  const up = UP.has(s.state);
  const busy = store.pending.has(s.id);
  const button = (action, enabled, danger = false) =>
    el(
      "button",
      {
        class: danger ? "btn danger" : "btn",
        type: "button",
        disabled: busy || !enabled,
        title: action === "stop" && s.is_self ? "cuthulu cannot stop itself" : null,
        onclick: (e) => {
          e.stopPropagation();
          act(s, action);
        },
      },
      action,
    );

  if (all) {
    return [button("start", !up), button("restart", true), button("stop", up && !s.is_self, true)];
  }
  if (!up) return [button("start", true)];
  return [button("restart", true), s.is_self ? null : button("stop", true, true)].filter(Boolean);
}

function toggleRun(s) {
  if (!s) return;
  if (!UP.has(s.state)) act(s, "start");
  else if (!s.is_self) act(s, "stop");
}

// ── dashboard ──────────────────────────────────────────────

function initIndex() {
  const rows = $("#rows");
  const filter = $("#filter");
  const showStopped = $("#show-stopped");
  let selected = null;
  let visible = [];

  const matches = (s, q) =>
    !q || [s.name, s.image, s.group].some((f) => f && f.toLowerCase().includes(q));

  const shown = (s) => showStopped.checked || UP.has(s.state);

  function row(s) {
    const label = stateLabel(s);
    return el(
      "tr",
      {
        "data-id": s.id,
        class: [UP.has(s.state) ? "" : "down", s.id === selected ? "sel" : ""].join(" ").trim() || null,
        onclick: () => (location.href = svcUrl(s.id)),
      },
      el("td", { class: "c-state" }, el("span", { class: `st ${label.cls}` }, label.text)),
      el(
        "td",
        { title: s.name },
        el("a", { href: svcUrl(s.id), onclick: (e) => e.stopPropagation() }, s.name),
        s.is_self ? el("span", { class: "self" }, "(this)") : null,
      ),
      el("td", { class: "c-group", title: s.group || "" }, s.group || ""),
      el("td", { class: "c-image", title: s.image || "" }, ...imageParts(s.image)),
      el("td", { class: "c-ports" }, portsText(s.ports)),
      el("td", { class: "c-up num" }, uptime(s)),
      el("td", { class: "c-act" }, ...actionButtons(s)),
    );
  }

  render = () => {
    const q = filter.value.trim().toLowerCase();
    const all = [...store.services.values()];
    visible = all
      .filter((s) => matches(s, q) && shown(s))
      .sort((a, b) => RANK[a.state] - RANK[b.state] || a.name.localeCompare(b.name));
    if (selected && !visible.some((s) => s.id === selected)) selected = null;

    if (!visible.length) {
      const hidden = all.filter((s) => matches(s, q) && !shown(s)).length;
      const msg = !store.loaded
        ? "watching…"
        : hidden
          ? `${hidden} stopped hidden · press a to show`
          : all.length ? "no match" : "no services found";
      rows.replaceChildren(el("tr", { class: "placeholder" }, el("td", { colspan: "7" }, msg)));
    } else {
      rows.replaceChildren(...visible.map(row));
    }

    const up = all.filter((s) => UP.has(s.state)).length;
    const bad = all.filter(isAlarming).length;
    fill(
      $("#counts"),
      el("b", {}, String(up)),
      " running · ",
      el("b", {}, String(all.length - up)),
      " stopped",
      !showStopped.checked && all.length > up ? el("span", { class: "muted" }, " (hidden)") : null,
      bad ? el("span", { class: "bad" }, ` · ${bad} failing`) : null,
    );
  };

  function move(delta) {
    if (!visible.length) return;
    const i = visible.findIndex((s) => s.id === selected);
    const next = i < 0 ? (delta > 0 ? 0 : visible.length - 1) : Math.min(visible.length - 1, Math.max(0, i + delta));
    selected = visible[next].id;
    render();
    rows.querySelector(`tr[data-id="${CSS.escape(selected)}"]`)?.scrollIntoView({ block: "nearest" });
  }

  const current = () => store.services.get(selected);

  filter.addEventListener("input", changed);
  showStopped.checked = pref("index.stopped", false);
  showStopped.addEventListener("change", () => {
    setPref("index.stopped", showStopped.checked);
    changed();
  });

  document.addEventListener("keydown", (e) => {
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    if (typing(e)) {
      if (e.key === "Escape") {
        e.target.value = "";
        e.target.blur();
        changed();
      } else if (e.key === "Enter" && e.target === filter) {
        filter.blur();
        move(1);
      }
      return;
    }
    switch (e.key) {
      case "/": e.preventDefault(); filter.focus(); break;
      case "j": case "ArrowDown": e.preventDefault(); move(1); break;
      case "k": case "ArrowUp": e.preventDefault(); move(-1); break;
      case "Enter": case "l": if (selected) location.href = svcUrl(selected); break;
      case "s": toggleRun(current()); break;
      case "r": if (current()) act(current(), "restart"); break;
      case "a": showStopped.click(); break;
      case "Escape": selected = null; changed(); break;
    }
  });
}

// ── service detail + logs ──────────────────────────────────

const MAX_LOG_LINES = 5000;

function initService() {
  const id = $("#detail").dataset.id;
  const log = $("#log");
  const status = $("#log-status");
  const followBtn = $("#log-follow");
  const logFilter = $("#log-filter");
  let follow = true;
  let es = null;
  let ended = false;
  let lastOpen = 0;
  let reopenTimer = null;
  let needle = "";

  const service = () => store.services.get(id);

  render = () => {
    const s = service();
    const line = $("#state-line");
    if (!s) {
      fill(line, el("span", { class: "st unknown" }, store.loaded ? "removed" : "…"));
      $("#actions").replaceChildren();
      return;
    }
    const label = stateLabel(s);
    const when = UP.has(s.state) ? `up ${uptime(s)}` : s.finished_at ? `stopped ${uptime(s)}` : "";
    fill(
      line,
      el("span", { class: `st ${label.cls}` }, label.text),
      when ? el("span", { class: "muted" }, ` · ${when}`) : null,
      s.is_self ? el("span", { class: "muted" }, " · this is cuthulu") : null,
    );
    $("#actions").replaceChildren(...actionButtons(s, { all: true }));
    for (const t of document.querySelectorAll(".time[data-time]")) {
      if (t.dataset.time) {
        t.textContent = fmtAgo(t.dataset.time);
        t.title = new Date(t.dataset.time).toLocaleString();
      }
    }
    if (ended && UP.has(s.state)) scheduleReopen();
  };

  // ── log stream

  function fmtTs(ts) {
    const d = new Date(ts);
    if (Number.isNaN(d.getTime())) return ts;
    const p = (n, w = 2) => String(n).padStart(w, "0");
    return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
  }

  function applyFilter(node) {
    node.classList.toggle("hide", needle !== "" && !node.dataset.lc.includes(needle));
  }

  /** Class list for a style span (see LogSpan in ARCHITECTURE.md). */
  function spanClass(s) {
    const cls = [];
    if (s.fg != null) cls.push(`a-f${s.fg}`);
    if (s.bg != null) cls.push(`a-b${s.bg}`, s.fg == null ? "a-on" : null);
    if (s.bold) cls.push("a-bold");
    if (s.dim) cls.push("a-dim");
    if (s.italic) cls.push("a-italic");
    if (s.underline) cls.push("a-ul");
    if (s.level) cls.push(`lv-${s.level}`);
    return cls.filter(Boolean).join(" ");
  }

  /** Line text with ANSI/level spans; offsets are UTF-16, as String#slice. */
  function logText(l) {
    if (!l.spans?.length) return l.text;
    const frag = document.createDocumentFragment();
    let at = 0;
    for (const s of l.spans) {
      if (s.start > at) frag.append(l.text.slice(at, s.start));
      frag.append(el("span", { class: spanClass(s) }, l.text.slice(s.start, s.end)));
      at = s.end;
    }
    if (at < l.text.length) frag.append(l.text.slice(at));
    return frag;
  }

  function addLines(lines) {
    const frag = document.createDocumentFragment();
    for (const l of lines) {
      const node = el(
        "div",
        { class: l.stream === "stderr" ? "ln e" : "ln" },
        l.ts ? el("span", { class: "ts", title: l.ts }, fmtTs(l.ts)) : null,
        logText(l),
      );
      node.dataset.lc = l.text.toLowerCase();
      applyFilter(node);
      frag.append(node);
    }
    log.append(frag);
    let extra = log.childElementCount - MAX_LOG_LINES;
    while (extra-- > 0) log.firstElementChild.remove();
    if (follow) log.scrollTop = log.scrollHeight;
  }

  function sysLine(text) {
    const node = el("div", { class: "ln sys" }, text);
    node.dataset.lc = "";
    log.append(node);
    if (follow) log.scrollTop = log.scrollHeight;
  }

  function open() {
    clearTimeout(reopenTimer);
    reopenTimer = null;
    ended = false;
    lastOpen = Date.now();
    log.replaceChildren();
    status.textContent = "streaming";
    es = new EventSource(`/api/services/${encodeURIComponent(id)}/logs`);
    es.addEventListener("lines", (e) => addLines(JSON.parse(e.data)));
    es.addEventListener("failure", (e) => {
      sysLine(`── error: ${e.data} ──`);
      end("error");
    });
    es.addEventListener("end", () => {
      sysLine("── log stream ended ──");
      end("ended");
    });
    // Never let the browser auto-retry: it would replay the history.
    es.onerror = () => end("disconnected");
  }

  function end(why) {
    if (ended) return;
    ended = true;
    es?.close();
    status.textContent = why;
    if (service() && UP.has(service().state)) scheduleReopen();
  }

  function scheduleReopen() {
    if (reopenTimer) return;
    const wait = Math.max(500, 3000 - (Date.now() - lastOpen));
    reopenTimer = setTimeout(open, wait);
  }

  function setFollow(on) {
    follow = on;
    followBtn.classList.toggle("on", on);
    if (on) log.scrollTop = log.scrollHeight;
  }

  log.addEventListener("scroll", () => {
    const atBottom = log.scrollHeight - log.scrollTop - log.clientHeight < 4;
    if (atBottom !== follow) setFollow(atBottom);
  });
  followBtn.addEventListener("click", () => setFollow(!follow));
  $("#log-clear").addEventListener("click", () => log.replaceChildren());

  let filterTimer;
  logFilter.addEventListener("input", () => {
    clearTimeout(filterTimer);
    filterTimer = setTimeout(() => {
      needle = logFilter.value.trim().toLowerCase();
      for (const node of log.children) applyFilter(node);
    }, 100);
  });

  const bindToggle = (input, key, cls, fallback) => {
    input.checked = pref(key, fallback);
    log.classList.toggle(cls, input.checked);
    input.addEventListener("change", () => {
      log.classList.toggle(cls, input.checked);
      setPref(key, input.checked);
    });
  };
  bindToggle($("#log-ts"), "logs.ts", "show-ts", false);
  bindToggle($("#log-wrap"), "logs.wrap", "wrap-lines", true);
  bindToggle($("#log-color"), "logs.color", "colors", true);

  document.addEventListener("keydown", (e) => {
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    if (typing(e)) {
      if (e.key === "Escape") e.target.blur();
      return;
    }
    switch (e.key) {
      case "/": e.preventDefault(); logFilter.focus(); break;
      case "f": setFollow(!follow); break;
      case "s": toggleRun(service()); break;
      case "r": if (service()) act(service(), "restart"); break;
      case "Escape": goBack(); break;
    }
  });

  open();
}

// ── host panel (fed by /api/system/stream) ─────────────────

/** htop-style sizes: 512K, 840M, 5.8G. */
function fmtBytes(b) {
  const K = 1024, M = K * K, G = M * K;
  if (b >= G) return `${(b / G).toFixed(1)}G`;
  if (b >= M) return `${Math.round(b / M)}M`;
  return `${Math.round(b / K)}K`;
}

/** ok / warn / err for a percentage; thresholds are in docs/DESIGN.md. */
const level = (pct, warn, err) => (pct >= err ? "err" : pct >= warn ? "warn" : "ok");

const METER_W = 26; // ch per CPU meter: label 4 + "[" + bar 20 + "]"
const SYS_TOP = 10; // processes listed (the server sends the top 10 by CPU and by memory)

/** Bytes per second, padded so the line does not jitter: "  1.2M/s". */
const fmtRate = (b) => `${b < 1024 ? `${b}B` : fmtBytes(b)}/s`.padStart(7);

/** Network speed the way ISPs quote it, for tooltips. */
const fmtBits = (b) => `${((b * 8) / 1e6).toFixed(2)} Mbit/s`;
const METER_GAP = 2; // ch between meter columns

/**
 * One text meter, `lbl[|||||||     text]`, like htop: `width` characters of
 * pipes and spaces with the value right-aligned at the end. The bar scales
 * to the room left of the value, and each pipe takes the color of the zone
 * it falls in (ok below `warn`%, warn below `err`%, err above), so a fuller
 * bar runs green → yellow → red.
 */
function meter(label, pct, text, width, [warn, err]) {
  const p = Math.max(0, Math.min(100, pct));
  // Reserve room for the widest value ("100.0%") plus a space, so the bar's
  // scale does not shift as the number changes width.
  const room = Math.max(0, width - Math.max(text.length, 6) - 1);
  const pipes = Math.round((p / 100) * room);
  const zone = (i) => level(((i + 1) / room) * 100, warn, err);
  const segs = [];
  for (let i = 0; i < pipes; i++) {
    const z = zone(i);
    if (segs.length && segs[segs.length - 1].z === z) segs[segs.length - 1].n++;
    else segs.push({ z, n: 1 });
  }
  return el(
    "div",
    {
      class: "meter",
      role: "meter",
      "aria-label": label.trim(),
      "aria-valuemin": "0",
      "aria-valuemax": "100",
      "aria-valuenow": String(Math.round(p)),
      "aria-valuetext": text,
    },
    el("span", { class: "lbl" }, label),
    el("span", { class: "br" }, "["),
    ...segs.map((g) => el("span", { class: `fill ${g.z}` }, "|".repeat(g.n))),
    " ".repeat(width - pipes - text.length),
    el("span", { class: "val" }, text),
    el("span", { class: "br" }, "]"),
  );
}

function initSystem() {
  const root = $("#sys");
  if (!root) return;
  const toggle = $("#sys-toggle");
  const stateEl = $("#sys-state");
  const narrow = matchMedia("(max-width: 600px)");
  let open = pref("sys.open", true);
  let byMem = pref("sys.mem", false);
  let es = null;
  let snap = null;
  let problem = "";

  function draw() {
    fill(stateEl, problem ? el("span", { class: "err" }, problem) : null);
    if (!snap) return;
    const s = snap;
    $("#sys-host").textContent = s.hostname || "";

    // Enough columns that the CPU block stays about four rows tall.
    const n = s.cpus.length;
    const cols = narrow.matches ? 1 : Math.min(8, Math.max(2, Math.ceil(n / 4)));
    const cpus = $("#sys-cpus");
    cpus.style.gridTemplateColumns = `repeat(${Math.min(cols, n || 1)}, ${METER_W}ch)`;
    cpus.replaceChildren(
      ...s.cpus.map((pct, i) =>
        meter(String(i).padStart(3) + " ", pct, `${pct.toFixed(1)}%`, METER_W - 6, [70, 90]),
      ),
    );

    // Memory meters span the CPU block's full width.
    const span = Math.min(cols, n || 1);
    const wide = span * METER_W + (span - 1) * METER_GAP - 6;
    const usage = (label, u, warn, err) => {
      const pct = u.total ? (u.used / u.total) * 100 : 0;
      return meter(label, pct, `${fmtBytes(u.used)}/${fmtBytes(u.total)}`, wide, [warn, err]);
    };
    fill($("#sys-mem"), usage("Mem ", s.mem, 75, 90), usage("Swp ", s.swap, 50, 80));

    const ncpu = n || 1;
    const rows = [
      ["Load", [
        el("span", { class: `lv-${level((s.load[0] / ncpu) * 100, 70, 100)}` }, s.load[0].toFixed(2)),
        ` ${s.load[1].toFixed(2)} ${s.load[2].toFixed(2)}`,
      ]],
      ["Tasks", [`${s.tasks}, ${s.threads} threads; ${s.running} running`]],
      ["Up", [fmtDur(s.uptime_secs * 1000)]],
    ];
    if (s.net) {
      const net = s.net;
      rows.push(["Net", [
        net.iface ? `${net.iface}  ` : "",
        el("span", { title: `download ${fmtBits(net.rx)}` }, `↓ ${fmtRate(net.rx)}`),
        "  ",
        el("span", { title: `upload ${fmtBits(net.tx)}` }, `↑ ${fmtRate(net.tx)}`),
      ]]);
      if (net.addrs.length) {
        const addrs = net.addrs.flatMap((a) => [
          el("span", { title: a.iface ? `on ${a.iface}` : "" }, a.ip),
          el("span", { class: "kind", title: a.iface ? `on ${a.iface}` : "" }, a.kind),
        ]);
        rows.push(["IP", [el("span", { class: "addrs" }, ...addrs)]]);
      }
    }
    if (s.disk) {
      rows.push(["Disk", [`read ${fmtRate(s.disk.read)}  write ${fmtRate(s.disk.write)}`]]);
    }
    fill($("#sys-info"), ...rows.flatMap(([k, v]) => [el("dt", {}, k), el("dd", {}, ...v)]));

    $("#sys-procs-box").hidden = !s.procs;
    if (!s.procs) return;

    for (const th of root.querySelectorAll("th[data-sort]")) {
      const on = (th.dataset.sort === "mem") === byMem;
      th.setAttribute("aria-sort", on ? "descending" : "none");
    }
    const key = byMem ? (p) => p.rss : (p) => p.cpu;
    const procs = [...s.procs].sort((a, b) => key(b) - key(a) || a.pid - b.pid).slice(0, SYS_TOP);
    const procRows = procs.map((p) =>
      el(
        "tr",
        {},
        el("td", { class: "p-pid num" }, String(p.pid)),
        el("td", { class: "p-user", title: `uid ${p.uid}` }, p.user || String(p.uid)),
        el("td", { class: "p-cpu num" }, p.cpu.toFixed(1)),
        el("td", { class: "p-mem num" }, p.mem.toFixed(1)),
        el("td", { class: "p-res num" }, fmtBytes(p.rss)),
        el("td", { class: "p-cmd", title: p.cmd }, p.cmd),
      ),
    );
    const body = $("#sys-procs");
    if (procRows.length) body.replaceChildren(...procRows);
    else body.replaceChildren(el("tr", { class: "placeholder" }, el("td", { colspan: "6" }, "no processes visible")));
  }

  // The stream is open only while the panel is expanded and the tab is
  // visible; the server samples only while some stream is open.
  function sync() {
    const want = open && document.visibilityState === "visible";
    if (want && !es) {
      es = new EventSource("/api/system/stream");
      es.addEventListener("system", (e) => {
        snap = JSON.parse(e.data);
        problem = "";
        draw();
      });
      es.addEventListener("failure", (e) => {
        problem = `unavailable: ${e.data}`;
        draw();
      });
      // The browser reconnects on its own; every event is a full snapshot.
      es.onerror = () => {
        problem = "reconnecting…";
        draw();
      };
    } else if (!want && es) {
      es.close();
      es = null;
    }
  }

  function setOpen(on) {
    open = on;
    root.dataset.open = String(on);
    toggle.setAttribute("aria-expanded", String(on));
    $("#sys-body").hidden = !on;
    setPref("sys.open", on);
    sync();
  }

  toggle.addEventListener("click", () => setOpen(!open));
  for (const b of root.querySelectorAll("button.sort")) {
    b.addEventListener("click", () => {
      byMem = b.dataset.sort === "mem";
      setPref("sys.mem", byMem);
      draw();
    });
  }
  document.addEventListener("visibilitychange", sync);
  narrow.addEventListener("change", draw);
  document.addEventListener("keydown", (e) => {
    if (e.ctrlKey || e.metaKey || e.altKey || typing(e)) return;
    if (e.key === "m") setOpen(!open);
  });

  setOpen(open);
  draw();
}

// ── service todos ──────────────────────────────────────────

function initTodos() {
  const id = $("#detail").dataset.id;
  const list = $("#todo-list");
  const count = $("#todo-count");
  const errBox = $("#todo-error");
  const form = $("#todo-form");
  const input = $("#todo-text");
  const base = `/api/services/${encodeURIComponent(id)}/todos`;
  let todos = [];
  let loaded = false;
  let busy = false;

  function showError(msg) {
    errBox.textContent = msg;
    errBox.hidden = !msg;
  }

  function item(t) {
    const mark = t.done ? "[x]" : "[ ]";
    const when = `added ${fmtAgo(t.created_at)}` + (t.done_at ? ` · done ${fmtAgo(t.done_at)}` : "");
    const check = readOnly
      ? el("span", { class: "todo-check" }, mark)
      : el(
          "button",
          {
            class: "todo-check",
            type: "button",
            "data-focus": `check-${t.id}`,
            "aria-pressed": String(t.done),
            title: t.done ? "mark open" : "mark done",
            onclick: () => send(`${base}/${t.id}/toggle`),
          },
          mark,
        );
    return el(
      "li",
      { class: t.done ? "todo done" : "todo" },
      check,
      el("span", { class: "todo-text", title: when }, t.text),
      readOnly
        ? null
        : el(
            "button",
            {
              class: "btn danger todo-del",
              type: "button",
              title: "delete",
              onclick: () => send(`${base}/${t.id}/delete`),
            },
            "del",
          ),
    );
  }

  function draw() {
    // Re-rendering replaces the buttons; keep keyboard focus on the same item.
    const focused = document.activeElement?.dataset?.focus;
    const done = todos.filter((t) => t.done).length;
    count.textContent = todos.length ? `${done}/${todos.length}` : "";
    count.title = `${done} of ${todos.length} done`;
    if (todos.length) list.replaceChildren(...todos.map(item));
    else list.replaceChildren(el("li", { class: "todo-empty muted" }, loaded ? "nothing to do" : "…"));
    if (focused) list.querySelector(`[data-focus="${CSS.escape(focused)}"]`)?.focus();
  }

  async function request(url, init) {
    const res = await fetch(url, init);
    const body = await res.json().catch(() => ({}));
    if (!res.ok) throw new Error(body.error || res.statusText);
    return body;
  }

  /** POSTs a change; the server answers with the service's full list. */
  async function send(url, payload) {
    if (busy) return false;
    busy = true;
    try {
      const headers = { "X-Cuthulu": "1" };
      if (payload) headers["Content-Type"] = "application/json";
      todos = await request(url, {
        method: "POST",
        headers,
        body: payload ? JSON.stringify(payload) : undefined,
      });
      showError("");
      return true;
    } catch (e) {
      showError(`todo: ${e.message}`);
      return false;
    } finally {
      busy = false;
      draw();
    }
  }

  form?.addEventListener("submit", async (e) => {
    e.preventDefault();
    const text = input.value.trim();
    if (!text) return;
    if (await send(base, { text })) input.value = "";
  });

  draw();
  request(base)
    .then((t) => {
      todos = t;
      loaded = true;
      draw();
    })
    .catch((e) => {
      loaded = true;
      draw();
      showError(`todo: ${e.message}`);
    });
}

if (page === "service") initTodos();

// ── tailscale ──────────────────────────────────────────────

/** Shows the topbar link to this machine in the Tailscale admin console. */
async function initTailscale() {
  try {
    const res = await fetch("/api/tailscale");
    const ts = res.ok ? await res.json() : null;
    if (!ts?.available || !/^https?:\/\//.test(ts.url ?? "")) return;
    const a = $("#tailscale");
    const where = [ts.host, ts.ip && `(${ts.ip})`].filter(Boolean).join(" ");
    const detail = [where, ts.tailnet && `on ${ts.tailnet}`].filter(Boolean).join(" ");
    const label = detail ? `tailscale admin: ${detail}` : "tailscale admin";
    a.href = ts.url;
    a.title = label;
    a.setAttribute("aria-label", label);
    a.hidden = false;
  } catch (_) {
    /* no tailscale: the button stays hidden */
  }
}

// ── boot ───────────────────────────────────────────────────

$("#theme").addEventListener("click", toggleTheme);
initBack();
initTailscale();
document.addEventListener("keydown", (e) => {
  if (e.ctrlKey || e.metaKey || e.altKey || typing(e)) return;
  if (e.key === "t") toggleTheme();
  if (e.key === "?") $("#help").showModal();
});

if (page === "index") initIndex();
if (page === "index") initSystem();
if (page === "index") initSysSplit();
if (page === "index") initColumnSplits();
if (page === "service") initService();
if (page === "service") initMetaSplit();
connect();
setInterval(changed, 10_000);
