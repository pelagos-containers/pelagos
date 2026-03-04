#!/usr/bin/env bash
# E2E tests for the embedded-wasm feature (in-process wasmtime execution).
#
# Exercises the full pipeline:
#   1. Build a Rust Wasm module from source
#   2. `pelagos build` a Wasm image from a Remfile (FROM scratch + COPY)
#   3. `pelagos run` the image with wasmtime/wasmedge removed from PATH
#      → proves the embedded in-process path executes the module
#
# Tests:
#   - Basic stdout ("hello embedded wasm")
#   - Env passthrough (--env EMBED_VAR)
#   - Preopened directory access (--bind host:/data)
#   - Verifies execution works WITHOUT any external Wasm runtime in PATH
#
# Requirements:
#   - Must run as root (layer store write access)
#   - rustc with wasm32-wasip1 target must be available
#   - pelagos compiled with --features embedded-wasm (done automatically)
#
# Usage:
#   sudo -E scripts/test-wasm-embedded-e2e.sh
set -uo pipefail

PASS=0
FAIL=0
SKIP=0
BINARY="${BINARY:-./target/debug/pelagos}"
CONTEXT_DIR="scripts/wasm-embedded-context"
TEST_IMAGE_REF="pelagos-embedded-wasm-e2e:latest"

# ── Helpers ──────────────────────────────────────────────────────────────────

