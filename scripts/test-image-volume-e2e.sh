#!/usr/bin/env bash
# End-to-end test: Kubernetes image volumes (`volumes: - image: {reference: ...}`,
# KEP-4639) through the real kubelet and pelagos-cri (#616).
#
# The kubelet pulls the volume's image and passes CreateContainer a mount with
# `image` (and `image_sub_path` for a volumeMount subPath) and no host path. The
# runtime must present the image read-only at the mount path. The pod's container
# image (alpine) and the volume image (busybox) differ, so the test can tell which
# one it is looking at.
#
# Run from anywhere with kubectl; node-local reads go through NODE_EXEC (e.g.
# "ssh -q ipc7", run under sudo), or run on the node as root with it unset.
#
# Usage: NODE=<node> KUBECTL="kubectl" [NODE_EXEC=""] scripts/test-image-volume-e2e.sh
# Optional: RUNTIMES="native" (default; add "kata" / "gvisor" when those handlers
#           are enabled), NS=ivol-e2e, TIMEOUT=240, VOL_IMAGE=docker.io/library/busybox:1.36-musl (musl: the container is alpine, so binaries from the volume must run on its libc)
set -uo pipefail

NODE="${NODE:?set NODE}"
KUBECTL="${KUBECTL:-kubectl}"
NODE_EXEC="${NODE_EXEC:-}"
NS="${NS:-ivol-e2e}"
RUNTIMES="${RUNTIMES:-native}"
TIMEOUT="${TIMEOUT:-240}"
CTR_IMAGE="${CTR_IMAGE:-docker.io/library/alpine:3.20}"
VOL_IMAGE="${VOL_IMAGE:-docker.io/library/busybox:1.36-musl}"
STAGING="${STAGING:-/run/pelagos-cri/imgvol}"
PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL: $1"; }
k() { $KUBECTL "$@"; }
nx() { if [ -n "$NODE_EXEC" ]; then $NODE_EXEC "sudo $(printf '%q ' "$@")"; else "$@"; fi; }
OWNED_RC=""
cleanup() { k delete namespace "$NS" --wait=true --timeout=120s >/dev/null 2>&1 || true
            for r in $OWNED_RC; do k delete runtimeclass "$r" >/dev/null 2>&1 || true; done; }
trap cleanup EXIT

echo "=== image volumes on $NODE ($RUNTIMES): volume image $VOL_IMAGE, container image $CTR_IMAGE ==="
k create namespace "$NS" >/dev/null 2>&1 || true
for _ in $(seq 1 60); do k -n "$NS" get serviceaccount default >/dev/null 2>&1 && break; sleep 1; done

for rt in $RUNTIMES; do
  echo "--- runtime: $rt"
  cls=""
  if [ "$rt" != native ]; then
    rc="$rt-ivol-$$"
    printf 'apiVersion: node.k8s.io/v1\nkind: RuntimeClass\nmetadata: {name: %s}\nhandler: %s\n' "$rc" "$rt" | k create -f - >/dev/null
    OWNED_RC="$OWNED_RC $rc"; cls="runtimeClassName: $rc"
  fi
  pod="ivol-$rt"
  k apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Pod
metadata: {name: $pod, namespace: $NS}
spec:
  $cls
  nodeName: $NODE
  containers:
  - name: a
    image: $CTR_IMAGE
    command: ["sleep","3600"]
    volumeMounts:
    - {name: img, mountPath: /img}
    - {name: img, mountPath: /imgsub, subPath: etc}
  volumes:
  - name: img
    image: {reference: $VOL_IMAGE, pullPolicy: IfNotPresent}
YAML
  for _ in $(seq 1 $((TIMEOUT/3))); do k -n "$NS" get pod "$pod" --no-headers 2>/dev/null | grep -q ' 1/1 *Running ' && break; sleep 3; done
  if ! k -n "$NS" get pod "$pod" --no-headers 2>/dev/null | grep -q ' 1/1 *Running '; then
    fail "$rt: pod with an image volume never Running"; k -n "$NS" describe pod "$pod" 2>&1 | tail -8; continue
  fi
  pass "$rt: pod with an image volume Ready through the kubelet"
  ex() { k -n "$NS" exec "$pod" -- "$@" 2>&1; }

  ex ls /img/bin/busybox >/dev/null && pass "$rt: the volume image's files are visible (/img/bin/busybox)" \
                                    || fail "$rt: /img/bin/busybox missing: the image was not mounted"
  # The container image is alpine; its release file must not be in the busybox volume.
  ex ls /img/etc/alpine-release >/dev/null 2>&1 && fail "$rt: the container's own image leaked into the volume" \
                                                || pass "$rt: the volume holds the volume image, not the container image"
  ex touch /img/new-file >/dev/null 2>&1 && fail "$rt: the image volume is writable" \
                                         || pass "$rt: the image volume is read-only"
  ex /img/bin/ls /img >/dev/null && pass "$rt: binaries from the volume execute (no noexec)" \
                                 || fail "$rt: cannot execute a binary from the volume"
  ex ls /imgsub/passwd >/dev/null && pass "$rt: subPath 'etc' mounts just that directory (/imgsub/passwd)" \
                                  || fail "$rt: subPath not applied (/imgsub/passwd missing)"
  ex ls /imgsub/etc >/dev/null 2>&1 && fail "$rt: subPath mounted the whole image root" \
                                    || pass "$rt: subPath did not expose the rest of the image"
done

k delete pod -n "$NS" --all --wait=true --timeout=120s >/dev/null 2>&1
sleep 5
left="$(nx ls "$STAGING" 2>/dev/null | wc -l)"
mnts="$(nx grep -c " $STAGING/" /proc/self/mountinfo 2>/dev/null || true)"
[ "${left:-0}" = 0 ] && [ "${mnts:-0}" = 0 ] && pass "no staged image volumes or mounts left after the pods are removed" \
  || fail "left behind: $left staging dirs, $mnts mounts under $STAGING"

echo "=== $PASS passed, $FAIL failed ==="
[ "$FAIL" = 0 ]
