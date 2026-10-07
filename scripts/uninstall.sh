#!/usr/bin/env bash
# Remove the Cuthulu systemd service.
#
#   scripts/uninstall.sh [--purge]
#
# Stops Cuthulu (its containers are removed) and removes cuthulu.service. By default
# /etc/cuthulu (settings, secrets), the data volumes and the images are kept, so a reinstall picks
# up where it left off. --purge deletes them too: TODOs, notification settings and the tailscale
# sidecar's identity are lost.
set -euo pipefail

readonly SERVICE=cuthulu
readonly PROJECT=cuthulu
readonly INSTALL_DIR=/etc/cuthulu

purge=false
case ${1:-} in
  "") ;;
  --purge) purge=true ;;
  -h | --help) sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
  *) echo "error: unknown argument '$1'" >&2; exit 1 ;;
esac

SUDO=()
[[ $EUID -ne 0 ]] && SUDO=(sudo)

# Stopping the unit runs `compose down`.
"${SUDO[@]}" systemctl disable --now "$SERVICE" 2>/dev/null || true
"${SUDO[@]}" rm -f "/etc/systemd/system/$SERVICE.service"
"${SUDO[@]}" systemctl daemon-reload
echo "==> Service removed"

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
