#!/usr/bin/env bash
# Tests for the companions step of scripts/install.sh: catalog parsing, detection, the toggle
# prompt, --companions and --list-companions. Needs no root, Docker or network: systemctl and
# docker are stubs on PATH, and the "remote" repositories are local git repos.
#
#   scripts/test-companions.sh

# The snippets in single quotes run in a child bash that sources install.sh; they must not expand
# here.
# shellcheck disable=SC2016
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
readonly ROOT
readonly INSTALL=$ROOT/scripts/install.sh

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

# --- Stubs -----------------------------------------------------------------------------------
# systemctl: units in STUB_ACTIVE / STUB_USER_ACTIVE are active, those in STUB_LOADED /
# STUB_USER_LOADED are known to systemd. docker: containers in STUB_RUNNING run.
mkdir -p "$TMP/bin"
cat >"$TMP/bin/systemctl" <<'EOF'
#!/usr/bin/env bash
active=${STUB_ACTIVE:-} loaded=${STUB_LOADED:-}
if [[ $1 == --user ]]; then active=${STUB_USER_ACTIVE:-} loaded=${STUB_USER_LOADED:-}; shift; fi
unit=${*: -1}
unit=${unit%.service}
case $1 in
  is-active) [[ " $active " == *" $unit "* ]] ;;
  show) if [[ " $loaded $active " == *" $unit "* ]]; then echo loaded; else echo not-found; fi ;;
  *) echo "systemctl stub: unexpected $*" >&2; exit 2 ;;
esac
EOF
cat >"$TMP/bin/docker" <<'EOF'
#!/usr/bin/env bash
[[ $1 == inspect ]] || { echo "docker stub: unexpected $*" >&2; exit 2; }
if [[ " ${STUB_RUNNING:-} " == *" ${*: -1} "* ]]; then echo true; else exit 1; fi
EOF
chmod +x "$TMP/bin/systemctl" "$TMP/bin/docker"
export PATH=$TMP/bin:$PATH
export COLUMNS=100
# install.sh clones into $XDG_DATA_HOME; keep anything that slips through inside $TMP.
export XDG_DATA_HOME=$TMP/xdg
unset STUB_ACTIVE STUB_USER_ACTIVE STUB_LOADED STUB_USER_LOADED STUB_RUNNING

# A catalog with three good entries (one not suggested) and broken ones.
CATALOG=$TMP/catalog
mkdir -p "$CATALOG"
entry() { # NAME SCOPE [extra lines...]
  local name=$1 scope=$2
  shift 2
  {
    printf 'NAME=%s\nDESCRIPTION=The %s service.\nREPO=%s\nIMAGE=example/%s\n' \
      "$name" "$name" "$TMP/remote/$name" "$name"
    printf 'CONTAINER=%s-ctr\nUNIT=%s\nUNIT_SCOPE=%s\nENV_FILE=/etc/%s/env\n' \
      "$name" "$name" "$scope" "$name"
    for line in "$@"; do printf '%s\n' "$line"; done
  } >"$CATALOG/$name.conf"
}
entry alpha system '# a comment' '' 'SUGGEST=true' 'SOME_FUTURE_KEY=ignored'
entry beta user
entry gamma system
entry hidden system SUGGEST=false
printf 'NAME=bad-line\nexport FOO=bar\n' >"$CATALOG/bad-line.conf"
entry bad-scope nobody
entry missing system
sed -i '/^REPO=/d' "$CATALOG/missing.conf"
entry dup system 'UNIT=again'
entry mismatch system
sed -i 's/^NAME=.*/NAME=other/' "$CATALOG/mismatch.conf"
entry bad-suggest system SUGGEST=maybe
entry dash-repo system
sed -i 's/^REPO=.*/REPO=--upload-pack=evil/' "$CATALOG/dash-repo.conf"

