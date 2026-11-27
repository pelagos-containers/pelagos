#!/usr/bin/env bash
# Kata pod with many shared Secret volumes (#659): the shape of the pod that did not recover
# after a node cold boot (11 Secret volumes, 3 containers, some volumes mounted by two
# containers). Creates a throwaway namespace, RuntimeClass (handler "kata", unique name) and pod
# pinned to NODE, then checks every container runs and can read its secrets.
#
#   NODE=ipc7 KUBECTL="ssh ipc4 sudo kubectl" scripts/test-kata-secret-volumes.sh up     # create + wait
#   NODE=ipc7 KUBECTL="..." scripts/test-kata-secret-volumes.sh check                    # verify (e.g. after a reboot)
#   NODE=ipc7 KUBECTL="..." scripts/test-kata-secret-volumes.sh down                     # remove everything
set -uo pipefail
: "${NODE:?set NODE to the node that has the Kata handler}"
KUBECTL="${KUBECTL:-kubectl}"
NS=kata-secrets-659
RC=kata-secrets-659
POD=cp
IMG="${IMG:-docker.io/library/busybox:1.36-musl}"
k() { $KUBECTL "$@"; }
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "  PASS: $1"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL: $1"; }

# volume name -> mount path
declare -A M=(
  [m-ca-limited]=ca [m-proxy-limited]=proxy [m-sa]=sa [m-apiserver]=apiserver
  [m-kubelet-client]=kubelet-client [m-front-proxy-client]=front-proxy-client
  [m-datastore]=datastore [m-ca]=ca [m-sa-limited]=sa
  [m-controller-manager-kubeconfig]=controller-manager-kubeconfig
  [m-scheduler-kubeconfig]=scheduler-kubeconfig )

mounts() { # container volume names...
  for v in "$@"; do printf '        - {name: %s, mountPath: /etc/kubernetes/pki/%s, readOnly: true}\n' "$v" "${M[$v]}"; done
}

manifest() {
  cat <<YAML
apiVersion: v1
kind: Namespace
metadata: {name: $NS}
---
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata: {name: $RC}
handler: kata
YAML
  for v in "${!M[@]}"; do
    # ~2 KiB of data, like a certificate and key
    printf -- '---\napiVersion: v1\nkind: Secret\nmetadata: {name: %s, namespace: %s}\nstringData:\n  tls.crt: "%s"\n  tls.key: "%s"\n  value: "%s"\n' \
      "$v" "$NS" "$(head -c 1200 /dev/urandom | base64 -w0)" "$(head -c 600 /dev/urandom | base64 -w0)" "$v"
  done
  cat <<YAML
---
apiVersion: v1
kind: Pod
metadata: {name: $POD, namespace: $NS}
spec:
  nodeName: $NODE
  runtimeClassName: $RC
  restartPolicy: Always
  tolerations: [{operator: Exists}]
  volumes:
YAML
  for v in "${!M[@]}"; do printf '    - {name: %s, secret: {secretName: %s}}\n' "$v" "$v"; done
  cat <<YAML
  containers:
    - name: apiserver
      image: $IMG
      command: [sleep, "3600"]
      resources: {requests: {cpu: 100m, memory: 256Mi}, limits: {memory: 1Gi}}
      volumeMounts:
$(mounts m-ca-limited m-proxy-limited m-sa m-apiserver m-kubelet-client m-front-proxy-client m-datastore)
    - name: controller-manager
      image: $IMG
      command: [sleep, "3600"]
      resources: {requests: {cpu: 25m, memory: 64Mi}, limits: {memory: 256Mi}}
      volumeMounts:
$(mounts m-ca m-proxy-limited m-sa-limited m-controller-manager-kubeconfig)
    - name: scheduler
      image: $IMG
      command: [sleep, "3600"]
      resources: {requests: {cpu: 10m, memory: 32Mi}, limits: {memory: 128Mi}}
      volumeMounts:
$(mounts m-scheduler-kubeconfig)
YAML
}

check() {
  # No shell quoting in any kubectl argument: KUBECTL may be "ssh host sudo kubectl", which
  # re-parses its arguments on the far side.
  echo "=== Kata secret-volume pod on $NODE"
  local phase; phase=$(k -n $NS get pod $POD -o jsonpath={.status.phase} 2>/dev/null)
  [ "$phase" = Running ] && ok "pod is Running" || bad "pod phase: ${phase:-absent}"
  local up; up=$(k -n $NS get pod $POD -o jsonpath={.status.containerStatuses[*].state.running.startedAt} 2>/dev/null | wc -w)
  [ "$up" = 3 ] && ok "all 3 containers running" || bad "$up of 3 containers running"
  local d miss
  miss=""; for d in ca proxy sa apiserver kubelet-client front-proxy-client datastore; do
    k -n $NS exec $POD -c apiserver -- test -s /etc/kubernetes/pki/$d/tls.crt >/dev/null 2>&1 || miss="$miss $d"; done
  [ -z "$miss" ] && ok "apiserver reads all 7 secret mounts" || bad "apiserver cannot read:$miss"
  miss=""; for d in ca proxy sa controller-manager-kubeconfig; do
    k -n $NS exec $POD -c controller-manager -- test -s /etc/kubernetes/pki/$d/value >/dev/null 2>&1 || miss="$miss $d"; done
  [ -z "$miss" ] && ok "controller-manager reads its 4 secret mounts" || bad "controller-manager cannot read:$miss"
  k -n $NS exec $POD -c scheduler -- test -s /etc/kubernetes/pki/scheduler-kubeconfig/value >/dev/null 2>&1 \
    && ok "scheduler reads its secret mount" || bad "scheduler cannot read its secret mount"
  echo "=== $PASS passed, $FAIL failed"; [ $FAIL = 0 ]
}

case "${1:-}" in
  up)    manifest | k apply -f - >/dev/null && echo "applied"; k -n $NS wait --for=condition=Ready pod/$POD --timeout=${WAIT:-300s}; check ;;
  check) check ;;
  down)  k delete ns $NS --wait=true 2>&1 | tail -1; k delete runtimeclass $RC 2>&1 | tail -1 ;;
  manifest) manifest ;;
  *) echo "usage: $0 up|check|down|manifest" >&2; exit 2 ;;
esac
