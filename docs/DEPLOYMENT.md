# Deployment

Cuthulu runs as a container next to the services it watches.

## Docker Compose (recommended)

The repository's [`compose.yaml`](../compose.yaml):

```yaml
services:
  cuthulu:
    build: .
    image: villegasmich/cuthulu:latest
    container_name: cuthulu
    restart: unless-stopped
    ports:
      - "127.0.0.1:8686:8686" # localhost only: the socket below is root-equivalent
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
    group_add:
      - "${DOCKER_GID:?set DOCKER_GID to the gid of /var/run/docker.sock}"
    environment:
      RUST_LOG: info
      # CUTHULU_READ_ONLY: "true"
```

```sh
DOCKER_GID=$(stat -c %g /var/run/docker.sock) docker compose up -d --build
```

Then open <http://localhost:8686>.

`DOCKER_GID` is needed because the image runs as an unprivileged user (uid
65532); adding it to the socket's group is what lets it talk to Docker.

## Plain docker run

```sh
docker build -t cuthulu .
docker run -d --name cuthulu --restart unless-stopped \
  -p 127.0.0.1:8686:8686 \
  -v /var/run/docker.sock:/var/run/docker.sock \
  --group-add "$(stat -c %g /var/run/docker.sock)" \
  cuthulu
```

## The image

- Build stage: `rust:1-alpine`, which produces a static musl binary; BuildKit
  cache mounts keep rebuilds fast.
- Runtime stage: `scratch` — the image holds a single file, `/cuthulu`.
  Templates, CSS, JS and fonts are embedded in it.
- Runs as uid/gid 65532. `CUTHULU_BIND` defaults to `0.0.0.0:8686` inside the
  image (the binary's own default is `127.0.0.1:8686`).
- Carries the `cuthulu.self=true` label so Cuthulu recognises itself and
  refuses to stop itself.
- `HEALTHCHECK` runs `/cuthulu healthcheck`, which requests `/healthz` over
  plain TCP (there is no `curl` in `scratch`).

## Running without Docker (development)

```sh
cargo run
# listens on 127.0.0.1:8686, uses /var/run/docker.sock
```

The user running it must be in the `docker` group. In debug builds the
`static/` files are read from disk, so CSS/JS edits show up on reload.

## Security

Access to the Docker socket is equivalent to root on the host. Keep the port
bound to `127.0.0.1`. Until `CUTHULU_AUTH_TOKEN` lands (roadmap phase 5) there
is no authentication, so for remote access use an SSH tunnel:

```sh
ssh -L 8686:127.0.0.1:8686 your-machine
```

Set `CUTHULU_READ_ONLY=true` if you only want to watch.
