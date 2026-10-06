# cuthulu

> The eye that never sleeps.

A self-hosted dashboard that watches the services running on your Linux
machine — Docker containers first — and lets you see their status, read their
logs and start / stop / restart them from one place. Cuthulu itself runs as a
container.

**Status:** early development. Nothing works yet; see the [roadmap](docs/ROADMAP.md).

## Features (planned)

- Auto-discovers every container on the host, no configuration
- Live state updates (event-driven, no polling)
- Live log tailing with search and stderr highlighting
- Start / stop / restart, with protection against stopping itself
- Light and dark themes, keyboard driven, dense terminal-style UI
- Scales from a handful to hundreds of services
- Single static binary in a tiny image

## Quick start (once released)

```sh
docker run -d --name cuthulu --restart unless-stopped \
  -p 127.0.0.1:8686:8686 \
  -v /var/run/docker.sock:/var/run/docker.sock \
  --group-add "$(stat -c %g /var/run/docker.sock)" \
  villegasmich/cuthulu:latest
```

Open <http://localhost:8686>.

> **Warning:** mounting the Docker socket gives Cuthulu root-level control of
> the host. Keep it bound to localhost. See [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md).

## Development

```sh
cargo run          # needs access to /var/run/docker.sock
cargo test
cargo clippy -- -D warnings
```

## Docs

- [Vision](docs/VISION.md) — what and why
- [Architecture](docs/ARCHITECTURE.md) — how
- [Design guide](docs/DESIGN.md) — look and feel
- [Deployment](docs/DEPLOYMENT.md) — running it
- [Roadmap](docs/ROADMAP.md) — what's next

## License

[MIT](LICENSE)
