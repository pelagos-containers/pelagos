#!/usr/bin/env bash
# E2E: Wasm services in `pelagos compose` (#70).
#
# Before #70 a Wasm image in a compose file was treated as a Linux container and
# silently failed to start (no log, no error). Now it runs through the Wasm runtime
# like `pelagos run` does. This test runs a mixed project: a Linux service writes
# a file into a shared named volume and a Wasm service reads it through a WASI
# preopened directory, with env passed as WASI env. It also checks that options a
# Wasm service cannot honour (ports, memory, networks, ...) are refused with a clear
# error, and that `compose down -v` cleans up.
#
# Not covered, still open: a Wasm service reaching a Linux one by DNS name over a
# bridge network needs a network namespace for Wasm and WASI sockets (#71).
#
# Requirements: root; wasmtime in PATH; rustc with the wasm32-wasip1 target (to build
# the test module); alpine:3.20 pullable.
# Usage: sudo -E env PATH="$HOME/.wasmtime/bin:$PATH" scripts/test-wasm-compose-e2e.sh
set -uo pipefail

PASS=0; FAIL=0; SKIP=0
BINARY="${BINARY:-./target/debug/pelagos}"
CTX="$(cd "$(dirname "$0")" && pwd)/wasm-compose-context"
LAYERS_DIR="/var/lib/pelagos/layers"; IMAGES_DIR="/var/lib/pelagos/images"
PROJECT="wasmcomposee2e"; BADPROJECT="wasmcomposebad"
IMG="pelagos-wasm-compose-e2e"
FAKE_HEX="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
pass() { PASS=$((PASS+1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL: $1"; }
skip() { SKIP=$((SKIP+1)); echo "  SKIP: $1"; }
has_cmd() { command -v "$1" &>/dev/null; }

echo "=== Wasm compose E2E ==="
[ "$(id -u)" -eq 0 ] || { echo "FATAL: must run as root"; exit 1; }
has_cmd wasmtime || { echo "SKIP: wasmtime not found in PATH"; exit 0; }
has_cmd rustc || { echo "SKIP: rustc not found"; exit 0; }
[ -x "$BINARY" ] || { echo "FATAL: $BINARY not found — run 'cargo build' first"; exit 1; }

W="$(mktemp -d)"
cleanup() {
  "$BINARY" compose down -f "$CTX/compose.reml" -p "$PROJECT" -v >/dev/null 2>&1 || true
  "$BINARY" compose down -f "$CTX/bad.reml" -p "$BADPROJECT" -v >/dev/null 2>&1 || true
  rm -rf "$IMAGES_DIR/$IMG" "$LAYERS_DIR/$FAKE_HEX" "$W"
}
trap cleanup EXIT

cat > "$W/reader.rs" <<'RUST'
use std::{env, fs, thread, time::Duration};
fn main() {
    let greeting = env::var("GREETING").unwrap_or_default();
    // Wait for the Linux service to write the shared file.
    let mut msg = String::new();
    for _ in 0..300 {
        if let Ok(m) = fs::read_to_string("/data/message.txt") { msg = m; break; }
        thread::sleep(Duration::from_millis(100));
    }
    println!("wasm-reader: greeting={} message={}", greeting, msg.trim());
    thread::sleep(Duration::from_secs(3600));
}
RUST
rustc --target wasm32-wasip1 --edition 2021 -o "$W/reader.wasm" "$W/reader.rs" 2>"$W/rustc.err" \
  || { cat "$W/rustc.err"; skip "cannot build the wasm32-wasip1 test module"; exit 0; }

# Seed a synthetic Wasm image (one application/wasm layer holding module.wasm).
mkdir -p "$LAYERS_DIR/$FAKE_HEX" "$IMAGES_DIR/$IMG"
cp "$W/reader.wasm" "$LAYERS_DIR/$FAKE_HEX/module.wasm"
cat > "$IMAGES_DIR/$IMG/manifest.json" <<JSON
{"reference": "$IMG", "digest": "sha256:$FAKE_HEX", "layers": ["sha256:$FAKE_HEX"],
 "layer_types": ["application/wasm"],
 "config": {"env": [], "cmd": [], "entrypoint": [], "working_dir": "", "healthcheck": null}}
JSON
"$BINARY" image pull alpine:3.20 >/dev/null 2>&1 || { fail "cannot pull alpine:3.20"; exit 1; }

echo "--- 1. mixed Linux + Wasm project"
"$BINARY" compose up -f "$CTX/compose.reml" -p "$PROJECT" >"$W/up.log" 2>&1 \
  && pass "compose up accepted a project with a Linux and a Wasm service" \
  || { fail "compose up failed: $(tail -3 "$W/up.log" | tr '\n' ' ')"; }

got=""
for _ in $(seq 1 60); do
  got="$("$BINARY" compose logs -f "$CTX/compose.reml" -p "$PROJECT" reader 2>&1 | grep 'wasm-reader:' | head -1)"
  [ -n "$got" ] && break; sleep 1
done
[ -n "$got" ] && pass "the Wasm service started and logged" || fail "the Wasm service never logged (the #70 symptom)"
echo "$got" | grep -q "greeting=hello-from-compose" && pass "service env reached the module as WASI env" \
  || fail "WASI env missing: '$got'"
echo "$got" | grep -q "message=from-linux-service" \
  && pass "the Wasm service read, via a preopened directory, the file the Linux service wrote to the shared volume" \
  || fail "shared-volume data did not reach the Wasm service: '$got'"
ps_out="$("$BINARY" compose ps -f "$CTX/compose.reml" -p "$PROJECT" 2>&1)"
echo "$ps_out" | grep -q "reader" && echo "$ps_out" | grep -q "writer" && pass "compose ps lists both services" \
  || fail "compose ps: $ps_out"

echo "--- 2. unsupported options are refused, not ignored"
"$BINARY" compose up -f "$CTX/bad.reml" -p "$BADPROJECT" >"$W/bad.log" 2>&1; rc=$?
if [ "$rc" -ne 0 ] && grep -q "does not support" "$W/bad.log" && grep -q "ports" "$W/bad.log" && grep -q "memory" "$W/bad.log"; then
  pass "a Wasm service asking for ports and memory is refused, naming both"
else
  fail "bad project not refused clearly (rc=$rc): $(tail -3 "$W/bad.log" | tr '\n' ' ')"
fi

echo "--- 3. compose down"
t0=$SECONDS
"$BINARY" compose down -f "$CTX/compose.reml" -p "$PROJECT" -v >/dev/null 2>&1 \
  && pass "compose down -v succeeds" || fail "compose down failed"
# The Wasm subprocess must be a process group leader so kill(-pid) reaches it: before
# that fix down waited out the supervisor's 60 s timeout and left the runtime behind.
[ $((SECONDS - t0)) -lt 30 ] && pass "compose down returned promptly ($((SECONDS - t0)) s)" \
  || fail "compose down took $((SECONDS - t0)) s (a Wasm service was not stopped by its signal)"
sleep 2
! pgrep -f "wasmtime.*module.wasm" >/dev/null && pass "no wasmtime process left" || fail "wasmtime still running after compose down"

echo "=== $PASS passed, $FAIL failed, $SKIP skipped ==="
[ "$FAIL" = 0 ]
