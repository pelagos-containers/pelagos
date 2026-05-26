#!/usr/bin/env bash
# End-to-end test script for the pelagos CRI gRPC server.
#
# Prerequisites:
#   - Run as root (pelagos networking requires root)
#   - pelagos and pelagos-cri built (cargo build)
#   - crictl installed (https://github.com/kubernetes-sigs/cri-tools)
#     Arch: sudo pacman -S cri-tools
#     Ubuntu: sudo apt-get install cri-tools
#   - alpine image pulled: pelagos image pull alpine
#
# Usage:
#   sudo -E env "PATH=$PATH" bash scripts/test-cri.sh
#   BINARY=/usr/local/bin/pelagos CRI_BINARY=/usr/local/bin/pelagos-cri sudo bash scripts/test-cri.sh

set -uo pipefail

BINARY="${BINARY:-./target/debug/pelagos}"
CRI_BINARY="${CRI_BINARY:-./target/debug/pelagos-cri}"
CRI_SOCK="/run/pelagos/cri.sock"
CRICTL="crictl --runtime-endpoint unix://${CRI_SOCK}"
CRI_PID_FILE="/tmp/pelagos-cri-test.pid"

# ── Helpers ──────────────────────────────────────────────────────────────────

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

PASS=0
FAIL=0

pass() { echo -e "  ${GREEN}PASS${NC}  $1"; PASS=$((PASS + 1)); }
fail() { echo -e "  ${RED}FAIL${NC}  $1"; FAIL=$((FAIL + 1)); }
step() { echo -e "\n${BOLD}${YELLOW}=== $1 ===${NC}"; }
info() { echo "  $1"; }

check_exit_ok() {
    local desc="$1"; shift
    if "$@" > /dev/null 2>&1; then
        pass "$desc"
    else
        fail "$desc"
    fi
}

check_contains() {
    local desc="$1"
    local output="$2"
    local pattern="$3"
    if echo "$output" | grep -q "$pattern"; then
        pass "$desc"
    else
        fail "$desc (expected '$pattern' in output)"
        info "  got: $(echo "$output" | head -3)"
    fi
}

CRICTL_YAML_WRITTEN=0

cleanup() {
    if [ -f "$CRI_PID_FILE" ]; then
        kill "$(cat $CRI_PID_FILE)" 2>/dev/null || true
        rm -f "$CRI_PID_FILE"
    fi
    rm -f "$CRI_SOCK"
    rm -rf /run/pelagos-cri
    if [ "$CRICTL_YAML_WRITTEN" = "1" ]; then
        rm -f /etc/crictl.yaml
    fi
}
trap cleanup EXIT

# Helper: run crictl and return only the last non-empty, non-warning line (for ID capture)
crictl_id() {
    $CRICTL "$@" 2>/dev/null | grep -v "^time=" | grep -v "^$" | tail -1
}

# ── Preflight ─────────────────────────────────────────────────────────────────

step "Preflight checks"

if [ "$(id -u)" != "0" ]; then
    echo "Must run as root"
    exit 1
fi

for bin in "$BINARY" "$CRI_BINARY"; do
    if [ ! -x "$bin" ]; then
        echo "Missing binary: $bin — run 'cargo build' first"
        exit 1
    fi
done

if ! command -v crictl > /dev/null 2>&1; then
    echo "crictl not found — install cri-tools (pacman -S cri-tools or apt-get install cri-tools)"
    exit 1
fi

pass "binaries present"
pass "crictl found"

# Write crictl config so it knows our socket and suppresses "config not found" warnings.
if [ ! -f /etc/crictl.yaml ]; then
    cat > /etc/crictl.yaml <<EOF
runtime-endpoint: unix://${CRI_SOCK}
image-endpoint: unix://${CRI_SOCK}
timeout: 10
debug: false
EOF
    CRICTL_YAML_WRITTEN=1
fi
pass "crictl config written"

# ── Start pelagos-cri ─────────────────────────────────────────────────────────

step "Start pelagos-cri"

cleanup  # clean up any stale socket

"$CRI_BINARY" --pelagos-bin "$BINARY" > /tmp/pelagos-cri.log 2>&1 &
echo $! > "$CRI_PID_FILE"

