# Roadmap

Each phase should end in something runnable. Tick items as they land.

## Phase 0 — Scaffolding ✓
- [x] Rename crate to `cuthulu`
- [x] Core dependencies (tokio, axum, bollard, askama, tracing)
- [x] `config.rs` with `CUTHULU_*` env vars
- [x] `/healthz` endpoint, tracing setup, graceful shutdown
- [x] CI: fmt, clippy (pedantic, `-D warnings`), tests, rustdoc, MSRV, cargo-deny, typos, docker build + smoke test
- [x] Dependabot for cargo, GitHub Actions and the Docker base image

## Phase 1 — See everything (read-only) ✓
- [x] `Provider` trait + Docker provider `list()` / `get()` / `detail()`
- [x] Registry with initial load
- [x] `GET /api/services`, `GET /api/services/{id}`
- [x] Dashboard table page + detail page
- [x] Light/dark theme per [DESIGN.md](DESIGN.md)

## Phase 2 — Live ✓
- [x] Docker events subscription + periodic reconcile + reconnect with backoff
- [x] `GET /api/events` SSE, table updates without reload
- [x] Log streaming endpoint + log viewer (tail, follow, filter, timestamps, wrap, stderr tint)

## Phase 3 — Control ✓
- [x] Start / stop / restart endpoints and UI buttons
- [x] Confirmation for stop, self-protection (`is_self`)
- [x] `CUTHULU_READ_ONLY`, same-origin check on POSTs

## Phase 4 — Ship as a container ✓
- [x] Multi-stage `Dockerfile` (static musl binary, `scratch`, non-root, healthcheck)
- [x] `compose.yaml` with socket mount, `127.0.0.1` port binding, `restart: unless-stopped`
- [x] Publish image (`villegasmich/cuthulu`, amd64 + arm64) from CI on tags
- [x] Release workflow: Conventional Commits semver bump, GitHub release, Docker Hub push
- [x] Version (and commit) in the footer, `GET /api/version`, OCI version/revision labels

## Phase 5 — Polish
- [x] Keyboard shortcuts
- [x] Search / state filter
- [x] Hide stopped services by default (`show stopped` toggle, `a`)
- [x] Topbar back arrow and sun/moon theme icon
- [ ] Sortable columns
- [ ] Group by compose project (collapsible sections)
- [ ] Optional `CUTHULU_AUTH_TOKEN`
- [x] Render ANSI colors in logs instead of stripping them
- [x] Highlight log level keywords on uncolored lines
- [ ] Download logs
- [x] Per-service TODO list on the detail page (`CUTHULU_DATA_DIR/todos.json`)
- [x] WCAG contrast pass (text ≥ 4.5:1, controls ≥ 3:1) and data colors (`--key`, `--project`, `--tag`)
- [x] Resizable panes: splitters for the detail view's info column, the dashboard's table columns and host panel height (remembered per browser)
- [x] Topbar link to this machine in the Tailscale admin console (`/api/tailscale`, tailscaled LocalAPI)

## Host panel ✓
- [x] Read host CPU, memory, swap, load, uptime, network (addresses, ↓/↑ throughput) and disk I/O from procfs (`src/system/`, no external binaries)
- [x] `GET /api/system` + `/api/system/stream`, one shared sampler that runs only while watched
- [x] htop-style panel above the services table: per-core meters, info column, collapsible (`m`)
- [x] Top processes by CPU / memory, opt-in via `CUTHULU_SYSTEM_PROCESSES`
- [x] `CUTHULU_PROC_DIR`, `CUTHULU_SYSTEM_SECS`; compose mounts the host's `/proc` read-only

## Later
- CPU / memory stats per service (on demand, only for visible rows)
- systemd provider
- Desktop / webhook notifications on crash
- Multiple Docker hosts
