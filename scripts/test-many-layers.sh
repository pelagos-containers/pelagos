#!/usr/bin/env bash
# #612 regression: an image with many layers must build and run. The kernel
# copies an overlay mount's option string in at most one page (4096 bytes) and
# silently truncates the rest, so a long `lowerdir=` list (46+ layers of
# /var/lib/pelagos/layers/<64 hex> paths) is cut mid-path and the mount fails
# with ENOENT ("mount overlay in host namespace: No such file or directory").
#
# Builds FROM $BASE with $STEPS `RUN` steps (one layer each), runs the result,
# and checks that every layer's marker file is visible in the merged rootfs.
#
# Needs root (overlay + bridge-less run). Usage:
#   sudo PELAGOS=target/debug/pelagos [BASE=alpine:3.20] [STEPS=130] scripts/test-many-layers.sh
set -uo pipefail
PELAGOS="${PELAGOS:-pelagos}"; BASE="${BASE:-alpine:3.20}"; STEPS="${STEPS:-130}"
TAG="manylayers-test:1"
W="$(mktemp -d)"; trap 'rm -rf "$W"; $PELAGOS image rm "$TAG" >/dev/null 2>&1 || true' EXIT
PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL: $1"; }

echo "=== many-layer image: $STEPS RUN steps on $BASE ==="
$PELAGOS image pull "$BASE" >/dev/null 2>&1 || { fail "cannot pull $BASE"; exit 1; }
{ echo "FROM $BASE"; for i in $(seq 1 "$STEPS"); do echo "RUN echo $i > /m$i"; done
  echo 'CMD ["/bin/sh","-c","ls / | grep -c ^m[0-9]"]'; } > "$W/Remfile"

if $PELAGOS build -t "$TAG" --file "$W/Remfile" "$W" >"$W/build.log" 2>&1; then
  pass "build with $STEPS RUN steps ($((STEPS + 1)) layers)"
else
  fail "build failed: $(grep -E 'error|ENOENT|No such' "$W/build.log" | tail -2 | tr '\n' ' ')"
fi

out="$($PELAGOS run "$TAG" 2>&1 | tail -1)"
if [ "$out" = "$STEPS" ]; then pass "run sees all $STEPS marker files in the merged rootfs"
else fail "run: expected $STEPS marker files, got '$out'"; fi

echo "=== $PASS passed, $FAIL failed ==="
[ "$FAIL" = 0 ]
