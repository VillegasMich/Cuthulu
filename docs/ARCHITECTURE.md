# Architecture

Status: **implemented** for the Docker provider (roadmap phases 0–4). Update
this file in the same change when a decision moves.

## Overview

```
 Browser ──HTTP/SSE──▶ cuthulu (single Rust binary, in a container)
                          │
                          ├── web       : HTML page shells + embedded static assets
                          ├── api       : JSON + Server-Sent Events
                          ├── registry  : in-memory view of all services
                          ├── providers
                          │     └── docker ──unix socket──▶ /var/run/docker.sock
                          ├── system    : host CPU/memory/network/disk ──▶ procfs (read-only)
                          └── notify    : alerts, heartbeat ──▶ SMTP, healthcheck URL (outbound only)
                          └── tailscale : admin console link ──unix socket──▶ tailscaled LocalAPI (GET status only)
```

One process, one binary, no database. Docker is the source of truth; the
registry is a cache of it kept current by events. The only state Cuthulu owns
itself (per-service TODOs, which services to alert about) lives in two small
JSON files in `CUTHULU_DATA_DIR`.

## Stack

| Concern            | Choice                                   | Why |
|--------------------|------------------------------------------|-----|
| Language           | Rust (edition 2024, MSRV 1.88)           | Small static binary, low idle footprint for an always-on tool |
| Async runtime      | `tokio`                                  | Standard |
| HTTP server        | `axum` 0.8                               | Tokio-native, first-class SSE |
| Docker client      | `bollard`                                | Async Docker Engine API client over the unix socket |
| Templates          | `askama`                                 | Compile-time checked HTML page shells |
| Frontend behaviour | vanilla JS (`static/app.js`), no framework | Live table, log viewer and keyboard handling are JS anyway; no Node toolchain, no build step |
| Styling            | hand-written CSS with custom properties  | Full control of the look; themes via CSS variables |
| Asset embedding    | `rust-embed`                             | One binary; works offline. In debug builds files are read from disk, so CSS/JS edits need no rebuild |
| Errors             | `thiserror` (library), `anyhow` (`main.rs` only) | |
| Logging            | `tracing` + `tracing-subscriber`         | `RUST_LOG` filter |
| Email              | `lettre` (tokio transport, rustls + bundled Mozilla roots) | Async SMTP; the `scratch` image has no CA store |
| Healthcheck ping   | `ureq` (rustls) on the blocking pool     | One GET every few minutes; smaller than an async client stack |

> **Decision (2026-10):** the first draft planned `htmx`. Because every
> dynamic part of the UI is driven by SSE events that patch rows client-side,
> htmx would have added a dependency without removing any JS. Dropped.

All frontend assets (JS, CSS, the JetBrains Mono font) live under `static/`
and are embedded in the binary. Nothing is loaded from a CDN.

UI preferences (theme, toggles, pane sizes from the splitters) are kept per
browser in `localStorage` under `cuthulu.*` keys — never on the server. Every
access is wrapped in `try`/`catch`, so the UI works with storage blocked.
`static/theme.js` runs in `<head>`, before first paint, and applies the
stored theme and pane sizes (as CSS custom properties on `<html>`), so pages
do not flash or jump on load.

## Source layout