# Run a snippet with install.sh sourced (functions only) and the test catalog loaded.
run() {
  bash -c 'source "$1"; companions_load "$2" 2>/dev/null; eval "$3"' _ "$INSTALL" "$CATALOG" "$1"
}

# --- Catalog parsing -------------------------------------------------------------------------
check "real catalog: suggested entries in file order" \
  "auto-git-commit-tool claude-session-starter producer-tag-on-merge" \
  "$(bash -c 'source "$1"; companions_load "$2"; echo "${companions[*]}"' _ "$INSTALL" \
    "$ROOT/deploy/companions")"
check "real catalog: cuthulu is read but not suggested" "cuthulu system /etc/cuthulu/.env" \
  "$(bash -c 'source "$1"; companions_load "$2"
    echo "${c_unit[cuthulu]} ${c_scope[cuthulu]} ${c_env_file[cuthulu]}"' _ "$INSTALL" \
    "$ROOT/deploy/companions")"
check "real catalog: no warnings" "" \
  "$(bash -c 'source "$1"; companions_load "$2" 2>&1 >/dev/null' _ "$INSTALL" \
    "$ROOT/deploy/companions")"
check "test catalog: only valid suggested entries" "alpha beta gamma" "$(run 'echo "${companions[*]}"')"
check "test catalog: fields" "user beta-ctr beta example/beta The beta service." \
  "$(run 'echo "${c_scope[beta]} ${c_container[beta]} ${c_unit[beta]} ${c_image[beta]} ${c_description[beta]}"')"
check "test catalog: nothing picked" "0 0 0" "$(run 'echo "${picked[*]}"')"
warnings=$(bash -c 'source "$1"; companions_load "$2" 2>&1 >/dev/null' _ "$INSTALL" "$CATALOG")
check_contains "warns on a non KEY=value line" "bad-line.conf:2: expected KEY=value" "$warnings"
check_contains "warns on a bad UNIT_SCOPE" "UNIT_SCOPE must be system or user" "$warnings"
check_contains "warns on a missing key" "missing.conf: REPO is missing or empty" "$warnings"
check_contains "warns on a duplicate key" "dup.conf:9: UNIT is set twice" "$warnings"
check_contains "warns when NAME does not match the file" "must match the file name" "$warnings"
check_contains "warns on a bad SUGGEST" "SUGGEST must be true or false" "$warnings"
check_contains "warns on a REPO that looks like an option" "is not a git URL" "$warnings"
printf 'NAME=crlf\r\nDESCRIPTION=Has = signs  and spaces \r\nREPO=r\r\nIMAGE=i\r\n' >"$TMP/crlf.conf"
printf 'CONTAINER=c\r\nUNIT=u.service\r\nUNIT_SCOPE=user\r\nENV_FILE=~/.config/crlf/env' \
  >>"$TMP/crlf.conf"
check "values are verbatim; CRLF, a missing final newline and .service are handled" \
  "[Has = signs  and spaces ] [u] [~/.config/crlf/env]" \
  "$(bash -c 'source "$1"; companion_parse "$2"
    echo "[${c_description[crlf]}] [${c_unit[crlf]}] [${c_env_file[crlf]}]"' _ "$INSTALL" \
    "$TMP/crlf.conf")"

# --- Detection -------------------------------------------------------------------------------
check "nothing found: not installed" "not-installed" "$(run 'companion_status alpha')"
check "active system unit: running" "running" "$(STUB_ACTIVE=alpha run 'companion_status alpha')"
check "active user unit: running" "running" "$(STUB_USER_ACTIVE=beta run 'companion_status beta')"
check "a user entry ignores system units" "not-installed" \
  "$(STUB_ACTIVE=beta STUB_LOADED=beta run 'companion_status beta')"
check "a system entry ignores user units" "not-installed" \
  "$(STUB_USER_ACTIVE=alpha run 'companion_status alpha')"
check "running container without an active unit: running" "running" \
  "$(STUB_RUNNING=alpha-ctr STUB_LOADED=alpha run 'companion_status alpha')"
