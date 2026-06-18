#!/usr/bin/env bash
# Probe for #402: does `pelagos exec -i` (tty) exit when the container command
# exits while stdin is held open by an upstream writer?
#
# IMPORTANT measurement note: do NOT write `sleep 5 | pelagos exec ...` and time
# the pipeline — the shell waits for BOTH pipe sides, so `sleep 5` dominates the
# wall time regardless of when pelagos exits. That artifact is what produced the
# bogus "exec blocks on stdin" root-cause originally filed on #402. To measure
# pelagos's OWN lifetime, hold stdin open with process substitution `< <(sleep N)`
# so `time`/elapsed wraps only the pelagos process.
#
# Finding (2026-06-17): pelagos exec -i returns in ~0.02s on command-exit even
# with stdin held open — it is NOT the cause of the critest exec tty+stdin
# timeout. See issue #402 for the corrected analysis.
set -u
PELAGOS="${PELAGOS:-./target/release/pelagos}"
ROOTFS="${ROOTFS:-alpine-rootfs}"
NAME=ttytest402

cleanup() { sudo "$PELAGOS" rm -f "$NAME" >/dev/null 2>&1; }
trap cleanup EXIT
cleanup

echo "== starting container $NAME =="
sudo "$PELAGOS" run --detach --name "$NAME" --rootfs "$ROOTFS" -- /bin/sleep 600
sleep 1
sudo "$PELAGOS" ps

echo
echo "== exec -i, stdin held OPEN by 'sleep 5' via process subst (measures pelagos only) =="
echo "   (correct behavior: ~0.0s — pelagos exits when echo exits, not on stdin EOF)"
start=$(date +%s.%N)
out=$(sudo "$PELAGOS" exec -i "$NAME" -- /bin/echo -n hello < <(sleep 5))
end=$(date +%s.%N)
elapsed=$(echo "$end - $start" | bc)
printf 'output=%q elapsed=%.2fs\n' "$out" "$elapsed"

echo
echo "== control: exec -i with stdin from /dev/null (should be fast) =="
start=$(date +%s.%N)
out=$(sudo "$PELAGOS" exec -i "$NAME" -- /bin/echo -n hello < /dev/null)
end=$(date +%s.%N)
elapsed=$(echo "$end - $start" | bc)
printf 'output=%q elapsed=%.2fs\n' "$out" "$elapsed"

echo
echo "== ANTI-PATTERN (artifact): 'sleep 5 | exec' times the PIPELINE, not pelagos =="
echo "   (will read ~5s even though pelagos itself exits immediately — do not trust this)"
start=$(date +%s.%N)
out=$(sleep 5 | sudo "$PELAGOS" exec -i "$NAME" -- /bin/echo -n hello)
end=$(date +%s.%N)
elapsed=$(echo "$end - $start" | bc)
printf 'output=%q elapsed=%.2fs (artifact: dominated by sleep 5)\n' "$out" "$elapsed"
