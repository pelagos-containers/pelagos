#!/usr/bin/env bash
# Mimics nginx's process model in a shared pod PID ns: a master that forks worker
# children. In a SHARED pid ns the workers are reparented to the pause (PID-1)
# when the master dies — they are NOT killed by the kernel (unlike a normal pod
# where the container's main IS PID-1). This tests whether `pelagos stop` leaves
# lingering workers in the sandbox netns and whether teardown then blocks.
#
# Run caged:
#   sudo systemd-run --scope -p MemoryMax=1500M -p TasksMax=400 -p CPUQuota=60% \
#        --quiet timeout 120 bash scripts/podpid-forking-teardown-repro.sh
set -u
BIN="${BIN:-./target/debug/pelagos}"
RT=/run/pelagos
ID="podpidfork$$"
SBDIR="$RT/sandboxes/$ID"
CNAME="podpidforkc$$"
NETNS="pforkrepro$$"
IMG="${IMG:-alpine}"
PAUSE_PARENT=""; PAUSE_CHILD=""

cleanup(){
  echo "--- cleanup ---"
  timeout 10 "$BIN" rm -f "$CNAME" >/dev/null 2>&1
  for p in $(ip netns pids "$NETNS" 2>/dev/null); do kill -KILL "$p" 2>/dev/null; done
  [ -n "$PAUSE_CHILD"  ] && kill -KILL "$PAUSE_CHILD"  2>/dev/null
  [ -n "$PAUSE_PARENT" ] && kill -KILL "$PAUSE_PARENT" 2>/dev/null
  ip netns del "$NETNS" 2>/dev/null
  rm -rf "$SBDIR"
  echo "cleanup done"
}
trap cleanup EXIT

ip netns add "$NETNS"; ip netns exec "$NETNS" ip link set lo up
echo "netns /run/netns/$NETNS created"

setsid "$BIN" sandbox __pause__ "$NETNS" --pod-pid >/dev/null 2>&1 < /dev/null &
PAUSE_PARENT=$!; sleep 1
PAUSE_CHILD=$(awk '{print $1}' "/proc/$PAUSE_PARENT/task/$PAUSE_PARENT/children" 2>/dev/null)
echo "pause parent=$PAUSE_PARENT child(PID-1)=$PAUSE_CHILD"
[ -z "$PAUSE_CHILD" ] && { echo "FAIL no pause child"; exit 1; }

mkdir -p "$SBDIR"
cat > "$SBDIR/state.json" <<JSON
{"id":"$ID","name":"$ID","pause_pid":$PAUSE_CHILD,"ns_name":"$NETNS","veth_host":"","container_ip":"","namespaces":{"network":"Pod","pid":"Pod","ipc":"Pod"}}
JSON

# Master forks two workers that IGNORE SIGTERM (worst case: like a worker mid-request),
# then the master execs a foreground process. `pelagos stop` SIGTERMs the master.
WORKLOAD='trap "" TERM; (trap "" TERM; while :; do sleep 1; done) & (trap "" TERM; while :; do sleep 1; done) & exec sleep 600'
echo "=== run --detach: master + 2 SIGTERM-ignoring workers (timeout 25) ==="
timeout 25 "$BIN" run --detach --name "$CNAME" --sandbox "$ID" --no-pid-ns "$IMG" sh -c "$WORKLOAD"
RC=$?
echo "run exit=$RC"; [ $RC -ne 0 ] && { echo "run FAILED/HUNG"; exit 0; }
sleep 1
echo "procs in netns BEFORE stop: $(ip netns pids "$NETNS" 2>/dev/null | tr '\n' ' ')"

echo "=== pelagos stop (timeout 30) ==="
t0=$(date +%s); timeout 30 "$BIN" stop "$CNAME"; RC=$?; t1=$(date +%s)
echo "stop exit=$RC elapsed=$((t1-t0))s  $([ $RC -eq 124 ] && echo '>>> STOP HUNG')"
echo "procs in netns AFTER stop: $(ip netns pids "$NETNS" 2>/dev/null | tr '\n' ' ')"

echo "=== probe production teardown steps with lingering workers ==="
echo "kill -TERM pause child (step 3)..."
kill -TERM "$PAUSE_CHILD" 2>/dev/null; sleep 1
echo "procs in netns after kill-pause: $(ip netns pids "$NETNS" 2>/dev/null | tr '\n' ' ')"
echo "ip netns del (delete_netns step) — TIMED:"
t0=$(date +%s); timeout 20 ip netns del "$NETNS"; RC=$?; t1=$(date +%s)
echo "ip netns del exit=$RC elapsed=$((t1-t0))s  $([ $RC -eq 124 ] && echo '>>> delete_netns HUNG')"
NETNS=""  # already deleted
echo "=== DONE ==="
