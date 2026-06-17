#!/usr/bin/env bash
# SAFE local repro of the PodPID hang (#398 / #399).
#
# Sets up a shared-PID pod sandbox (pause `--pod-pid`), runs a detached container
# that joins it, then execs into it (`cat /proc/1/cmdline`) — mirroring what the
# critest "PodPID" spec does — to find which step hangs.
#
# SAFETY: every pelagos op is wrapped in `timeout`, and this is meant to be run
# INSIDE a resource cage so a hang/leak/fork-bomb can't harm the host:
#   sudo systemd-run --scope -p MemoryMax=1500M -p TasksMax=400 -p CPUQuota=60% \
#        --quiet timeout 120 bash scripts/podpid-local-repro.sh
set -u
BIN="${BIN:-./target/debug/pelagos}"
RT=/run/pelagos
ID="podpidrepro$$"
SBDIR="$RT/sandboxes/$ID"
CNAME="podpidc$$"
IMG="${IMG:-alpine}"
PAUSE_PARENT=""; PAUSE_CHILD=""

cleanup(){
  echo "--- cleanup ---"
  timeout 10 "$BIN" rm -f "$CNAME" >/dev/null 2>&1
  [ -n "$PAUSE_CHILD"  ] && kill -KILL "$PAUSE_CHILD"  2>/dev/null
  [ -n "$PAUSE_PARENT" ] && kill -KILL "$PAUSE_PARENT" 2>/dev/null
  pkill -KILL -f "exec $CNAME" 2>/dev/null
  rm -rf "$SBDIR"
  echo "cleanup done"
}
trap cleanup EXIT

echo "host pid-ns: $(readlink /proc/self/ns/pid)"

echo "=== 1. spawn pod-pid pause (--host-net --pod-pid) ==="
setsid "$BIN" sandbox __pause__ "" --host-net --pod-pid >/dev/null 2>&1 < /dev/null &
PAUSE_PARENT=$!
sleep 1
PAUSE_CHILD=$(awk '{print $1}' "/proc/$PAUSE_PARENT/task/$PAUSE_PARENT/children" 2>/dev/null)
echo "pause parent=$PAUSE_PARENT  child(PID-1)=$PAUSE_CHILD"
[ -z "$PAUSE_CHILD" ] && { echo "FAIL: pause did not fork a PID-1 child"; exit 1; }
echo "pause-child pid-ns: $(readlink /proc/$PAUSE_CHILD/ns/pid)  (differs from host = good)"

echo "=== 2. write sandbox state (pid=Pod -> shared pod PID ns) ==="
mkdir -p "$SBDIR"
cat > "$SBDIR/state.json" <<JSON
{"id":"$ID","name":"$ID","pause_pid":$PAUSE_CHILD,"ns_name":"","veth_host":"","container_ip":"","namespaces":{"network":"Node","pid":"Pod","ipc":"Pod"}}
JSON
echo "wrote $SBDIR/state.json"

echo "=== 3. run --detach container joining the pod-pid sandbox (timeout 25) ==="
# --no-pid-ns matches the CRI flow for shared-PID pods: the container must NOT
# unshare its own PID ns; with_sandbox joins the pod PID ns instead (#398).
timeout 25 "$BIN" run --detach --name "$CNAME" --sandbox "$ID" --no-pid-ns "$IMG" sleep 600
RC=$?
echo "run --detach exit=$RC  (124 = HUNG at START)"
if [ $RC -eq 124 ]; then echo ">>> HANG IS AT CONTAINER START"; exit 0; fi
if [ $RC -ne 0 ]; then echo ">>> run --detach FAILED (not a hang) — stopping"; exit 0; fi

sleep 1
echo "container row: $(timeout 8 "$BIN" ps -a 2>/dev/null | grep "$CNAME" || echo '(not listed)')"

echo "=== 4. exec into the pod-pid container: cat /proc/1/cmdline (timeout 25) ==="
OUT=$(timeout 25 "$BIN" exec "$CNAME" cat /proc/1/cmdline 2>&1)
RC=$?
echo "exec exit=$RC  (124 = HUNG at EXEC)"
echo "exec output (NUL->space): $(printf '%s' "$OUT" | tr '\0' ' ')"
if [ $RC -eq 124 ]; then echo ">>> HANG IS AT EXEC (cat /proc/1/cmdline)"; fi

echo "=== DONE (no hang at start or exec if we got here) ==="
