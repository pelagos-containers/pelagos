#!/usr/bin/env bash
# End-to-end test: a Kata pod recovers when its shim or its VMM is killed
# (#641, #642), through the real kubelet and pelagos-cri.
#
# Expected behaviour (as with containerd + Kata): the dead shim/VM makes the
# sandbox NotReady, the kubelet emits SandboxChanged, replaces the sandbox, and
# the pod is Running again. No qemu/virtiofsd/shim of the old sandbox is left.
#
# Run ON the node, as root (it kills processes and reads the CRI socket).
# Preconditions: as scripts/test-shim-runtimeclass-e2e.sh (handler `kata` enabled,
# Kata at /opt/kata); crictl able to reach pelagos-cri.
#
# Usage: NODE=<node-name> sudo -E scripts/test-shim-failure-recovery.sh
# Optional: KUBECTL="sudo kubectl", CRICTL="crictl -r unix:///run/pelagos/cri.sock",
#           NS=shim-fail, TIMEOUT=180, SCENARIOS="shim vmm", KILL=kill
set -uo pipefail

NODE="${NODE:?set NODE to the name of this node}"
KUBECTL="${KUBECTL:-kubectl}"
CRICTL="${CRICTL:-crictl -r unix:///run/pelagos/cri.sock}"
NS="${NS:-shim-fail}"
TIMEOUT="${TIMEOUT:-180}"
SCENARIOS="${SCENARIOS:-shim vmm}"
IMAGE="${IMAGE:-docker.io/library/alpine:3.20}"
PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL: $1"; }
k() { $KUBECTL "$@"; }
KILL="${KILL:-kill}"
cleanup() { k delete namespace "$NS" --wait=true --timeout=120s >/dev/null 2>&1 || true
            # cluster-scoped: only remove a RuntimeClass this run created
            [ -z "$OWNED_RC" ] || k delete runtimeclass kata >/dev/null 2>&1 || true; }
OWNED_RC=""
trap cleanup EXIT

ready_sandbox() { $CRICTL pods --name kp --state ready -q 2>/dev/null | head -1; }
# Anchored: an unanchored pattern also matches the shell running the pgrep.
shim_pid()  { pgrep -f "^/opt/kata/runtime-rs/bin/containerd-shim-kata-v2 -id $1" | head -1; }
vmm_pid()   { pgrep -f "^/opt/kata/bin/qemu-system.*sandbox-$1" | head -1; }
vm_procs()  { pgrep -fc "^(/opt/kata/bin/qemu-system.*sandbox-$1|/opt/kata/runtime-rs/bin/containerd-shim-kata-v2 -id $1)" || true; }
pod_running() { k -n "$NS" get pod kp --no-headers 2>/dev/null | grep -q ' 1/1 *Running '; }

echo "=== Kata failure recovery on $NODE ==="
k create namespace "$NS" >/dev/null 2>&1 || true
for _ in $(seq 1 60); do k -n "$NS" get serviceaccount default >/dev/null 2>&1 && break; sleep 1; done
k -n "$NS" create secret generic s --from-literal=a=b >/dev/null 2>&1 || true
if ! k get runtimeclass kata >/dev/null 2>&1; then
  printf 'apiVersion: node.k8s.io/v1\nkind: RuntimeClass\nmetadata: {name: kata}\nhandler: kata\n' | k create -f - >/dev/null
  OWNED_RC=1
fi
k apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Pod
metadata: {name: kp, namespace: $NS}
spec:
  runtimeClassName: kata
  nodeName: $NODE
  containers:
  - name: a
    image: $IMAGE
    command: ["sleep","3600"]
    volumeMounts: [{name: s, mountPath: /s}]
  volumes: [{name: s, secret: {secretName: s}}]
YAML
for _ in $(seq 1 $((TIMEOUT/3))); do pod_running && break; sleep 3; done
pod_running && pass "pod Running before the kill" || { fail "pod never Running"; exit 1; }

for sc in $SCENARIOS; do
  echo "--- kill the $sc"
  old="$(ready_sandbox)"
  [ -n "$old" ] || { fail "$sc: no ready sandbox"; continue; }
  if [ "$sc" = shim ]; then pid="$(shim_pid "$old")"; else pid="$(vmm_pid "$old")"; fi
  [ -n "$pid" ] || { fail "$sc: process not found"; continue; }
  $KILL -9 "$pid"
  deadline=$((SECONDS+TIMEOUT)); new=""
  while [ $SECONDS -lt $deadline ]; do
    new="$(ready_sandbox)"
    [ -n "$new" ] && [ "$new" != "$old" ] && pod_running && break
    sleep 3
  done
  if [ -n "$new" ] && [ "$new" != "$old" ] && pod_running; then
    pass "$sc: sandbox replaced (${old:0:12} -> ${new:0:12}) and pod Running again"
  else
    fail "$sc: pod did not recover within ${TIMEOUT}s (ready sandbox: '${new:0:12}')"
    k -n "$NS" get pod kp 2>&1 | tail -2
  fi
  for _ in $(seq 1 20); do [ "$(vm_procs "$old")" = 0 ] && break; sleep 1; done
  [ "$(vm_procs "$old")" = 0 ] && pass "$sc: no VM process of the old sandbox left" \
                                || fail "$sc: orphaned VM process(es) of ${old:0:12} remain"
done

echo "=== $PASS passed, $FAIL failed ==="
[ "$FAIL" = 0 ]