```
src/
  main.rs              CLI (run | healthcheck | --version), tracing, graceful shutdown
  lib.rs
  config.rs            CUTHULU_* env parsing (pure, unit-tested via a lookup fn)
  envfile.rs           optional ./.env reader layered under the real environment
  build_info.rs        version + repo URL (Cargo.toml) + git commit injected at build time
  model.rs             Service, ServiceId, ServiceState, Action, LogLine, …
  registry.rs          in-memory state, watch loop per provider, broadcast; MockProvider for tests
  server.rs            AppState, router, security headers; HTTP-level tests
  web.rs               askama page handlers, embedded asset handler (ETag)
  todos.rs             per-service TODO store: JSON file, in-memory cache, atomic writes
  notify/
    mod.rs             Notifier: alert loop on the registry broadcast, email outbox, shutdown email
    alerts.rs          pure down/up state machine: settle, cooldown, operator actions
    mail.rs            Mailer trait, SMTP (lettre), email texts
    heartbeat.rs       healthcheck pings (ureq), latest attempt for the topbar, error redaction
    store.rs           watched services + global switch: notify.json, atomic writes
  tailscale.rs         tailscaled LocalAPI status → admin console link (cached, on demand)
  hosts.rs             allowed Host names (DNS-rebinding protection), CUTHULU_ALLOWED_HOSTS
  api/
    mod.rs             /api router
    services.rs        list / detail / actions
    events.rs          SSE: registry changes
    logs.rs            SSE: log lines
    system.rs          host snapshot + SSE stream
    guard.rs           CSRF / same-origin check for POSTs
    todos.rs           per-service TODO list / create / toggle / delete
    notify.rs          notification settings, watch toggle, test
    healthcheck.rs     heartbeat status + dashboard link for the topbar
    tailscale.rs       link to this machine in the Tailscale admin console
    version.rs         running build (version, commit)
    error.rs           ApiError → JSON { error, code }
  providers/
    mod.rs             Provider trait, ProviderError, ProviderEvent
    docker.rs          bollard-backed implementation (the only bollard user)
    lines.rs           log chunk → line splitting, timestamp parsing
    ansi.rs            ANSI SGR → style spans, other escapes stripped
    level.rs           level keyword detection (INFO, level=warn, …) for uncolored lines
  system/
    mod.rs             SystemMonitor: on-demand shared sampler, Snapshot, rates, addresses, top processes
    proc.rs            pure parsers for /proc/stat, meminfo, loadavg, uptime, diskstats, net/{dev,route,fib_trie,if_inet6},
                       [pid]/stat|status|cmdline, /etc/passwd
templates/             base, index, service, not_found
static/                app.css, app.js, theme.js, eye.svg, fonts/
```

## Core model

```rust
/// "<provider>:<native id>", e.g. "docker:3f2a9c…". Validated on parse.
pub struct ServiceId(String);

pub enum ServiceState { Running, Restarting, Paused, Created, Stopped, Dead, Unknown }
pub enum Health { Healthy, Unhealthy, Starting, None }

pub struct Service {
    pub id: ServiceId,
    pub provider: ProviderKind,
    pub name: String,
    pub image: Option<String>,
    pub state: ServiceState,
    pub health: Health,
    pub started_at: Option<String>,   // RFC 3339; the client computes uptime
    pub finished_at: Option<String>,
    pub exit_code: Option<i64>,       // only when stopped/dead
    pub ports: Vec<PortMapping>,
    pub group: Option<String>,        // compose project
    pub is_self: bool,
}
```

`ServiceDetail` adds command, restart policy/count, mounts, networks, labels
and **env var names only** — values are never read into the model.

