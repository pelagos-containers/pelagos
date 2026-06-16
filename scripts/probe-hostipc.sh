#!/usr/bin/env bash
# Probe: does a hostIPC pod's CONTAINER land in the host IPC namespace?
#   sudo bash scripts/probe-hostipc.sh
set -u
SOCK="unix:///run/pelagos/cri.sock"
C() { sudo crictl -r "$SOCK" "$@"; }

ipcmk -M 1024 >/dev/null 2>&1 || true
echo "host    ipc ns: $(readlink /proc/self/ns/ipc)"

cat >/tmp/hostipc-pod.json <<JSON
{ "metadata": {"name":"hipc-probe","namespace":"default","uid":"hipc-probe-uid","attempt":1},
  "log_directory":"/tmp",
  "linux": {"security_context": {"namespace_options": {"ipc": 2}}} }
JSON
cat >/tmp/hostipc-ctr.json <<JSON
{ "metadata": {"name":"hipc-c"},
  "image": {"image":"registry.k8s.io/e2e-test-images/busybox:1.29-2"},
  "command": ["sleep","600"], "log_path":"hipc-c.log" }
JSON

SB=$(C runp /tmp/hostipc-pod.json 2>/dev/null | tail -1)
CID=$(C create "$SB" /tmp/hostipc-ctr.json /tmp/hostipc-pod.json 2>/dev/null | tail -1)
C start "$CID" >/dev/null 2>&1
CPID=$(C inspect "$CID" 2>/dev/null | python3 -c 'import sys,json; print(json.load(sys.stdin)["info"]["pid"])' 2>/dev/null)
echo "sandbox=$SB container=$CID cpid=$CPID"
echo "ctr     ipc ns: $( [ -n "$CPID" ] && readlink /proc/$CPID/ns/ipc )"
echo "ctr     net ns: $( [ -n "$CPID" ] && readlink /proc/$CPID/ns/net )  (for reference)"
echo "--- ipcs -m as seen from inside the container's IPC ns (nsenter) ---"
[ -n "$CPID" ] && sudo nsenter -t "$CPID" -i ipcs -m 2>&1 | head -8

echo "=== cleanup ==="
C stop "$CID" >/dev/null 2>&1; C rm "$CID" >/dev/null 2>&1
C stopp "$SB" >/dev/null 2>&1; C rmp "$SB" >/dev/null 2>&1