check "loaded but inactive unit: installed" "installed" \
  "$(STUB_LOADED=alpha run 'companion_status alpha')"
check "inactive user unit: installed" "installed" \
  "$(STUB_USER_LOADED=beta run 'companion_status beta')"
check "detect fills every status" "running installed not-installed" \
  "$(STUB_ACTIVE=alpha STUB_USER_LOADED=beta run 'companions_detect
    echo "${c_status[alpha]} ${c_status[beta]} ${c_status[gamma]}"')"

# --- List and toggling -----------------------------------------------------------------------
list=$(STUB_ACTIVE=alpha run 'companions_detect; picked=(1 0 0); companions_print')
check_contains "list marks picked entries" "  1 [x] alpha  running        The alpha service." "$list"
check_contains "list shows status with a space" "  3 [ ] gamma  not installed  The gamma service." \
  "$list"
check "--no-marks list" "  1 alpha  not installed  The alpha service." \
  "$(run 'companions_detect; companions_print --no-marks | sed -n 1p')"
wrapped=$(COLUMNS=60 run 'c_description[alpha]="one two three four five six seven eight nine ten"
  companions_detect; companions_print | sed -n 1,2p')
check "a long description wraps under itself" \
  "$(printf '%s\n%s' '  1 [ ] alpha  not installed  one two three four five six' \
    '                              seven eight nine ten')" "$wrapped"
check "a narrow terminal puts the description below" \
  "$(printf '%s\n%s' '  1 [ ] alpha  not installed' '        The alpha service.')" \
  "$(COLUMNS=30 run 'companions_detect; companions_print | sed -n 1,2p')"

check "toggle '1 3'" "1 0 1" "$(run 'companions_toggle "1 3"; echo "${picked[*]}"')"
check "toggle '1,3'" "1 0 1" "$(run 'companions_toggle "1,3"; echo "${picked[*]}"')"
check "toggle twice clears" "0 0 1" \
  "$(run 'companions_toggle "1 3"; companions_toggle 1; echo "${picked[*]}"')"
check "toggle 'all'" "1 1 1" "$(run 'companions_toggle all; echo "${picked[*]}"')"
check "toggle 'ALL' then 'none'" "0 0 0" \
  "$(run 'companions_toggle ALL; companions_toggle none; echo "${picked[*]}"')"
check "toggle 'all 2' leaves 2 out" "1 0 1" "$(run 'companions_toggle "all 2"; echo "${picked[*]}"')"
check "toggle '03' is 3" "0 0 1" "$(run 'companions_toggle 03; echo "${picked[*]}"')"
for bad in 0 4 x "2 x" -1 "1.5"; do
  check "toggle rejects '$bad' and changes nothing" "1 rejected 1 0 0" \
    "$(run "companions_toggle 1; companions_toggle '$bad' 2>/dev/null || echo -n '1 rejected '
      echo \"\${picked[*]}\"")"
done
check_contains "toggle explains a bad word" "'9' is not a number from 1 to 3, all or none" \
  "$(run 'companions_toggle 9 2>&1 || true')"

prompt_out=$(printf '1 3\nbogus\n3\n\n' | run 'companions_prompt 2>&1; echo "picked=${picked[*]}"')
check_contains "prompt applies toggles until Enter" "picked=1 0 0" "$prompt_out"
check_contains "prompt re-prints the list with marks" "  1 [x] alpha" "$prompt_out"
check_contains "prompt reports bad input" "'bogus' is not a number" "$prompt_out"
check "prompt: end of input picks nothing" "picked=0 0 0" \
  "$(printf '1 2' | run 'companions_prompt >/dev/null 2>&1; echo "picked=${picked[*]}"')"

# --- --companions --------------------------------------------------------------------------
check "--companions names" "1 0 1" \
  "$(run 'companions_pick_list gamma,alpha; echo "${picked[*]}"')"
