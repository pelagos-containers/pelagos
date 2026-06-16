#!/usr/bin/env bash
# Focused critest conformance run for the #343 OOMKilled reporting fix.
#
# Runs ONLY the "Container OOM" spec against the live pelagos-cri socket and
# writes a result file the caller can read separately (critest is long-running;
# do not pipe it to tail).
#
# Verifies an OOM-killed container reports `reason: OOMKilled` and exit code 137
# in ContainerStatus.
#
# Usage (on an IPC test node, never omen):
#   sudo bash scripts/cri-conformance-343.sh /tmp/cri-343.result
set -u

SOCK="unix:///run/pelagos/cri.sock"
OUT="${1:-/tmp/cri-343.result}"

# Ginkgo regex for the OOM spec(s).
FOCUS='OOM|OOMKilled'

: > "$OUT"
echo "# critest focus: $FOCUS" >> "$OUT"
echo "# endpoint: $SOCK" >> "$OUT"
echo "# pelagos-cri: $(systemctl is-active pelagos-cri 2>/dev/null)" >> "$OUT"
echo "# started: $(date -u +%FT%TZ)" >> "$OUT"
echo "----------------------------------------" >> "$OUT"

critest --runtime-endpoint "$SOCK" --ginkgo.focus="$FOCUS" >> "$OUT" 2>&1
rc=$?

echo "----------------------------------------" >> "$OUT"
echo "# critest exit: $rc" >> "$OUT"
echo "# finished: $(date -u +%FT%TZ)" >> "$OUT"
exit "$rc"
