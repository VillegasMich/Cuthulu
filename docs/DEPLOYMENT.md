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
    # Host port 80 on every interface, for http://<machine>/ from the tailnet.
    # No login and the socket below is root-equivalent: anyone on your LAN or
    # tailnet gets control. "127.0.0.1:80:8686" keeps it on this machine;
    # CUTHULU_READ_ONLY=true allows watching only.
    ports:
      - "80:8686"
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

  # Optional HTTPS on the tailnet: https://cuthulu.<tailnet>.ts.net with a real
  # certificate (secure origin: desktop notifications, HTTP/2). Joins the
  # tailnet as its own machine; nothing runs on the host. Turn it on with
  # COMPOSE_PROFILES=tailscale in .env; see docs/DEPLOYMENT.md#https-on-the-tailnet.
  tailscale:
    image: tailscale/tailscale:stable
    profiles: [tailscale]
    container_name: cuthulu-tailscale
    restart: unless-stopped
    depends_on: [cuthulu]
    environment:
      TS_HOSTNAME: ${TS_HOSTNAME:-cuthulu}
      # Only for the first start; without it the log prints a login link.
      TS_AUTHKEY: ${TS_AUTHKEY:-}
      TS_STATE_DIR: /var/lib/tailscale
      TS_USERSPACE: "true" # no NET_ADMIN, no /dev/net/tun
      TS_SERVE_CONFIG: /config/serve.json
    configs:
      - source: tailscale-serve
        target: /config/serve.json
    volumes:
      - tailscale-state:/var/lib/tailscale # the node's identity; keep it

configs:
  # tailnet-only HTTPS on 443 to cuthulu:8686 (no AllowFunnel: never public).
  # $$ escapes compose interpolation; containerboot fills in the cert domain.
  tailscale-serve:
    content: |
      {
        "TCP": { "443": { "HTTPS": true } },
        "Web": {
          "$${TS_CERT_DOMAIN}:443": {
            "Handlers": { "/": { "Proxy": "http://cuthulu:8686" } }
          }
        }
      }

volumes:
  cuthulu-data:
  tailscale-state:
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

Then open <http://localhost/>, or `http://<machine>/` (MagicDNS name) /
`http://<100.x address>/` from other devices on your tailnet. The footer
shows the running version (linked to its GitHub release) and, for CI-built
images, the short commit; `GET /api/version` returns the same as JSON.

Host port 80 must be free: if another web server already uses it, change the
host side of the mapping (e.g. `"8080:8686"`). Before this default, compose
published `127.0.0.1:8686`; existing deployments move from
`http://localhost:8686` to `http://localhost/` on the next `up`.

`DOCKER_GID` is needed because the image runs as an unprivileged user (uid
65532); adding it to the socket's group is what lets it talk to Docker.

## As a systemd service

[`scripts/install.sh`](../scripts/install.sh) runs the same `compose.yaml`
under systemd, as `cuthulu.service`
([`deploy/systemd/cuthulu.service`](../deploy/systemd/cuthulu.service)),
enabled at boot. Run it from a checkout as your normal user; it uses sudo only
for system changes:

```sh
scripts/install.sh                                    # pull villegasmich/cuthulu:latest
IMAGE=villegasmich/cuthulu:1.2.3 scripts/install.sh   # pin a release (also how to upgrade)
scripts/install.sh --build                            # build this checkout as cuthulu:local
```

