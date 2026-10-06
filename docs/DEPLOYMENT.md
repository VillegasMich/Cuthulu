# Deployment

Cuthulu runs as a container next to the services it watches. Target plan —
the `Dockerfile` and `compose.yaml` arrive in Phase 4 of the
[roadmap](ROADMAP.md).

## compose.yaml (target)

```yaml
services:
  cuthulu:
    image: villegasmich/cuthulu:latest
    container_name: cuthulu
    restart: unless-stopped
    ports:
      - "127.0.0.1:8686:8686"      # localhost only — see Security
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
    group_add:
      - "${DOCKER_GID}"            # gid of the docker group, so a non-root user can use the socket
    environment:
      RUST_LOG: info
      # CUTHULU_READ_ONLY: "true"
      # CUTHULU_AUTH_TOKEN: "change-me"
    labels:
      cuthulu.self: "true"
    healthcheck:
      test: ["CMD", "/cuthulu", "healthcheck"]
      interval: 30s
```

```sh
DOCKER_GID=$(stat -c %g /var/run/docker.sock) docker compose up -d
```

Then open <http://localhost:8686>.

## Plain docker run

```sh
docker run -d --name cuthulu --restart unless-stopped \
  -p 127.0.0.1:8686:8686 \
  -v /var/run/docker.sock:/var/run/docker.sock \
  --group-add "$(stat -c %g /var/run/docker.sock)" \
  --label cuthulu.self=true \
  villegasmich/cuthulu:latest
```

## Image (target)

- Build stage: `rust` image, target `x86_64-unknown-linux-musl`, `--release`.
- Runtime stage: `gcr.io/distroless/static` (or `scratch`), non-root user.
- Templates and static assets are embedded in the binary — the image holds a
  single file.
- The binary supports a `healthcheck` subcommand (hits `/healthz`) because the
  runtime image has no `curl`.

## Running without Docker (development)

```sh
cargo run
# listens on 0.0.0.0:8686, uses /var/run/docker.sock
```

The user running it must be in the `docker` group.

## Security

Access to the Docker socket is equivalent to root on the host. Keep the port
bound to `127.0.0.1`. If you need remote access, put it behind a reverse proxy
with TLS **and** set `CUTHULU_AUTH_TOKEN`, or use an SSH tunnel:

```sh
ssh -L 8686:127.0.0.1:8686 your-machine
```
