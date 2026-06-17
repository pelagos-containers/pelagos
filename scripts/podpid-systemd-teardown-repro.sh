#!/usr/bin/env bash
# Isolates the StopPodSandbox teardown hang for a shared-PID (pod-pid) sandbox.
#
# Mirrors pelagos-cri EXACTLY: the pause runs as a transient systemd *service*
# with `--property=KillMode=mixed` (build_service_argv), then StopPodSandbox does
#   1. kill -TERM <pause_pid==PID-1 child>     (runtime.rs line ~41-43)
#   2. systemctl stop <unit>                   (runtime.rs line ~49)
# We TIME each step. The host-net repro used a plain setsid pause (no unit), so it
# never exercised KillMode=mixed — this does.
#
# Run caged:
#   sudo systemd-run --scope -p MemoryMax=500M -p TasksMax=200 -p CPUQuota=50% \
#        --quiet timeout 150 bash scripts/podpid-systemd-teardown-repro.sh
set -u
BIN="${BIN:-$PWD/target/debug/pelagos}"
UNIT="pelagos-sbx-podpidteardown$$.service"

cleanup(){ systemctl stop "$UNIT" >/dev/null 2>&1; systemctl reset-failed "$UNIT" >/dev/null 2>&1; }
trap cleanup EXIT

echo "=== 1. start pod-pid pause as a systemd service (KillMode=mixed, exactly like pelagos-cri) ==="
systemd-run --collect --slice=pelagos.slice --unit="$UNIT" \
  --property=KillMode=mixed --quiet -- \
  "$BIN" sandbox __pause__ "" --host-net --pod-pid
sleep 0.5
MAIN=$(systemctl show -p MainPID --value "$UNIT")
echo "unit=$UNIT  MainPID(parent)=$MAIN"
[ -z "$MAIN" ] || [ "$MAIN" = 0 ] && { echo "FAIL: no MainPID"; exit 1; }
CHILD=""
for _ in $(seq 1 40); do
  CHILD=$(awk '{print $1}' "/proc/$MAIN/task/$MAIN/children" 2>/dev/null)
  [ -n "$CHILD" ] && break; sleep 0.025
done
echo "PID-1 child(pause_pid in state)=$CHILD"
[ -z "$CHILD" ] && { echo "FAIL: no PID-1 child"; exit 1; }

echo "=== 2. kill -TERM pause_pid (the child)  [runtime.rs:41-43] ==="
t0=$(date +%s.%N)
kill -TERM "$CHILD" 2>/dev/null
# give it a moment, then observe whether parent+child actually died
for _ in $(seq 1 40); do
  kill -0 "$CHILD" 2>/dev/null || break; sleep 0.025
done
t1=$(date +%s.%N)
CH_ALIVE=$(kill -0 "$CHILD" 2>/dev/null && echo YES || echo no)
PA_ALIVE=$(kill -0 "$MAIN"  2>/dev/null && echo YES || echo no)
printf "after SIGTERM->child (%.2fs): child_alive=%s parent_alive=%s\n" "$(echo "$t1-$t0"|bc)" "$CH_ALIVE" "$PA_ALIVE"
[ "$CH_ALIVE" = YES ] && echo ">>> child IGNORED SIGTERM (PID-1 handler not firing from host?)"
[ "$PA_ALIVE" = YES ] && [ "$CH_ALIVE" = no ] && echo ">>> child died but PARENT still alive (parent waitpid loop stuck?)"

echo "=== 3. systemctl stop \$UNIT  [runtime.rs:49] — TIMED ==="
t0=$(date +%s.%N)
timeout 120 systemctl stop "$UNIT"
RC=$?
t1=$(date +%s.%N)
printf "systemctl stop exit=%s  elapsed=%.2fs\n" "$RC" "$(echo "$t1-$t0"|bc)"
if [ "$RC" = 124 ]; then echo ">>> stop_unit HUNG (>120s)"; fi
# A ~90s elapsed == hit TimeoutStopSec then SIGKILL == THE BUG
echo "=== DONE ==="
