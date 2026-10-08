#!/usr/bin/env bash
# Install Cuthulu as a systemd service (Docker Compose under systemd).
#
#   scripts/install.sh [--build] [--reconfigure] [--companions LIST]
#   scripts/install.sh --companions-only [--companions LIST]
#   scripts/install.sh --list-companions
#
#   --build            build the image from this checkout (tagged cuthulu:local) instead of pulling
#                      a published one.
#   --reconfigure      rewrite /etc/cuthulu/.env from this checkout's .env (or .env.example)
#                      instead of keeping the installed one.
#   --companions LIST  companion services to install after Cuthulu, without asking: names
#                      separated by commas, `all` or `none`. Without it, a terminal gets a list to
#                      pick from; no terminal (e.g. piped stdin) skips them.
#   --companions-only  leave Cuthulu alone (no image, /etc/cuthulu, service restart or sudo for
#                      it) and only offer, or install with --companions, the companion services.
#   --list-companions  only show the companion services and whether each is running, installed
#                      or not installed; changes nothing.
#
# Copies compose.yaml and the settings to /etc/cuthulu, pulls (or builds) the image, and enables
# and starts cuthulu.service. Run it as your normal user: it uses sudo only for system changes.
# Host requirements: docker with the compose plugin, systemd.
#
# Image: IMAGE exported when running this script (e.g. villegasmich/cuthulu:1.2.3), else the one
# in /etc/cuthulu/.env, else villegasmich/cuthulu:latest. Upgrade: re-run, with IMAGE exported to
# switch to another release. Settings (SMTP, healthcheck, ...): edit /etc/cuthulu/.env, then
# `sudo systemctl reload cuthulu`. Re-run after pulling a new compose.yaml into this checkout.
#
# Companions (deploy/companions/*.conf) are cloned to ~/.local/share/cuthulu/companions/<name>
# and installed with their own scripts/install.sh and published image; picking an installed one
# upgrades and restarts it. scripts/uninstall.sh leaves them alone.
set -euo pipefail

readonly SERVICE=cuthulu
readonly PROJECT=cuthulu
readonly DEFAULT_IMAGE=villegasmich/cuthulu:latest
readonly LOCAL_IMAGE=cuthulu:local
readonly INSTALL_DIR=/etc/cuthulu
readonly ENV_FILE=$INSTALL_DIR/.env
readonly UNIT_FILE=/etc/systemd/system/$SERVICE.service
readonly SOCKET=/var/run/docker.sock
# Host helper container of the env editor (busybox has nsenter), pinned by digest. Keep in sync
# with DEFAULT_HELPER_IMAGE in src/config.rs (a test checks).
readonly HELPER_IMAGE=busybox@sha256:73aaf090f3d85aa34ee199857f03fa3a95c8ede2ffd4cc2cdb5b94e566b11662
# Every catalog entry needs these; SUGGEST (true|false, default true) is optional.
readonly COMPANION_KEYS=(NAME DESCRIPTION REPO IMAGE CONTAINER UNIT UNIT_SCOPE ENV_FILE)
# Width of the status column: the longest status, "not installed".
readonly STATUS_WIDTH=13

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
readonly ROOT
readonly COMPANIONS_CATALOG=$ROOT/deploy/companions
COMPANIONS_DIR=${XDG_DATA_HOME:-$HOME/.local/share}/cuthulu/companions

