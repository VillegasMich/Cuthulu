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

## Phase 4 — Ship as a container
- [x] Multi-stage `Dockerfile` (static musl binary, `scratch`, non-root, healthcheck)
- [x] `compose.yaml` with socket mount, `127.0.0.1` port binding, `restart: unless-stopped`
- [ ] Publish image (`villegasmich/cuthulu`) from CI on tags

## Phase 5 — Polish
- [x] Keyboard shortcuts
- [x] Search / state filter
- [ ] Sortable columns
- [ ] Group by compose project (collapsible sections)
- [ ] Optional `CUTHULU_AUTH_TOKEN`
- [x] Render ANSI colors in logs instead of stripping them
- [x] Highlight log level keywords on uncolored lines
- [ ] Download logs

## Later
- CPU / memory stats per service (on demand, only for visible rows)
- systemd provider
- Desktop / webhook notifications on crash
- Multiple Docker hosts
