# Architecture

Status: **proposed**. Nothing below is implemented yet; this is the plan the
first phases should follow. Update this file when decisions change.

## Overview

```
 Browser ──HTTP/SSE──▶ cuthulu (single Rust binary, in a container)
                          │
                          ├── web      : HTML pages + static assets (embedded)
                          ├── api      : JSON + Server-Sent Events
                          ├── registry : in-memory view of all services
                          └── providers
                                └── docker ──unix socket──▶ /var/run/docker.sock
```

One process, one binary, no database. All state is derived from the
providers (Docker is the source of truth) and kept in memory.

## Stack

| Concern            | Choice                                   | Why |
|--------------------|------------------------------------------|-----|
| Language           | Rust (edition 2024)                      | Already initialised; small static binary, low idle footprint for an always-on tool |
| Async runtime      | `tokio`                                  | Standard |
| HTTP server        | `axum`                                   | Tokio-native, first-class SSE support |
| Docker client      | `bollard`                                | Async Docker Engine API client over the unix socket |
| Templates          | `askama`                                 | Compile-time checked HTML templates |
| Frontend behaviour | `htmx` + small vanilla JS (log viewer)   | No Node toolchain, no build step, fits server-rendered pages |
| Styling            | Hand-written CSS with custom properties  | Full control over the look; themes via CSS variables |
| Asset embedding    | `rust-embed`                             | Ship one binary; works offline |
| Errors             | `thiserror` (lib code), `anyhow` (main)  | |
| Logging            | `tracing` + `tracing-subscriber`         | |
| Config             | env vars (`CUTHULU_*`)                   | Natural for a container |

Frontend assets (htmx, fonts) are vendored into the repo, never loaded from a
CDN — the dashboard must work with no internet connection.

## Core model

```rust
/// Stable id across providers: "<provider>:<native id>", e.g. "docker:3f2a9c…"
pub struct ServiceId(String);

pub enum ServiceState { Running, Stopped, Restarting, Paused, Created, Dead, Unknown }

pub enum Health { Healthy, Unhealthy, Starting, None }

pub struct Service {
    pub id: ServiceId,
    pub provider: ProviderKind,      // Docker for now
    pub name: String,
    pub image: Option<String>,
    pub state: ServiceState,
    pub health: Health,
    pub started_at: Option<DateTime>,
    pub ports: Vec<PortMapping>,
    pub group: Option<String>,       // e.g. compose project
    pub labels: BTreeMap<String, String>,
    pub is_self: bool,               // true for Cuthulu's own container
}

pub enum Action { Start, Stop, Restart }
```

## Provider abstraction (the scalability seam)

Every source of services implements one trait. Docker is the first
implementation; systemd, plain processes or remote hosts come later without
touching the API or UI.

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    async fn list(&self) -> Result<Vec<Service>>;
    async fn inspect(&self, id: &ServiceId) -> Result<ServiceDetail>;
    async fn logs(&self, id: &ServiceId, opts: LogOptions) -> Result<BoxStream<'static, Result<LogLine>>>;
    async fn act(&self, id: &ServiceId, action: Action) -> Result<()>;
    fn events(&self) -> BoxStream<'static, ServiceEvent>;
}
```

Native `async fn` in traits is not dyn-compatible, so use `async-trait` (or
hand-written `BoxFuture`s) to keep `Arc<dyn Provider>` working.

## Registry and live updates

- On startup the registry calls `list()` on every provider.
- It then subscribes to `events()` (for Docker: the `/events` stream filtered
  to container events) and applies changes incrementally.
- A periodic **reconcile** (e.g. every 30s) re-lists to heal any missed event.
- Changes are broadcast on a `tokio::sync::broadcast` channel; each browser
  tab holds one SSE connection to `/api/events`.

**No per-container polling.** Cost of idle monitoring must not grow with the
number of services.

## HTTP API

| Method | Path                               | Description |
|--------|------------------------------------|-------------|
| GET    | `/`                                | Dashboard page |
| GET    | `/services/{id}`                   | Service detail page |
| GET    | `/api/services`                    | JSON list (supports `?q=`, `?state=`, `?group=`) |
| GET    | `/api/services/{id}`               | JSON detail |
| POST   | `/api/services/{id}/start`         | Start |
| POST   | `/api/services/{id}/stop`          | Stop (refused if `is_self`) |
| POST   | `/api/services/{id}/restart`       | Restart |
| GET    | `/api/services/{id}/logs`          | SSE log stream (`?tail=500&follow=true&since=`) |
| GET    | `/api/events`                      | SSE stream of registry changes |
| GET    | `/healthz`                         | Liveness for Cuthulu's own healthcheck |

Actions return the updated `Service`. Errors are JSON `{ "error": "...", "code": "..." }`.

## Logs

- Docker logs are demultiplexed into stdout/stderr lines with timestamps.
- Initial request sends the last `tail` lines (default 500), then follows.
- The browser keeps a bounded ring buffer (e.g. 5 000 lines) and renders only
  what is visible, so a chatty container cannot freeze the tab.
- Server side, a slow client is dropped rather than buffered forever.

## Scalability checklist

- Event-driven updates, periodic reconcile, no per-service polling.
- List endpoint supports filtering server-side.
- UI: search, state filter, compose-project grouping, keyboard navigation;
  long lists stay a plain dense table (no heavy per-row widgets).
- Bounded buffers everywhere (broadcast channel, log streams, client ring buffer).

## Self-awareness

Cuthulu detects its own container (via `HOSTNAME` / cgroup id matched against
container ids, or a `cuthulu.self=true` label) and marks it `is_self`. Stop is
disabled for it; restart is allowed with a warning.

## Configuration

| Variable               | Default                        | Meaning |
|------------------------|--------------------------------|---------|
| `CUTHULU_BIND`         | `0.0.0.0:8686`                 | Listen address inside the container |
| `CUTHULU_DOCKER_HOST`  | `unix:///var/run/docker.sock`  | Docker endpoint |
| `CUTHULU_READ_ONLY`    | `false`                        | Hide/disable start/stop/restart |
| `CUTHULU_AUTH_TOKEN`   | unset                          | If set, required as bearer token / login |
| `CUTHULU_LOG_TAIL`     | `500`                          | Default lines of history per log view |
| `CUTHULU_RECONCILE_SECS` | `30`                         | Full re-list interval |
| `RUST_LOG`             | `info`                         | Tracing filter |

## Security

Mounting `/var/run/docker.sock` gives the container **root-equivalent access
to the host**. Therefore:

- Publish the port on `127.0.0.1` only by default. Never expose it to a
  network without `CUTHULU_AUTH_TOKEN` set and a TLS-terminating proxy.
- State-changing endpoints are `POST` only and require a same-origin request
  (check `Origin` / a custom header sent by htmx) to block CSRF from other
  local sites.
- `CUTHULU_READ_ONLY=true` for a pure viewer.
- Never display env var *values* in the detail view by default (secrets live
  there); show keys only, values behind an explicit reveal.

## Planned source layout

```
src/
  main.rs            entry: config, tracing, build router, serve
  config.rs          CUTHULU_* env parsing
  model.rs           Service, ServiceId, ServiceState, Action, …
  registry.rs        in-memory state, reconcile loop, broadcast
  providers/
    mod.rs           Provider trait, ProviderKind
    docker.rs        bollard-backed implementation
  api/
    mod.rs           router
    services.rs      list/detail/actions
    logs.rs          SSE log streaming
    events.rs        SSE registry events
  web/
    mod.rs           page handlers
templates/           askama templates
static/              css, js, vendored htmx, fonts, favicon
Dockerfile
compose.yaml
```