log() { printf '\033[1m==>\033[0m %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not installed${2:+ ($2)}"; }
# A function, not a test of $EUID inline, so the tests can pretend to be root.
is_root() { [[ $EUID -eq 0 ]]; }

# --- Companion catalog -----------------------------------------------------------------------
# Suggestable entries in catalog order, and every entry's fields by NAME (including those with
# SUGGEST=false, which other readers of the catalog still need).
companions=()
declare -A c_description=() c_repo=() c_image=() c_container=() c_unit=() c_scope=()
declare -A c_env_file=() c_status=()
# picked[i] is 1 when companions[i] is selected.
picked=()

# Read one catalog file: `KEY=value` lines, blank lines and `#` comments. Nothing is evaluated
# (the value is the rest of the line, verbatim), so other tools can read the same files. Returns
# 1 with a warning for a malformed file.
companion_parse() {
  local file=$1 line key n=0 name
  local -A kv=()
  while IFS= read -r line || [[ -n $line ]]; do
    n=$((n + 1))
    line=${line%$'\r'}
    [[ -z ${line//[[:space:]]/} || $line == '#'* ]] && continue
    if [[ ! $line =~ ^([A-Z][A-Z0-9_]*)=(.*)$ ]]; then
      warn "$file:$n: expected KEY=value"
      return 1
    fi
    key=${BASH_REMATCH[1]}
    if [[ -n ${kv[$key]+set} ]]; then
      warn "$file:$n: $key is set twice"
      return 1
    fi
    kv[$key]=${BASH_REMATCH[2]}
  done <"$file"

  for key in "${COMPANION_KEYS[@]}"; do
    [[ -n ${kv[$key]:-} ]] || { warn "$file: $key is missing or empty"; return 1; }
  done
  name=${kv[NAME]}
  # The values end up as arguments (git, docker, systemctl, a path): no leading dash, no spaces.
  local problem=
  if [[ ! $name =~ ^[a-z0-9][a-z0-9._-]*$ ]]; then
    problem="NAME '$name' must be lowercase letters, digits, '.', '_' or '-'"
  elif [[ $(basename "$file" .conf) != "$name" ]]; then
    problem="NAME '$name' must match the file name"
  elif [[ ! ${kv[UNIT_SCOPE]} =~ ^(system|user)$ ]]; then
    problem="UNIT_SCOPE must be system or user"
  elif [[ ! ${kv[SUGGEST]:-true} =~ ^(true|false)$ ]]; then
    problem="SUGGEST must be true or false"
  elif [[ ! ${kv[IMAGE]} =~ ^[A-Za-z0-9][A-Za-z0-9._/:@-]*$ ]]; then
    problem="IMAGE '${kv[IMAGE]}' is not a Docker image name"
  elif [[ ! ${kv[CONTAINER]} =~ ^[A-Za-z0-9][A-Za-z0-9_.-]*$ ]]; then
    problem="CONTAINER '${kv[CONTAINER]}' is not a container name"
  elif [[ ! ${kv[UNIT]} =~ ^[A-Za-z0-9][A-Za-z0-9_.@-]*$ ]]; then
    problem="UNIT '${kv[UNIT]}' is not a unit name"
  elif [[ ${kv[REPO]} == -* || ${kv[REPO]} =~ [[:space:]] ]]; then
    problem="REPO '${kv[REPO]}' is not a git URL"
  fi
  [[ -z $problem ]] || { warn "$file: $problem"; return 1; }

  c_description[$name]=${kv[DESCRIPTION]}
  c_repo[$name]=${kv[REPO]}
  c_image[$name]=${kv[IMAGE]}
  c_container[$name]=${kv[CONTAINER]}
  c_unit[$name]=${kv[UNIT]%.service}
  c_scope[$name]=${kv[UNIT_SCOPE]}
  # Not used by the install itself; kept so the whole entry is checked and available.
  # shellcheck disable=SC2034
  c_env_file[$name]=${kv[ENV_FILE]}
  [[ ${kv[SUGGEST]:-true} == false ]] || companions+=("$name")
}

# Load every *.conf in a catalog directory. A broken file is skipped with a warning: it must not
# stop Cuthulu's own install.
companions_load() {
  local file
  companions=()
  picked=()
  for file in "$1"/*.conf; do
    [[ -f $file ]] || continue
    companion_parse "$file" || warn "skipping $(basename "$file")"
  done
  for file in "${companions[@]}"; do picked+=(0); done
}

# --- Detection -------------------------------------------------------------------------------
# running: the unit is active in its scope, or the container runs (e.g. started by hand).
# installed: systemd knows the unit, but it is not active. Read-only; never needs sudo (docker
# without access just looks like "no container").
companion_status() {
  local name=$1 scope=()
  local unit=${c_unit[$name]}.service
  [[ ${c_scope[$name]} == user ]] && scope=(--user)
  if systemctl "${scope[@]}" is-active --quiet "$unit" 2>/dev/null; then
    echo running
  elif [[ $(docker inspect --type container --format '{{.State.Running}}' \
    "${c_container[$name]}" 2>/dev/null) == true ]]; then
    echo running
  elif [[ $(systemctl "${scope[@]}" show --property=LoadState --value "$unit" 2>/dev/null) \
    == loaded ]]; then
    echo installed
  else
    echo not-installed
  fi
}

companions_detect() {
  local name
  for name in "${companions[@]}"; do c_status[$name]=$(companion_status "$name"); done
}

# --- Selection -------------------------------------------------------------------------------
term_cols() {
  local cols=${COLUMNS:-}
  [[ $cols =~ ^[0-9]+$ ]] || cols=$(stty size 2>/dev/null </dev/tty | cut -d ' ' -f 2 || true)
  if [[ ! $cols =~ ^[0-9]+$ ]] || ((cols == 0)); then cols=80; fi
  echo "$cols"
}

# Print the numbered list, `[x]` marking picked entries (no marks with --no-marks):
#    1 [ ] name   status         description, wrapped to the terminal and aligned under itself
# On a narrow terminal the description goes on its own lines, under the name.
companions_print() {
  local marks=true
  [[ ${1:-} == --no-marks ]] && marks=false
  local i name status box line cols width_name=4 width_num=${#companions[@]}
  width_num=${#width_num}
  for name in "${companions[@]}"; do ((${#name} > width_name)) && width_name=${#name}; done
  cols=$(term_cols)

  local box_width=4
  [[ $marks == true ]] || box_width=0
  local indent=$((2 + width_num + 1 + box_width + width_name + 2 + STATUS_WIDTH + 2))
  local desc_width=$((cols - indent)) below=false
  if ((desc_width < 30)); then
    below=true
    indent=$((2 + width_num + 1 + box_width))
    desc_width=$((cols - indent))
    ((desc_width >= 20)) || desc_width=20
  fi

  for i in "${!companions[@]}"; do
    name=${companions[i]}
    status=${c_status[$name]:-unknown}
    box=
    if [[ $marks == true ]]; then
      box='[ ] '
      ((picked[i])) && box='[x] '
    fi
    line=$(printf '  %*d %s%-*s  %-*s' "$width_num" $((i + 1)) "$box" "$width_name" "$name" \
      "$STATUS_WIDTH" "${status/-/ }")
    local wrapped=()
    mapfile -t wrapped < <(fold -s -w "$desc_width" <<<"${c_description[$name]}" | sed 's/ *$//')
    if [[ $below == true ]]; then
      printf '%s\n' "${line%"${line##*[! ]}"}"
    else
      printf '%s  %s\n' "$line" "${wrapped[0]}"
      wrapped=("${wrapped[@]:1}")
    fi
    for line in "${wrapped[@]}"; do printf '%*s%s\n' "$indent" '' "$line"; done
  done
}

# Apply one line typed at the prompt to picked[]: numbers toggle their entry, `all` and `none`
# set every entry; words are separated by spaces or commas. A line with any invalid word changes
# nothing, so a typo never half-applies.
companions_toggle() {
  local word i count=${#companions[@]} words=()
  local next=("${picked[@]}")
  read -ra words <<<"${1//,/ }"
  for word in "${words[@]}"; do
    case ${word,,} in
      all) for i in "${!next[@]}"; do next[i]=1; done ;;
      none) for i in "${!next[@]}"; do next[i]=0; done ;;
      *)
        if [[ ! $word =~ ^[0-9]+$ ]] || ((10#$word < 1 || 10#$word > count)); then
          warn "'$word' is not a number from 1 to $count, all or none"
          return 1
        fi
        i=$((10#$word - 1))
        next[i]=$((1 - next[i]))
        ;;
    esac
  done
  picked=("${next[@]}")
}

# Ask on the terminal until an empty line. End of input picks nothing.
companions_prompt() {
  local answer
  log "Companion services: other tools that can run next to Cuthulu (optional)"
  echo "    Picking one that is running or installed upgrades and restarts it."
  while true; do
    echo
    companions_print
    if ! read -r -p "Toggle with numbers (e.g. 1 3), all or none; Enter to confirm: " answer; then
      echo
      companions_toggle none
      return 0
    fi
    [[ -n ${answer//[[:space:]]/} ]] || return 0
    companions_toggle "$answer" || true
  done
}

# Set picked[] from --companions: names separated by commas, `all` or `none`.
companions_pick_list() {
  local list=$1 word i found names=()
  companions_toggle none
  case $list in
    all) companions_toggle all; return 0 ;;
    none) return 0 ;;
  esac
  IFS=, read -ra names <<<"$list"
  for word in "${names[@]}"; do
    [[ -n $word ]] || continue
    found=false
    for i in "${!companions[@]}"; do
      [[ ${companions[i]} == "$word" ]] && picked[i]=1 && found=true
    done
    [[ $found == true ]] || {
      warn "unknown companion '$word'; known: ${companions[*]:-none}"
      return 1
    }
  done
}

# --- Install ---------------------------------------------------------------------------------
# Clone (or fast-forward) the companion's repository and run its own installer as this user (it
# uses sudo itself if it needs to), pulling the published image instead of building one.
companion_install() {
  local name=$1 dir=$COMPANIONS_DIR/$1 image=${c_image[$1]}
  # Tagless: the latest release. An explicit tag or digest in the catalog is kept.
  [[ ${image##*/} == *[:@]* ]] || image+=:latest
  if [[ -d $dir/.git ]]; then
    log "Updating $name in $dir"
    git -C "$dir" pull --ff-only --quiet || return
  else
    log "Downloading $name to $dir"
    mkdir -p "$COMPANIONS_DIR" || return
    git clone --quiet -- "${c_repo[$name]}" "$dir" || return
  fi
  log "Running $name's installer (IMAGE=$image)"
  IMAGE=$image "$dir/scripts/install.sh"
}

# The companions step after Cuthulu is installed. $1 is the --companions value ("" if not
# given). Returns 1 if any picked companion failed to install; the others are still tried.
companions_step() {
  local flag=$1 i name rc failed=0 chosen=() results=()
  ((${#companions[@]})) || return 0
  if is_root; then
    log "Companion services skipped: run scripts/install.sh as your normal user to get them"
    return 0
  fi
  if [[ -z $flag ]]; then
    if [[ ! -t 0 ]]; then
      log "Companion services skipped (no terminal); pick them with --companions LIST (see --help)"
      return 0
    fi
    echo
    companions_detect
    companions_prompt
  fi
  for i in "${!companions[@]}"; do ((picked[i])) && chosen+=("${companions[i]}"); done
  if ((${#chosen[@]} == 0)); then
    [[ $flag == none ]] || log "No companion services selected"
    return 0
  fi
  command -v git >/dev/null 2>&1 || {
    warn "'git' is required to install companion services; skipped ${chosen[*]}"
    return 1
  }

  for name in "${chosen[@]}"; do
    if companion_install "$name"; then
      results+=("installed")
    else
      rc=$?
      results+=("FAILED (exit $rc)")
      failed=1
    fi
  done
  log "Companion services"
  for i in "${!chosen[@]}"; do printf '    %-30s %s\n' "${chosen[i]}" "${results[i]}"; done
  ((failed == 0)) || warn "some companion services failed; Cuthulu itself is installed"
  return "$failed"
}

# --- Cuthulu ---------------------------------------------------------------------------------
# Replace KEY's line in the env file, or add it.
set_env() {
  "${SUDO[@]}" sed -i "/^$1=/d" "$ENV_FILE"
  printf '%s=%s\n' "$1" "$2" | "${SUDO[@]}" tee -a "$ENV_FILE" >/dev/null
}

install_cuthulu() {
  SUDO=()
  if [[ $EUID -ne 0 ]]; then
    need sudo
    SUDO=(sudo)
  fi

  # --- Dependencies --------------------------------------------------------------------------
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

  # --- Environment file ----------------------------------------------------------------------
  # The env file holds secrets (SMTP password, healthcheck URL), so it is root-only (0600).
  keep_env=false
  if "${SUDO[@]}" test -f "$ENV_FILE" && [[ $reconfigure == false ]]; then keep_env=true; fi

  "${SUDO[@]}" install -d -m 0755 "$INSTALL_DIR"
  if [[ $keep_env == true ]]; then
    log "Keeping existing $ENV_FILE (use --reconfigure to rewrite it)"
  else
    seed=$ROOT/.env
    [[ -f $seed ]] || seed=$ROOT/.env.example
    log "Writing $ENV_FILE from ${seed#"$ROOT"/} (mode 600, root only)"
    "${SUDO[@]}" install -m 0600 "$seed" "$ENV_FILE"
  fi

  # --- Image ---------------------------------------------------------------------------------
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

  # --- Env editor ----------------------------------------------------------------------------
  # The dashboard's "edit env" asks for this user's sudo password (never root's: sudo asks root
  # for none). An existing value is kept.
  if ! "${SUDO[@]}" grep -q '^CUTHULU_HOST_USER=.' "$ENV_FILE"; then
    host_user=${SUDO_USER:-$(id -un)}
    if [[ $host_user == root ]]; then
      warn "run as root: set CUTHULU_HOST_USER=<your user> in $ENV_FILE to edit env files"
    else
      set_env CUTHULU_HOST_USER "$host_user"
    fi
  fi
  # Pulled now so editing works offline later; an override in the env file wins.
  helper_image=$("${SUDO[@]}" sed -n 's/^CUTHULU_HELPER_IMAGE=//p' "$ENV_FILE" | tail -n 1)
  helper_image=${helper_image:-$HELPER_IMAGE}
  log "Pulling the env editor's helper image $helper_image"
  "${DOCKER[@]}" pull --quiet "$helper_image" >/dev/null \
    || warn "cannot pull $helper_image; the env editor pulls it on first use"

  # --- Compose file and systemd unit ---------------------------------------------------------
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
}

main() {
  build=false
  reconfigure=false
  list_companions=false
  local companions_only=false companions_flag=
  while (($#)); do
    case $1 in
      --build) build=true ;;
      --reconfigure) reconfigure=true ;;
      --companions)
        if (($# < 2)) || [[ -z $2 ]]; then
          die "--companions needs a list (names, all or none)"
        fi
        companions_flag=$2
        shift
        ;;
      --companions=*)
        companions_flag=${1#*=}
        [[ -n $companions_flag ]] || die "--companions needs a list (names, all or none)"
        ;;
      --companions-only) companions_only=true ;;
      --list-companions) list_companions=true ;;
      -h | --help) sed -n '2,31p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
      *) die "unknown argument '$1' (try --help)" ;;
    esac
    shift
  done

  if [[ $companions_only == true ]]; then
    [[ $build == false && $reconfigure == false ]] \
      || die "--companions-only does not install Cuthulu; drop --build and --reconfigure"
    [[ $companions_flag != none ]] || die "--companions-only with --companions none does nothing"
  fi

  # Before anything is installed, so a typo in --companions changes nothing.
  companions_load "$COMPANIONS_CATALOG"
  if [[ $list_companions == true ]]; then
    companions_detect
    companions_print --no-marks
    exit 0
  fi
  if [[ -n $companions_flag ]]; then
    companions_pick_list "$companions_flag" || die "invalid --companions '$companions_flag'"
  fi

  [[ $companions_only == true ]] || install_cuthulu
  companions_step "$companions_flag"
}

# Sourcing (the tests do) only defines the functions.
if [[ ${BASH_SOURCE[0]} == "$0" ]]; then main "$@"; fi
