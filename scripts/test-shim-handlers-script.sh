#!/usr/bin/env bash
# Offline test of scripts/pelagos-cri-shim-handlers.sh (no root, no systemd,
# no cluster): add/replace/remove/list, preserved extra flags, validation,
# and that the unit templates actually consume PELAGOS_CRI_ARGS.
set -euo pipefail
cd "$(dirname "$0")/.."
S=scripts/pelagos-cri-shim-handlers.sh
D=$(mktemp -d); trap 'rm -rf "$D"' EXIT
export ENV_FILE="$D/cri.env"
B1=$D/shim-a; B2=$D/shim-b; touch "$B1" "$B2"; chmod +x "$B1" "$B2"
fail() { echo "FAIL: $*" >&2; exit 1; }
eq() { [ "$1" = "$2" ] || fail "$3: got '$1' want '$2'"; }

$S add kata="$B1" --no-restart >/dev/null
eq "$($S list)" "kata=$B1" "add one"
$S add gvisor="$B2" --no-restart >/dev/null
eq "$($S list | sort | tr '\n' ' ')" "gvisor=$B2 kata=$B1 " "add two"
$S add kata="$B2" --no-restart >/dev/null
eq "$($S list | grep -c '^kata=')" 1 "replace keeps one entry"
eq "$($S list | grep '^kata=')" "kata=$B2" "replace updates binary"

# Unrelated flags and other variables survive.
echo 'RUST_LOG=debug' >>"$ENV_FILE"
sed -i 's|^PELAGOS_CRI_ARGS="|PELAGOS_CRI_ARGS="--metrics-addr 0.0.0.0:9091 |' "$ENV_FILE"
$S add extra="$B1" --no-restart >/dev/null
grep -q -- '--metrics-addr 0.0.0.0:9091' "$ENV_FILE" || fail "extra flag lost"
grep -q '^RUST_LOG=debug' "$ENV_FILE" || fail "other variable lost"

$S remove kata gvisor extra --no-restart >/dev/null
eq "$($S list)" "" "remove all"
grep -q -- '--metrics-addr' "$ENV_FILE" && true  # flag may remain; handlers must not
grep -q -- '--shim-handler' "$ENV_FILE" && fail "handler left behind"

# Validation.
$S add 'bad name'="$B1" --no-restart 2>/dev/null && fail "accepted bad name"
$S add pelagos="$B1" --no-restart 2>/dev/null && fail "accepted reserved name"
$S add kata="$D/missing" --no-restart 2>/dev/null && fail "accepted missing binary"
$S add kata --no-restart 2>/dev/null && fail "accepted spec without ="

# Units consume the variable (systemd expands $VAR in ExecStart).
for u in pelagos-cri/pelagos-cri-server.service pelagos-cri/pelagos-cri-agent.service; do
  grep -q '^EnvironmentFile=-/etc/pelagos/cri.env' "$u" || fail "$u: no EnvironmentFile"
  grep -q '^ExecStart=.*\$PELAGOS_CRI_ARGS' "$u" || fail "$u: ExecStart ignores PELAGOS_CRI_ARGS"
done
echo "ok: shim-handler script and unit wiring"
