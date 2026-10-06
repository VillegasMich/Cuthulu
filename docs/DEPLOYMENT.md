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
      - cuthulu-data:/data # todos.json, notify.json; a named volume keeps the image's 65532 owner
    group_add:
      - "${DOCKER_GID:?set DOCKER_GID to the gid of /var/run/docker.sock}"
    # Notification settings and secrets (SMTP, healthcheck URL): copy
    # .env.example to .env. Optional; never commit .env.
    env_file:
      - path: .env
        required: false
    # Room for the shutdown email after SIGTERM (it gives up after 8 s).
    stop_grace_period: 15s
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
  -v cuthulu-data:/data \
  --group-add "$(stat -c %g /var/run/docker.sock)" \
  --stop-timeout 15 \
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
## Notifications

Optional, three independent parts — set up any of them:

| Part | Tells you | Needs |
|------|-----------|-------|
| **Service alerts** | a watched service went down (`exited 1`, `unhealthy`, `removed`, …) and when it is back | the bell on the service; email needs SMTP, browser alerts need nothing |
| **Shutdown email** | Cuthulu was stopped (`docker stop`, `compose down`/`up`, reboot) | SMTP |
| **Healthcheck** | the machine is off, Cuthulu crashed, or it cannot reach Docker | a free [healthchecks.io](https://healthchecks.io) check |

A machine that loses power, or a process killed with `SIGKILL`, cannot send
anything — hence the heartbeat: Cuthulu pings healthchecks.io every few
minutes and healthchecks.io alerts when the pings stop.

Settings are environment variables (table in
[ARCHITECTURE.md](ARCHITECTURE.md#configuration)). With compose, put them in
a `.env` file next to `compose.yaml`; it is read if present and is
git-ignored:

```sh
cp .env.example .env    # then edit; never commit it
DOCKER_GID=$(stat -c %g /var/run/docker.sock) docker compose up -d
```

`docker compose up -d` re-creates the container when `.env` changes. With
plain `docker run`, pass `--env-file .env`.

### Email (Gmail example)

Gmail needs an **app password**: turn on 2-Step Verification, create one at
<https://myaccount.google.com/apppasswords> and remove the spaces from it.

```dotenv
CUTHULU_SMTP_HOST=smtp.gmail.com
CUTHULU_SMTP_USERNAME=you@gmail.com
CUTHULU_SMTP_PASSWORD=abcdabcdabcdabcd
# CUTHULU_NOTIFY_EMAIL_TO=you+cuthulu@gmail.com
# CUTHULU_NOTIFY_HOST=home-server
```

Email goes from and to the username by default. Port `465` (default) uses
implicit TLS, any other (`CUTHULU_SMTP_PORT=587`) STARTTLS; unencrypted SMTP
is refused unless the server is on localhost. TLS roots are built into the
binary, so the `scratch` image needs no CA bundle. Subjects start with
`[cuthulu] <host>:` — inside a container the real host name is not visible,
so set `CUTHULU_NOTIFY_HOST` to tell machines apart.

What you receive, at most:

| Event | Email |
|-------|-------|
| watched service down for 30 s | `[cuthulu] box: web is down (exited 1)` |
| …and back up for 30 s | `[cuthulu] box: web is back up` (only after a "down") |
| crash loop | one "down", then at most one more per `CUTHULU_NOTIFY_COOLDOWN_MINUTES` (15) |
| stop / restart clicked in Cuthulu | nothing — you did it |
| Cuthulu stops cleanly | `[cuthulu] box: cuthulu stopped` |

### Choosing services

Click the bell at the end of a row (or `b` on the selected row, or the
`notify` button on a service page). Watched services are remembered by name
in `notify.json`, so they survive container re-creation. The bell in the top
bar opens the notifications dialog: the global on/off switch for service
alerts, which channels are configured, and `send test` (a test email, one
ping and a desktop notification).

### Desktop notifications

While a Cuthulu page is open, a watched service going down raises a desktop
notification (after 10 s, so restarts stay quiet). The browser asks for
permission when you first click a bell, the alerts switch or `allow` in the
dialog. Browsers allow notifications only on secure origins:
`http://localhost:8686` and `http://127.0.0.1:8686` work (also through
`ssh -L`); a plain-HTTP LAN address does not, and alerts then show in the
page instead.

### Healthcheck with healthchecks.io

1. Sign up at <https://healthchecks.io> (free plan) and **Add Check**,
   e.g. `cuthulu <host>`.
2. Schedule: **Period** 5 minutes (match
   `CUTHULU_HEALTHCHECK_INTERVAL_MINUTES`, default 5), **Grace** 15 minutes
   (room for a reboot).
3. Copy the ping URL into `.env`; treat it like a password:

   ```dotenv
   CUTHULU_HEALTHCHECK_URL=https://hc-ping.com/your-uuid
   ```

4. Integrations (email by default; Telegram, ntfy, Slack, …) are configured
   on healthchecks.io; Cuthulu does not change.

Cuthulu pings right after it starts and connects to Docker, then every
interval. While Docker is unreachable it skips pings, so that is reported as
down too. Nothing is sent on shutdown: healthchecks.io reports a stopped
Cuthulu after period + grace (~20 min), and the shutdown email has already
told you it was deliberate. A quick restart (compose update) sends only the
shutdown email. When you stop Cuthulu for good, **pause** the check.

### Test and turn off

Open the bell dialog and press `send test`, or:

```sh
curl -X POST -H 'X-Cuthulu: 1' http://127.0.0.1:8686/api/notify/test
```

- Service alerts only: the switch in the dialog (kept in `notify.json`).
- Email and pings, keeping the settings: `CUTHULU_NOTIFY_ENABLED=false`.
- Only email: unset `CUTHULU_SMTP_HOST` (and the other `CUTHULU_SMTP_*` /
  `CUTHULU_NOTIFY_EMAIL_*`); only pings: unset `CUTHULU_HEALTHCHECK_URL`.

Invalid settings (a missing password, a bad address, `none` TLS to a remote
host) stop Cuthulu at startup with a message that never contains the
password or the ping URL.

## Data

Cuthulu keeps two small files of its own in `CUTHULU_DATA_DIR`:
`todos.json` (per-service TODOs) and `notify.json` (watched services, alert
switch), `/data` in the image, declared as a `VOLUME` and owned by
uid 65532. Use a named volume as above: Docker copies the image's ownership
into a fresh named volume. A bind mount needs a host directory writable by
65532 (`sudo chown 65532:65532 ./cuthulu-data`). Without a writable data dir
Cuthulu still runs; only saving TODOs and notification settings fails, with
the reason shown in the UI.

Back them up by copying the files; they are written atomically (temp file +
rename).

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