# Wait for socket to appear (up to 5 seconds)
for i in $(seq 1 10); do
    if [ -S "$CRI_SOCK" ]; then break; fi
    sleep 0.5
done

if [ -S "$CRI_SOCK" ]; then
    pass "pelagos-cri socket present"
else
    fail "pelagos-cri socket not present after 5s"
    info "--- pelagos-cri log ---"
    cat /tmp/pelagos-cri.log
    exit 1
fi

# ── Version / Status ──────────────────────────────────────────────────────────

step "C2: Version and Status"

OUT=$($CRICTL version 2>&1)
check_contains "crictl version: runtimeName=pelagos" "$OUT" "pelagos"

OUT=$($CRICTL info 2>&1)
check_contains "crictl info: RuntimeReady=true" "$OUT" "RuntimeReady"

# ── Image operations ──────────────────────────────────────────────────────────

step "C3: ImageService"

# Pull alpine (may already be cached)
if $CRICTL pull alpine > /tmp/crictl-pull.log 2>&1; then
    pass "crictl pull alpine"
else
    fail "crictl pull alpine"
    cat /tmp/crictl-pull.log
fi

OUT=$($CRICTL images 2>&1)
check_contains "crictl images lists alpine" "$OUT" "alpine"

# ImageStatus
OUT=$($CRICTL img alpine 2>&1)
check_contains "crictl img alpine: has tag" "$OUT" "alpine"

# ── Pod sandbox ───────────────────────────────────────────────────────────────

step "C4: Pod sandbox"

cat > /tmp/test-pod.json <<'EOF'
{
  "metadata": {
    "name": "test-pod",
    "namespace": "default",
    "uid": "test-uid-12345",
    "attempt": 0
  },
  "hostname": "test-pod",
  "log_directory": "/tmp/test-pod-logs",
  "dns_config": {},
  "port_mappings": [],
  "labels": {"test": "cri"},
  "annotations": {},
  "linux": {}
}
EOF

SANDBOX_ID=$(crictl_id runp /tmp/test-pod.json)
if [ -n "$SANDBOX_ID" ]; then
    pass "crictl runp created sandbox"
    info "sandbox ID: $SANDBOX_ID"
else
    fail "crictl runp returned empty ID"
    exit 1
fi

OUT=$($CRICTL pods 2>&1)
check_contains "crictl pods lists sandbox" "$OUT" "test-pod"

OUT=$($CRICTL inspectp "$SANDBOX_ID" 2>&1)
check_contains "crictl inspectp: state=SANDBOX_READY" "$OUT" "SANDBOX_READY"

# ── Container lifecycle ───────────────────────────────────────────────────────

step "C6: Container lifecycle"

cat > /tmp/test-container.json <<'EOF'
{
  "metadata": {
    "name": "test-container",
    "attempt": 0
  },
  "image": {"image": "alpine:latest"},
  "command": ["/bin/sh"],
  "args": ["-c", "echo hello-from-cri; sleep 30"],
  "working_dir": "/",
  "envs": [{"key": "TEST_VAR", "value": "hello"}],
  "mounts": [],
  "labels": {"test": "cri"},
  "annotations": {},
  "log_path": "test-container.log",
  "linux": {}
}
EOF

CONTAINER_ID=$(crictl_id create "$SANDBOX_ID" /tmp/test-container.json /tmp/test-pod.json)
if [ -n "$CONTAINER_ID" ]; then
    pass "crictl create returned container ID"
    info "container ID: $CONTAINER_ID"
else
    fail "crictl create returned empty ID"
    exit 1
fi

OUT=$($CRICTL ps -a 2>&1)
check_contains "crictl ps -a shows created container" "$OUT" "test-container"

check_exit_ok "crictl start container" $CRICTL start "$CONTAINER_ID"

# Give the container a moment to be running
sleep 1

OUT=$($CRICTL ps 2>&1)
check_contains "crictl ps shows running container" "$OUT" "test-container"

# ── ExecSync ──────────────────────────────────────────────────────────────────

step "C6: ExecSync"

