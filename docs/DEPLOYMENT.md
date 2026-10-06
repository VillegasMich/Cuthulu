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
      # Host panel (CPU, memory, processes): the host's procfs, read-only.
      - /proc:/host/proc:ro
      # Only with CUTHULU_SYSTEM_PROCESSES: resolve process uids to user names.
      # - /etc/passwd:/etc/passwd:ro
      # Optional: tailscaled's LocalAPI, for the topbar link to this machine in
      # the Tailscale admin console. The directory, not the socket file, so a
      # tailscaled restart (new socket) is picked up. As uid 65532 the API only
      # allows reads; Cuthulu only calls GET /localapi/v0/status.
      - /var/run/tailscale:/var/run/tailscale:ro
      - cuthulu-data:/data # todos.json; a named volume keeps the image's 65532 owner
    group_add:
      - "${DOCKER_GID:?set DOCKER_GID to the gid of /var/run/docker.sock}"
    environment:
      RUST_LOG: info
      CUTHULU_PROC_DIR: /host/proc
      # CUTHULU_SYSTEM_PROCESSES: "true"
      # CUTHULU_READ_ONLY: "true"

volumes:
  cuthulu-data:
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
  -v /proc:/host/proc:ro -e CUTHULU_PROC_DIR=/host/proc \
  -v /var/run/tailscale:/var/run/tailscale:ro \
  -v cuthulu-data:/data \
  --group-add "$(stat -c %g /var/run/docker.sock)" \
  cuthulu
```

## Host panel

The dashboard's host panel (per-core CPU, memory, swap, load, network
throughput and addresses, disk I/O, and optionally the top processes) reads
procfs directly — no `htop`, shell or other binary is involved, so it works
in the `scratch` image. Inside a container `/proc` describes the container
(its processes, its network namespace), hence the host's procfs is
bind-mounted read-only at `/host/proc` and `CUTHULU_PROC_DIR` points there.
Network data is read through `/host/proc/1/net`, the host's namespace.

- Without that mount CPU, memory, load and disk I/O are still host-wide
  (those files are not namespaced), but network shows the container's own
  interface.
- The process list is off by default; set `CUTHULU_SYSTEM_PROCESSES=true` to
  show it. `/etc/passwd` is then optional: mount the host's read-only to see
  user names instead of uids.
- If the host mounts `/proc` with `hidepid=`, processes of other users are
  skipped; the rest of the panel is unaffected.
- Sampling (every `CUTHULU_SYSTEM_SECS`, default 2) happens only while a
  browser has the panel open and visible.
- "Network speed" is the current throughput of the default-route
  interface. Cuthulu never runs a bandwidth test or contacts the internet.
## Tailscale (optional)

If the host runs Tailscale, the topbar shows a small button that opens this
machine in the Tailscale admin console
(`https://login.tailscale.com/admin/machines/<its 100.x address>`). Cuthulu
finds the address through tailscaled's local API socket, which the compose
file mounts read-only from `/var/run/tailscale`. Without Tailscale the button
simply does not appear (on a host without it, Docker creates an empty
`/var/run/tailscale`; drop the mount line if you mind).

- **Access:** tailscaled checks who connects. Root and the configured
  operator get full control; anyone else — including the image's uid 65532 —
  gets read-only access: status, preferences and whois can be read (checked
  on tailscaled 1.102), nothing can be changed, and the profile list is
  refused. Cuthulu calls nothing but
  `GET /localapi/v0/status?peers=false`, and its own API only returns the
  link, tailnet name, MagicDNS name and IPv4 of this node. Do not run the
  container as root with this socket mounted: root gets write access.
- `CUTHULU_TAILSCALE_SOCKET` changes the socket path; set it empty to
  disable the lookup. `CUTHULU_TAILSCALE_URL` sets the button's URL
  explicitly (no socket needed), e.g. for a custom admin page.
- The lookup happens when a page loads and is cached for a minute; nothing
  polls tailscaled.

## Data

Cuthulu keeps one file of its own: `todos.json` (per-service TODOs) in
`CUTHULU_DATA_DIR`, `/data` in the image, declared as a `VOLUME` and owned by
uid 65532. Use a named volume as above: Docker copies the image's ownership
into a fresh named volume. A bind mount needs a host directory writable by
65532 (`sudo chown 65532:65532 ./cuthulu-data`). Without a writable data dir
Cuthulu still runs; only saving TODOs fails, with the reason shown in the UI.

Back it up by copying the file; it is written atomically (temp file + rename).

## The image

- Build stage: `rust:1-alpine`, which produces a static musl binary; BuildKit
  cache mounts keep rebuilds fast.
- Runtime stage: `scratch` — the image holds the binary `/cuthulu` and the
  empty data dir `/data`. Templates, CSS, JS and fonts are embedded in the
  binary.
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
TODOs are saved to `./data/todos.json` (git-ignored); set `CUTHULU_DATA_DIR`
to put them elsewhere.

## Security

Access to the Docker socket is equivalent to root on the host. Keep the port
bound to `127.0.0.1`. Until `CUTHULU_AUTH_TOKEN` lands (roadmap phase 5) there
is no authentication, so for remote access use an SSH tunnel:

```sh
ssh -L 8686:127.0.0.1:8686 your-machine
```

Set `CUTHULU_READ_ONLY=true` if you only want to watch.
