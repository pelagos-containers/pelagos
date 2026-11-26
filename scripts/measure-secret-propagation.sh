#!/usr/bin/env bash
# Measure how long a Secret update takes to reach a Kata guest vs a native pod
# on the same node (#645), and separate the kubelet's own sync delay from the
# extra time the Kata path adds.
#
# The kubelet syncs every pod's volumes on its own schedule (about a minute, with
# jitter), so each pod has to be compared with ITS OWN host-side volume dir:
#   host_n / native   the native pod's kubelet volume dir, and what that pod sees
#                     (a bind mount of the same dir, so they should coincide),
#   host_k / kata     the Kata pod's kubelet volume dir, and what its guest sees
#                     (only through the shim's shared-dir sync).
# kata - host_k is the delay the Kata path adds; host_k itself is the kubelet's.
#
# Run on a node with the kata handler, from anywhere with kubectl; node-local reads
# go through NODE_EXEC (e.g. "ssh -q ipc7") under sudo, or run locally as root.
#
# Usage: NODE=<node> KUBECTL="kubectl" [NODE_EXEC="ssh -q <node>"] scripts/measure-secret-propagation.sh
# Optional: ROUNDS=4, NS=secret-prop, TIMEOUT=300, KUBELET_DIR=/var/lib/kubelet
set -uo pipefail

NODE="${NODE:?set NODE}"
KUBECTL="${KUBECTL:-kubectl}"
NODE_EXEC="${NODE_EXEC:-}"
NS="${NS:-secret-prop}"
ROUNDS="${ROUNDS:-4}"
TIMEOUT="${TIMEOUT:-300}"
KUBELET_DIR="${KUBELET_DIR:-/var/lib/kubelet}"
IMAGE="${IMAGE:-docker.io/library/alpine:3.20}"
k() { $KUBECTL "$@"; }
nx() { if [ -n "$NODE_EXEC" ]; then $NODE_EXEC "sudo $(printf '%q ' "$@")"; else "$@"; fi; }
OWNED_RC=""
cleanup() { k delete namespace "$NS" --wait=true --timeout=120s >/dev/null 2>&1 || true
            for r in $OWNED_RC; do k delete runtimeclass "$r" >/dev/null 2>&1 || true; done; }
trap cleanup EXIT

k create namespace "$NS" >/dev/null 2>&1 || true
for _ in $(seq 1 60); do k -n "$NS" get serviceaccount default >/dev/null 2>&1 && break; sleep 1; done
set_secret() { k -n "$NS" create secret generic s --from-literal=v="$1" --dry-run=client -o yaml | k apply -f - >/dev/null; }
set_secret v0
rc="kata-secretprop-$$"
printf 'apiVersion: node.k8s.io/v1\nkind: RuntimeClass\nmetadata: {name: %s}\nhandler: kata\n' "$rc" | k create -f - >/dev/null
OWNED_RC="$rc"
for who in native kata; do
  cls=""; [ "$who" = kata ] && cls="runtimeClassName: $rc"
  k apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Pod
metadata: {name: $who, namespace: $NS}
spec:
  $cls
  nodeName: $NODE
  containers:
  - name: a
    image: $IMAGE
    command: ["sleep","7200"]
    volumeMounts: [{name: s, mountPath: /s}]
  volumes: [{name: s, secret: {secretName: s}}]
YAML
done
for who in native kata; do
  for _ in $(seq 1 60); do k -n "$NS" get pod $who --no-headers 2>/dev/null | grep -q ' 1/1 *Running ' && break; sleep 3; done
done
uid_n="$(k -n "$NS" get pod native -o jsonpath='{.metadata.uid}')"
uid_k="$(k -n "$NS" get pod kata -o jsonpath='{.metadata.uid}')"
hf_n="$KUBELET_DIR/pods/$uid_n/volumes/kubernetes.io~secret/s/v"
hf_k="$KUBELET_DIR/pods/$uid_k/volumes/kubernetes.io~secret/s/v"
echo "=== secret propagation on $NODE: $ROUNDS rounds"
HNS=(); NATS=(); HKS=(); KATS=(); ADDN=(); ADDK=()
for r in $(seq 1 "$ROUNDS"); do
  val="round$r-$RANDOM"
  set_secret "$val"; t0=$(date +%s)
  hn=""; tn=""; hk=""; tk=""
  while [ $(( $(date +%s) - t0 )) -lt "$TIMEOUT" ]; do
    [ -z "$hn" ] && [ "$(nx cat "$hf_n" 2>/dev/null)" = "$val" ] && hn=$(( $(date +%s) - t0 ))
    [ -z "$tn" ] && [ "$(k -n "$NS" exec native -- cat /s/v 2>/dev/null)" = "$val" ] && tn=$(( $(date +%s) - t0 ))
    [ -z "$hk" ] && [ "$(nx cat "$hf_k" 2>/dev/null)" = "$val" ] && hk=$(( $(date +%s) - t0 ))
    [ -z "$tk" ] && [ "$(k -n "$NS" exec kata -- cat /s/v 2>/dev/null)" = "$val" ] && tk=$(( $(date +%s) - t0 ))
    [ -n "$hn" ] && [ -n "$tn" ] && [ -n "$hk" ] && [ -n "$tk" ] && break
    sleep 1
  done
  d() { [ -n "$1" ] && [ -n "$2" ] && echo $(( $2 - $1 )) || echo n/a; }
  echo "round $r: native: host=${hn:-timeout}s pod=${tn:-timeout}s (+$(d "$hn" "$tn")s) | kata: host=${hk:-timeout}s guest=${tk:-timeout}s (+$(d "$hk" "$tk")s)"
  HNS+=("${hn:-}"); NATS+=("${tn:-}"); HKS+=("${hk:-}"); KATS+=("${tk:-}")
  ADDN+=("$(d "$hn" "$tn")"); ADDK+=("$(d "$hk" "$tk")")
done
med() { printf '%s\n' "$@" | grep -v '^$' | sort -n | awk '{a[NR]=$1} END{ if(NR) print a[int((NR+1)/2)]; else print "n/a"}'; }
echo "=== median: native host=$(med "${HNS[@]}")s pod=$(med "${NATS[@]}")s (adds $(med "${ADDN[@]}")s) | kata host=$(med "${HKS[@]}")s guest=$(med "${KATS[@]}")s (adds $(med "${ADDK[@]}")s)"