OUT=$($CRICTL exec --sync "$CONTAINER_ID" echo hello-exec 2>&1)
check_contains "crictl exec --sync echo" "$OUT" "hello-exec"

OUT=$($CRICTL exec --sync "$CONTAINER_ID" /bin/sh -c 'echo $TEST_VAR' 2>&1)
check_contains "crictl exec: env var TEST_VAR is set" "$OUT" "hello"

# crictl itself exits 1 for any non-zero container exit; the actual exit code
# appears in its stderr as "exited with N". Verify 42 is propagated correctly.
OUT=$($CRICTL exec --sync "$CONTAINER_ID" /bin/sh -c 'exit 42' 2>&1 || true)
check_contains "crictl exec --sync: non-zero exit code propagated" "$OUT" "exited with 42"

START=$(date +%s)
$CRICTL exec --sync --timeout 2 "$CONTAINER_ID" /bin/sh -c 'sleep 30' 2>&1 || true
ELAPSED=$(( $(date +%s) - START ))
if [ "$ELAPSED" -le 5 ]; then
    pass "crictl exec --sync: timeout kills long-running command (${ELAPSED}s)"
else
    fail "crictl exec --sync: timeout did not fire (${ELAPSED}s elapsed)"
fi

# ── Streaming Exec (kubectl-style) ───────────────────────────────────────────
# Uses crictl exec WITHOUT --sync to exercise the SPDY streaming path.

step "C6: Streaming Exec"

OUT=$(timeout 10 $CRICTL exec "$CONTAINER_ID" /bin/echo streaming-hello 2>&1)
check_contains "crictl exec (streaming): stdout relay" "$OUT" "streaming-hello"

OUT=$(timeout 10 $CRICTL exec "$CONTAINER_ID" /bin/sh -c 'echo $TEST_VAR' 2>&1)
check_contains "crictl exec (streaming): env var relay" "$OUT" "hello"

OUT=$(timeout 10 $CRICTL exec "$CONTAINER_ID" /bin/sh -c 'echo err >&2; echo out' 2>&1)
check_contains "crictl exec (streaming): stderr relay" "$OUT" "err"
check_contains "crictl exec (streaming): stdout relay with stderr" "$OUT" "out"

# ── Container status ──────────────────────────────────────────────────────────

step "C6: Container status"

OUT=$($CRICTL inspect "$CONTAINER_ID" 2>&1)
check_contains "crictl inspect: state=CONTAINER_RUNNING" "$OUT" "CONTAINER_RUNNING"

# ── Container stats ──────────────────────────────────────────────────────────

step "C6: ListContainerStats / ContainerStats"

OUT=$($CRICTL stats --output json 2>&1)
check_contains "crictl stats returns JSON output with stats array" "$OUT" '"stats"'

OUT=$($CRICTL stats "$CONTAINER_ID" 2>&1)
check_exit_ok "crictl stats <id> exits 0" $CRICTL stats "$CONTAINER_ID"

# ── Stop / remove ─────────────────────────────────────────────────────────────

step "C6: Stop and remove"

check_exit_ok "crictl stop container" $CRICTL stop "$CONTAINER_ID"

OUT=$($CRICTL ps -a 2>&1)
check_contains "crictl ps -a shows exited container" "$OUT" "Exited"

check_exit_ok "crictl rm container" $CRICTL rm "$CONTAINER_ID"

OUT=$($CRICTL ps -a 2>&1)
if ! echo "$OUT" | grep -q "$CONTAINER_ID"; then
    pass "container removed from crictl ps"
else
    fail "container still visible after rm"
fi

check_exit_ok "crictl stopp sandbox" $CRICTL stopp "$SANDBOX_ID"
check_exit_ok "crictl rmp sandbox" $CRICTL rmp "$SANDBOX_ID"

OUT=$($CRICTL pods 2>&1)
if ! echo "$OUT" | grep -q "test-pod"; then
    pass "sandbox removed from crictl pods"
else
    fail "sandbox still visible after rmp"
fi

# ── Summary ───────────────────────────────────────────────────────────────────

echo ""
echo -e "${BOLD}Results: ${GREEN}${PASS} passed${NC}  ${RED}${FAIL} failed${NC}"

[ "$FAIL" -eq 0 ]
