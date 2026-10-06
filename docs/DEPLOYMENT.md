# Deployment

Cuthulu runs as a container next to the services it watches.

## Docker Compose (recommended)

The repository's [`compose.yaml`](../compose.yaml):

```yaml
services:
  cuthulu:
    image: ${IMAGE:-villegasmich/cuthulu:latest}
    build: .
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

Run the [published image](#published-images) (Compose pulls it; before the
first release, or offline, it falls back to building the checkout):

```sh
DOCKER_GID=$(stat -c %g /var/run/docker.sock) docker compose up -d
# pin a release instead of latest:
IMAGE=villegasmich/cuthulu:1.2.3 DOCKER_GID=$(stat -c %g /var/run/docker.sock) docker compose up -d
# update: docker compose pull && docker compose up -d
```

Or build this checkout (the result is tagged with the `image:` name):

```sh
DOCKER_GID=$(stat -c %g /var/run/docker.sock) docker compose up -d --build
```

Then open <http://localhost:8686>. The footer shows the running version
(linked to its GitHub release) and, for CI-built images, the short commit;
`GET /api/version` returns the same as JSON.

`DOCKER_GID` is needed because the image runs as an unprivileged user (uid
65532); adding it to the socket's group is what lets it talk to Docker.

## Plain docker run

```sh
docker run -d --name cuthulu --restart unless-stopped \
  -p 127.0.0.1:8686:8686 \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v /proc:/host/proc:ro -e CUTHULU_PROC_DIR=/host/proc \
  -v /var/run/tailscale:/var/run/tailscale:ro \
  -v cuthulu-data:/data \
  --group-add "$(stat -c %g /var/run/docker.sock)" \
  --stop-timeout 15 \
  villegasmich/cuthulu:1.2.3   # or `cuthulu` after `docker build -t cuthulu .`
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
plain `docker run`, pass `--env-file .env`. `cargo run` reads the same file
from the directory it is started in (real environment variables win).

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
| crash hidden by an automatic restart (exit code ≠ 0/130/137/143, back within 30 s) | `[cuthulu] box: web crashed and was restarted (exited 1)` |
| stop clicked in Cuthulu, down for 30 s | `[cuthulu] box: web stopped from cuthulu (exited 143)` — not limited by the cooldown |
| …and something else starts it again | the "back up" email says so (see below) |
| Cuthulu stops cleanly | `[cuthulu] box: cuthulu stopped` |

### Choosing services

Click the bell at the end of a row (or `b` on the selected row, or the
`notify` button on a service page). Watched services are remembered by name
in `notify.json`, so they survive container re-creation. The bell in the top
bar opens the notifications dialog: the global on/off switch for service
alerts, which channels are configured, and `send test` (a test email, one
ping and a desktop notification).

### Containers managed by systemd