## Provider abstraction (the scalability seam)

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    async fn list(&self) -> Result<Vec<Service>>;
    async fn get(&self, id: &ServiceId) -> Result<Option<Service>>;
    async fn detail(&self, id: &ServiceId) -> Result<ServiceDetail>;
    async fn logs(&self, id: &ServiceId, opts: LogOptions) -> Result<BoxStream<'static, Result<LogLine>>>;
    async fn act(&self, id: &ServiceId, action: Action) -> Result<()>;
    fn events(&self) -> BoxStream<'static, Result<ProviderEvent>>;   // Changed(id) | Removed(id)
}
```

`async-trait` keeps `Arc<dyn Provider>` possible (native async fn in traits is
not dyn-compatible). Adding systemd later = one new file in `providers/`.

Docker specifics:
- `list()` = one `GET /containers/json?all=1`, then inspect each container with
  bounded concurrency (16), because only inspect has `StartedAt`, health and
  exit code.
- Events are filtered to container events and to the actions that change what
  is displayed (`start`, `die`, `health_status: …`, `destroy`, …); noisy
  `exec_*` events from healthchecks are ignored.
- `act()` treats HTTP 304 ("already started/stopped") as success.

## Registry and live updates

Per provider, a watch loop:

1. subscribe to `events()`, then `list()` (subscribe first so nothing is lost);
2. apply each event by re-reading just that service (`get`);
3. every `CUTHULU_RECONCILE_SECS` (default 60) re-`list()` to heal anything
   missed;
4. on any error: mark the provider disconnected, back off (1s → 30s), restart.

Every change is diffed against the cache and only real changes are broadcast
on a bounded `tokio::sync::broadcast` channel (capacity 1024).

**No per-container polling.** Idle cost does not grow with the number of
services.

## HTTP API

| Method | Path                               | Description |
|--------|------------------------------------|-------------|
| GET    | `/`                                | Dashboard page |
| GET    | `/services/{id}`                   | Service detail page |
| GET    | `/static/{path}`                   | Embedded assets (`ETag`, `Cache-Control: no-cache`) |
| GET    | `/healthz`                         | Liveness (`ok`) |
| GET    | `/api/services`                    | JSON list; `?q=` (name/image/project), `?state=`, `?group=` |
| GET    | `/api/services/{id}`               | JSON detail |
| POST   | `/api/services/{id}/start\|stop\|restart` | Returns the updated service |
| GET    | `/api/services/{id}/logs`          | SSE log stream; `?tail=` (≤ 10 000), `?follow=` |
| GET    | `/api/events`                      | SSE registry stream |
| GET    | `/api/system`                      | JSON host snapshot (CPU, memory, load, network, disk I/O, opt-in top processes); 503 if procfs is unreadable |
| GET    | `/api/system/stream`               | SSE host snapshots, one per `CUTHULU_SYSTEM_SECS` |
| GET    | `/api/services/{id}/todos`         | The service's TODO items, oldest first |
| POST   | `/api/services/{id}/todos`         | Add an item, body `{"text": "..."}`; returns the list |
| POST   | `/api/services/{id}/todos/{todo_id}/toggle` | Flip done; returns the list |
| POST   | `/api/services/{id}/todos/{todo_id}/delete` | Remove; returns the list |
| GET    | `/api/notify`                      | `{enabled, watched, email, healthcheck, cooldown_minutes, restarted_elsewhere}`; never addresses or URLs |
| POST   | `/api/notify`                      | Global alert switch, body `{"enabled": bool}`; returns the state |
| POST   | `/api/services/{id}/notify`        | Watch / unwatch, body `{"watch": bool}`; returns the state |
| POST   | `/api/notify/test`                 | Send a test email and one ping now; `{email, healthcheck}` each `{status: sent\|off\|failed, error?}` |
| GET    | `/api/healthcheck`                 | `{available, url, state: ok\|failed\|skipped\|pending\|off, last_ping_at, error}` for the topbar's healthchecks.io link; `url` is the dashboard, never the ping URL; always 200 |
| GET    | `/api/tailscale`                   | `{available, url, tailnet, host, ip}` for the topbar's Tailscale admin link; always 200 |
| GET    | `/api/version`                     | `{"version": "0.1.0", "git_sha": "<full sha>" \| null}` |

Errors: JSON `{ "error": "...", "code": "bad_request|forbidden|not_found|unavailable|internal" }`.

### SSE events

`/api/events`: `snapshot` `{services, status}` on connect, then `upsert`
(service), `remove` (id), `status` (provider connection). A client that lags
past the channel capacity gets `resync` and the stream ends; it reconnects and
receives a fresh snapshot.

`/api/services/{id}/logs`: `lines` (JSON array, lines batched in 50 ms windows,
≤ 500 per event), `failure` (message), `end` (the container stopped writing).

`/api/system/stream`: `system` (snapshot JSON) every interval, the latest one
replayed on connect when it is still fresh; `failure` (message) when procfs
cannot be read — the stream stays open and recovers. Every event is a full
snapshot, so a lagging client simply skips some (no `resync`).
A log line is `{ts, stream, text, spans?}`. `text` is plain (escapes removed),
so filtering works on it. `spans` is present only when part of the line is
styled: sorted, non-overlapping `{start, end, fg?, bg?, bold?, dim?, italic?,
underline?, level?}`. Offsets are UTF-16 code units, end exclusive — exactly
JavaScript's `text.slice(start, end)`. `fg`/`bg` are 16-color palette indices
(0–7 normal, 8–15 bright); `level` is `debug` | `info` | `warn` | `error`.

Every event carries non-empty `data` — browsers silently drop events whose
data is empty.

## Logs

- Docker log frames are reassembled into lines per stream (stdout/stderr),
  timestamps split off. A line longer than 16 KiB is emitted in pieces
  instead of buffered forever.
- ANSI SGR sequences become style spans: reset, bold, dim, italic,
  underline, the 16 standard + bright fg/bg colors. 256-color and truecolor
  values are mapped to the nearest of the 16, so every color comes from the
  theme's `--ansi-*` tokens and stays readable. Other escapes (cursor
  movement, OSC titles, charset switches) are stripped.
- A line with no ANSI color gets its first level keyword marked: bare
  uppercase words (`INFO`, `[WARN]`, `ERROR:`, `FATAL`, `DEBUG`, …),
  `level=` / `lvl=` / `severity=` key-value and JSON forms (case-insensitive),
  and the glog `I1005 …` prefix. Lowercase prose (`an error occurred`) is not
  matched, to avoid false positives.
- The browser builds styled lines from spans with `textContent` only and can
  turn colors off (`color` toggle, stored in `localStorage`).
- The browser keeps at most 5 000 lines and never lets `EventSource`
  auto-retry (that would replay history). When the container is running
  again, the client reopens the stream itself.
- Backpressure is natural: the SSE body is only polled as fast as the client
  reads, which in turn slows the read from Docker.

## Host system panel

The dashboard's host panel is **not** a `Provider`: the host is not a service
source and has no actions. `src/system/` reads procfs directly — no PTY, no
`htop`/`ps`/`ip`/speedtest or any other binary, so it works in the `scratch`
image.

- **What is read:** `stat` (aggregate + per-core CPU ticks), `meminfo`
  (used = `MemTotal − MemAvailable`, falling back to htop's
  free/buffers/cache formula on old kernels), `loadavg` (load, runnable and
  total threads), `uptime`, `diskstats`, `sys/kernel/hostname` (skipped
  inside a Docker container, where it names the container), and the
  network files below. The pid directories are only counted (`tasks`).
- **Network:** `net/dev` (byte counters), `net/route` (the default route
  picks the primary interface; the longest matching prefix maps each address
  to its interface), `net/fib_trie` (the host's IPv4 addresses: its
  `/32 host LOCAL` leaves) and `net/if_inet6` (global, non-temporary IPv6).
  `<proc>/net` links to `self/net`, the *reader's* network namespace — in a
  container that is the container's — so `<proc>/1/net` (pid 1, the host's
  namespace) is read first, falling back to `<proc>/net`. Loopback and
  container/VM plumbing (`docker*`, `br-*`, `veth*`, `virbr*`, `cni*`,
  `flannel*`, `cali*`, `vxlan*`) are left out. Addresses without a
  main-table route (e.g. a VPN using its own table) are listed without an
  interface. Each address gets a `kind`: by interface name first
  (`tailscale*`, `wg*`, `zt*`, `tun*`/`tap*`), then by range — private,
  link-local and ULA are `local`; `100.64.0.0/10` is `cgnat` on the uplink
  and `tailscale` elsewhere (Tailscale allocates from it), as is
  `fd7a:115c:a1e0::/48`; anything else is `public`.
- **Rates** (CPU%, network ↓/↑ and disk read/write) are deltas between two
  reads over the time between them. "Network speed" is the *current
  throughput* of the default-route interface, not a bandwidth test: that
  would need an external server and generate traffic. Disk I/O sums whole
  physical disks only (partitions, loop, ram, optical, `dm-*` and `md*` are
  skipped so nothing counts twice). A one-off `GET /api/system` with no
  recent baseline reads twice, 250 ms apart.
- **Processes (opt-in, `CUTHULU_SYSTEM_PROCESSES`):** per pid `stat`
  (utime+stime, start time) and `status` (uid, `VmRSS`); `cmdline` only for
  the processes that make the top lists; uids named from `/etc/passwd` when
  readable (read once at startup). Per-process CPU% is relative to one core
  like htop, so it can exceed 100; processes are matched across reads by
  pid *and* start time, so a reused pid never inherits another process's
  ticks. The snapshot carries the union of the top 10 by CPU and the top 10
  by RSS (≤ 20 rows) so the client can sort by either; `procs` is absent
  from the JSON when disabled, and no per-pid file is read at all.
- **Robustness:** pids that vanish mid-scan, or are hidden by `hidepid`, are
  skipped. Unreadable network or disk files just leave `net` / `disk` null.
  An unreadable proc dir (`stat`, `meminfo`, …) is an error (`503` /
  `failure` event), not a crash.
- **Cost:** one shared sampler task, started by the first subscriber and
  stopped at the next tick after the last one leaves (the receiver count is
  checked under the same lock that starts it, so no subscriber is ever left
  without a sampler). Nobody watching = no sampling. Without processes a
  tick reads about ten small files; the work runs on the blocking pool.
- **Bounded:** broadcast capacity 4; ≤ 12 addresses; ≤ 20
  processes per snapshot; command lines cut at 512 bytes.

> **Decision (2026-10):** host updates use a dedicated
> `/api/system/stream` instead of a new event on `/api/events`. Every page
> holds `/api/events` open (the detail page too), so riding on it would
> sample whenever any tab is open. A separate stream ties the sampler's
> lifetime to the panel: the client opens it only while the panel is
> expanded and the tab visible. Snapshots also need different lag handling
> (skip, not resync).

> **Decision (2026-10):** the process list is off by default
> (`CUTHULU_SYSTEM_PROCESSES=true` enables it). The panel is for an
> at-a-glance view of the machine; per-process detail is noise for most
> users and the most expensive part of a sample.
## Service TODOs

Each service has a small TODO list on its detail page.

- **Keyed by service name**, not id: the name survives `docker compose up`
  re-creating the container, the id does not. `{id}` in the URL must belong
  to a service the registry currently knows; it is resolved to its name.
  Items of services that disappeared stay in the file. (If a second provider
  ever produces clashing names, the key will need a provider prefix.)
- **Storage:** `<CUTHULU_DATA_DIR>/todos.json`,
  `{version, next_id, services: {name: [Todo]}}` with
  `Todo = {id, text, done, created_at, done_at}` (RFC 3339 UTC). Ids are
  global and never reused.
- Loaded once at startup into memory. Every write is serialised behind a
  mutex, written to `todos.json.tmp`, fsynced and renamed over the file, and
  only then committed to the cache.
- If the directory is not writable the app still starts; writes return
  `503 unavailable` with the reason and the UI shows it. A file that cannot be
  parsed is never overwritten: every TODO request fails until it is fixed.
- **Bounds:** 200 items per service, 500 characters per item, text is trimmed
  and must be non-empty and free of control characters.
- Writes are POST + same-origin check and refused under `CUTHULU_READ_ONLY`
  (the UI still lists items, without edit controls).

## Notifications

Three independent parts, all optional:

| Part | When | Channel |
|------|------|---------|
| **Service alerts** | a *watched* service goes down, crashes and is restarted, and when it is back | email + browser |
| **Shutdown email** | Cuthulu itself stops cleanly (SIGTERM / SIGINT) | email |
| **Heartbeat** | every `CUTHULU_HEALTHCHECK_INTERVAL_MINUTES` | GET `CUTHULU_HEALTHCHECK_URL` (e.g. healthchecks.io) |

Modelled on `auto-git-commit-tool`'s notifications (same variable names with
a `CUTHULU_` prefix, same TLS rules, same heartbeat semantics).

- **Watched services** are chosen in the UI (bell per service) and stored
  **by name** in `<CUTHULU_DATA_DIR>/notify.json`,
  `{version, enabled, watched: [name]}`, with the same atomic-write and
  never-overwrite-a-corrupt-file rules as `todos.json`. At most 500 names.
  `enabled` is the global switch for service alerts (email and browser); it
  does not affect the heartbeat or the shutdown email. Writes are POST +
  same-origin and refused under `CUTHULU_READ_ONLY`.
- **Down** means: state is not running/paused, or the health check reports
  unhealthy, or the container was removed. Cuthulu itself is never alerted
  on (its shutdown has its own email).
- **No polling.** One task follows the registry broadcast and keeps a small
  per-name state machine (`notify/alerts.rs`, pure and unit-tested). It
  sleeps until the next deadline (settle or cooldown end) or the next event.
  A lagging subscriber resyncs from the registry snapshot.
- **Email rules:**
  - *Settle 30 s:* a change must last 30 s before it is mailed, so
    `docker restart` and `docker compose up` re-creating a container stay
    silent.
  - *One per transition:* "down", then "back up" only after a "down" was
    sent.
  - *Cooldown* (`CUTHULU_NOTIFY_COOLDOWN_MINUTES`, default 15): at most one
    "down" per service per cooldown. A crash loop sends one email, then — if
    it is still down when the cooldown ends — one more that says how often it
    went down in between. Short outages during the cooldown that recover are
    not mailed.
  - *Crashed and restarted:* a non-clean exit (any code but 0, 130, 137,
    143), `dead` or `restarting`, followed by running again *before* the
    settle ends — a restart policy or a systemd `Restart=` hiding the crash —
    sends one email right away ("crashed and was restarted"), under the same
    cooldown. The exit code is read before a `--rm` container disappears.
  - *Operator actions are labelled:* a down flip within 60 s of a
    stop/restart requested through Cuthulu is mailed as "stopped from
    cuthulu" and is exempt from the cooldown (deliberate, rare, and a
    repeated test should not go silent). A quick restart stays silent as
    usual.
  - *Started by something else:* a service stopped through Cuthulu that is
    running again within 15 min without a start from Cuthulu was brought
    back by something Cuthulu does not control — typically a systemd unit
    running `docker run --rm` with `Restart=always`, where stopping the
    container cannot keep it down. The "back up" email says so and suggests
    stopping the unit; `GET /api/notify` lists such services in
    `restarted_elsewhere` (in memory, cleared when the service goes down
    again or Cuthulu restarts) and the UI marks them.
  - Outages that began before Cuthulu saw the service, or while it was not
    watched, are not reported.
  - Sending is best effort: a bounded outbox (32), 3 attempts 10 s / 20 s
    apart, no retry on permanent SMTP errors; failures are logged.
- **Browser alerts** are computed client-side from `/api/events` while a
  Cuthulu page is open: a watched service that flips from up to down and is
  still down 10 s later raises a desktop `Notification` (tag per service, at
  most one per service per minute), or an in-page flash when permission is
  not granted. Permission is only requested from a click (the bell, the
  alerts switch, "allow", "send test"). A stop/restart clicked in the same
  tab is alerted with "stopped from the dashboard". Independently of
  watching, a service stopped from this tab that comes back on its own
  raises a warning flash. The Notification API needs a secure context:
  `localhost` / `127.0.0.1` (or an SSH tunnel to them) qualify, a plain-HTTP
  LAN address does not — then only the flash is shown.
- **Shutdown email** after the HTTP server has stopped, within 8 s (fits
  `docker stop`'s default 10 s grace). Lists watched services that are down.
  A crash or `SIGKILL` sends nothing; that is what the heartbeat is for.
- **Heartbeat:** a GET to the URL right after start and then every interval.
  A ping is skipped while a provider is disconnected (Cuthulu that cannot
  see Docker is not watching anything), and retried 5 s later. Nothing is
  sent on shutdown (no `/fail` ping): as in the reference, a stopped
  Cuthulu is reported by healthchecks.io once period + grace pass, and the
  shutdown email says it was deliberate. A `200 OK (not found)` answer (an
  unknown check) counts as a failure. Failures are logged once, then again
  when pings recover.
- **Topbar button:** with `CUTHULU_HEALTHCHECK_URL` set, a pulse-line icon
  links to the healthchecks.io dashboard (`https://healthchecks.io/`, or
  `CUTHULU_HEALTHCHECK_LINK`) — not the check's own page, whose address
  holds the ping UUID. Its tooltip comes from Cuthulu's own latest attempt
  (heartbeat or test ping), kept in memory, latest only: `last ping ok 2m
  ago`, `last ping failed 30s ago: <error>`, `ping skipped …` (a provider is
  disconnected), `no ping yet`, or `pings off` (`CUTHULU_NOTIFY_ENABLED=false`).
  Failed and skipped tint the icon with `--err`: either way healthchecks.io
  will report Cuthulu down. No call to the healthchecks.io API is made. The
  page fetches `GET /api/healthcheck` on load, when it becomes visible and
  at most once a minute while visible; "2m ago" is computed client-side.
  `CUTHULU_HEALTHCHECK_LINK` without a ping URL is accepted but has no
  effect, and a warning says so at startup and every hour.
