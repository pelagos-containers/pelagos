#!/usr/bin/env bash
# Reproduces the StopPodSandbox teardown hang for a shared-PID pod whose
# container ALSO joined a non-host (CNI-style) network namespace.
#
# The host-net PodPID repro (podpid-local-repro.sh) skipped the NET-ns join; the
# passing CNI critest specs skipped the PID-ns join. This exercises BOTH at once —
# the only combination critest's PodPID spec hits that nothing else does:
#   container joins  /run/netns/<name> (NET) + pod PID ns (PID) + pause IPC,
# then `pelagos stop` (what StopPodSandbox runs first).
#
# The netns is a throwaway `ip netns` with only `lo` — NO flannel/bridge/veth/
# Tailscale churn, so it cannot disturb host networking (safe on omen). Still run
# caged:
#   sudo systemd-run --scope -p MemoryMax=1500M -p TasksMax=400 -p CPUQuota=60% \
#        --quiet timeout 120 bash scripts/podpid-cni-teardown-repro.sh
set -u
BIN="${BIN:-./target/debug/pelagos}"
RT=/run/pelagos
ID="podpidcni$$"
SBDIR="$RT/sandboxes/$ID"
CNAME="podpidcnic$$"
NETNS="pcnirepro$$"
IMG="${IMG:-alpine}"
# CMD: the container workload. Default a do-nothing sleep; override with a real
# listening/forking daemon (e.g. CMD='redis-server --port 6390') to mimic nginx.
CMD="${CMD:-sleep 600}"
PAUSE_PARENT=""; PAUSE_CHILD=""

cleanup(){
  echo "--- cleanup ---"
  timeout 10 "$BIN" rm -f "$CNAME" >/dev/null 2>&1
  [ -n "$PAUSE_CHILD"  ] && kill -KILL "$PAUSE_CHILD"  2>/dev/null
  [ -n "$PAUSE_PARENT" ] && kill -KILL "$PAUSE_PARENT" 2>/dev/null
  ip netns del "$NETNS" 2>/dev/null
  rm -rf "$SBDIR"
  echo "cleanup done"
}
trap cleanup EXIT

echo "=== 0. create throwaway named netns (lo only) ==="
ip netns add "$NETNS"
ip netns exec "$NETNS" ip link set lo up
echo "created /run/netns/$NETNS"

echo "=== 1. spawn pod-pid pause INTO the netns (sandbox __pause__ <ns> --pod-pid) ==="
# Exactly like the real CNI path: the pause joins /run/netns/<name>, unshares
# IPC+UTS, then (--pod-pid) unshares PID and forks a PID-1 init — all INSIDE the
# CNI netns. This is the fidelity gap the host-net repro missed.
setsid "$BIN" sandbox __pause__ "$NETNS" --pod-pid >/dev/null 2>&1 < /dev/null &
PAUSE_PARENT=$!
sleep 1
PAUSE_CHILD=$(awk '{print $1}' "/proc/$PAUSE_PARENT/task/$PAUSE_PARENT/children" 2>/dev/null)
echo "pause parent=$PAUSE_PARENT  child(PID-1)=$PAUSE_CHILD"
[ -z "$PAUSE_CHILD" ] && { echo "FAIL: pause did not fork a PID-1 child"; exit 1; }

echo "=== 2. write sandbox state: network=Pod (join named netns) + pid=Pod (shared) ==="
mkdir -p "$SBDIR"
cat > "$SBDIR/state.json" <<JSON
{"id":"$ID","name":"$ID","pause_pid":$PAUSE_CHILD,"ns_name":"$NETNS","veth_host":"","container_ip":"","namespaces":{"network":"Pod","pid":"Pod","ipc":"Pod"}}
JSON
echo "wrote $SBDIR/state.json (ns_name=$NETNS)"

echo "=== 3. run --detach joining named-netns + pod-pid (timeout 25) ==="
timeout 25 "$BIN" run --detach --name "$CNAME" --sandbox "$ID" --no-pid-ns "$IMG" $CMD
RC=$?
echo "run --detach exit=$RC"
if [ $RC -eq 124 ]; then echo ">>> HANG AT START (netns-join + pod-pid)"; exit 0; fi
if [ $RC -ne 0 ]; then echo ">>> run --detach FAILED (not a hang)"; exit 0; fi
sleep 1
echo "container netns: $(timeout 5 "$BIN" exec "$CNAME" sh -c 'readlink /proc/self/ns/net' 2>&1 | tr -d '\n')"

echo "=== 4. graceful 'pelagos stop' — what StopPodSandbox runs FIRST (timeout 30) ==="
t0=$(date +%s)
timeout 30 "$BIN" stop "$CNAME"
RC=$?
t1=$(date +%s)
echo "stop exit=$RC  elapsed=$((t1-t0))s"
if [ $RC -eq 124 ]; then echo ">>> CONFIRMED: stop of a CNI+pod-pid container HANGS"; fi

echo "=== DONE ==="