check "--companions all" "1 1 1" "$(run 'companions_pick_list all; echo "${picked[*]}"')"
check "--companions none" "0 0 0" "$(run 'companions_pick_list none; echo "${picked[*]}"')"
check "--companions rejects an unknown name" "rejected" \
  "$(run 'companions_pick_list alpha,nope 2>/dev/null || echo rejected')"
check "--companions rejects an entry that is not suggested" "rejected" \
  "$(run 'companions_pick_list hidden 2>/dev/null || echo rejected')"
# These fail in argument checking, before sudo or Docker are touched.
out=$("$INSTALL" --companions nope 2>&1 || true)
check_contains "install.sh --companions with an unknown name stops early" \
  "error: invalid --companions 'nope'" "$out"
out=$("$INSTALL" --companions 2>&1 || true)
check_contains "install.sh --companions without a value" "--companions needs a list" "$out"
out=$("$INSTALL" --companions= 2>&1 || true)
check_contains "install.sh --companions= without a value" "--companions needs a list" "$out"
help=$("$INSTALL" --help)
check_contains "--help shows --companions" "--companions LIST" "$help"
check_contains "--help shows --list-companions" "--list-companions" "$help"
check_contains "--help ends with the header" "scripts/uninstall.sh leaves them alone." \
  "$(tail -n 1 <<<"$help")"
check_contains "--list-companions against the real catalog (stubbed host)" \
  "  1 auto-git-commit-tool    running        Keeps your GitHub contribution graph" \
  "$(STUB_ACTIVE=auto-git-commit-tool "$INSTALL" --list-companions | sed -n 1p)"

# --- The step --------------------------------------------------------------------------------
# companion_install is replaced: beta "fails".
fake_install='companion_install() { echo "INSTALL $1"; [[ $1 != beta ]]; }'
out=$(run "$fake_install; companions_step '' </dev/null")
check "no terminal and no flag: one-line hint, nothing installed" \
  "$(printf '\033[1m==>\033[0m %s' \
    'Companion services skipped (no terminal); pick them with --companions LIST (see --help)')" "$out"
check "--companions none: silent" "" "$(run "$fake_install; companions_pick_list none
  companions_step none </dev/null")"