- **Secrets:** the SMTP password and the ping URL are wrapped in a type whose
  `Debug` prints `[redacted]`; config errors never echo them; the API only
  says whether each channel is configured. Ping errors are scrubbed of the
  URL, its path and long path segments (UUID, ping key) and cut to 160
  characters before they are logged, stored or returned.

> **Decision (2026-10):** stops requested through Cuthulu's own buttons are
> *labelled*, not suppressed (first shipped suppressed; changed after testing
> showed a silent stop looks like broken notifications, and stops of
> systemd-managed containers need the "started by something else" follow-up
> anyway).
>
> **Decision (2026-10):** Cuthulu does not stop systemd units. It only talks
> to Docker; reaching the host's system and user D-Bus from the container
> would be a separate provider with its own security review. It detects and
> explains the situation instead.

## Tailscale admin link

A topbar button opens this machine's page in the Tailscale admin console,
`https://login.tailscale.com/admin/machines/<Tailscale IPv4>`, or the machine
list when the node has no IPv4 yet (logged out, starting). Like the host
panel it is **not** a `Provider`: Tailscale is not a service source.

- **Source:** tailscaled's LocalAPI on its unix socket
  (`CUTHULU_TAILSCALE_SOCKET`), `GET /localapi/v0/status?peers=false` over
  HTTP/1.1 (hyper, no extra client). Only `Self.TailscaleIPs`,
  `Self.DNSName` / `HostName` and `CurrentTailnet.Name` (the current
  profile's tailnet) are deserialized; keys, peers and users never enter the
  model. No other LocalAPI endpoint is ever called.
- **Cost:** on demand — each page asks once — and cached for 60 s, failures
  included; concurrent requests share one lookup. 2 s timeout, body capped at
  256 KiB.
- **Unavailable** (no socket, tailscaled down, empty
  `CUTHULU_TAILSCALE_SOCKET`): `available: false` and the button stays
  hidden. `CUTHULU_TAILSCALE_URL` overrides the link and needs no socket;
  when the socket also answers, the tooltip still names host and tailnet.
- **Permissions:** tailscaled gives a client that is neither root nor the
  configured operator read-only access: `status`, `prefs` and `whois` read
  fine, nothing can be changed, `profiles/` is refused (tailscaled 1.102).
  The image runs as 65532, so that is all a mounted socket offers it.
- The admin console opens in the tailnet the *browser* is signed into; if
  that differs from the node's tailnet, Tailscale shows its own error.

> **Decision (2026-10):** the link is resolved in the browser
> (`fetch('/api/tailscale')` after load) rather than rendered into the page
> shell, so a slow or missing tailscaled never delays a page.

## Self-awareness

Cuthulu marks its own container `is_self` when the container carries the
`cuthulu.self=true` label (set by the image) or, inside a container, when its
id starts with `$HOSTNAME`. Stop is refused for it (UI and API); restart is
allowed after a confirmation.

## Configuration

Settings come from environment variables. A `.env` file in the working
directory is read too (so `cargo run` sees the same settings as
`docker compose`, which reads that file): Compose syntax without
interpolation — `KEY=value`, `#` comments, optional `export`, `'literal'` or
`"escaped"` quotes. The real environment always wins over the file, the file
is never copied into the process environment (the crate forbids `unsafe`, and
`set_var` is unsafe in edition 2024), and only variable *names* are logged. A
malformed file stops startup with the line number, never the value. In the
image the working directory is `/` and `.env` is excluded from the build
context, so containers get their settings from Compose as before.

| Variable                 | Default                        | Meaning |
|--------------------------|--------------------------------|---------|
| `CUTHULU_BIND`           | `127.0.0.1:8686` (image: `0.0.0.0:8686`) | Listen address |
| `CUTHULU_ALLOWED_HOSTS`  | unset                          | Extra `Host` names to answer to, comma-separated: `name`, `.domain` (it and its subdomains) or `*` (any). Always allowed: `localhost`, IPs, single-label names, `*.ts.net`, `.local`, `.lan`, `.home.arpa`, `.internal`, `.localhost` |
| `CUTHULU_DOCKER_HOST`    | `unix:///var/run/docker.sock`  | Docker endpoint (`unix://` or `tcp://`) |
| `CUTHULU_READ_ONLY`      | `false`                        | Refuse start/stop/restart, hide the buttons |
| `CUTHULU_LOG_TAIL`       | `500`                          | History lines per log view (max 10 000) |
| `CUTHULU_RECONCILE_SECS` | `60`                           | Full re-list interval |
| `CUTHULU_PROC_DIR`       | `/proc` (compose: `/host/proc`) | procfs the host panel reads; mount the host's read-only in a container |
| `CUTHULU_SYSTEM_SECS`    | `2`                            | Host panel sampling interval (1–60), only while someone watches |
| `CUTHULU_SYSTEM_PROCESSES` | `false`                      | Also list the top processes (by CPU / memory) in the host panel |
| `CUTHULU_DATA_DIR`       | `./data` (image: `/data`)      | Directory for `todos.json` and `notify.json`; created on first write |
| `CUTHULU_NOTIFY_ENABLED` | `true`                         | Master switch for email and healthcheck pings (`false` keeps the settings but sends nothing) |
| `CUTHULU_SMTP_HOST`      | —                              | SMTP server; unset = no email |
| `CUTHULU_SMTP_PORT`      | `465`                          | `465` = implicit TLS, any other port = STARTTLS |
| `CUTHULU_SMTP_TLS`       | from the port                  | `implicit` \| `starttls` \| `none` (`none` only for a server on localhost) |
| `CUTHULU_SMTP_USERNAME`  | —                              | SMTP login (with `CUTHULU_SMTP_PASSWORD`) |
| `CUTHULU_SMTP_PASSWORD`  | —                              | SMTP password / app password; never logged or shown |
| `CUTHULU_NOTIFY_EMAIL_FROM` | the username                | Sender; display name `cuthulu` when it has none |
| `CUTHULU_NOTIFY_EMAIL_TO` | the sender                    | Recipient |
| `CUTHULU_NOTIFY_COOLDOWN_MINUTES` | `15`                  | Least time between two "down" emails for one service (1–1440) |
| `CUTHULU_NOTIFY_HOST`    | host name (outside a container), else `cuthulu` | Machine name in email subjects |
| `CUTHULU_HEALTHCHECK_URL` | —                             | Ping URL, e.g. `https://hc-ping.com/<uuid>`; secret |
| `CUTHULU_HEALTHCHECK_INTERVAL_MINUTES` | `5`              | Ping interval (1–1440) |
| `CUTHULU_HEALTHCHECK_LINK` | `https://healthchecks.io/`   | http(s) URL the topbar's healthchecks.io button opens (a project or check page, a self-hosted instance); not secret. Without `CUTHULU_HEALTHCHECK_URL` it has no effect and is warned about hourly |
| `CUTHULU_TAILSCALE_SOCKET` | `/var/run/tailscale/tailscaled.sock` | tailscaled LocalAPI socket for the topbar's admin console link; set empty to disable |
| `CUTHULU_TAILSCALE_URL`  | unset                          | Explicit http(s) URL for the Tailscale button; shown even without the socket |
| `RUST_LOG`               | `info`                         | Tracing filter |

Planned: `CUTHULU_AUTH_TOKEN` (phase 5).

Build time, not runtime: `CUTHULU_BUILD_SHA` (set from the image's `GIT_SHA`
build arg) is compiled in as the commit shown in the footer and
`/api/version`; without it only the version is shown. The version is
`Cargo.toml`'s, bumped by the release workflow
([DEPLOYMENT.md](DEPLOYMENT.md#releasing)). `Cargo.toml`'s `repository` is
the topbar's GitHub link and the base of the footer's release-notes link.

## Security

Mounting `/var/run/docker.sock` gives the container **root-equivalent access
to the host**. Therefore:

- The binary binds `127.0.0.1:8686` by default. The image binds
  `0.0.0.0:8686` and the compose file publishes it as host port **80 on all
  interfaces** — the user's decision, so tailnet devices reach
  `http://<machine>/` with nothing to run on the host. The trust boundary is
  therefore everyone on the LAN and tailnet; `127.0.0.1:80:8686` narrows it
  to the machine ([DEPLOYMENT.md](DEPLOYMENT.md#security)).
- Every request's `Host` must be a name a website cannot take over (see
  `CUTHULU_ALLOWED_HOSTS`), else `421`. Without it, a page could rebind its
  own domain to this machine and pass the same-origin check below. Requests
  without a `Host` (non-browser clients) pass.
- HTTPS is not built in: the optional `tailscale` compose service (a
  `tailscale/tailscale` sidecar, its own tailnet node) terminates TLS and
  proxies to `cuthulu:8686`, keeping the browser's `Host`.
- POSTs require the `X-Cuthulu: 1` header (a cross-site form cannot set it and
  a cross-site `fetch` with it needs a CORS preflight that is never granted),
  plus `Origin` must match `Host` and `Sec-Fetch-Site` must be same-origin
  when the browser sends them.
- Strict CSP (`default-src 'self'`, no inline script), `X-Frame-Options: DENY`,
  `nosniff`, `no-referrer`.
- `CUTHULU_READ_ONLY=true` for a pure viewer.
- TODO text is user input: rendered with `textContent` only, length-bounded.
- Env var values never leave the Docker provider.
- Notification secrets (SMTP password, ping URL) are never logged, echoed in
  config errors or returned by the API (`/api/healthcheck` returns the
  dashboard link and a redacted error only). Unencrypted SMTP is refused unless
  the server is on localhost.
- The image runs as a non-root user (65532) from `scratch`.
- The host's `/proc` (and optionally `/etc/passwd`) are mounted read-only;
  the panel shows process command lines but never environments.
- The tailscaled socket (optional) is only used for `GET
  /localapi/v0/status`; `/api/tailscale` returns the admin URL, tailnet name,
  MagicDNS name and IPv4 of this node, nothing about keys or other peers.
