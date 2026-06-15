#!/usr/bin/env bash
# Controlled CRI diagnostic for the Phase-1 conformance work (#354/#355) — isolates
# each failing dimension with a LONG-LIVED container (sleep), so namespace-sharing /
# sysctl / hostname / DNS questions are separated from any start/exit race.
#
# Writes everything to a result file (critest-style); read it separately.
# Usage (on an IPC test node, never omen):
#   sudo bash scripts/cri-diag-sandbox.sh /tmp/cri-diag.result
set -u
OUT="${1:-/tmp/cri-diag.result}"
SOCK="unix:///run/pelagos/cri.sock"
CR="crictl --runtime-endpoint $SOCK --image-endpoint $SOCK"
IMG="registry.k8s.io/e2e-test-images/busybox:1.29-2"
WORK="$(mktemp -d)"

cat > "$WORK/pod.json" <<JSON
{ "metadata": {"name":"diag-pod","namespace":"default","uid":"diag-uid-1","attempt":1},
  "hostname": "diag-hostname",
  "log_directory": "$WORK",
  "linux": { "sysctls": {"kernel.shm_rmid_forced": "1"} } }
JSON
cat > "$WORK/ctr.json" <<JSON
{ "metadata": {"name":"diag-ctr","attempt":1},
  "image": {"image":"$IMG"},
  "command": ["sleep","3600"],
  "log_path": "diag-ctr.0.log",
  "linux": {} }
JSON

exec > "$OUT" 2>&1
echo "# cri-diag @ $(date -u +%FT%TZ)  cri=$(systemctl is-active pelagos-cri)"
$CR pull "$IMG" >/dev/null 2>&1 || echo "WARN: image pull failed"

POD=$($CR runp "$WORK/pod.json"); echo "POD=$POD"
CID=$($CR create "$POD" "$WORK/ctr.json" "$WORK/pod.json"); echo "CID=$CID"
$CR start "$CID"; sleep 1

echo "=== [running?] crictl ps ==="
$CR ps -a | grep -E "diag-ctr|CONTAINER" || true

echo "=== [exit reason] crictl inspect ==="
$CR inspect "$CID" 2>/dev/null | grep -iE '"state"|exitCode|"reason"|"message"|finishedAt|startedAt' | head
echo "=== [container log] crictl logs ==="
$CR logs "$CID" 2>&1 | head -15
PN="pcri-${CID:0:12}"
echo "=== [pelagos stderr/stdout for $PN] ==="
sudo sh -c "cat /run/pelagos/containers/$PN/stderr.log 2>&1 | head -15; echo '--- stdout ---'; cat /run/pelagos/containers/$PN/stdout.log 2>&1 | head -8"
echo "=== [pelagos-cri journal: run line] ==="
sudo journalctl -u pelagos-cri --since '40 sec ago' --no-pager 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g' | grep -iE "$PN|run failed|--hostname|--publish|sysctl|error|exited" | tail -12

echo "=== [hostname] exec hostname ==="
$CR exec "$CID" hostname 2>&1 || true

echo "=== [sysctl] exec cat /proc/sys/kernel/shm_rmid_forced (want 1) ==="
$CR exec "$CID" cat /proc/sys/kernel/shm_rmid_forced 2>&1 || true

echo "=== [dns] exec cat /etc/resolv.conf ==="
$CR exec "$CID" cat /etc/resolv.conf 2>&1 || true

echo "=== [ns sharing] container vs pause IPC/UTS/NET inodes ==="
CPID=$($CR inspect "$CID" 2>/dev/null | grep -oE '"pid": *[0-9]+' | head -1 | grep -oE '[0-9]+')
PAUSE=$(pgrep -f "sandbox __pause__" | head -1)
echo "container pid=$CPID  pause pid=$PAUSE"
for ns in ipc uts net; do
  c=$(sudo readlink "/proc/$CPID/ns/$ns" 2>/dev/null)
  p=$(sudo readlink "/proc/$PAUSE/ns/$ns" 2>/dev/null)
  echo "  $ns: container=$c pause=$p  $([ "$c" = "$p" ] && echo SHARED || echo SEPARATE)"
done

echo "=== [pause ns sysctl] value as seen inside the pause IPC ns (want 1) ==="
sudo nsenter --ipc="/proc/$PAUSE/ns/ipc" -- cat /proc/sys/kernel/shm_rmid_forced 2>&1 || true

echo "=== cleanup ==="
$CR stop "$CID" >/dev/null 2>&1; $CR rm "$CID" >/dev/null 2>&1
$CR stopp "$POD" >/dev/null 2>&1; $CR rmp "$POD" >/dev/null 2>&1
rm -rf "$WORK"
echo "# done @ $(date -u +%FT%TZ)"
