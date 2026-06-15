#!/usr/bin/env bash
# Focused critest conformance run for the Phase-1 CRI fixes (#354, #355).
#
# Runs ONLY the specs targeted by the hostname / host-port / DNS / sysctls
# wire-ups against the live pelagos-cri socket, and writes a result file the
# caller can read separately (critest is long-running; do not pipe it to tail).
#
# Usage (on an IPC test node, never omen):
#   sudo bash scripts/cri-conformance-354-355.sh /tmp/cri-354-355.result
set -u

SOCK="unix:///run/pelagos/cri.sock"
OUT="${1:-/tmp/cri-354-355.result}"

# The four Phase-1 specs (Ginkgo regex). Keep in sync with #354/#355.
FOCUS='should support set hostname|port mapping with host port and container port|should support DNS config|should support (safe|unsafe) sysctls|runtime should support sysctls'

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