It copies `compose.yaml` and the settings to `/etc/cuthulu`: `.env` is seeded
from the checkout's `.env` (or `.env.example`), is root-only (mode 600), and
gets `IMAGE` and `DOCKER_GID` filled in, plus `CUTHULU_HOST_USER` (you, for
the [env editor](#editing-env-files)) unless it is already set. A re-run
keeps the installed `.env` (only `IMAGE` and `DOCKER_GID` are updated) unless
you pass `--reconfigure`, re-copies `compose.yaml`, pulls or builds the image
and the env editor's helper image, and restarts the service. On a machine with
RAPL it also asks, once, whether to enable the
[CPU power meter](#cpu-power-meter).

```sh
sudo $EDITOR /etc/cuthulu/.env && sudo systemctl reload cuthulu   # change settings
systemctl status cuthulu
docker logs -f cuthulu
scripts/uninstall.sh            # remove the service (and CPU power access); keeps /etc/cuthulu, volumes, images
scripts/uninstall.sh --purge    # also delete them (TODOs, notification settings, tailscale identity)
```

systemd decides when Cuthulu runs (boot, `systemctl start|stop|restart`);
the containers' `restart: unless-stopped` still brings a crashed one back.
Stopping the unit runs `docker compose down`, which removes the containers but
keeps the volumes. The compose project is named `cuthulu`, so the service uses
the same volumes as `docker compose up` from a checkout directory named
`cuthulu`: switching from one to the other keeps your data. A `cuthulu`
container from `docker run`, or from a compose project with another name,
blocks the install; remove it first (`docker rm -f cuthulu`; its volume stays).

### Companion services

After Cuthulu is up, `install.sh` offers related services that can run next to
it: [producer-tag-on-merge](https://github.com/VillegasMich/producer-tag-on-merge),
[claude-session-starter](https://github.com/VillegasMich/claude-session-auto-starter)
and [auto-git-commit-tool](https://github.com/VillegasMich/auto-git-commit-tool).
It lists each with its status (`running`, `installed` but not active, or
`not installed`) and a description; type numbers to toggle them (`1 3`), `all`
or `none`, and Enter to confirm. Nothing is picked by default. Picking one that
is already running or installed re-runs its installer, which upgrades and
restarts it.

```sh
scripts/install.sh --list-companions                  # show them and their status; changes nothing
scripts/install.sh --companions auto-git-commit-tool  # install without asking (names, comma-separated)
scripts/install.sh --companions all                   # or none
```

To add (or upgrade) companions later without touching Cuthulu, use
`--companions-only`: it skips Cuthulu's own install (no image pull or build,
no `/etc/cuthulu` writes, no restart of `cuthulu.service`, no sudo for it) and
runs only this step, with the same prompt, rules and exit code. It can't be
combined with `--build`, `--reconfigure` or `--companions none`.

```sh
scripts/install.sh --companions-only                                     # pick from the list
scripts/install.sh --companions-only --companions producer-tag-on-merge  # without asking
```

Without a terminal on stdin (e.g. piped, or CI) and without `--companions`,
the step is skipped with a hint. Run as root (`sudo scripts/install.sh`), it is
skipped too: run the script as your normal user, since producer-tag-on-merge is
a systemd *user* service.

Each picked service is cloned to
`~/.local/share/cuthulu/companions/<name>` (`$XDG_DATA_HOME` if set; a re-run
does `git pull --ff-only`), then its own `scripts/install.sh` runs as your user
with `IMAGE=<published image>:latest` exported, so it pulls the published
image instead of building one. Those installers ask for their own tokens and
use sudo themselves; settings you export for them (see each project's README)
reach them too. One failing doesn't stop the others; a summary follows, and
`install.sh` exits 1 if any failed (Cuthulu stays installed).

The list comes from [`deploy/companions/`](../deploy/companions/README.md), one
`<name>.conf` per service; adding a service is adding a file there.
`scripts/uninstall.sh` does not touch companion services: remove them with
their own uninstall scripts (in their clones).

### Editing env files

A catalog service's detail page (Cuthulu itself and the companions above) has
an **edit env** button: it asks for your sudo password on the host, shows the
service's env file (secret-looking values masked, each with a `show`
toggle), lets you change, add and delete variables, and **save & restart**
writes the file and restarts its systemd unit, so the change takes effect.
Saving Cuthulu's own settings restarts Cuthulu; the page reconnects.

| Service | File | Restarts |
|---------|------|----------|
| cuthulu | `/etc/cuthulu/.env` | `cuthulu.service` |
| auto-git-commit-tool | `/etc/auto-git-commit-tool/env` | `auto-git-commit-tool.service` |
| claude-session-starter | `/etc/claude-session-starter/env` | `claude-session-starter.service` |
| producer-tag-on-merge | `~/.config/producer-tag-on-merge/env` | `producer-tag-on-merge.service` (user unit) |

Requirements:

- **`CUTHULU_HOST_USER`** in `/etc/cuthulu/.env`: your login on the host, a
  sudo user. `install.sh` writes it (the user who ran it); without it the
  button is disabled. The password is checked with `sudo` as the env file's
  owner when that is a normal user, else as this user — never as root. It
  also says whose home `~` is.
- The Docker socket (already mounted) and the **helper image**: each step
  runs in a short-lived privileged `cuthulu-helper-…` container that enters
  the host's namespaces with `nsenter` (it is root on the host, like the
  socket itself). `install.sh` pre-pulls it, so it works offline later;
  otherwise it is pulled on first use. `CUTHULU_HELPER_IMAGE` picks another
  image with `nsenter` (default: `busybox`, pinned by digest).
- systemd on the host, with `runuser`, `sudo`, `stat`, `mktemp` and coreutils.

What it does: a one-file backup (`<file>.bak`, same owner and mode,
overwritten each save), an atomic replace that keeps the owner and mode,
then `systemctl restart` (`systemctl --user restart` as you for user units).
Values are written exactly as typed: it never adds or removes quotes.
Comments, blank lines and untouched lines stay as they were. Five wrong
passwords within five minutes lock it until the window passes. Read-only mode
hides the button. Every failed attempt also lands in the host's auth log, as
with any sudo.

Adding a `deploy/companions/<name>.conf` entry (and rebuilding) makes another
service editable.

### Starting and stopping catalog services

The start, stop and restart buttons of a catalog service run its systemd
unit's `systemctl start|stop|restart` on the host (the button's tooltip shows
the exact command; `systemctl --user …` as `CUTHULU_HOST_USER` for user units),
through the same helper container, without a password. So **stop keeps the
service down** instead of systemd starting a new container. It is a stop,
not a disable: an enabled unit starts again at boot.

A stopped `docker run --rm` service has no container, so Cuthulu keeps a row
for it, marked `(systemctl)`, with a start button (remembered in
`CUTHULU_DATA_DIR/stopped.json`, so it survives a restart of Cuthulu). The row
goes away when you start it, or when a container of that name runs again
however it was started. A service stopped outside Cuthulu
(`systemctl stop` in a shell) gets no such row.

Cuthulu itself cannot be stopped; restarting it runs
`systemctl --no-block restart cuthulu` and the page reconnects. When a
container has a catalog name but its unit is not installed on the host
(started by hand, say), the buttons act on the container as for any other
service. A user unit without `CUTHULU_HOST_USER`, or a failing `systemctl`,
shows an error instead.

## Plain docker run

```sh
docker run -d --name cuthulu --restart unless-stopped \
  -p 80:8686 \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v /proc:/host/proc:ro -e CUTHULU_PROC_DIR=/host/proc \
  -v /var/run/tailscale:/var/run/tailscale:ro \
  -v cuthulu-data:/data \
  --group-add "$(stat -c %g /var/run/docker.sock)" \
  --stop-timeout 15 \
  villegasmich/cuthulu:1.2.3   # or `cuthulu` after `docker build -t cuthulu .`
```

## HTTPS on the tailnet

The compose file has an optional `tailscale` service (a
[`tailscale/tailscale`](https://hub.docker.com/r/tailscale/tailscale)
sidecar) that joins your tailnet as its own machine, `cuthulu`, and serves
`https://cuthulu.<tailnet>.ts.net/` with a real Let's Encrypt certificate,
proxying to the `cuthulu` container. Nothing is installed or run on the host,
and it is reachable from the tailnet only (no Funnel). A secure origin is
what browsers require for [desktop notifications](#desktop-notifications),
and HTTPS brings HTTP/2, so many open tabs no longer run into the
six-connections-per-site limit of plain HTTP. `http://<machine>/` keeps
working next to it.

One-time setup:

1. Admin console → **DNS**: MagicDNS on, **HTTPS Certificates** →
   *Enable*.
2. Turn the service on — add to `.env` next to `compose.yaml`:

   ```dotenv
   COMPOSE_PROFILES=tailscale
   # TS_HOSTNAME=cuthulu   # tailnet machine name (default cuthulu)
   ```

3. Start it and log the new machine in, either with the link it prints:

   ```sh
   DOCKER_GID=$(stat -c %g /var/run/docker.sock) docker compose up -d
   docker compose logs tailscale | grep -m1 login.tailscale.com
   ```

   or with an auth key (admin console → *Settings* → *Keys*, one-off) for
   the first start only:

   ```sh
   TS_AUTHKEY=tskey-auth-... DOCKER_GID=$(stat -c %g /var/run/docker.sock) docker compose up -d
   ```

   Passing the key on the command line keeps it out of `.env`, which the
   `cuthulu` container also reads. After the login the node's identity lives
   in the `tailscale-state` volume; later `docker compose up -d` runs need no
   key.
4. Admin console → *Machines* → `cuthulu` → *Disable key expiry*, or it has
   to log in again after 180 days.
5. Open `https://cuthulu.<tailnet>.ts.net/` (the first request waits a few
   seconds for the certificate).

Installing Cuthulu as an app (Chrome/Edge *Install app*, Android *Add to
home screen* or *Install*, iOS Safari *Share* → *Add to Home Screen*) also
needs a secure origin: this HTTPS address, or `http://localhost`. It then
opens in its own window with the eye icon. Over plain `http://<machine>/`
Chrome and Edge offer at most a shortcut that opens in a browser tab.

- The machine name appears in public certificate-transparency logs; that is
  how Tailscale HTTPS certificates work.
- Tailscale forwards the browser's `Host`, so the
  [host check](#security) and the same-origin check pass; the event and log
  streams are passed through as they arrive.
- To remove it: `docker compose down`, remove `COMPOSE_PROFILES` from
  `.env`, delete the machine in the admin console and, if you like, the
  volume (`docker volume rm <project>_tailscale-state`).

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
- Temperatures (hwmon), the power source (`power_supply`) and the GPU's load
  and clock (amdgpu) come from `/sys`, which containers see read-only and
  host-wide: no mount or permission needed. Machines without them simply
  show fewer rows.
- The CPU power meter needs host setup, see below; without it the meter
  reads a muted `n/a` and its tooltip says why.

### CPU power meter

The `Pwr` meter and the `Energy` row come from the CPU's RAPL energy
counters (`/sys/class/powercap/intel-rapl:*`, Intel and AMD Zen alike). Two
things keep Cuthulu from reading them by default:

1. **The kernel makes `energy_uj` root-only.** Fine-grained power readings
   can leak data from other programs on some CPUs (PLATYPUS,
   [CVE-2020-8694](https://nvd.nist.gov/vuln/detail/CVE-2020-8694)), so
   since late 2020 only root may read them. Cuthulu runs as uid 65532.
2. **Docker hides `/sys/devices/virtual/powercap`** in every container
   (same reason), so even root inside one sees dangling links.

The trade-off: letting a group read the counters gives its members (here
only the Cuthulu container) the same power readings root has. On a
single-user home server that is usually fine; on a machine shared with
untrusted local users or untrusted containers, leave it off.

**With `scripts/install.sh`:** when the machine has RAPL and you run it in a
terminal, it asks. Yes:

- creates the system group `cuthulu-energy`;
- installs `/etc/udev/rules.d/70-cuthulu-energy.rules`, which gives that
  group read access to every RAPL zone's `energy_uj` whenever the zones
  appear (boot, module reload), and applies it right away. A udev rule
  rather than systemd-tmpfiles: tmpfiles runs once at boot and misses a
  module loaded later;
- writes `/etc/cuthulu/compose.override.yaml`, which Compose merges into
  `compose.yaml`: the read-only mount
  `/sys/devices/virtual/powercap:/host/powercap`, the group's gid in
  `group_add`, and `CUTHULU_POWERCAP_DIR=/host/powercap`.

Machines without RAPL are never asked and get no mount. Without a terminal
the question is skipped; re-run the installer in one to answer it. Once
enabled it is kept on re-runs; `scripts/uninstall.sh` removes the rule, the
group and the override and puts the counters back to root-only.

**With plain `compose.yaml`:** do the same by hand, then uncomment the three
CPU power lines in `compose.yaml` (the mount, the `group_add` entry with the
group's gid, and `CUTHULU_POWERCAP_DIR`):

```sh
sudo groupadd --system cuthulu-energy
sudo tee /etc/udev/rules.d/70-cuthulu-energy.rules <<'EOF'
ACTION=="add|change", SUBSYSTEM=="powercap", KERNEL=="intel-rapl:*", \
  RUN+="/bin/chgrp cuthulu-energy /sys%p/energy_uj", RUN+="/bin/chmod g+r /sys%p/energy_uj"
EOF
sudo udevadm control --reload-rules
sudo udevadm trigger --settle --action=change --subsystem-match=powercap
getent group cuthulu-energy | cut -d: -f3    # the gid for group_add
```

`cargo run` reads `/sys/class/powercap` directly: add your user to
`cuthulu-energy` (and log in again) instead of mounting anything.

What it shows:

- **Power** is the package energy between two reads over the time between
  them, summed over CPU packages; the tooltip adds the cores' share when the
  CPU reports it. The meter's full scale is the CPU's long-term power limit
  (PL1) when the kernel exposes it, else `CUTHULU_POWER_MAX_W` (both color
  the bar yellow / red at 70 / 90 %), else the highest power seen since
  Cuthulu started, rounded up to 5 W and never colored as a warning.
- **Energy** is counted since Cuthulu started, in memory only: a restart
  starts from zero. The counters wrap (about every 1–2 hours at full load
  on a laptop), so a light tick reads them once a minute, or more often if
  they could wrap sooner, even when nobody has the dashboard open. That is
  the only host reading that runs unwatched, and only when RAPL is
  readable.
- This is the CPU package only (cores, integrated GPU, memory controller),
  not what the machine draws from the wall.
## Notifications

Optional, three independent parts — set up any of them:

| Part | Tells you | Needs |
|------|-----------|-------|
| **Service alerts** | a watched service went down (`exited 1`, `unhealthy`, `removed`, …) and when it is back | nothing for services with a restart policy or a systemd unit, the bell for the rest; email needs SMTP, browser alerts need nothing |
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

Containers with restart policy `always` or `unless-stopped`, and services
run by a catalog systemd unit, are watched automatically from the moment
Cuthulu sees them (`on-failure` is not: such jobs often exit 0 on purpose).
For any other service, click the bell at the end of a row (or `b` on the
selected row, or the `notify` button on a service page); the same bell turns
an automatically watched service off. Choices are remembered by name in
`notify.json` (`watched` and `muted`), so they survive container
re-creation. The bell in the top bar opens the notifications dialog: the
global on/off switch for service alerts, which channels are configured, and
`send test` (a test email, one ping and a desktop notification).

### Containers managed by systemd

Catalog services are stopped through their unit and stay down (see
[Starting and stopping catalog services](#starting-and-stopping-catalog-services)).
Any other container started by a systemd unit (`ExecStart=docker run --rm …`
with `Restart=always`) comes back after you stop it in Cuthulu, because the
unit starts a new one. Cuthulu notices — the row gets a `↻` marker, the
service page says "restarts by itself", a warning shows in the page, and the
"back up" email explains it. To keep such a service down, stop the unit:
`systemctl stop <unit>` (or `systemctl --user stop <unit>`), or add it to the
catalog.

### Desktop notifications

While a Cuthulu page is open, a watched service going down raises a desktop
notification (after 10 s, so restarts stay quiet). The browser asks for
permission when you first click a bell, the alerts switch or `allow` in the
dialog. Browsers allow notifications only on secure origins:
`http://localhost` and `http://127.0.0.1` work, on any port (also through
`ssh -L`), and so does the [HTTPS tailnet address](#https-on-the-tailnet);
a plain-HTTP LAN or tailnet address does not, and alerts then show in the
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

With a ping URL set, the topbar shows a pulse icon that opens healthchecks.io
in a new tab; its tooltip says how Cuthulu's last ping went (`last ping ok 2m
ago`), and it turns red when the last ping failed or was skipped. It opens
`https://healthchecks.io/` unless `CUTHULU_HEALTHCHECK_LINK` names another
page (your project, or a self-hosted instance).

### Test and turn off

Open the bell dialog and press `send test`, or:

```sh
curl -X POST -H 'X-Cuthulu: 1' http://127.0.0.1/api/notify/test
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
  times slower. The trade-off: crates that compile C need a cross C
  compiler. The only one today is `ring` (rustls' crypto, via `lettre` and
  `ureq`), built for arm64 with Alpine's `clang`
  (`CC_aarch64_unknown_linux_musl=clang`). If a future crate needs a full
  aarch64 sysroot, use `cargo-zigbuild` or switch back to QEMU (drop
  `--platform=$BUILDPLATFORM` and add `docker/setup-qemu-action` in CI).

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

Access to the Docker socket is equivalent to root on the host, and until
`CUTHULU_AUTH_TOKEN` lands (roadmap phase 5) there is no authentication. The
compose file publishes port 80 on every interface so tailnet devices can
reach `http://<machine>/`: everyone who can reach this machine on your LAN or
tailnet can stop, start and restart its containers. If that is too wide:

- publish `127.0.0.1:80:8686` (or `-p 127.0.0.1:80:8686`) and reach it from
  elsewhere through an SSH tunnel: `ssh -L 8686:127.0.0.1:80 your-machine`,
  then <http://localhost:8686>;
- block port 80 from the LAN with the host firewall — note that Docker's
  published ports bypass `ufw`'s default rules, so use the `DOCKER-USER`
  chain;
- set `CUTHULU_READ_ONLY=true` if you only want to watch.

The [CPU power meter](#cpu-power-meter) is opt-in because reading the RAPL
energy counters is root-only by default (a power side channel); enabling it
lets the `cuthulu-energy` group read them.

The [env editor](#editing-env-files) asks for your sudo password on every
load and save; the password crosses the network in clear text over plain
`http://`, so prefer the [HTTPS sidecar](#https-on-the-tailnet) (or
`localhost`) when you use it.

`cargo run` and the bare binary keep listening on `127.0.0.1:8686`.

Cuthulu only answers to host names a website cannot take over: `localhost`,
IP addresses, single-word names (MagicDNS short names), and names ending in
`.ts.net`, `.local`, `.lan`, `.home.arpa`, `.internal` or `.localhost`. Any
other `Host` gets `421 Misdirected Request`. This stops DNS rebinding, where
a page you visit points its own domain at your machine and then uses the
dashboard as if it were same-origin. If you reach Cuthulu under another
name (say your own domain pointing at the machine), list it:

```dotenv
CUTHULU_ALLOWED_HOSTS=dash.example.com,.home.example.org
```

A leading dot allows the domain and its subdomains; `*` turns the check off.
