#!/usr/bin/env bash
# Like cri-podpid-teardown-diag.sh but CAPTURES LIVE PROCESS STATE during the
# StopPodSandbox teardown — to identify exactly which process wedges into
# uninterruptible (D) state and what it is blocked on (wchan). Writes everything
# to persistent disk so it survives the connectivity wedge.
#
#   sudo bash scripts/cri-podpid-livestate-diag.sh
set -u
SOCK="unix:///run/pelagos/cri.sock"
CRICTL="crictl --runtime-endpoint $SOCK --image-endpoint $SOCK"
WORK="/home/cb/podpid-diag"; mkdir -p "$WORK"
CAP="$WORK/livestate.log"; : > "$CAP"
NGINX="registry.k8s.io/e2e-test-images/nginx:1.14-2"

snap(){ # label -> append a process+net snapshot
  { echo "===== SNAP $1 @ $(date +%T.%N) =====";
    echo "-- D-state (uninterruptible) procs --";
    ps -eo pid,ppid,stat,wchan:40,comm,args | awk 'NR==1 || $3 ~ /D/';
    echo "-- pause / nginx / pelagos procs (stat,wchan) --";
    ps -eo pid,ppid,stat,wchan:40,comm,args | grep -iE 'pause|nginx|pelagos|crictl' | grep -v 'grep\|awk';
    echo "-- conntrack count: $(conntrack -C 2>/dev/null || echo n/a) --";
  } >> "$CAP" 2>&1; }

cat > "$WORK/pod.json" <<'JSON'
{ "metadata": { "name": "podpid-live-sb", "namespace": "default", "uid": "podpid-live-uid", "attempt": 1 },
  "log_directory": "/tmp",
  "linux": { "security_context": { "namespace_options": { "network": 0, "pid": 0, "ipc": 0 } } } }
JSON
cat > "$WORK/ctr.json" <<JSON
{ "metadata": { "name": "podpid-live-nginx", "attempt": 1 },
  "image": { "image": "$NGINX" }, "log_path": "podpid-live-nginx.log",
  "linux": { "security_context": { "namespace_options": { "network": 0, "pid": 0, "ipc": 0 } } } }
JSON

$CRICTL pull "$NGINX" >/dev/null 2>&1 || true
POD=$($CRICTL runp "$WORK/pod.json") || { echo "runp FAILED" >>"$CAP"; exit 1; }
CTR=$($CRICTL create "$POD" "$WORK/ctr.json" "$WORK/pod.json") && $CRICTL start "$CTR"
echo "POD=$POD CTR=$CTR" >> "$CAP"
sleep 1
snap "running"
echo "proc1: $($CRICTL exec "$CTR" cat /proc/1/cmdline 2>&1 | tr '\0' ' ')" >> "$CAP"

# Launch the teardown in the background, then snapshot rapidly while it runs.
( timeout 120 $CRICTL stopp "$POD" > "$WORK/stopp.out" 2>&1; echo "stopp rc=$? @ $(date +%T)" >> "$WORK/stopp.out" ) &
STOPP=$!
for i in $(seq 1 20); do snap "teardown+${i}"; sleep 1; done
wait $STOPP 2>/dev/null

{ echo "===== StopPodSandbox step journal =====";
  journalctl -u pelagos-cri -b0 --no-pager -o cat 2>/dev/null | grep StopPodSandbox | tail -16;
  echo "===== stopp.out ====="; cat "$WORK/stopp.out" 2>/dev/null; } >> "$CAP"
echo "DONE — see $CAP" >> "$CAP"
timeout 30 $CRICTL rmp -f "$POD" >/dev/null 2>&1 || true
