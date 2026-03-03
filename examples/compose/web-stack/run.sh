#!/usr/bin/env bash
#
# Remora Compose Web Stack Demo
# ==============================
# The same 3-container blog stack as examples/web-stack, but
# orchestrated with `pelagos compose` instead of imperative shell.
#
# Usage:  sudo ./examples/compose/web-stack/run.sh
#

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WEB_STACK_DIR="$(cd "$SCRIPT_DIR/../../web-stack" && pwd)"
PELAGOS="${PELAGOS:-pelagos}"

RED='\033[0;31m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

pass=0
fail=0

log()  { echo -e "${CYAN}==>${NC} $*"; }
ok()   { echo -e "  ${GREEN}PASS${NC} $*"; pass=$((pass + 1)); }
fail() { echo -e "  ${RED}FAIL${NC} $*"; fail=$((fail + 1)); }
die()  { echo -e "${RED}ERROR:${NC} $*" >&2; exit 1; }

command -v "$PELAGOS" >/dev/null 2>&1 || \
    die "pelagos not found in PATH.  Run: cargo build --release && export PATH=\$PWD/target/release:\$PATH"

# ── Build Phase ───────────────────────────────────────────────────
# Images are built once from the existing web-stack Remfiles.

if ! $REMORA image ls 2>/dev/null | grep -q "alpine:latest"; then
    log "Pulling alpine:latest..."
    $REMORA image pull alpine:latest
fi

for svc in redis app proxy; do
    tag="web-stack-${svc}:latest"
    if $REMORA image ls 2>/dev/null | grep -q "$tag"; then
        log "Image ${BOLD}${tag}${NC} already built"
    else
        log "Building ${BOLD}${tag}${NC}..."
        $REMORA build -t "web-stack-${svc}" --network bridge "$WEB_STACK_DIR/${svc}"
    fi
done

# ── Compose Up ────────────────────────────────────────────────────
# One command replaces 50 lines of network/volume/container setup.

log "Starting stack with ${BOLD}remora compose up${NC} (compose.reml)..."
$REMORA compose up -f "$SCRIPT_DIR/compose.reml" -p blog --foreground &
COMPOSE_PID=$!

cleanup() {
    log "Tearing down..."
    $REMORA compose down -f "$SCRIPT_DIR/compose.reml" -p blog -v 2>/dev/null || true
    wait "$COMPOSE_PID" 2>/dev/null || true
    log "Done."
}
trap cleanup EXIT

# Wait for the stack to be ready (proxy publishes port 8080).
log "Waiting for stack to become ready..."
for i in $(seq 1 30); do
    if curl -s --max-time 1 http://127.0.0.1:8080/ >/dev/null 2>&1; then
        break
    fi
    sleep 1
done

# ── Verification ──────────────────────────────────────────────────

echo
log "${BOLD}Running verification tests...${NC}"
echo

CURL="curl -s --max-time 5"
BASE="http://127.0.0.1:8080"

# Test 1: Static page
BODY=$($CURL "$BASE/" 2>/dev/null || true)
if echo "$BODY" | grep -q "Remora Blog"; then
    ok "GET / — contains 'Remora Blog'"
else
    fail "GET / — expected 'Remora Blog' in response"
fi

# Test 2: Health check (proxied to app:5000)
BODY=$($CURL "$BASE/health" 2>/dev/null || true)
if echo "$BODY" | grep -q '"status"'; then
    ok "GET /health — returns JSON status"
else
    sleep 2
    BODY=$($CURL "$BASE/health" 2>/dev/null || true)
    if echo "$BODY" | grep -q '"status"'; then
        ok "GET /health — returns JSON status (retry)"
    else
        fail "GET /health — expected JSON status"
    fi
fi

# Test 3: Empty notes list
BODY=$($CURL "$BASE/api/notes" 2>/dev/null || true)
if [ "$BODY" = "[]" ]; then
    ok "GET /api/notes — returns empty list"
else
    fail "GET /api/notes — expected [], got: $BODY"
fi

# Test 4: Post a note
BODY=$($CURL -X POST -H 'Content-Type: application/json' \
    -d '{"text":"hello from pelagos compose"}' \
    "$BASE/api/notes" 2>/dev/null || true)
if echo "$BODY" | grep -q '"ok"'; then
    ok "POST /api/notes — note created"
else
    fail "POST /api/notes — expected ok, got: $BODY"
fi

# Test 5: Verify note persisted
BODY=$($CURL "$BASE/api/notes" 2>/dev/null || true)
if echo "$BODY" | grep -q "hello from pelagos compose"; then
    ok "GET /api/notes — note persisted through redis"
else
    fail "GET /api/notes — expected note in list, got: $BODY"
fi

# Test 6: Service list
echo
log "Service status:"
$REMORA compose ps -f "$SCRIPT_DIR/compose.reml" -p blog

# ── Summary ───────────────────────────────────────────────────────

echo
echo -e "${BOLD}Results: ${GREEN}${pass} passed${NC}, ${RED}${fail} failed${NC}"

if [ "$fail" -gt 0 ]; then
    echo -e "\nCheck service logs:"
    echo "  $REMORA compose logs -f $SCRIPT_DIR/compose.reml -p blog redis"
    echo "  $REMORA compose logs -f $SCRIPT_DIR/compose.reml -p blog app"
    echo "  $REMORA compose logs -f $SCRIPT_DIR/compose.reml -p blog proxy"
    echo -e "\nPress Enter to tear down..."
    read -r
fi
