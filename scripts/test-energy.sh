#!/usr/bin/env bash
# Tests for the CPU power meter step of scripts/install.sh and its removal by
# scripts/uninstall.sh: the prompt, the udev rule, the compose override. Needs no root, Docker
# or network: groupadd, groupdel, getent, udevadm, chgrp and chmod are stubs on PATH, and every
# path the scripts write to is in a temp dir.
#
#   scripts/test-energy.sh

# The snippets in single quotes run in a child bash that sources the scripts; they must not
# expand here.
# shellcheck disable=SC2016
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
readonly ROOT
readonly INSTALL=$ROOT/scripts/install.sh
readonly UNINSTALL=$ROOT/scripts/uninstall.sh

TMP=$(mktemp -d)
readonly TMP
trap 'rm -rf "$TMP"' EXIT

failures=0
tests=0
pass() { tests=$((tests + 1)); printf 'ok    %s\n' "$1"; }
fail() { tests=$((tests + 1)); failures=$((failures + 1)); printf 'FAIL  %s\n' "$1"; }
# check NAME EXPECTED ACTUAL
check() {
  if [[ $2 == "$3" ]]; then
    pass "$1"
  else
    fail "$1"
    printf '      expected: %q\n      actual:   %q\n' "$2" "$3"
  fi
}
# check_contains NAME NEEDLE HAYSTACK
check_contains() {
  if [[ $3 == *"$2"* ]]; then
    pass "$1"
  else
    fail "$1"
    printf '      missing: %q\n      in:      %q\n' "$2" "$3"
  fi
}
# check_absent NAME FILE...
check_absent() {
  local name=$1 f
  shift
  for f in "$@"; do
    if [[ -e $f ]]; then
      fail "$name"
      printf '      exists: %s\n' "$f"
      return
    fi
  done
  pass "$name"
}

# --- Stubs -----------------------------------------------------------------------------------
# Every call is logged to $CALLS. The group exists while $GROUP_FILE does.
mkdir -p "$TMP/bin"
export CALLS=$TMP/calls GROUP_FILE=$TMP/group
for cmd in groupadd groupdel udevadm chgrp chmod; do
  cat >"$TMP/bin/$cmd" <<EOF
#!/usr/bin/env bash
echo "$cmd \$*" >>"\$CALLS"
case $cmd in
  groupadd) echo "cuthulu-energy:x:991:" >"\$GROUP_FILE" ;;
  groupdel) rm -f "\$GROUP_FILE" ;;
esac
EOF
done
cat >"$TMP/bin/getent" <<'EOF'
#!/usr/bin/env bash
[[ $1 == group && -f $GROUP_FILE ]] && grep "^$2:" "$GROUP_FILE"
EOF
chmod +x "$TMP/bin/"*
export PATH=$TMP/bin:$PATH

# A fresh host for each case: RAPL zones (unless $1 is "none"), no rule, no group.
host() {
  rm -rf "$TMP/host" "$CALLS" "$GROUP_FILE"
  mkdir -p "$TMP/host/powercap" "$TMP/host/rules" "$TMP/host/etc"
  if [[ ${1:-} != none ]]; then
    mkdir -p "$TMP/host/powercap/intel-rapl:0"
    touch "$TMP/host/powercap/intel-rapl:0/energy_uj"
  fi
  : >"$CALLS"
}

# Run a snippet with SCRIPT sourced (functions only) and its paths in the temp host;
# TERMINAL=yes pretends stdin is a terminal.
run_in() {
  local script=$1 snippet=$2
  bash -c 'source "$1"
    SUDO=()
    ENERGY_RULE=$2/rules/70-cuthulu-energy.rules
    ENERGY_OVERRIDE=$2/etc/compose.override.yaml
    POWERCAP=$2/powercap
    is_terminal() { [[ ${TERMINAL:-} == yes ]]; }
    eval "$3"' _ "$script" "$TMP/host" "$snippet" 2>&1
}
install_step() { run_in "$INSTALL" energy_step; }
calls() { tr '\n' ';' <"$CALLS"; }
rule=$TMP/host/rules/70-cuthulu-energy.rules
override=$TMP/host/etc/compose.override.yaml

# --- install.sh ------------------------------------------------------------------------------
host none
out=$(TERMINAL=yes install_step <<<y)
check "no RAPL: nothing asked" "" "$out"
check "no RAPL: nothing done" "" "$(calls)"

host
out=$(install_step <<<y)
check_contains "no terminal: skipped with a hint" "skipped (no terminal)" "$out"
check "no terminal: nothing done" "" "$(calls)"
check_absent "no terminal: no files" "$rule" "$override"

for answer in "" n no N whatever; do
  host
  out=$(TERMINAL=yes install_step <<<"$answer")
  check_contains "answer '$answer': asks with the trade-off" "CVE-2020-8694" "$out"
  check_contains "answer '$answer': not enabled" "CPU power meter not enabled" "$out"
  check "answer '$answer': nothing done" "" "$(calls)"