pass() { PASS=$((PASS+1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL: $1"; }
skip() { SKIP=$((SKIP+1)); echo "  SKIP: $1"; }

has_cmd() { command -v "$1" &>/dev/null; }

check_contains() {
    local output="$1" expected="$2" label="$3"
    if echo "$output" | grep -qF "$expected"; then
        pass "$label"
    else
        fail "$label — expected '$expected' in output"
        echo "    actual output: $output"
    fi
}

check_not_contains() {
    local output="$1" unwanted="$2" label="$3"
    if echo "$output" | grep -qF "$unwanted"; then
        fail "$label — found unwanted '$unwanted' in output"
        echo "    actual output: $output"
    else
        pass "$label"
    fi
}

# ── Pre-flight checks ─────────────────────────────────────────────────────────

echo "=== Wasm Embedded E2E Tests ==="
echo ""

if [ "$(id -u)" -ne 0 ]; then
    echo "FATAL: must run as root (sudo -E scripts/test-wasm-embedded-e2e.sh)"
    exit 1
fi

if ! [ -d "$CONTEXT_DIR" ]; then
    echo "FATAL: build context not found at $CONTEXT_DIR"
    exit 1
fi

if ! has_cmd rustc; then
    skip "rustc not found — cannot build Wasm test module"
    echo ""
    echo "Results: $PASS passed, $FAIL failed, $SKIP skipped"
    exit 0
fi

# Ensure wasm32-wasip1 target is available.
if ! rustc --print target-list 2>/dev/null | grep -q "wasm32-wasip1"; then
    if has_cmd rustup; then
        echo "  Installing wasm32-wasip1 target via rustup..."
        rustup target add wasm32-wasip1 2>/dev/null || true
    fi
fi

if ! rustc --print target-list 2>/dev/null | grep -q "wasm32-wasip1"; then
    skip "wasm32-wasip1 target not available — run: rustup target add wasm32-wasip1"
    echo ""
    echo "Results: $PASS passed, $FAIL failed, $SKIP skipped"
    exit 0
fi

# ── Build pelagos with embedded-wasm feature ─────────────────────────────────

echo "--- Building pelagos with --features embedded-wasm ---"
if ! cargo build --features embedded-wasm 2>/tmp/pelagos-build.err; then
    echo "FATAL: cargo build --features embedded-wasm failed:"
    cat /tmp/pelagos-build.err
    exit 1
fi
echo "  pelagos: $BINARY"

if ! [ -x "$BINARY" ]; then
    echo "FATAL: pelagos binary not found at $BINARY after build"
    exit 1
fi

# ── Compile the test Wasm module ─────────────────────────────────────────────

WORK_DIR=$(mktemp -d)
trap 'cleanup_all' EXIT

cleanup_all() {
    # Remove the test image from the pelagos store (best-effort).
    "$BINARY" image rm "$TEST_IMAGE_REF" 2>/dev/null || true
    rm -rf "$WORK_DIR"
    rm -f /tmp/pelagos-build.err
}

# Copy the build context to a temp dir so we can add hello.wasm without
# modifying the checked-in scripts/wasm-embedded-context directory.
TEMP_CTX="${WORK_DIR}/context"
cp -r "$CONTEXT_DIR" "$TEMP_CTX"

echo ""
echo "--- Compiling hello.rs → wasm32-wasip1 ---"

WASM_SRC="${TEMP_CTX}/hello.rs"
WASM_BIN="${TEMP_CTX}/hello.wasm"

if ! rustc --target wasm32-wasip1 --edition 2021 \
        -o "$WASM_BIN" "$WASM_SRC" 2>"${WORK_DIR}/rustc.err"; then
    echo "  SKIP: failed to compile wasm32-wasip1 module:"
    cat "${WORK_DIR}/rustc.err"
    skip "wasm32-wasip1 compilation failed"
    echo ""
    echo "Results: $PASS passed, $FAIL failed, $SKIP skipped"
    exit 0
fi

echo "  hello.wasm: $(wc -c < "$WASM_BIN") bytes"

# ── Build the Wasm image via pelagos build + Remfile ─────────────────────────

echo ""
echo "--- Building Wasm image with pelagos build (Remfile) ---"
echo "  Remfile:"
sed 's/^/    /' "${TEMP_CTX}/Remfile"
echo ""

BUILD_OUT=$("$BINARY" build -t "$TEST_IMAGE_REF" "$TEMP_CTX" 2>&1)
BUILD_RC=$?

if [ "$BUILD_RC" -ne 0 ]; then
    fail "pelagos build exited with code $BUILD_RC"
    echo "    build output: $BUILD_OUT"
    echo ""
    echo "Results: $PASS passed, $FAIL failed, $SKIP skipped"
    exit 1
fi

pass "pelagos build succeeded"

# Verify the image was registered and has the wasm type.
LS_OUT=$("$BINARY" image ls 2>&1)
check_contains "$LS_OUT" "wasm" "image ls shows TYPE=wasm for built image"
check_contains "$LS_OUT" "pelagos-embedded-wasm-e2e" "image ls lists the test image"

# ── Strip Wasm runtimes from PATH to force the embedded path ─────────────────
#
# Build a clean PATH with wasmtime and wasmedge directories removed.
# This proves the embedded in-process path is used, not a subprocess runtime.

ORIG_PATH="$PATH"
CLEAN_PATH=""
IFS=: read -ra DIRS <<< "$PATH"
for d in "${DIRS[@]}"; do
    if [ -x "${d}/wasmtime" ] || [ -x "${d}/wasmedge" ]; then
        echo "  Removing $d from PATH (contains wasmtime/wasmedge)"
    else
        CLEAN_PATH="${CLEAN_PATH:+${CLEAN_PATH}:}${d}"
    fi
done
export PATH="$CLEAN_PATH"

if has_cmd wasmtime || has_cmd wasmedge; then
    echo "  WARNING: wasmtime or wasmedge still in PATH; test may use subprocess path"
else
    echo "  PATH: wasmtime/wasmedge removed — embedded path will be used"
fi

# ── Tests ─────────────────────────────────────────────────────────────────────

echo ""
echo "--- 1. pelagos run — basic output (no external runtime in PATH) ---"

RUN_OUT=$("$BINARY" run "$TEST_IMAGE_REF" 2>&1)
check_contains "$RUN_OUT" "hello embedded wasm" "run: Wasm module prints 'hello embedded wasm'"
check_not_contains "$RUN_OUT" "no Wasm runtime found" "run: no 'runtime not found' error"
check_not_contains "$RUN_OUT" "error" "run: no error message"

echo ""
echo "--- 2. pelagos run — env passthrough (--env) ---"

ENV_OUT=$("$BINARY" run \
    --env EMBED_VAR=hello42 \
    "$TEST_IMAGE_REF" 2>&1)
check_contains "$ENV_OUT" "env:EMBED_VAR=hello42" "run: --env value reaches the Wasm module"

echo ""
echo "--- 3. pelagos run — preopened directory (--bind) ---"

BIND_DIR="${WORK_DIR}/binddata"
mkdir -p "$BIND_DIR"
echo "embed test" > "${BIND_DIR}/test.txt"

BIND_OUT=$("$BINARY" run \
    --bind "${BIND_DIR}:/data" \
    "$TEST_IMAGE_REF" 2>&1)
check_contains "$BIND_OUT" "file:embed test" "run: --bind dir visible as /data inside Wasm"

# ── Summary ───────────────────────────────────────────────────────────────────

export PATH="$ORIG_PATH"

echo ""
echo "Results: $PASS passed, $FAIL failed, $SKIP skipped"
echo ""

[ "$FAIL" -eq 0 ]
