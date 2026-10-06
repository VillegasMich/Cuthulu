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
                          └── system    : host CPU/memory/network/disk ──▶ procfs (read-only)
```

One process, one binary, no database. Docker is the source of truth; the
registry is a cache of it kept current by events.

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

> **Decision (2026-10):** the first draft planned `htmx`. Because every
> dynamic part of the UI is driven by SSE events that patch rows client-side,
> htmx would have added a dependency without removing any JS. Dropped.

All frontend assets (JS, CSS, the JetBrains Mono font) live under `static/`
and are embedded in the binary. Nothing is loaded from a CDN.

## Source layout

```
src/
  main.rs              CLI (run | healthcheck | --version), tracing, graceful shutdown
  lib.rs
  config.rs            CUTHULU_* env parsing (pure, unit-tested via a lookup fn)
  model.rs             Service, ServiceId, ServiceState, Action, LogLine, …
  registry.rs          in-memory state, watch loop per provider, broadcast; MockProvider for tests
  server.rs            AppState, router, security headers; HTTP-level tests
  web.rs               askama page handlers, embedded asset handler (ETag)
  api/
    mod.rs             /api router
    services.rs        list / detail / actions
    events.rs          SSE: registry changes
    logs.rs            SSE: log lines
    system.rs          host snapshot + SSE stream
    guard.rs           CSRF / same-origin check for POSTs
    error.rs           ApiError → JSON { error, code }
  providers/
    mod.rs             Provider trait, ProviderError, ProviderEvent
    docker.rs          bollard-backed implementation (the only bollard user)
    lines.rs           log chunk → line splitting, timestamp parsing, ANSI stripping
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

Every event carries non-empty `data` — browsers silently drop events whose
data is empty.

## Logs

- Docker log frames are reassembled into lines per stream (stdout/stderr),
  timestamps split off, ANSI escapes stripped. A line longer than 16 KiB is
  emitted in pieces instead of buffered forever.
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
  interface.
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
- **Bounded:** broadcast capacity 4; ≤ 8 addresses per list; ≤ 20
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

## Self-awareness

Cuthulu marks its own container `is_self` when the container carries the
`cuthulu.self=true` label (set by the image) or, inside a container, when its
id starts with `$HOSTNAME`. Stop is refused for it (UI and API); restart is
allowed after a confirmation.

## Configuration

| Variable                 | Default                        | Meaning |
|--------------------------|--------------------------------|---------|
| `CUTHULU_BIND`           | `127.0.0.1:8686` (image: `0.0.0.0:8686`) | Listen address |
| `CUTHULU_DOCKER_HOST`    | `unix:///var/run/docker.sock`  | Docker endpoint (`unix://` or `tcp://`) |
| `CUTHULU_READ_ONLY`      | `false`                        | Refuse start/stop/restart, hide the buttons |
| `CUTHULU_LOG_TAIL`       | `500`                          | History lines per log view (max 10 000) |
| `CUTHULU_RECONCILE_SECS` | `60`                           | Full re-list interval |
| `CUTHULU_PROC_DIR`       | `/proc` (compose: `/host/proc`) | procfs the host panel reads; mount the host's read-only in a container |
| `CUTHULU_SYSTEM_SECS`    | `2`                            | Host panel sampling interval (1–60), only while someone watches |
| `CUTHULU_SYSTEM_PROCESSES` | `false`                      | Also list the top processes (by CPU / memory) in the host panel |
| `RUST_LOG`               | `info`                         | Tracing filter |

Planned: `CUTHULU_AUTH_TOKEN` (phase 5).

## Security

Mounting `/var/run/docker.sock` gives the container **root-equivalent access
to the host**. Therefore:

- Default bind is `127.0.0.1`; the compose file publishes on `127.0.0.1` only.
- POSTs require the `X-Cuthulu: 1` header (a cross-site form cannot set it and
  a cross-site `fetch` with it needs a CORS preflight that is never granted),
  plus `Origin` must match `Host` and `Sec-Fetch-Site` must be same-origin
  when the browser sends them.
- Strict CSP (`default-src 'self'`, no inline script), `X-Frame-Options: DENY`,
  `nosniff`, `no-referrer`.
- `CUTHULU_READ_ONLY=true` for a pure viewer.
- Env var values never leave the Docker provider.
- The image runs as a non-root user (65532) from `scratch`.
- The host's `/proc` (and optionally `/etc/passwd`) are mounted read-only;
  the panel shows process command lines but never environments.
