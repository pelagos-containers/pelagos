#!/usr/bin/env bash
# #561 repro/fix-proof: fire N fast-exiting pods at once on a given node, wait
# for them all to reach Succeeded, then check how many `kubectl logs` calls
# fail with "unable to retrieve container logs" (the reopened #438 symptom)
# and whether the image service (`crictl images`) is still responsive
# afterward. Run from a node with kubectl access (e.g. a k3s control-plane
# node); pass the target node name as $1 (default: the node this runs on).
#
# Before the #561 fix: a meaningful fraction of the N `kubectl logs` calls
# fail even though the pods completed cleanly, and `crictl images` over the
# node's pelagos CRI socket may wedge. After the fix: 0 failures expected,
# `crictl images` stays responsive.
set -u

N="${2:-20}"
NODE="${1:?usage: repro-561-pod-churn.sh <node-name> [pod-count]}"
PREFIX="repro561-$$"
KUBECTL="${KUBECTL:-kubectl}"

cleanup() {
  for i in $(seq 1 "$N"); do
    $KUBECTL delete pod "${PREFIX}-${i}" --ignore-not-found --wait=false >/dev/null 2>&1
  done
}
trap cleanup EXIT

echo "== firing $N fast-exiting pods at node $NODE =="
for i in $(seq 1 "$N"); do
  cat <<EOF | $KUBECTL apply -f - >/dev/null &
apiVersion: v1
kind: Pod
metadata:
  name: ${PREFIX}-${i}
spec:
  nodeName: ${NODE}
  restartPolicy: Never
  containers:
  - name: repro
    image: busybox:1.36
    command: ['sh', '-c', 'echo marker-${i}; exit 0']
EOF
done
wait

echo "== waiting for all $N pods to reach Succeeded (up to 60s) =="
for _ in $(seq 1 60); do
  done_count=0
  for i in $(seq 1 "$N"); do
    phase=$($KUBECTL get pod "${PREFIX}-${i}" -o jsonpath='{.status.phase}' 2>/dev/null)
    [ "$phase" = "Succeeded" ] && done_count=$((done_count + 1))
  done
  [ "$done_count" -eq "$N" ] && break
  sleep 1
done
echo "   $done_count/$N reached Succeeded"

echo "== checking kubectl logs for all $N pods =="
fail=0
for i in $(seq 1 "$N"); do
  if ! $KUBECTL logs "${PREFIX}-${i}" >/dev/null 2>&1; then
    fail=$((fail + 1))
    echo "   FAIL: ${PREFIX}-${i}"
  fi
done
echo "RESULT: $fail/$N kubectl logs calls failed (#438/#561 regression if > 0)"

echo "== checking image-service responsiveness on $NODE (#561 part 2) =="
if ssh -o ConnectTimeout=5 "cb@${NODE}" 'sudo crictl images' >/dev/null 2>&1; then
  echo "RESULT: crictl images OK on ${NODE}"
else
  echo "RESULT: crictl images FAILED/wedged on ${NODE}"
fi
