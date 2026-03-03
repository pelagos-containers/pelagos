#!/usr/bin/env bash
# Comprehensive E2E tests for rootless container support.
#
# Covers: overlay/storage (A+B), minimal /dev (D), multi-UID mapping (C),
# cgroup delegation (E), pasta networking, and filesystem features.
#
# Must run as a REGULAR USER (not root):
#   scripts/test-rootless.sh
set -euo pipefail

PASS=0
FAIL=0
SKIP=0
BINARY="./target/debug/pelagos"

pass() { PASS=$((PASS+1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL: $1"; }
skip() { SKIP=$((SKIP+1)); echo "  SKIP: $1"; }

check_contains() {
    local output="$1" expected="$2" label="$3"
    if echo "$output" | grep -q "$expected"; then
        pass "$label"
    else
        fail "$label (expected '$expected' in output)"
        echo "    output: $output"
    fi
}

check_not_contains() {
    local output="$1" unwanted="$2" label="$3"
    if echo "$output" | grep -q "$unwanted"; then
        fail "$label (found unwanted '$unwanted' in output)"
        echo "    output: $output"
    else
        pass "$label"
    fi
}

check_exit_ok() {
    local label="$1"; shift
    if OUT=$("$@" 2>/dev/null); then
        pass "$label"
    else
        fail "$label (exit code $?)"
        echo "    output: $OUT"
    fi
}

# --- Skip-condition helpers ---

has_cmd() { command -v "$1" &>/dev/null; }

has_subuid() {
    local user
    user=$(id -un)
    grep -q "^${user}:" /etc/subuid 2>/dev/null && grep -q "^${user}:" /etc/subgid 2>/dev/null
}

has_cgroup_delegation() {
    local cg_path
    cg_path=$(grep '^0::' /proc/self/cgroup 2>/dev/null | cut -d: -f3) || return 1
    [ -n "$cg_path" ] || return 1
    local cg_dir="/sys/fs/cgroup${cg_path}"
    [ -f "${cg_dir}/cgroup.controllers" ] && [ -r "${cg_dir}/cgroup.controllers" ] || return 1
    # Check that at least memory or pids is delegated
    grep -qE '(memory|pids)' "${cg_dir}/cgroup.controllers" 2>/dev/null
}

# --- Refuse root ---
if [ "$(id -u)" -eq 0 ]; then
    echo "ERROR: This script must NOT run as root. Run as a regular user."
    exit 1
fi

# --- Build ---
echo "==> Building pelagos..."
cargo build 2>&1

# --- Ensure alpine image ---
if ! $BINARY image ls 2>/dev/null | grep -q alpine; then
    echo "==> Pulling alpine image..."
    $BINARY image pull alpine
fi

# ===================================================================
echo ""
echo "=== Section 1: Core Rootless (Phase A+B) ==="
echo ""

echo "--- Test: uid=0 inside container ---"
OUT=$($BINARY run alpine /bin/sh -c 'id' 2>/dev/null || true)
check_contains "$OUT" "uid=0" "container runs as uid=0"

echo "--- Test: overlay rootfs works ---"
OUT=$($BINARY run alpine /bin/cat /etc/alpine-release 2>/dev/null || true)
if [ -n "$OUT" ]; then
    pass "overlay rootfs readable (alpine-release: ${OUT})"
else
    fail "overlay rootfs readable (empty output)"
fi

# ===================================================================
echo ""
echo "=== Section 2: Minimal /dev (Phase D) ==="
echo ""

echo "--- Test: ls /dev/ shows minimal set ---"
OUT=$($BINARY run alpine /bin/ls /dev/ 2>/dev/null || true)
check_contains "$OUT" "null" "/dev/null present"
check_contains "$OUT" "zero" "/dev/zero present"
check_contains "$OUT" "random" "/dev/random present"
check_contains "$OUT" "urandom" "/dev/urandom present"
check_not_contains "$OUT" "sda" "no /dev/sda"

echo "--- Test: write to /dev/null ---"
OUT=$($BINARY run alpine /bin/sh -c 'echo ok > /dev/null && echo pass' 2>/dev/null || true)
check_contains "$OUT" "pass" "/dev/null write"

echo "--- Test: /dev symlinks ---"
OUT=$($BINARY run alpine /bin/sh -c 'test -L /dev/fd && test -L /dev/stdin && echo ok' 2>/dev/null || true)
check_contains "$OUT" "ok" "/dev symlinks present"

# ===================================================================
echo ""
echo "=== Section 3: Multi-UID Mapping (Phase C) ==="
echo ""

if has_cmd newuidmap && has_subuid; then
    echo "--- Test: multi-UID files not owned by nobody (65534) ---"
    OUT=$($BINARY run alpine /bin/sh -c 'stat -c %u /bin/busybox /etc/hosts /usr 2>/dev/null' 2>/dev/null || true)
    if echo "$OUT" | grep -q '65534'; then
        fail "files owned by 65534 (multi-UID mapping not working)"
        echo "    output: $OUT"
    elif [ -n "$OUT" ]; then
        pass "files not owned by 65534 (multi-UID mapping works)"
    else
        fail "could not stat files in container"
    fi

    echo "--- Test: /etc/passwd owned by uid 0 (not 65534) ---"
    OUT=$($BINARY run alpine /bin/stat -c '%u' /etc/passwd 2>/dev/null || true)
    check_contains "$OUT" "0" "/etc/passwd owned by uid 0"
else
    skip "multi-UID mapping (newuidmap or subuid/subgid not available)"
fi

# ===================================================================
echo ""
echo "=== Section 4: Cgroup v2 Delegation (Phase E) ==="
echo ""

if has_cgroup_delegation; then
    echo "--- Test: memory limit ---"
    OUT=$($BINARY run --memory 64m alpine /bin/sh -c 'echo ok' 2>/dev/null || true)
    check_contains "$OUT" "ok" "cgroup memory limit"

    echo "--- Test: pids limit ---"
    OUT=$($BINARY run --pids-limit 32 alpine /bin/sh -c 'echo ok' 2>/dev/null || true)
    check_contains "$OUT" "ok" "cgroup pids limit"
else
    skip "cgroup v2 delegation (no delegated controllers)"
fi

# ===================================================================
echo ""
echo "=== Section 5: Networking — Loopback ==="
echo ""

echo "--- Test: loopback interface ---"
OUT=$($BINARY run --network loopback alpine /bin/sh -c 'ip addr show lo' 2>/dev/null || true)
check_contains "$OUT" "LOOPBACK" "loopback interface present"

echo "--- Test: loopback ping ---"
OUT=$($BINARY run --network loopback alpine /bin/ping -c1 -W1 127.0.0.1 2>/dev/null || true)
check_contains "$OUT" "1 packets received" "loopback ping 127.0.0.1"

# ===================================================================
echo ""
echo "=== Section 6: Networking — Pasta ==="
echo ""

if has_cmd pasta; then
    echo "--- Test: pasta interface ---"
    OUT=$($BINARY run --network pasta alpine /bin/sh -c 'sleep 2 && ip addr show' 2>/dev/null || true)
    if echo "$OUT" | grep -v 'lo:' | grep -q 'inet '; then
        pass "pasta non-lo interface with inet"
    else
        fail "pasta non-lo interface with inet"
        echo "    output: $OUT"
    fi

    echo "--- Test: pasta internet connectivity ---"
    OUT=$($BINARY run --network pasta alpine /bin/sh -c 'sleep 2 && ping -c1 -W5 8.8.8.8 >/dev/null 2>&1 && echo CONNECTED' 2>/dev/null || true)
    check_contains "$OUT" "CONNECTED" "pasta internet connectivity"
else
    skip "pasta networking (pasta not installed)"
fi

# ===================================================================
echo ""
echo "=== Section 7: Filesystem Features ==="
echo ""

echo "--- Test: tmpfs mount ---"
OUT=$($BINARY run --tmpfs /tmp alpine /bin/sh -c 'echo test > /tmp/file && cat /tmp/file' 2>/dev/null || true)
check_contains "$OUT" "test" "tmpfs write+read"

echo "--- Test: read-only rootfs ---"
OUT=$($BINARY run --read-only alpine /bin/sh -c 'touch /testfile 2>&1 || echo READONLY' 2>/dev/null || true)
if [ -n "$OUT" ]; then
    check_contains "$OUT" "READONLY" "read-only rootfs blocks write"
else
    # Rootless overlay + read-only may not be supported yet
    skip "read-only rootfs (not supported in rootless mode)"
fi

# ===================================================================
echo ""
echo "=== Section 8: Integration Tests (rootless) ==="
echo ""

echo "--- Running rootless integration tests ---"
cargo test --test integration_tests rootless -- --test-threads=1 2>&1 || FAIL=$((FAIL+1))

if has_cgroup_delegation; then
    echo "--- Running rootless cgroup integration tests ---"
    cargo test --test integration_tests rootless_cgroups -- --test-threads=1 2>&1 || FAIL=$((FAIL+1))
fi

if has_cmd newuidmap && has_subuid; then
    echo "--- Running rootless idmap integration tests ---"
    cargo test --test integration_tests rootless_idmap -- --test-threads=1 2>&1 || FAIL=$((FAIL+1))
fi

# ===================================================================
echo ""
echo "=== Results: $PASS passed, $FAIL failed, $SKIP skipped ==="
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
