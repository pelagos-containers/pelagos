#!/usr/bin/env bash
# Diagnose the #352 user/group bucket: run a container with RunAsUser/RunAsGroup/
# SupplementalGroups set and report the EFFECTIVE id inside it, plus test the
# RunAsGroup-without-RunAsUser validation. Writes a result file.
#
# Usage (IPC node, never omen): sudo bash scripts/cri-diag-usergroup.sh /tmp/ug.result
set -u
OUT="${1:-/tmp/ug.result}"
SOCK="unix:///run/pelagos/cri.sock"
CR="crictl --runtime-endpoint $SOCK --image-endpoint $SOCK"
IMG="registry.k8s.io/e2e-test-images/busybox:1.29-2"
W=$(mktemp -d)

cat > "$W/pod.json" <<JSON
{ "metadata": {"name":"ug-pod","namespace":"default","uid":"ug-1","attempt":1}, "log_directory": "$W" }
JSON
# Container with runAsUser=1000, runAsGroup=2000, supplementalGroups=[3000,4000].
cat > "$W/ctr.json" <<JSON
{ "metadata": {"name":"ug-ctr","attempt":1}, "image": {"image":"$IMG"},
  "command": ["sh","-c","id; sleep 3600"], "log_path": "c.log",
  "linux": { "security_context": {
    "run_as_user": {"value": 1000}, "run_as_group": {"value": 2000},
    "supplemental_groups": [3000, 4000] } } }
JSON
# Container with runAsGroup but NO runAsUser → CRI says this must be rejected.
cat > "$W/badgrp.json" <<JSON
{ "metadata": {"name":"badgrp-ctr","attempt":1}, "image": {"image":"$IMG"},
  "command": ["sleep","3600"], "log_path": "b.log",
  "linux": { "security_context": { "run_as_group": {"value": 2000} } } }
JSON

exec > "$OUT" 2>&1
echo "# cri-diag-usergroup @ $(date -u +%FT%TZ)"
$CR pull "$IMG" >/dev/null 2>&1
POD=$($CR runp "$W/pod.json"); echo "POD=$POD"

echo "=== [uid/gid/groups] expect uid=1000 gid=2000 groups=2000,3000,4000 ==="
CID=$($CR create "$POD" "$W/ctr.json" "$W/pod.json"); $CR start "$CID" >/dev/null 2>&1; sleep 1
# The CONTAINER's own `id` (its real process uid), from the container log:
echo "  container-process id (from log): $($CR logs "$CID" 2>/dev/null | head -1)"
# What `crictl exec` runs as (exec's own uid — may differ from the container):
echo "  crictl-exec id                 : $($CR exec "$CID" id 2>&1)"
# Host-side truth: the container process's /proc/<pid>/status Uid line:
PN="pcri-${CID:0:12}"
CPID=$(sudo grep -oE '"pid":[ ]*[0-9]+' "/run/pelagos/containers/$PN/state.json" 2>/dev/null | head -1 | grep -oE '[0-9]+')
echo "  /proc/$CPID/status Uid/Gid/Groups: $(sudo grep -E '^(Uid|Gid|Groups):' /proc/$CPID/status 2>/dev/null | tr '\n' ' ')"

echo "=== [validation] create with RunAsGroup and NO RunAsUser → expect an ERROR ==="
BADOUT=$($CR create "$POD" "$W/badgrp.json" "$W/pod.json" 2>&1)
echo "  create result: $BADOUT"

echo "=== cleanup ==="
$CR stop "$CID" >/dev/null 2>&1; $CR rm "$CID" >/dev/null 2>&1
$CR stopp "$POD" >/dev/null 2>&1; $CR rmp "$POD" >/dev/null 2>&1
rm -rf "$W"
echo "# done"
