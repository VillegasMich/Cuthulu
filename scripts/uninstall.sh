#!/usr/bin/env bash
# Remove the Cuthulu systemd service.
#
#   scripts/uninstall.sh [--purge]
#
# Stops Cuthulu (its containers are removed) and removes cuthulu.service, and the CPU power
# meter's access if install.sh enabled it (udev rule, group cuthulu-energy, compose override). By
# default /etc/cuthulu (settings, secrets), the data volumes and the images are kept, so a
# reinstall picks up where it left off. --purge deletes them too: TODOs, notification settings
# and the tailscale sidecar's identity are lost.
set -euo pipefail

readonly SERVICE=cuthulu
readonly PROJECT=cuthulu
readonly INSTALL_DIR=/etc/cuthulu
readonly ENERGY_GROUP=cuthulu-energy
# Not readonly: the tests point these into a temp dir. Keep in sync with scripts/install.sh.
ENERGY_RULE=/etc/udev/rules.d/70-cuthulu-energy.rules
ENERGY_OVERRIDE=$INSTALL_DIR/compose.override.yaml
POWERCAP=/sys/class/powercap
SUDO=()

# Undo install.sh's CPU power meter step; nothing to do if it was never enabled.
energy_remove() {
  local f
  if [[ ! -f $ENERGY_RULE && ! -f $ENERGY_OVERRIDE ]] \
    && ! getent group "$ENERGY_GROUP" >/dev/null; then
    return 0
  fi
  "${SUDO[@]}" rm -f "$ENERGY_RULE" "$ENERGY_OVERRIDE"
  "${SUDO[@]}" udevadm control --reload-rules 2>/dev/null || true
  # Back to the kernel's root-only default now, not only after the next boot.
  for f in "$POWERCAP"/intel-rapl:*/energy_uj; do
    [[ -e $f ]] || continue
    "${SUDO[@]}" chgrp root "$f" 2>/dev/null || true
    "${SUDO[@]}" chmod g-r "$f" 2>/dev/null || true
  done
  if getent group "$ENERGY_GROUP" >/dev/null; then
    "${SUDO[@]}" groupdel "$ENERGY_GROUP" || true
  fi
  echo "==> CPU power meter access removed"
}

main() {
  local purge=false image
  case ${1:-} in
    "") ;;
    --purge) purge=true ;;
    -h | --help) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "error: unknown argument '$1'" >&2; exit 1 ;;
  esac

  [[ $EUID -ne 0 ]] && SUDO=(sudo)

  # Stopping the unit runs `compose down`.
  "${SUDO[@]}" systemctl disable --now "$SERVICE" 2>/dev/null || true
  "${SUDO[@]}" rm -f "/etc/systemd/system/$SERVICE.service"
  "${SUDO[@]}" systemctl daemon-reload
  echo "==> Service removed"

  energy_remove

  if [[ $purge == true ]]; then
    image=$("${SUDO[@]}" sed -n 's/^IMAGE=//p' "$INSTALL_DIR/.env" 2>/dev/null | tail -n 1 || true)
    if command -v docker >/dev/null 2>&1; then
      "${SUDO[@]}" docker rm --force "$SERVICE" "$SERVICE-tailscale" >/dev/null 2>&1 || true
      "${SUDO[@]}" docker volume rm "${PROJECT}_cuthulu-data" "${PROJECT}_tailscale-state" \
        >/dev/null 2>&1 || true
      "${SUDO[@]}" docker image rm cuthulu:local ${image:+"$image"} >/dev/null 2>&1 || true
    fi
    "${SUDO[@]}" rm -rf "$INSTALL_DIR"
    echo "==> Purged $INSTALL_DIR, the data volumes and the image"
  fi
}

# Sourcing (the tests do) only defines the functions.
if [[ ${BASH_SOURCE[0]} == "$0" ]]; then main "$@"; fi
