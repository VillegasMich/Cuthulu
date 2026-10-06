# cuthulu

> The eye that never sleeps.

A self-hosted dashboard that watches the services running on your Linux
machine — Docker containers first — and lets you see their status, read their
logs and start / stop / restart them from one place. Cuthulu itself runs as a
container.

**Status:** early but usable — the dashboard, live updates, logs and
start/stop/restart work. See the [roadmap](docs/ROADMAP.md).

## Features

- Auto-discovers every container on the host, no configuration
- Live state updates (event-driven, no polling)
- Live log tailing with search and stderr highlighting
- Start / stop / restart, with protection against stopping itself
- htop-style host panel: per-core CPU, memory, swap, load and top processes,
  read straight from `/proc` (sampled only while someone is watching)
- Light and dark themes, keyboard driven, dense terminal-style UI
- Scales from a handful to hundreds of services (event-driven, no polling)
- Single static binary in a `scratch` image
- Read-only mode, CSRF protection, strict CSP

## Quick start

```sh
git clone https://github.com/VillegasMich/cuthulu && cd cuthulu
DOCKER_GID=$(stat -c %g /var/run/docker.sock) docker compose up -d --build
```

Open <http://localhost:8686>.

> **Warning:** mounting the Docker socket gives Cuthulu root-level control of
> the host. Keep it bound to localhost. See [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md).

## Development

```sh
cargo run                                    # http://127.0.0.1:8686, needs access to /var/run/docker.sock
cargo test
cargo clippy --all-targets -- -D warnings
```

Keyboard: `/` filter · `j`/`k` move · `enter` open · `s` start/stop ·
`r` restart · `m` host panel · `t` theme · `?` help.

## Docs

- [Vision](docs/VISION.md) — what and why
- [Architecture](docs/ARCHITECTURE.md) — how
- [Design guide](docs/DESIGN.md) — look and feel
- [Deployment](docs/DEPLOYMENT.md) — running it
- [Roadmap](docs/ROADMAP.md) — what's next

## License

[MIT](LICENSE)
