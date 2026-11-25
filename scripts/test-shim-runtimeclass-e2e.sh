#!/usr/bin/env bash
# End-to-end test: Kubernetes pods on shim-backed runtime classes (Kata, gVisor)
# through the real kubelet and pelagos-cri (epic #574).
#
# Why this exists: crictl-level tests pass an image *reference*; the kubelet
# passes an image *id* (#622), sets a cgroup parent (#623), garbage-collects
# containers after stopping the sandbox (#624, #625) and mounts Kubernetes
# volumes. Only a real kubelet exercises all of that.
#
# Preconditions (not done by this script):
#   - NODE runs k3s/kubelet with pelagos-cri started with
#       --shim-handler kata=<containerd-shim-kata-v2> --shim-handler gvisor=<containerd-shim-runsc-v1>
#     (Kata installed at its shipped paths, e.g. /opt/kata; runsc on the CRI's PATH)
#   - kubectl points at the cluster; the node can pull docker.io/library/alpine:3.20
#   - Run on a node you can disrupt: pods are pinned to NODE (works while cordoned).
#
# Usage: NODE=<node-name> scripts/test-shim-runtimeclass-e2e.sh
# Optional: RUNTIMES="kata gvisor" (default), KUBECTL="sudo kubectl", NS=shim-e2e
set -uo pipefail

NODE="${NODE:?set NODE to the node to run on}"
KUBECTL="${KUBECTL:-kubectl}"
NS="${NS:-shim-e2e}"
RUNTIMES="${RUNTIMES:-kata gvisor}"
IMAGE="${IMAGE:-docker.io/library/alpine:3.20}"
# Deliberately unqualified: the kubelet passes the pod's image string as written (#630).
SHORT_IMAGE="${SHORT_IMAGE:-busybox}"
TIMEOUT="${TIMEOUT:-240}"
PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL: $1"; }
k() { $KUBECTL "$@"; }

cleanup() { k delete namespace "$NS" --wait=true --timeout=120s >/dev/null 2>&1 || true
            # Only delete RuntimeClasses this run created: they are cluster-scoped and an
            # operator's own `kata`/`gvisor` must survive a test run.
            for r in $OWNED_RC; do k delete runtimeclass "$r" >/dev/null 2>&1 || true; done; }
OWNED_RC=""
trap cleanup EXIT

echo "=== shim RuntimeClass end-to-end on $NODE ==="
k create namespace "$NS" >/dev/null 2>&1 || true
# A fresh cluster creates the namespace's default ServiceAccount asynchronously;
# pods are rejected until it exists.
for _ in $(seq 1 60); do k -n "$NS" get serviceaccount default >/dev/null 2>&1 && break; sleep 1; done
k -n "$NS" create configmap probe --from-literal=greeting=hello-from-configmap >/dev/null 2>&1 || true
host_kernel="$(k get node "$NODE" -o jsonpath='{.status.nodeInfo.kernelVersion}')"

for rt in $RUNTIMES; do
  echo "--- runtime: $rt"
  if ! k get runtimeclass "$rt" >/dev/null 2>&1; then
    printf 'apiVersion: node.k8s.io/v1\nkind: RuntimeClass\nmetadata: {name: %s}\nhandler: %s\n' "$rt" "$rt" | k create -f - >/dev/null
    OWNED_RC="$OWNED_RC $rt"
  fi
  k apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Pod
metadata: {name: $rt-pod, namespace: $NS}
spec:
  runtimeClassName: $rt
  nodeName: $NODE
  containers:
  - name: main
    image: $IMAGE
    command: ["sh","-c","uname -r; echo main-up; sleep 3600"]
    resources: {requests: {cpu: 100m, memory: 64Mi}, limits: {cpu: 500m, memory: 256Mi}}
    volumeMounts: [{name: cm, mountPath: /cfg}, {name: scratch, mountPath: /scratch}]
  - name: sidecar
    image: $SHORT_IMAGE
    command: ["sh","-c","echo sidecar-up; echo shared > /scratch/from-sidecar; sleep 3600"]
    volumeMounts: [{name: scratch, mountPath: /scratch}]
  volumes: [{name: cm, configMap: {name: probe}}, {name: scratch, emptyDir: {}}]
