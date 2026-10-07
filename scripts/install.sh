#!/usr/bin/env bash
# Install Cuthulu as a systemd service (Docker Compose under systemd).
#
#   scripts/install.sh [--build] [--reconfigure]
#
#   --build        build the image from this checkout (tagged cuthulu:local) instead of pulling a
#                  published one.
#   --reconfigure  rewrite /etc/cuthulu/.env from this checkout's .env (or .env.example) instead
#                  of keeping the installed one.
#
# Copies compose.yaml and the settings to /etc/cuthulu, pulls (or builds) the image, and enables
# and starts cuthulu.service. Run it as your normal user: it uses sudo only for system changes.
# Host requirements: docker with the compose plugin, systemd.
#
# Image: IMAGE exported when running this script (e.g. villegasmich/cuthulu:1.2.3), else the one
# in /etc/cuthulu/.env, else villegasmich/cuthulu:latest. Upgrade: re-run, with IMAGE exported to
# switch to another release. Settings (SMTP, healthcheck, ...): edit /etc/cuthulu/.env, then
# `sudo systemctl reload cuthulu`. Re-run after pulling a new compose.yaml into this checkout.
set -euo pipefail

readonly SERVICE=cuthulu
readonly PROJECT=cuthulu
readonly DEFAULT_IMAGE=villegasmich/cuthulu:latest
readonly LOCAL_IMAGE=cuthulu:local
readonly INSTALL_DIR=/etc/cuthulu
readonly ENV_FILE=$INSTALL_DIR/.env
readonly UNIT_FILE=/etc/systemd/system/$SERVICE.service
readonly SOCKET=/var/run/docker.sock

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
readonly ROOT

log() { printf '\033[1m==>\033[0m %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not installed${2:+ ($2)}"; }

build=false
reconfigure=false
for arg in "$@"; do
  case $arg in
    --build) build=true ;;
    --reconfigure) reconfigure=true ;;
    -h | --help) sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument '$arg' (try --help)" ;;
  esac
done

SUDO=()
if [[ $EUID -ne 0 ]]; then
  need sudo
  SUDO=(sudo)
fi

# --- Dependencies ----------------------------------------------------------------------------
need systemctl "systemd is required to run the service"
need docker "https://docs.docker.com/engine/install/"
DOCKER=(docker)
if ! docker info >/dev/null 2>&1; then
  DOCKER=("${SUDO[@]}" docker)
  "${DOCKER[@]}" info >/dev/null 2>&1 || die "cannot talk to the Docker daemon; is it running?"
fi
"${DOCKER[@]}" compose version >/dev/null 2>&1 \
  || die "the Docker Compose plugin is required (https://docs.docker.com/compose/install/)"
[[ -S $SOCKET ]] || die "$SOCKET not found; Cuthulu needs the Docker socket"
docker_gid=$(stat -c %g "$SOCKET")

# A container named cuthulu from `docker run` or another compose project would block the
# service's (container_name: cuthulu), and its data lives in a different volume.
project=$("${DOCKER[@]}" inspect --format '{{index .Config.Labels "com.docker.compose.project"}}' \
  "$SERVICE" 2>/dev/null || true)
if "${DOCKER[@]}" inspect "$SERVICE" >/dev/null 2>&1 && [[ $project != "$PROJECT" ]]; then
  die "a '$SERVICE' container not started by compose project '$PROJECT' exists; remove it" \
    "first (docker rm -f $SERVICE). Its data volume is kept; see docs/DEPLOYMENT.md#data"
fi

# --- Environment file ------------------------------------------------------------------------
# The env file holds secrets (SMTP password, healthcheck URL), so it is root-only (0600).
keep_env=false
if "${SUDO[@]}" test -f "$ENV_FILE" && [[ $reconfigure == false ]]; then keep_env=true; fi

# Replace KEY's line in the env file, or add it.
set_env() {
  "${SUDO[@]}" sed -i "/^$1=/d" "$ENV_FILE"
  printf '%s=%s\n' "$1" "$2" | "${SUDO[@]}" tee -a "$ENV_FILE" >/dev/null
}

"${SUDO[@]}" install -d -m 0755 "$INSTALL_DIR"
if [[ $keep_env == true ]]; then
  log "Keeping existing $ENV_FILE (use --reconfigure to rewrite it)"
else
  seed=$ROOT/.env
  [[ -f $seed ]] || seed=$ROOT/.env.example
  log "Writing $ENV_FILE from ${seed#"$ROOT"/} (mode 600, root only)"
  "${SUDO[@]}" install -m 0600 "$seed" "$ENV_FILE"
fi

# --- Image -----------------------------------------------------------------------------------
# Same precedence as compose: exported IMAGE, then the env file, then the default.
if [[ $build == true ]]; then
  image=$LOCAL_IMAGE
  log "Building Docker image $image"
  "${DOCKER[@]}" build --tag "$image" \
    --build-arg "VERSION=$("$ROOT/scripts/bump-version.sh")" \
    --build-arg "GIT_SHA=$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || true)" "$ROOT"
else
  image=${IMAGE:-}
  if [[ -z $image ]]; then
    image=$("${SUDO[@]}" sed -n 's/^IMAGE=//p' "$ENV_FILE" | tail -n 1)
  fi
  image=${image:-$DEFAULT_IMAGE}
  if [[ $image == "$LOCAL_IMAGE" ]]; then
    "${DOCKER[@]}" image inspect "$image" >/dev/null 2>&1 \
      || die "$image is not built; run with --build, or export IMAGE=$DEFAULT_IMAGE"
  else
    log "Pulling Docker image $image"
    "${DOCKER[@]}" pull "$image" || die "cannot pull '$image'; check IMAGE"
  fi
fi
set_env IMAGE "$image"
set_env DOCKER_GID "$docker_gid"

# --- Compose file and systemd unit -----------------------------------------------------------
log "Installing $INSTALL_DIR/compose.yaml"
"${SUDO[@]}" install -m 0644 "$ROOT/compose.yaml" "$INSTALL_DIR/compose.yaml"

log "Installing $UNIT_FILE"
docker_bin=$(command -v docker)
sed "s|@DOCKER@|$docker_bin|g" "$ROOT/deploy/systemd/$SERVICE.service" \
  | "${SUDO[@]}" tee "$UNIT_FILE" >/dev/null

"${SUDO[@]}" systemctl daemon-reload
"${SUDO[@]}" systemctl enable "$SERVICE" >/dev/null
# restart, not start: a re-run picks up the new image, compose.yaml and settings.
"${SUDO[@]}" systemctl restart "$SERVICE"

log "Done. $SERVICE is running and will start on boot."
echo "    Image:     $image"
echo "    Open:      http://localhost/ (or http://<machine>/ from the tailnet)"
echo "    Settings:  sudo \$EDITOR $ENV_FILE && sudo systemctl reload $SERVICE"
echo "    Logs:      docker logs -f $SERVICE"
echo "    Status:    systemctl status $SERVICE"
echo "    Remove:    scripts/uninstall.sh [--purge]"
