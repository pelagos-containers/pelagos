#!/usr/bin/env bash
# Run a FOCUSED subset of critest against pelagos-cri — fast dev loop for one area.
#
#   sudo bash scripts/cri-conformance-focus.sh 'Streaming'                 # exec+attach+portforward
#   sudo bash scripts/cri-conformance-focus.sh 'portforward'              # just portforward
#   sudo bash scripts/cri-conformance-focus.sh 'NamespaceOption'         # the namespace bucket
#   sudo bash scripts/cri-conformance-focus.sh 'SecurityContext' /home/cb/sc.result
#
# Arg 1: a ginkgo focus regex (matched against the full spec name).
# Arg 2: result file (default ./cri-focus.result).
#
# critest is ginkgo-based, so --ginkgo.focus runs ONLY matching specs — turning a
# ~15-min full sweep into a few-second iteration loop. Pair with --ginkgo.dry-run
# (uncomment below) to just LIST which specs a regex would match.
set -u
SOCK="unix:///run/pelagos/cri.sock"
FOCUS="${1:?usage: $0 <ginkgo-focus-regex> [result-file]}"
OUT="${2:-./cri-focus.result}"
BIN_VER="$(/usr/local/bin/pelagos --version 2>/dev/null || echo unknown)"

: > "$OUT"
{
  echo "# critest focus: ${FOCUS}"
  echo "# host: $(hostname)  pelagos: ${BIN_VER}"
  echo "# started: $(date -u +%FT%TZ)"
  echo "----------------------------------------"
} >> "$OUT"

# To list (not run) the matching specs, add: --ginkgo.dry-run
critest --runtime-endpoint "$SOCK" --image-endpoint "$SOCK" \
        --ginkgo.focus="$FOCUS" >> "$OUT" 2>&1
rc=$?

echo "----------------------------------------" >> "$OUT"
echo "# critest exit: ${rc}" >> "$OUT"
exit "$rc"
