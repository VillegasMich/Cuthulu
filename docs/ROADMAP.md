# Roadmap

Each phase should end in something runnable. Tick items as they land.

## Phase 0 — Scaffolding
- [x] Rename crate to `cuthulu`
- [ ] Add core dependencies (tokio, axum, bollard, askama, tracing)
- [ ] `config.rs` with `CUTHULU_*` env vars
- [ ] `/healthz` endpoint, tracing setup, graceful shutdown
- [ ] CI: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`

## Phase 1 — See everything (read-only)
- [ ] `Provider` trait + Docker provider `list()` / `inspect()`
- [ ] Registry with initial load
- [ ] `GET /api/services`, `GET /api/services/{id}`
- [ ] Dashboard table page + detail page
- [ ] Light/dark theme per [DESIGN.md](DESIGN.md)

## Phase 2 — Live
- [ ] Docker events subscription + periodic reconcile
- [ ] `GET /api/events` SSE, table updates without reload
- [ ] Log streaming endpoint + log viewer (tail, follow, filter, stderr tint)

## Phase 3 — Control
- [ ] Start / stop / restart endpoints and UI buttons
- [ ] Confirmation for stop, self-protection (`is_self`)
- [ ] `CUTHULU_READ_ONLY`, same-origin check on POSTs

## Phase 4 — Ship as a container
- [ ] Multi-stage `Dockerfile` (static musl binary, minimal runtime image, non-root)
- [ ] `compose.yaml` with socket mount, `127.0.0.1` port binding, healthcheck, `restart: unless-stopped`
- [ ] Publish image (`villegasmich/cuthulu`)

## Phase 5 — Polish
- [ ] Keyboard shortcuts
- [ ] Search / state filter / sort
- [ ] Group by compose project
- [ ] Optional `CUTHULU_AUTH_TOKEN`

## Later
- CPU / memory stats per service (on demand, only for visible rows)
- systemd provider
- Desktop / webhook notifications on crash
- Multiple Docker hosts