done
host
out=$(TERMINAL=yes install_step </dev/null)
check_contains "end of input: not enabled" "CPU power meter not enabled" "$out"

for answer in y Y yes YES; do
  host
  out=$(TERMINAL=yes install_step <<<"$answer")
  check_contains "answer '$answer': enabled" "may read the CPU energy counters" "$out"
done
check "yes: group, rule reload, trigger" \
  "groupadd --system cuthulu-energy;udevadm control --reload-rules;udevadm trigger --settle --action=change --subsystem-match=powercap;" \
  "$(calls)"
check_contains "the rule lets the group read energy_uj" \
  'RUN+="/bin/chgrp cuthulu-energy /sys%p/energy_uj", RUN+="/bin/chmod g+r /sys%p/energy_uj"' \
  "$(cat "$rule")"
check_contains "the rule matches the RAPL zones on add and change" \
  'ACTION=="add|change", SUBSYSTEM=="powercap", KERNEL=="intel-rapl:*"' "$(cat "$rule")"
check_contains "the override mounts powercap read-only" \
  "- /sys/devices/virtual/powercap:/host/powercap:ro" "$(cat "$override")"
check_contains "the override adds the group's gid" '- "991" # cuthulu-energy' "$(cat "$override")"
check_contains "the override points Cuthulu at the mount" \
  "CUTHULU_POWERCAP_DIR: /host/powercap" "$(cat "$override")"
if udevadm_real=$(PATH=${PATH#"$TMP/bin:"} command -v udevadm) \
  && "$udevadm_real" verify --help >/dev/null 2>&1; then
  if out=$("$udevadm_real" verify "$rule" 2>&1); then
    pass "udevadm verify accepts the rule"
  else
    fail "udevadm verify accepts the rule"
    echo "$out"
  fi
else
  echo "skip  udevadm verify (not available)"
fi

# An existing group is reused.
host
echo "cuthulu-energy:x:777:" >"$GROUP_FILE"
TERMINAL=yes install_step <<<y >/dev/null
check "existing group: no groupadd" \
  "udevadm control --reload-rules;udevadm trigger --settle --action=change --subsystem-match=powercap;" \
  "$(calls)"
check_contains "existing group: its gid" '- "777"' "$(cat "$override")"

# A re-run keeps it without asking and rewrites the override (deleted here).
rm -f "$override"
: >"$CALLS"
out=$(install_step <<<n)
check_contains "re-run: kept without asking" "enabled earlier, keeping it" "$out"
check "re-run: nothing re-applied" "" "$(calls)"
check_contains "re-run: override rewritten" '- "777"' "$(cat "$override")"

# A failing udevadm does not fail the install.
host
cat >"$TMP/bin/udevadm" <<'EOF'
#!/usr/bin/env bash
echo "udevadm $*" >>"$CALLS"
exit 1
EOF
out=$(TERMINAL=yes install_step <<<y; echo "rc=$?")
check_contains "udevadm fails: warns" "could not enable the CPU power meter" "$out"
check_contains "udevadm fails: the step still succeeds" "rc=0" "$out"
check_absent "udevadm fails: no override" "$override"
cat >"$TMP/bin/udevadm" <<'EOF'
#!/usr/bin/env bash
echo "udevadm $*" >>"$CALLS"
EOF

if docker compose version >/dev/null 2>&1; then
  mkdir -p "$TMP/compose"
  cp "$ROOT/compose.yaml" "$TMP/compose/"
  run_in "$INSTALL" 'energy_override 991' >"$TMP/compose/compose.override.yaml"
  merged=$(cd "$TMP/compose" && DOCKER_GID=988 docker compose config 2>&1)
  check_contains "compose merges the override: both groups" $'- "988"\n      - "991"' "$merged"
  check_contains "compose merges the override: the mount" \
    "source: /sys/devices/virtual/powercap" "$merged"
else
  echo "skip  docker compose config (not available)"
fi

# --- uninstall.sh ----------------------------------------------------------------------------
host
TERMINAL=yes install_step <<<y >/dev/null
: >"$CALLS"
out=$(run_in "$UNINSTALL" energy_remove)
check_contains "uninstall: says so" "CPU power meter access removed" "$out"
check_absent "uninstall: rule and override removed" "$rule" "$override"
check "uninstall: reload, restore the default, drop the group" \
  "udevadm control --reload-rules;chgrp root $TMP/host/powercap/intel-rapl:0/energy_uj;chmod g-r $TMP/host/powercap/intel-rapl:0/energy_uj;groupdel cuthulu-energy;" \
  "$(calls)"
: >"$CALLS"
out=$(run_in "$UNINSTALL" energy_remove)
check "uninstall when never enabled: silent" "" "$out"
check "uninstall when never enabled: nothing done" "" "$(calls)"

echo
if ((failures)); then
  echo "$failures of $tests tests failed"
  exit 1
fi
echo "all $tests tests passed"
