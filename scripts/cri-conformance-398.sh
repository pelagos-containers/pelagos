#!/usr/bin/env bash
# Focused critest conformance run for #398 (PodPID — shared pod PID namespace).
#   sudo bash scripts/cri-conformance-398.sh /tmp/cri-398.result
set -u
SOCK="unix:///run/pelagos/cri.sock"
OUT="${1:-/tmp/cri-398.result}"
FOCUS='PodPID'
: > "$OUT"
echo "# critest focus: $FOCUS" >> "$OUT"
echo "# pelagos-cri: $(systemctl is-active pelagos-cri 2>/dev/null)" >> "$OUT"
echo "# pelagos: $(/usr/local/bin/pelagos --version 2>/dev/null)" >> "$OUT"
echo "# started: $(date -u +%FT%TZ)" >> "$OUT"
echo "----------------------------------------" >> "$OUT"
critest --runtime-endpoint "$SOCK" --ginkgo.focus="$FOCUS" >> "$OUT" 2>&1
rc=$?
echo "----------------------------------------" >> "$OUT"
echo "# critest exit: $rc" >> "$OUT"
exit "$rc"