YAML
  if k -n "$NS" wait --for=condition=Ready "pod/$rt-pod" --timeout="${TIMEOUT}s" >/dev/null 2>&1; then
    pass "$rt: pod Ready (2/2 containers) through the kubelet"
  else
    fail "$rt: pod not Ready within ${TIMEOUT}s"; k -n "$NS" describe "pod/$rt-pod" | tail -8; continue
  fi
  guest="$(k -n "$NS" exec "$rt-pod" -c main -- uname -r 2>/dev/null | tr -d '\r')"
  if [ -n "$guest" ] && [ "$guest" != "$host_kernel" ]; then pass "$rt: runs on its own kernel ($guest, host $host_kernel)"
  else fail "$rt: guest kernel '$guest' is not distinct from host '$host_kernel'"; fi
  if [ "$rt" = kata ]; then   # #626: pod limits (256Mi) size the VM, not the ~2 GiB default
    mem_mib="$(k -n "$NS" exec "$rt-pod" -c main -- cat /proc/meminfo 2>/dev/null | awk '/MemTotal/{print int($2/1024)}')"
    if [ -n "$mem_mib" ] && [ "$mem_mib" -ge 128 ] && [ "$mem_mib" -le 512 ]; then pass "kata: VM sized from pod limits (guest ${mem_mib} MiB for a 256Mi limit)"
    else fail "kata: guest memory '${mem_mib}' MiB not sized from the 256Mi pod limit"; fi
  fi
  [ "$(k -n "$NS" exec "$rt-pod" -c main -- cat /cfg/greeting 2>/dev/null)" = "hello-from-configmap" ] \
    && pass "$rt: ConfigMap volume mounted" || fail "$rt: ConfigMap volume missing"
  k -n "$NS" exec "$rt-pod" -c main -- test -s /var/run/secrets/kubernetes.io/serviceaccount/token 2>/dev/null \
    && pass "$rt: service-account token mounted" || fail "$rt: service-account token missing"
  [ "$(k -n "$NS" exec "$rt-pod" -c main -- cat /scratch/from-sidecar 2>/dev/null)" = "shared" ] \
    && pass "$rt: emptyDir shared between containers" || fail "$rt: emptyDir not shared"
  k -n "$NS" logs "$rt-pod" -c main 2>/dev/null | grep -q main-up \
    && pass "$rt: container logs" || fail "$rt: container logs missing"
  ip="$(k -n "$NS" get pod "$rt-pod" -o jsonpath='{.status.podIP}')"
  [ -n "$ip" ] && pass "$rt: pod IP from the cluster CNI ($ip)" || fail "$rt: no pod IP"
done

if [ "$(echo $RUNTIMES | wc -w)" -ge 2 ]; then
  set -- $RUNTIMES
  a="$1-pod"; b="$2-pod"; bip="$(k -n "$NS" get pod "$b" -o jsonpath='{.status.podIP}' 2>/dev/null)"
  [ -n "$bip" ] && k -n "$NS" exec "$a" -c main -- ping -c2 -W2 "$bip" >/dev/null 2>&1 \
    && pass "cross-runtime pod-to-pod network ($a -> $b)" || fail "cross-runtime network ($a -> $b)"
fi

echo "--- delete: no leaked pods"
k -n "$NS" delete pod --all --wait=true --timeout=120s >/dev/null 2>&1
left="$(k -n "$NS" get pods --no-headers 2>/dev/null | wc -l)"
[ "$left" -eq 0 ] && pass "pods deleted" || fail "$left pods remain"

echo ""; echo "Results: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
