# CLAUDE.md

Cuthulu — self-hosted dashboard to monitor and control services (Docker
containers first) on a single Linux machine. Runs as a container itself.
Roadmap phases 0–4 are implemented; `docs/ARCHITECTURE.md` describes the code
as it is.

## Read first

- `docs/VISION.md` — scope, goals, non-goals, user stories
- `docs/ARCHITECTURE.md` — stack, Provider trait, API, config, security
- `docs/DESIGN.md` — UI rules (check before touching any HTML/CSS)
- `docs/ROADMAP.md` — current phase; tick items when done
- `docs/DEPLOYMENT.md` — container setup

## Stack

Rust 2024 (MSRV 1.88) · tokio · axum · bollard (Docker API) · askama page
shells · vanilla JS (`static/app.js`) · hand-written CSS · rust-embed. No
Node toolchain, no CDN assets, no database.

## Commands

```sh
cargo run                         # dev server on 127.0.0.1:8686, needs docker socket access
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings   # pedantic lints are on (Cargo.toml [lints])
cargo deny check                  # licenses, advisories, bans
docker build -t cuthulu .
DOCKER_GID=$(stat -c %g /var/run/docker.sock) docker compose up -d --build
scripts/install.sh [--build]       # install as cuthulu.service (systemd, /etc/cuthulu); uninstall.sh removes it
```

Run fmt, clippy and tests before considering a change done. CI
(`.github/workflows/ci.yml`) also runs rustdoc, MSRV, cargo-deny, typos and a
docker build + smoke test.

## Where things live

- `src/providers/docker.rs` — all Docker access; mapping functions are pure and unit-tested.
- `src/registry.rs` — cache + watch loop; `tests::MockProvider` drives registry and HTTP tests.
- `src/server.rs` — router + HTTP-level tests (`tower::ServiceExt::oneshot`).
- `src/api/` — JSON + SSE handlers; `src/web.rs` — page shells and assets.
- `templates/` (askama) and `static/` (embedded; read from disk in debug builds,
  so CSS/JS changes need only a browser reload).

## Testing

- Unit/HTTP tests need no Docker: use `MockProvider`.
- Against real Docker, use throwaway `cuthulu-*` containers (see below).
- UI checks: headless Chrome is available (`google-chrome --headless=new`).
  SSE keeps pages "loading", so drive it over CDP with a fixed wait rather
  than `--screenshot` with `--virtual-time-budget`.

## Rules

- **New service sources go through the `Provider` trait.** API and UI must
  stay provider-agnostic; never call bollard outside `src/providers/docker.rs`.
- **No per-service polling.** Use provider event streams + periodic reconcile.
  Anything per-service (stats, logs) is on demand and bounded.
- **Bounded buffers** for every channel and stream (broadcast, logs, client ring buffer).
- **Self-protection:** never allow stopping the service flagged `is_self`.
- **Security:** state-changing routes are POST + same-origin check; respect
  `CUTHULU_READ_ONLY`; never render env var values by default. The binary's
  default bind stays `127.0.0.1`; `compose.yaml` publishes host port 80 on
  all interfaces by the user's decision (tailnet access, see
  `docs/DEPLOYMENT.md`). Don't widen anything else.
- **UI style:** dense, monospace, terminal-like, light+dark via CSS custom
  properties. No gradients, glassmorphism, emoji icons, big rounded shadowed
  cards, or marketing copy. Follow `docs/DESIGN.md` tokens exactly.
- **Offline:** vendor all frontend assets (fonts, any future JS lib) under `static/`.
- **SSE events need non-empty `data`** — browsers drop empty ones.
- **Frontend:** build DOM with `el()`/`textContent`, never `innerHTML` with
  service data (names, logs are untrusted). CSP forbids inline scripts.
- Library code returns typed errors (`thiserror`); `anyhow` only in `main.rs`.
- Config only via `CUTHULU_*` env vars; document new ones in ARCHITECTURE.md.
- Keep docs in sync: if a decision in `docs/` changes, update the doc in the
  same change.

## Commit message suggestion

At the end of every feature or request that changes files, end the reply with
a suggested commit message in a code block. Do not commit unless asked.

- Follow commitlint (`@commitlint/config-conventional`):
  `type(scope): subject` — types `feat`, `fix`, `docs`, `style`, `refactor`,
  `perf`, `test`, `build`, `ci`, `chore`, `revert`.
- Scope = area touched, matching existing ones: `ui`, `notify`, `system`,
  `logs`, `todos`, `config`, `version`, `release`, `tailscale`. Omit if the
  change is cross-cutting.
- Subject: imperative, lowercase, no trailing period, ≤ 72 chars; say what
  changed for the user, not how. Add a body only when the "why" isn't obvious.
- Describe only the changes from this request (check `git status`/`git diff`),
  not unrelated uncommitted work. If they split cleanly, suggest one message
  per commit.
- Match the tone of `git log --oneline`, e.g.
  `feat(notify): healthcheck heartbeat, shutdown email and service-down alerts`,
  `fix(system): hide the container's hostname and spell out threads`,
  `docs: document the host panel, its endpoints and CUTHULU_PROC_DIR`.

## Local environment notes

- Host has Docker 29.x; socket `/var/run/docker.sock` owned by group `docker`.
- Existing containers to test against: `auto-git-commit-tool`,
  `claude-session-starter`, `producer-tag-on-merge`. Do **not** stop or
  restart them during development without asking — they are real services.
  Spin up a throwaway container (e.g. `docker run -d --name cuthulu-test alpine sleep 1d`)
  for testing actions.
