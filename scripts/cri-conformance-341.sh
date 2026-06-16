#!/usr/bin/env bash
# Focused critest conformance run for #341 (mount propagation: rshared/rslave/rprivate).
# Usage (on an IPC test node, never omen):
#   sudo bash scripts/cri-conformance-341.sh /tmp/cri-341.result
set -u
SOCK="unix:///run/pelagos/cri.sock"
OUT="${1:-/tmp/cri-341.result}"
FOCUS='Mount Propagation'
: > "$OUT"
echo "# critest focus: $FOCUS" >> "$OUT"
echo "# pelagos-cri: $(systemctl is-active pelagos-cri 2>/dev/null)" >> "$OUT"
echo "# started: $(date -u +%FT%TZ)" >> "$OUT"
echo "----------------------------------------" >> "$OUT"
critest --runtime-endpoint "$SOCK" --ginkgo.focus="$FOCUS" >> "$OUT" 2>&1
rc=$?
echo "----------------------------------------" >> "$OUT"
echo "# critest exit: $rc" >> "$OUT"
exit "$rc"