Cuthulu only controls Docker. A container started by a systemd unit
(`ExecStart=docker run --rm …` with `Restart=always`) comes back after you
stop it in Cuthulu, because the unit starts a new one. Cuthulu notices — the
row gets a `↻` marker, the service page says "restarts by itself", a warning
shows in the page, and the "back up" email explains it. To keep such a
service down, stop the unit: `systemctl stop <unit>` (or
`systemctl --user stop <unit>`).

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
- Multi-arch without emulation: the build stage runs on the build machine's
  platform (`--platform=$BUILDPLATFORM`) and cross-compiles for the target
  (`x86_64-` / `aarch64-unknown-linux-musl`, arm64 linked with the
  toolchain's `rust-lld`); the runtime stage only copies files. A cold arm64
  build takes about as long as an amd64 one, where QEMU would make it many
  times slower. The trade-off: this works because every dependency is pure
  Rust. A crate that compiles C code would need a cross C toolchain (e.g.
  `cargo-zigbuild`), or a switch back to QEMU (drop `--platform=$BUILDPLATFORM`
  and add `docker/setup-qemu-action` in CI).

  ```sh
  docker buildx build --platform linux/amd64,linux/arm64 -t <you>/cuthulu --push .
  ```
- Build args: `GIT_SHA` (full commit, compiled in and shown in the footer and
  `/api/version`; also the `org.opencontainers.image.revision` label) and
  `VERSION` (the `org.opencontainers.image.version` label). Both are optional;
  CI sets them, for a local build:

  ```sh
  docker build --build-arg GIT_SHA=$(git rev-parse HEAD) \
    --build-arg VERSION=$(scripts/bump-version.sh) -t cuthulu .
  ```

## Published images

CI (`docker` job in [`ci.yml`](../.github/workflows/ci.yml)) pushes the image
to Docker Hub when a GitHub release is **published**, or when run manually on
a release tag with *publish* (what the [Release workflow](#releasing) does).
The tag must be `v<version>` with the version in `Cargo.toml` (the job checks);
`v1.2.3` becomes the image tags `1.2.3`, `1.2` and `latest` (no `latest` for
pre-releases like `v1.3.0-rc.1`), for `linux/amd64` and `linux/arm64`. Every
other run (PRs, pushes to `main`) builds the amd64 image, smoke-tests it
(including `/api/version` against `Cargo.toml` and the commit) and only does a
dry-run push: the tags it would get are in the job summary.

One-time setup:

1. Docker Hub → *Account settings* → *Personal access tokens* → *Generate new
   token*, access **Read & Write**. Copy it (it's shown once).
2. GitHub repo → *Settings* → *Secrets and variables* → *Actions*:
   - *Secrets* tab → `DOCKERHUB_TOKEN` = the token.
   - *Variables* tab → `DOCKERHUB_USERNAME` = your Docker Hub username.
   - Optional variable `DOCKERHUB_IMAGE` (e.g. `myorg/cuthulu`); defaults to
     `<DOCKERHUB_USERNAME>/cuthulu`. Use lowercase. The Docker Hub repository
     is created on first push (public on free plans) if it doesn't exist.

Or with `gh`:

```sh
gh secret set DOCKERHUB_TOKEN          # paste the token when prompted
gh variable set DOCKERHUB_USERNAME --body <you>
```

The job fails with an error if the secret or variable is missing. Secrets are
not exposed to pull requests from forks, and only the publish path logs in.

## Releasing

### From GitHub Actions (recommended)

*Actions* → **Release** → *Run workflow* on `main`
([`release.yml`](../.github/workflows/release.yml)), or:

```sh
gh workflow run release.yml                       # automatic bump if needed (below)
gh workflow run release.yml -f bump=minor         # force patch, minor or major
gh workflow run release.yml -f version=1.3.0-rc.1 # exact version
gh workflow run release.yml -f dry_run=true       # show the version and diff only
```

The job:

1. Refuses to run on anything but `main`, or if CI hasn't passed on the commit.
2. If `v<version>` from `Cargo.toml` is already tagged, bumps it with
   [`scripts/bump-version.sh`](../scripts/bump-version.sh), which also updates
   `Cargo.lock`, and pushes `chore(release): bump version to X.Y.Z` to `main`
   as `github-actions[bot]`. A version that isn't released yet is released as
   is (so the first run releases `0.1.0`). With `bump=auto` (default) the bump
   comes from the [Conventional Commits](https://www.conventionalcommits.org/)
   since that tag (merge commits ignored); the biggest one wins, and the job
   log lists each commit's:

   | Commits                                                   | `1.x.y` and up | `0.x.y` |
   | --------------------------------------------------------- | -------------- | ------- |
   | breaking: `type!:` or a `BREAKING CHANGE:` footer         | major          | minor   |
   | `feat`                                                    | minor          | minor   |
   | anything else (`fix`, `chore`, `docs`, `ci`, ..., non-conventional) | patch | patch |

   With no commits since the tag the job fails (nothing to release).
   `bump=patch|minor|major` forces a bump, `version` sets an exact one.
3. Runs `scripts/release.sh --yes` (below): tag + GitHub release with
   generated notes.
4. Runs CI on the new tag with `publish=true` and waits for it, so the job
   only goes green once the image is on Docker Hub. (A release created with
   the workflow's `GITHUB_TOKEN` doesn't trigger CI's `release` event; a
   manual run does.)

It uses the built-in `GITHUB_TOKEN` (no extra secret), so it needs the Docker
Hub settings above, and `main` must accept pushes from GitHub Actions: with
branch protection or rulesets on `main`, allow the GitHub Actions app to
bypass them. The bump commit itself doesn't trigger a push CI run (also a
`GITHUB_TOKEN` effect); the run on the tag tests it. If the job fails after
pushing the bump, run CI on `main` (*Actions* → *CI* → *Run workflow*), then
rerun **Release**: the bumped version isn't released yet, so it's released as
is.

To republish an existing release's image: *Actions* → *CI* → *Run workflow*,
pick the tag, tick *publish*.

### Locally

From an up-to-date, clean `main` with
[`scripts/release.sh`](../scripts/release.sh) (`--dry-run` to only check, `-y`
to skip the prompt). It tags the current commit as `v<version>` from
`Cargo.toml` and runs `gh release create --generate-notes` (versions like
`1.3.0-rc.1` become pre-releases), and CI publishes the image on the
`release` event. It refuses a version that was already released, or a
`Cargo.lock` that doesn't match: run `scripts/bump-version.sh auto` (or
`patch`, `minor`, `major`, an exact version), commit both files, push, then
rerun.

## Running without Docker (development)

```sh
cargo run
# listens on 127.0.0.1:8686, uses /var/run/docker.sock
```

The user running it must be in the `docker` group. In debug builds the
`static/` files are read from disk, so CSS/JS edits show up on reload.
TODOs are saved to `./data/todos.json` (git-ignored); set `CUTHULU_DATA_DIR`
to put them elsewhere. Settings in `./.env` (see `.env.example`) are picked
up automatically; variables set in the shell override them.

## Security

Access to the Docker socket is equivalent to root on the host. Keep the port
bound to `127.0.0.1`. Until `CUTHULU_AUTH_TOKEN` lands (roadmap phase 5) there
is no authentication, so for remote access use an SSH tunnel:

```sh
ssh -L 8686:127.0.0.1:8686 your-machine
```

Set `CUTHULU_READ_ONLY=true` if you only want to watch.
