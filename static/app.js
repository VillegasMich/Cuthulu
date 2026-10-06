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

const svcUrl = (id) => `/services/${encodeURIComponent(id)}`;

function typing(e) {
  const t = e.target;
  return t instanceof HTMLInputElement || t instanceof HTMLSelectElement || t instanceof HTMLTextAreaElement;
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
  const stateFilter = $("#state-filter");
  let selected = null;
  let visible = [];

  const matches = (s, q) =>
    !q || [s.name, s.image, s.group].some((f) => f && f.toLowerCase().includes(q));

  const matchesState = (s, want) =>
    !want || (want === "up" ? UP.has(s.state) : !UP.has(s.state));

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
      el("td", { class: "c-image", title: s.image || "" }, s.image || ""),
      el("td", { class: "c-ports" }, portsText(s.ports)),
      el("td", { class: "c-up num" }, uptime(s)),
      el("td", { class: "c-act" }, ...actionButtons(s)),
    );
  }

  render = () => {
    const q = filter.value.trim().toLowerCase();
    const all = [...store.services.values()];
    visible = all
      .filter((s) => matches(s, q) && matchesState(s, stateFilter.value))
      .sort((a, b) => RANK[a.state] - RANK[b.state] || a.name.localeCompare(b.name));
    if (selected && !visible.some((s) => s.id === selected)) selected = null;

    if (!visible.length) {
      const msg = !store.loaded ? "watching…" : all.length ? "no match" : "no services found";
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
  stateFilter.addEventListener("change", changed);

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
      case "Escape": location.href = "/"; break;
    }
  });

  open();
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

// ── boot ───────────────────────────────────────────────────

$("#theme").addEventListener("click", toggleTheme);
document.addEventListener("keydown", (e) => {
  if (e.ctrlKey || e.metaKey || e.altKey || typing(e)) return;
  if (e.key === "t") toggleTheme();
  if (e.key === "?") $("#help").showModal();
});

if (page === "index") initIndex();
if (page === "service") initService();
connect();
setInterval(changed, 10_000);
