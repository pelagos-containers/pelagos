#!/usr/bin/env bash
# Surgical reproduction of the PodPID StopPodSandbox teardown hang, driven by
# crictl (NOT the full critest suite — one pod, controlled teardown). Mirrors the
# critest "PodPID" spec: a shared-PID pod sandbox + one nginx container, both with
# namespace pid=POD, then `crictl stopp` (= StopPodSandbox) with a timeout.
#
# pelagos-cri must be the INSTRUMENTED build (logs StopPodSandbox step=... lines).
# Everything is written to persistent disk + the journal so it survives the
# connectivity loss the CNI teardown tends to trigger on this node.
#
#   sudo bash scripts/cri-podpid-teardown-diag.sh 2>&1 | tee /home/cb/podpid-diag.out
set -u
SOCK="unix:///run/pelagos/cri.sock"
CRICTL="crictl --runtime-endpoint $SOCK --image-endpoint $SOCK"
WORK="/home/cb/podpid-diag"
NGINX="registry.k8s.io/e2e-test-images/nginx:1.14-2"
mkdir -p "$WORK"

echo "=== pelagos-cri version + instrumentation check ==="
systemctl is-active pelagos-cri
journalctl -u pelagos-cri -n1 --no-pager -o cat 2>/dev/null | head -1

cat > "$WORK/pod.json" <<'JSON'
{
  "metadata": { "name": "podpid-diag-sb", "namespace": "default", "uid": "podpid-diag-uid", "attempt": 1 },
  "log_directory": "/tmp",
  "linux": {
    "security_context": {
      "namespace_options": { "network": 0, "pid": 0, "ipc": 0 }
    }
  }
}
JSON

cat > "$WORK/ctr.json" <<JSON
{
  "metadata": { "name": "podpid-diag-nginx", "attempt": 1 },
  "image": { "image": "$NGINX" },
  "log_path": "podpid-diag-nginx.log",
  "linux": {
    "security_context": {
      "namespace_options": { "network": 0, "pid": 0, "ipc": 0 }
    }
  }
}
JSON

echo "=== ensure nginx image present ==="
$CRICTL pull "$NGINX" >/dev/null 2>&1 || echo "(pull failed — may already be cached)"

echo "=== RunPodSandbox (pid=POD) ==="
POD=$($CRICTL runp "$WORK/pod.json") || { echo "runp FAILED"; exit 1; }
echo "POD=$POD"

echo "=== CreateContainer + StartContainer (pid=POD) ==="
CTR=$($CRICTL create "$POD" "$WORK/ctr.json" "$WORK/pod.json") || { echo "create FAILED"; $CRICTL stopp "$POD"; $CRICTL rmp "$POD"; exit 1; }
$CRICTL start "$CTR" || echo "start FAILED"
echo "CTR=$CTR"
sleep 1
echo "=== verify shared PID ns: cat /proc/1/cmdline (should be the pause, NOT nginx) ==="
$CRICTL exec "$CTR" cat /proc/1/cmdline 2>&1 | tr '\0' ' '; echo

# Mark a journal cursor so we can dump exactly the StopPodSandbox logs.
CURSOR=$(journalctl -u pelagos-cri -n0 --show-cursor 2>/dev/null | grep -o 'cursor: .*' | sed 's/cursor: //')

echo "=== StopPodSandbox (crictl stopp) — TIMED, this is the suspect ==="
t0=$(date +%s)
timeout 60 $CRICTL stopp "$POD"
RC=$?
t1=$(date +%s)
echo "stopp exit=$RC  elapsed=$((t1-t0))s   (124 == HUNG)"

echo "=== StopPodSandbox step log (last step printed without a matching DONE = the hang) ==="
if [ -n "$CURSOR" ]; then
  journalctl -u pelagos-cri --after-cursor "$CURSOR" --no-pager -o cat 2>/dev/null | grep -E "StopPodSandbox" | tee "$WORK/stop-steps.log"
else
  journalctl -u pelagos-cri -n 60 --no-pager -o cat 2>/dev/null | grep -E "StopPodSandbox" | tee "$WORK/stop-steps.log"
fi

echo "=== cleanup (best effort) ==="
timeout 30 $CRICTL rmp -f "$POD" 2>/dev/null
echo "=== DONE rc=$RC ==="
