#!/usr/bin/env bash
# #617 regression: one `kubectl port-forward` must serve many connections.
# Runs a tiny HTTP pod pinned to NODE, forwards a local port, and issues
# several sequential and concurrent requests through the same forward.
#
# Usage: NODE=<node> [KUBECTL="sudo kubectl"] [NS=pf-multi] scripts/test-portforward-multi.sh
# kubectl must reach the cluster from this machine (the forward listens locally).
set -uo pipefail
NODE="${NODE:?set NODE}"; KUBECTL="${KUBECTL:-kubectl}"; NS="${NS:-pf-multi}"
IMAGE="${IMAGE:-docker.io/library/busybox:1.36}"
LPORT="${LPORT:-18931}"
PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL: $1"; }
k() { $KUBECTL "$@"; }
PF=""; RES=$(mktemp)
cleanup() { rm -f "$RES"; [ -n "$PF" ] && kill "$PF" 2>/dev/null; k delete ns "$NS" --wait=false >/dev/null 2>&1 || true; }
trap cleanup EXIT

k create ns "$NS" >/dev/null 2>&1 || true
for _ in $(seq 1 60); do k -n "$NS" get serviceaccount default >/dev/null 2>&1 && break; sleep 1; done
k -n "$NS" apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Pod
metadata: {name: web}
spec:
  nodeName: $NODE
  containers:
  - name: web
    image: $IMAGE
    command: ["sh","-c","mkdir -p /www && echo pf-ok > /www/index.html && exec httpd -f -p 8080 -h /www"]
YAML
k -n "$NS" wait --for=condition=Ready pod/web --timeout=180s >/dev/null 2>&1 || { fail "pod not Ready"; exit 1; }

k -n "$NS" port-forward pod/web "$LPORT:8080" >/dev/null 2>&1 & PF=$!
for _ in $(seq 1 30); do curl -s -m 2 "http://127.0.0.1:$LPORT/" >/dev/null 2>&1 && break; sleep 1; done

ok=0; for i in 1 2 3 4 5 6 7 8; do
  [ "$(curl -s -m 5 "http://127.0.0.1:$LPORT/")" = "pf-ok" ] && ok=$((ok+1)); sleep 0.3; done
[ "$ok" -eq 8 ] && pass "8/8 sequential connections through one port-forward" || fail "only $ok/8 sequential connections worked"

sleep 8   # idle gap: the forward must survive it
[ "$(curl -s -m 5 "http://127.0.0.1:$LPORT/")" = "pf-ok" ] && pass "connection after an idle gap" || fail "no connection after idle gap"

pids=""; for i in $(seq 1 10); do ( [ "$(curl -s -m 8 "http://127.0.0.1:$LPORT/")" = "pf-ok" ] && echo y >>"$RES" ) & pids="$pids $!"; done
wait $pids   # not a bare `wait`: that would also wait for the port-forward itself
ok=$(grep -c y "$RES" 2>/dev/null || true); ok=${ok:-0}
[ "$ok" -eq 10 ] && pass "10/10 concurrent connections" || fail "only $ok/10 concurrent connections worked"
kill -0 "$PF" 2>/dev/null && pass "port-forward process still running" || fail "port-forward process exited"

echo ""; echo "Results: $PASS passed, $FAIL failed"; [ "$FAIL" -eq 0 ]
