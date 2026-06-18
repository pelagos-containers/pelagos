#!/usr/bin/env bash
# Driver for the cluster build/deploy loop (run from the dev box, e.g. omen):
#   1. launch the k3s build Job on the 32G build node (ipc4) for a given git ref
#   2. wait for it to finish, surface its log
#   3. deliver the staged binaries ipc4 -> ipc6 over the cluster LAN (never via
#      the dev box's uplink)
#   4. touch the delivery marker on ipc6 so its pelagos-install.path installs them
#      and restarts pelagos-cri
#   5. optionally run a focused critest on ipc6
#
# Only small control commands cross the dev-box link; the multi-MB binary moves
# ipc4 -> ipc6 over gigabit LAN.
#
# One-time prerequisites: see k8s/build/README.md (node labels, hostPath dirs,
# the systemd path unit on ipc6, and cb@ipc4 -> cb@ipc6 SSH trust).
#
# Usage: scripts/cluster-build.sh <git-ref> [critest-focus]
set -euo pipefail

REF="${1:?usage: cluster-build.sh <git-ref> [critest-focus]}"
FOCUS="${2:-}"

JUMP=ipc1                       # bastion the dev box reaches the cluster through
CP=cb@ipc1                     # control-plane (kubectl) host
BUILD=cb@192.168.88.55         # ipc4 (build node)
TEST=cb@192.168.88.57          # ipc6 (guinea pig)
TEST_LAN=cb@ipc6               # how the build node addresses the guinea pig
SOCK=unix:///run/pelagos/cri.sock
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

kc() { ssh -J "$JUMP" "$CP" sudo kubectl "$@"; }

echo "== launch build Job for ref '$REF' on ipc4 =="
kc delete job pelagos-build --ignore-not-found >/dev/null 2>&1 || true
sed "s/__GIT_REF__/${REF//\//\\/}/" "$REPO_ROOT/k8s/build/pelagos-build-job.yaml" \
  | ssh -J "$JUMP" "$CP" 'sudo kubectl apply -f -'

echo "== wait for build (up to 20m) =="
kc wait --for=condition=complete job/pelagos-build --timeout=1200s &
WAIT_OK=$!
# Surface progress; the wait above returns when the Job finishes or times out.
kc logs -f job/pelagos-build 2>/dev/null || true
wait "$WAIT_OK" || { echo "build did not complete; recent log:"; kc logs job/pelagos-build | tail -30; exit 1; }

echo "== deliver ipc4 -> ipc6 over LAN =="
ssh -J "$JUMP" "$BUILD" "
  set -e
  cd /srv/pelagos-build/staging
  scp -p pelagos pelagos-cri .commit $TEST_LAN:/srv/pelagos-incoming/
  ssh $TEST_LAN 'date -u +%FT%TZ > /srv/pelagos-incoming/.ready'
"
echo "== guinea pig should now install + restart pelagos-cri (pelagos-install.path) =="
sleep 4
ssh -J "$JUMP" "$TEST" 'systemctl is-active pelagos-cri; pelagos --version 2>/dev/null || true'

if [ -n "$FOCUS" ]; then
  # Model B (transient guinea pig): ipc6 is a normal k3s member, but the kubelet's
  # orphan-sandbox GC corrupts a critest run (it deletes critest's sandboxes mid
  # sweep — the #379/#353 flakiness). That GC runs regardless of `cordon`, so we
  # STOP the agent for the sweep, then always restore it (cordon too, so nothing
  # schedules onto the half-drained node in the meantime).
  echo "== drain ipc6 for a clean critest sweep (stop kubelet GC) =="
  kc cordon ipc6 || true
  ssh -J "$JUMP" "$TEST" 'sudo systemctl stop k3s-agent' || true

  restore_agent() {
    echo "== restore ipc6 to the cluster =="
    ssh -J "$JUMP" "$TEST" 'sudo systemctl start k3s-agent' || true
    kc uncordon ipc6 || true
  }
  # Restore even if critest hangs/fails or the driver is interrupted.
  trap restore_agent EXIT INT TERM

  echo "== critest --ginkgo.focus='$FOCUS' on ipc6 (kubelet stopped) =="
  ssh -J "$JUMP" "$TEST" "sudo timeout 200 critest --runtime-endpoint $SOCK --image-endpoint $SOCK \
    --ginkgo.focus='$FOCUS' 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' \
    | grep -E 'Ran [0-9]|Passed|Failed|SUCCESS|FAIL!|\[FAIL\]' | tail -8" || true

  restore_agent
  trap - EXIT INT TERM
fi
