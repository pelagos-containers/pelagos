#!/usr/bin/env bash
# Definitive confirmation of pod-namespace sharing for a CRI container.
# Runs a long-lived container with a UNIQUE marker command so the real container
# process can be found unambiguously (not the host-side watcher), then compares
# its net/uts/ipc namespaces + IP against the sandbox pause AND a real cluster pod.
#
# Usage (IPC node, never omen): sudo bash scripts/cri-confirm-ns.sh /tmp/ns.result
set -u
OUT="${1:-/tmp/cri-confirm-ns.result}"
SOCK="unix:///run/pelagos/cri.sock"
CR="crictl --runtime-endpoint $SOCK --image-endpoint $SOCK"
IMG="registry.k8s.io/e2e-test-images/busybox:1.29-2"
W=$(mktemp -d)

cat > "$W/pod.json" <<JSON
{ "metadata": {"name":"nsdiag-pod","namespace":"default","uid":"nsdiag-1","attempt":1},
  "hostname": "nsdiag-host", "log_directory": "$W",
  "linux": { "sysctls": {"kernel.shm_rmid_forced":"1"} } }
JSON
cat > "$W/ctr.json" <<JSON
{ "metadata": {"name":"nsdiag-ctr","attempt":1},
  "image": {"image":"$IMG"}, "command": ["sleep","31415926"],
  "log_path": "c.log", "linux": {} }
JSON

exec > "$OUT" 2>&1
echo "# cri-confirm-ns @ $(date -u +%FT%TZ)"
echo "host init: net=$(sudo readlink /proc/1/ns/net) uts=$(sudo readlink /proc/1/ns/uts) ipc=$(sudo readlink /proc/1/ns/ipc)"
$CR pull "$IMG" >/dev/null 2>&1
POD=$($CR runp "$W/pod.json"); echo "POD=$POD"
CID=$($CR create "$POD" "$W/ctr.json" "$W/pod.json"); echo "CID=$CID"
$CR start "$CID"; sleep 2
PN="pcri-${CID:0:12}"

echo "=== state.json (pids) ==="
sudo cat "/run/pelagos/containers/$PN/state.json" 2>/dev/null | tr ',{}' '\n' | grep -iE 'pid' || echo "no state.json"

SLEEP_PID=$(pgrep -f "sleep 31415926" | head -1)
STATE_PID=$(sudo grep -oE '"pid":[ ]*[0-9]+' "/run/pelagos/containers/$PN/state.json" 2>/dev/null | head -1 | grep -oE '[0-9]+')
PAUSE=$(sudo grep -oE '"pause_pid":[ ]*[0-9]+' "/run/pelagos-cri/sandboxes/$POD.json" 2>/dev/null | head -1 | grep -oE '[0-9]+')
echo "REAL container process (sleep) pid=$SLEEP_PID | state.json pid=$STATE_PID | pause pid=$PAUSE"

echo "=== host-side netns/uts/ipc by pid ==="
for lp in "sleep:$SLEEP_PID" "state:$STATE_PID" "pause:$PAUSE"; do
  l=${lp%%:*}; p=${lp##*:}
  echo "  $l(pid=$p): net=$(sudo readlink /proc/$p/ns/net 2>/dev/null) uts=$(sudo readlink /proc/$p/ns/uts 2>/dev/null) ipc=$(sudo readlink /proc/$p/ns/ipc 2>/dev/null)"
done

echo "=== SHARING verdict: in-container inode (crictl exec) vs pause inode ==="
for ns in net uts ipc; do
  cin=$($CR exec "$CID" readlink "/proc/1/ns/$ns" 2>/dev/null)
  pin=$(sudo readlink "/proc/$PAUSE/ns/$ns" 2>/dev/null)
  echo "  $ns: container=$cin pause=$pin  $([ -n "$cin" ] && [ "$cin" = "$pin" ] && echo SHARED || echo SEPARATE)"
done
echo "  hostname inside: $($CR exec "$CID" hostname 2>&1)  (want nsdiag-host)"
echo "  ip addr:  $($CR exec "$CID" ip -o addr show 2>&1 | tr '\n' ' ')"
echo "  shm_rmid inside: $($CR exec "$CID" cat /proc/sys/kernel/shm_rmid_forced 2>&1)  (want 1)"
echo "  shm_rmid in pause ipc-ns: $(sudo nsenter --ipc=/proc/$PAUSE/ns/ipc -- cat /proc/sys/kernel/shm_rmid_forced 2>&1)  (want 1)"

echo "=== REAL cluster pod (container process vs its pause) ==="
RC=$($CR ps -a --state Running -q 2>/dev/null | grep -v "$CID" | head -1)
echo "real container id: ${RC:-<none running>}"
if [ -n "${RC:-}" ]; then
  RPN="pcri-${RC:0:12}"
  RCPID=$(sudo grep -oE '"pid":[ ]*[0-9]+' "/run/pelagos/containers/$RPN/state.json" 2>/dev/null | head -1 | grep -oE '[0-9]+')
  RPOD=$($CR inspect "$RC" 2>/dev/null | grep -oE '"sandboxID": *"[a-f0-9]+"' | grep -oE '[a-f0-9]{64}' | head -1)
  RPAUSE=$(sudo grep -oE '"pause_pid":[ ]*[0-9]+' "/run/pelagos-cri/sandboxes/$RPOD.json" 2>/dev/null | head -1 | grep -oE '[0-9]+')
  echo "  real container(pid=$RCPID): net=$(sudo readlink /proc/$RCPID/ns/net 2>/dev/null) uts=$(sudo readlink /proc/$RCPID/ns/uts 2>/dev/null)"
  echo "  real pause(pid=$RPAUSE):     net=$(sudo readlink /proc/$RPAUSE/ns/net 2>/dev/null) uts=$(sudo readlink /proc/$RPAUSE/ns/uts 2>/dev/null)"
fi

echo "=== cleanup ==="
$CR stop "$CID" >/dev/null 2>&1; $CR rm "$CID" >/dev/null 2>&1
$CR stopp "$POD" >/dev/null 2>&1; $CR rmp "$POD" >/dev/null 2>&1
rm -rf "$W"
echo "# done @ $(date -u +%FT%TZ)"