out=$(run "$fake_install; is_root() { true; }; companions_pick_list all; companions_step all")
check_contains "as root: skipped with a hint" "run scripts/install.sh as your normal user" "$out"
out=$(run "$fake_install; companions_pick_list all
  companions_step all 2>&1 </dev/null && echo rc=0 || echo rc=\$?")
check_contains "every picked one is tried after a failure" "INSTALL alpha
INSTALL beta
INSTALL gamma" "$out"
check_contains "summary shows the failure" "beta                           FAILED (exit 1)" "$out"
check_contains "summary shows successes" "gamma                          installed" "$out"
check_contains "a failure makes the step fail" "rc=1" "$out"
out=$(run "$fake_install; companions_pick_list alpha; companions_step alpha && echo rc=0")
check_contains "only the listed one is installed" "INSTALL alpha" "$out"
check_contains "all succeeded: rc 0" "rc=0" "$out"

# --- --companions-only -----------------------------------------------------------------------
# main against the real catalog, with Cuthulu's install replaced by a tripwire and
# companion_install faked. Prints the output and the exit code.
run_main() {
  bash -c 'source "$1"; shift
    install_cuthulu() { echo "CUTHULU INSTALLED"; exit 99; }
    companion_install() { echo "INSTALL $1"; }
    eval "${PRE:-}"
    main "$@"' _ "$INSTALL" "$@" 2>&1 </dev/null && echo rc=0 || echo "rc=$?"
}
out=$(run_main --companions-only --companions producer-tag-on-merge)
check "--companions-only installs the listed companion only, not Cuthulu" \
  "INSTALL producer-tag-on-merge" "$(grep -E 'INSTALL|CUTHULU' <<<"$out")"
check_contains "--companions-only succeeds" "rc=0" "$out"
out=$(run_main --companions=all --companions-only)
check "--companions-only --companions=all installs every companion" \
  "INSTALL auto-git-commit-tool
INSTALL claude-session-starter
INSTALL producer-tag-on-merge" "$(grep -E 'INSTALL|CUTHULU' <<<"$out")"
out=$(run_main --companions-only)
check_contains "--companions-only without a terminal: hint" \
  "Companion services skipped (no terminal)" "$out"
check_contains "--companions-only without a terminal: Cuthulu untouched" "rc=0" "$out"
out=$(PRE='is_root() { true; }' run_main --companions-only --companions all)
check_contains "--companions-only as root: hint" "run scripts/install.sh as your normal user" "$out"
check "--companions-only as root: nothing installed" "" "$(grep -E 'INSTALL|CUTHULU' <<<"$out")"
out=$(PRE='companion_install() { echo "INSTALL $1"; [[ $1 != claude-session-starter ]]; }' \
  run_main --companions-only --companions all)
check_contains "--companions-only: a failing companion fails the run" "rc=1" "$out"
check_contains "without --companions-only, Cuthulu is installed (tripwire works)" \
  "CUTHULU INSTALLED" "$(run_main --companions none)"
for args in "--build" "--reconfigure" "--build --reconfigure"; do
  # shellcheck disable=SC2086 # split the flags on purpose
  out=$(run_main --companions-only $args --companions all)
  check_contains "--companions-only rejects $args" \
    "error: --companions-only does not install Cuthulu; drop --build and --reconfigure" "$out"
  check "--companions-only $args changes nothing" "" "$(grep -E 'INSTALL|CUTHULU' <<<"$out")"
done
out=$(run_main --companions none --companions-only)
check_contains "--companions-only rejects --companions none" \
  "error: --companions-only with --companions none does nothing" "$out"
check_contains "--companions-only --companions none exits 1" "rc=1" "$out"
out=$("$INSTALL" --companions-only --build 2>&1 || true)
check_contains "install.sh --companions-only --build stops before anything" \
  "--companions-only does not install Cuthulu" "$out"
check_contains "--help shows --companions-only" "--companions-only [--companions LIST]" "$help"

# companion_install for real, against a local "remote": clone, then pull on the second run.
mkdir -p "$TMP/remote/alpha/scripts"
cat >"$TMP/remote/alpha/scripts/install.sh" <<'EOF'
#!/usr/bin/env bash
echo "installer ran with IMAGE=$IMAGE from $(cd "$(dirname "$0")/.." && pwd)"
EOF
chmod +x "$TMP/remote/alpha/scripts/install.sh"
git_q() { git -C "$TMP/remote/alpha" -c user.name=t -c user.email=t@t "$@" >/dev/null; }
git_q init --quiet
git_q add -A
git_q commit --quiet -m one
out=$(run "COMPANIONS_DIR=$TMP/share; companion_install alpha" 2>&1)
check_contains "clones and runs the installer with the published image" \
  "installer ran with IMAGE=example/alpha:latest from $TMP/share/alpha" "$out"
echo changed >"$TMP/remote/alpha/NEW"
git_q add -A
git_q commit --quiet -m two
out=$(run "COMPANIONS_DIR=$TMP/share; c_image[alpha]=example/alpha:1.2; companion_install alpha" 2>&1)
check_contains "a second run updates the clone" "Updating alpha" "$out"
check "the update fast-forwarded" "changed" "$(cat "$TMP/share/alpha/NEW")"
check_contains "an explicit tag in the catalog is kept" "IMAGE=example/alpha:1.2 " "$out"
out=$(run "COMPANIONS_DIR=$TMP/share; companion_install gamma >/dev/null 2>&1 && echo ok || echo failed")
check "a repository that cannot be cloned fails" "failed" "$out"

echo
if ((failures)); then
  echo "$failures of $tests tests failed"
  exit 1
fi
echo "all $tests tests passed"
