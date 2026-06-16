#!/usr/bin/env bash
# Run a focused critest reliably on an IPC node that also runs the k3s kubelet.
#
# WHY (#379): critest creates pod sandboxes directly on the pelagos-cri socket.
# On a node where the k3s kubelet shares that same CRI socket, the kubelet's
# pod-sandbox garbage collector sees critest's sandboxes as ORPHANS (they don't
# correspond to any Kubernetes pod) and calls StopPodSandbox on them — killing
# the pause and tripping the phantom-sandbox reaper, which removes the sandbox's
# containers MID-TEST. The result is intermittent `NotFound` failures (most
# visible in short-lived exit-0 specs, e.g. the AppArmor permissive-profile
# tests). This is a TEST-ENVIRONMENT interference, not a runtime bug: with the
# kubelet stopped the same specs pass reliably (verified: AppArmor 9/9, two
# consecutive runs).
#
# This wrapper stops k3s-agent for the duration of the run and ALWAYS restarts
# it (trap on EXIT/INT/TERM). It does NOT defend against SIGKILL — if the script
# is hard-killed, restart k3s-agent manually: `sudo systemctl start k3s-agent`.
#
# Usage (IPC test node, NEVER omen):
#   sudo bash scripts/run-critest.sh '<ginkgo focus regex>' [result-file]
# Example:
#   sudo bash scripts/run-critest.sh 'AppArmor' /tmp/cri-aa.result
set -u

FOCUS="${1:?usage: run-critest.sh <ginkgo-focus-regex> [result-file]}"
OUT="${2:-/tmp/critest.result}"
SOCK="unix:///run/pelagos/cri.sock"

K3S_WAS_ACTIVE="$(systemctl is-active k3s-agent 2>/dev/null || echo inactive)"
restore_kubelet() {
    if [ "$K3S_WAS_ACTIVE" = active ]; then
        echo "# restarting k3s-agent" >&2
        systemctl start k3s-agent 2>/dev/null || true
    fi
}
trap restore_kubelet EXIT INT TERM

if [ "$K3S_WAS_ACTIVE" = active ]; then
    echo "# stopping k3s-agent for the run (will restart on exit)" >&2
    systemctl stop k3s-agent
    sleep 2
fi

: > "$OUT"
echo "# critest focus: $FOCUS" >> "$OUT"
echo "# pelagos-cri: $(systemctl is-active pelagos-cri 2>/dev/null)" >> "$OUT"
echo "# k3s-agent stopped for run: $([ "$K3S_WAS_ACTIVE" = active ] && echo yes || echo 'n/a (was inactive)')" >> "$OUT"
echo "# started: $(date -u +%FT%TZ)" >> "$OUT"
echo "----------------------------------------" >> "$OUT"

critest --runtime-endpoint "$SOCK" --ginkgo.focus="$FOCUS" >> "$OUT" 2>&1
rc=$?

echo "----------------------------------------" >> "$OUT"
echo "# critest exit: $rc" >> "$OUT"
echo "# finished: $(date -u +%FT%TZ)" >> "$OUT"
exit "$rc"
