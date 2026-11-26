#!/usr/bin/env bash
# E2E: `pelagos compose up` reports whether the project really started (#653).
#
# Background-mode `compose up` used to fork the supervisor, sleep 200 ms and print
# "Project started" whatever happened next. A service that could not spawn, a missing
# runtime or a dependency that never became ready was then invisible: exit 0, "started",
# and later `no project found`. Now `up` waits for the supervisor to report.
#
# Requires root and alpine:3.20 (pulled on demand). Usage:
#   sudo BINARY=target/debug/pelagos scripts/test-compose-startup.sh
set -uo pipefail
BINARY="${BINARY:-./target/debug/pelagos}"
CTX="$(cd "$(dirname "$0")" && pwd)/compose-startup-context"
PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL: $1"; }
[ "$(id -u)" -eq 0 ] || { echo "FATAL: must run as root"; exit 1; }
[ -x "$BINARY" ] || { echo "FATAL: $BINARY not found"; exit 1; }
W="$(mktemp -d)"
projects="csok csfail csnowait csslow"
down_all() { for p in $projects; do
  for f in ok fail slow; do "$BINARY" compose down -f "$CTX/$f.reml" -p "$p" -v >/dev/null 2>&1 || true; done; done; }
trap 'down_all; rm -rf "$W"' EXIT
secs() { echo $(( $(date +%s) - $1 )); }
leftover() { # project -> number of container state dirs still present
  ls /run/pelagos/containers 2>/dev/null | grep -c "^$1-" || true; }

"$BINARY" image pull alpine:3.20 >/dev/null 2>&1 || { fail "cannot pull alpine:3.20"; exit 1; }
down_all

echo "=== compose up startup reporting"

echo "--- 1. a healthy project: up returns success only once the service is running"
t0=$(date +%s)
"$BINARY" compose up -f "$CTX/ok.reml" -p csok >"$W/ok.out" 2>&1; rc=$?
[ $rc -eq 0 ] && grep -q "Project 'csok' started" "$W/ok.out" && pass "up exits 0 and says started" \
  || fail "healthy up failed (rc=$rc): $(tail -2 "$W/ok.out" | tr '\n' ' ')"
# No sleep between: with the handshake the service must already be listed.
"$BINARY" compose ps -f "$CTX/ok.reml" -p csok >"$W/ps.out" 2>&1
grep -q "web" "$W/ps.out" && pass "the service is already listed by compose ps the moment up returns" \
  || fail "compose ps right after up: $(cat "$W/ps.out" | tr '\n' ' ')"

echo "--- 2. a service that cannot start: up fails, says why, leaves nothing"
t0=$(date +%s)
"$BINARY" compose up -f "$CTX/fail.reml" -p csfail >"$W/fail.out" 2>&1; rc=$?
[ $rc -ne 0 ] && pass "up exits non-zero (rc=$rc, after $(secs $t0) s)" || fail "up exited 0 for a project that cannot start"
grep -q "failed to start" "$W/fail.out" && grep -q "broken" "$W/fail.out" \
  && pass "the error names the project failure and the service ('broken')" \
  || fail "error not clear: $(tail -3 "$W/fail.out" | tr '\n' ' ')"
"$BINARY" compose ps -f "$CTX/fail.reml" -p csfail >"$W/psf.out" 2>&1
grep -q "no project" "$W/psf.out" && pass "no project is left registered" || fail "project still registered: $(cat "$W/psf.out" | tr '\n' ' ')"
[ "$(leftover csfail)" = 0 ] && pass "the service that did start ('first') was rolled back" || fail "container state left: $(ls /run/pelagos/containers | grep '^csfail-' | tr '\n' ' ')"
[ "$(secs $t0)" -lt 60 ] && pass "it failed promptly, not after a timeout" || fail "took $(secs $t0) s"

echo "--- 3. --no-wait keeps the old behaviour (returns at once, says started)"
t0=$(date +%s)
"$BINARY" compose up -f "$CTX/fail.reml" -p csnowait --no-wait >"$W/nw.out" 2>&1; rc=$?
[ $rc -eq 0 ] && grep -q "started" "$W/nw.out" && [ "$(secs $t0)" -lt 10 ] \
  && pass "--no-wait returned 0 straight away even though startup will fail" \
  || fail "--no-wait (rc=$rc, $(secs $t0) s): $(tail -2 "$W/nw.out" | tr '\n' ' ')"
for _ in $(seq 1 30); do "$BINARY" compose ps -f "$CTX/fail.reml" -p csnowait 2>&1 | grep -q "no project" && break; sleep 1; done
"$BINARY" compose ps -f "$CTX/fail.reml" -p csnowait 2>&1 | grep -q "no project" \
  && pass "and the supervisor still rolled the project back in the background" || fail "csnowait project left behind"

echo "--- 4. a start that takes too long: --startup-timeout gives up waiting, says so"
t0=$(date +%s)
"$BINARY" compose up -f "$CTX/slow.reml" -p csslow --startup-timeout 5 >"$W/slow.out" 2>&1; rc=$?; took=$(secs $t0)
[ $rc -ne 0 ] && grep -q "still starting after 5 s" "$W/slow.out" && pass "up exits non-zero with 'still starting after 5 s' (after $took s)" \
  || fail "timeout not reported (rc=$rc, ${took}s): $(tail -2 "$W/slow.out" | tr '\n' ' ')"
[ "$took" -ge 4 ] && [ "$took" -lt 30 ] && pass "and it stopped waiting at about the timeout, not the dependency's 60 s" || fail "took $took s"
"$BINARY" compose down -f "$CTX/slow.reml" -p csslow -v >/dev/null 2>&1 && pass "compose down abandons the still-starting project cleanly" || fail "down failed"

echo "=== $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
