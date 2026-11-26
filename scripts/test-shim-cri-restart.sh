#!/usr/bin/env bash
# End-to-end test: shim-backed pods survive a pelagos-cri restart (#602, #618, #644).
#
# A pod on each runtime handler is started through the real kubelet, then the
# CRI service is restarted. The shim (and, for Kata, the VM) must be the SAME
# processes afterwards, the sandbox must still be Ready, the container must not
# have restarted, and the guest's uptime must keep counting (a restarted VM or
# sandbox would reset it). Note: Kata moves its shim into the pod's kubelet
# cgroup after start, so the pelagos-shim-<id>.scope that wraps `start` is
# normally gone; what protects the shim from a CRI restart is that it is outside
# the CRI's own cgroup. This test checks the property, not the mechanism.
#
# Run ON the node as root, or from elsewhere with NODE_EXEC="ssh -q <node>" (every
# node-local command then runs remotely under sudo).
#
# Usage: NODE=<node-name> sudo -E scripts/test-shim-cri-restart.sh
# Optional: RUNTIMES="kata gvisor" (default), KUBECTL="kubectl", NS=shim-restart,
#           CRICTL="crictl -r unix:///run/pelagos/cri.sock", CRI_UNIT=pelagos-cri,
#           NODE_EXEC="", TIMEOUT=180
set -uo pipefail

NODE="${NODE:?set NODE to the name of this node}"
KUBECTL="${KUBECTL:-kubectl}"
CRICTL="${CRICTL:-crictl -r unix:///run/pelagos/cri.sock}"
CRI_UNIT="${CRI_UNIT:-pelagos-cri}"
NS="${NS:-shim-restart}"
RUNTIMES="${RUNTIMES:-kata gvisor}"
NODE_EXEC="${NODE_EXEC:-}"
TIMEOUT="${TIMEOUT:-180}"
IMAGE="${IMAGE:-docker.io/library/alpine:3.20}"
PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL: $1"; }
k() { $KUBECTL "$@"; }
# Run a node-local command (locally, or over NODE_EXEC under sudo).
nx() { if [ -n "$NODE_EXEC" ]; then $NODE_EXEC "sudo $(printf '%q ' "$@")"; else "$@"; fi; }

OWNED_RC=""
cleanup() { k delete namespace "$NS" --wait=true --timeout=120s >/dev/null 2>&1 || true
            # cluster-scoped: only the uniquely named classes this run created
            for r in $OWNED_RC; do k delete runtimeclass "$r" >/dev/null 2>&1 || true; done; }
trap cleanup EXIT

echo "=== shim pods survive a $CRI_UNIT restart on $NODE ($RUNTIMES) ==="
k create namespace "$NS" >/dev/null 2>&1 || true
for _ in $(seq 1 60); do k -n "$NS" get serviceaccount default >/dev/null 2>&1 && break; sleep 1; done

uptime_of() { k -n "$NS" exec "$1" -- cat /proc/uptime 2>/dev/null | cut -d. -f1; }
ready_sandbox() { nx $CRICTL pods --name "$1" --state ready -q 2>/dev/null | head -1; }
pod_running() { k -n "$NS" get pod "$1" --no-headers 2>/dev/null | grep -q ' 1/1 *Running '; }

for rt in $RUNTIMES; do
  echo "--- runtime: $rt"
  rc="$rt-restart-$$"; pod="$rt-pod"
  printf 'apiVersion: node.k8s.io/v1\nkind: RuntimeClass\nmetadata: {name: %s}\nhandler: %s\n' "$rc" "$rt" | k create -f - >/dev/null
  OWNED_RC="$OWNED_RC $rc"
  k apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Pod
metadata: {name: $pod, namespace: $NS}
spec:
  runtimeClassName: $rc
  nodeName: $NODE
  containers: [{name: a, image: $IMAGE, command: ["sleep","3600"]}]
YAML
  for _ in $(seq 1 $((TIMEOUT/3))); do pod_running "$pod" && break; sleep 3; done
  pod_running "$pod" || { fail "$rt: pod never Running"; continue; }

  sb="$(ready_sandbox "$pod")"
  shim="$(nx pgrep -f "containerd-shim.* -id $sb" | head -1)"
  up0="$(uptime_of "$pod")"
  [ -n "$sb" ] && [ -n "$shim" ] && [ -n "$up0" ] || { fail "$rt: could not find sandbox/shim/uptime (sb='$sb' shim='$shim' up='$up0')"; continue; }
  echo "  sandbox ${sb:0:12}, shim pid $shim, shim cgroup: $(nx cat /proc/$shim/cgroup | head -1), guest uptime ${up0}s"

  nx systemctl restart "$CRI_UNIT" || { fail "$rt: restart failed"; continue; }
  for _ in $(seq 1 60); do nx $CRICTL pods -q >/dev/null 2>&1 && break; sleep 1; done
  sleep 10

  nx test -d "/proc/$shim" && pass "$rt: same shim process (pid $shim) alive after the restart" \
                           || fail "$rt: shim pid $shim died with the CRI restart"
  [ "$(ready_sandbox "$pod")" = "$sb" ] && pass "$rt: sandbox ${sb:0:12} still Ready" \
                                         || fail "$rt: sandbox changed or not Ready after restart"
  for _ in $(seq 1 30); do pod_running "$pod" && break; sleep 2; done
  restarts="$(k -n "$NS" get pod "$pod" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null)"
  pod_running "$pod" && [ "${restarts:-x}" = 0 ] && pass "$rt: pod Running, container never restarted" \
                                                 || fail "$rt: pod not Running or container restarted (restarts=$restarts)"
  up1="$(uptime_of "$pod")"
  if [ -n "$up1" ] && [ "$up1" -ge "$up0" ]; then pass "$rt: guest uptime kept counting (${up0}s -> ${up1}s): same VM/sandbox, exec works"
  else fail "$rt: exec failed or uptime reset (before ${up0}s, after '${up1}')"; fi
done

echo "=== $PASS passed, $FAIL failed ==="
[ "$FAIL" = 0 ]
