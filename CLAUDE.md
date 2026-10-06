# CLAUDE.md

Cuthulu — self-hosted dashboard to monitor and control services (Docker
containers first) on a single Linux machine. Runs as a container itself.
Early stage: docs are ahead of code.

## Read first

- `docs/VISION.md` — scope, goals, non-goals, user stories
- `docs/ARCHITECTURE.md` — stack, Provider trait, API, config, security
- `docs/DESIGN.md` — UI rules (check before touching any HTML/CSS)
- `docs/ROADMAP.md` — current phase; tick items when done
- `docs/DEPLOYMENT.md` — container setup

## Stack

Rust 2024 · tokio · axum · bollard (Docker API) · askama templates · htmx +
small vanilla JS · hand-written CSS · rust-embed. No Node toolchain, no CDN
assets, no database.

## Commands

```sh
cargo run                         # dev server on :8686, needs docker socket access
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
docker build -t cuthulu .         # once Dockerfile exists
```

Run fmt, clippy and tests before considering a change done.

## Rules

- **New service sources go through the `Provider` trait.** API and UI must
  stay provider-agnostic; never call bollard outside `src/providers/docker.rs`.
- **No per-service polling.** Use provider event streams + periodic reconcile.
  Anything per-service (stats, logs) is on demand and bounded.
- **Bounded buffers** for every channel and stream (broadcast, logs, client ring buffer).
- **Self-protection:** never allow stopping the service flagged `is_self`.
- **Security:** state-changing routes are POST + same-origin check; respect
  `CUTHULU_READ_ONLY`; never render env var values by default; keep default
  port binding `127.0.0.1` in examples.
- **UI style:** dense, monospace, terminal-like, light+dark via CSS custom
  properties. No gradients, glassmorphism, emoji icons, big rounded shadowed
  cards, or marketing copy. Follow `docs/DESIGN.md` tokens exactly.
- **Offline:** vendor all frontend assets (htmx, fonts) under `static/`.
- Library code returns typed errors (`thiserror`); `anyhow` only in `main.rs`.
- Config only via `CUTHULU_*` env vars; document new ones in ARCHITECTURE.md.
- Keep docs in sync: if a decision in `docs/` changes, update the doc in the
  same change.

## Local environment notes

- Host has Docker 29.x; socket `/var/run/docker.sock` owned by group `docker`.
- Existing containers to test against: `auto-git-commit-tool`,
  `claude-session-starter`, `producer-tag-on-merge`. Do **not** stop or
  restart them during development without asking — they are real services.
  Spin up a throwaway container (e.g. `docker run -d --name cuthulu-test alpine sleep 1d`)
  for testing actions.
