#!/usr/bin/env bash
# Focused critest conformance run for #386 (hostIPC — pod shares the host IPC namespace).
# Usage (on an IPC test node, never omen):
#   sudo bash scripts/cri-conformance-386.sh /tmp/cri-386.result
set -u
SOCK="unix:///run/pelagos/cri.sock"
OUT="${1:-/tmp/cri-386.result}"
FOCUS='HostIpc is true'

# ENV NOTE: this spec creates a host SysV shm segment with `ipcmk` (the creator
# then exits, leaving nattch=0) and expects the container to see it. If the node
# has `kernel.shm_rmid_forced=1`, the kernel force-frees that unattached segment
# instantly and the spec fails for ANY runtime — it is not a pelagos isolation
# bug. Run with the kernel default (0) to exercise the real behavior:
#   sudo sysctl -w kernel.shm_rmid_forced=0   # remember to restore the prior value
RMID_FORCED="$(sysctl -n kernel.shm_rmid_forced 2>/dev/null || echo '?')"
[ "$RMID_FORCED" = "1" ] && echo "WARNING: kernel.shm_rmid_forced=1 will fail this spec environmentally (unattached shm reaped); set it to 0 for a valid run." >&2
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
