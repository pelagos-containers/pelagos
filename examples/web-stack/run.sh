#!/usr/bin/env bash
#
# Remora Web Stack Demo
# =====================
# Builds and runs a 3-container blog stack:
#   nginx (reverse proxy) → bottle (Python API) → redis (data store)
#
# Usage:  sudo ./examples/web-stack/run.sh
#

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REMORA="${REMORA:-remora}"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

pass=0
fail=0

log()  { echo -e "${CYAN}==>${NC} $*"; }
ok()   { echo -e "  ${GREEN}PASS${NC} $*"; pass=$((pass + 1)); }
fail() { echo -e "  ${RED}FAIL${NC} $*"; fail=$((fail + 1)); }

cleanup() {
    log "Cleaning up..."
    $REMORA stop proxy  2>/dev/null || true
    $REMORA stop app    2>/dev/null || true
    $REMORA stop redis  2>/dev/null || true
    sleep 1
    $REMORA rm proxy    2>/dev/null || true
    $REMORA rm app      2>/dev/null || true
    $REMORA rm redis    2>/dev/null || true
    $REMORA volume rm notes-data 2>/dev/null || true
    log "Done."
}

die() { echo -e "${RED}ERROR:${NC} $*" >&2; exit 1; }

# ── Prerequisites ──────────────────────────────────────────────────────

log "Checking prerequisites..."

command -v "$REMORA" >/dev/null 2>&1 || die "remora not found in PATH. Run: cargo build --release && export PATH=\$PWD/target/release:\$PATH"

# Ensure alpine:latest is pulled
if ! $REMORA image ls 2>/dev/null | grep -q "alpine:latest"; then
    log "Pulling alpine:latest..."
    $REMORA image pull alpine:latest
fi

# ── Build Phase ────────────────────────────────────────────────────────
# remora build now enables NAT + DNS automatically for bridge RUN steps.

log "Building ${BOLD}web-stack-redis${NC}..."
$REMORA build -t web-stack-redis --network bridge "$SCRIPT_DIR/redis"

log "Building ${BOLD}web-stack-app${NC}..."
$REMORA build -t web-stack-app --network bridge "$SCRIPT_DIR/app"

log "Building ${BOLD}web-stack-proxy${NC}..."
$REMORA build -t web-stack-proxy --network bridge "$SCRIPT_DIR/proxy"

# ── Create Volume ──────────────────────────────────────────────────────

log "Creating volume ${BOLD}notes-data${NC}..."
$REMORA volume create notes-data 2>/dev/null || true

# ── Launch Containers ──────────────────────────────────────────────────

trap cleanup EXIT

start_container() {
    local name="$1"; shift
    log "Starting ${BOLD}${name}${NC}..."
    $REMORA run -d --name "$name" "$@"
    sleep 2
    # Verify container is still running.
    if ! $REMORA ps 2>/dev/null | grep -q "$name"; then
        echo -e "  ${RED}Container '${name}' exited immediately!${NC}"
        echo "  stdout: $(cat /run/remora/containers/${name}/stdout.log 2>/dev/null || echo '<empty>')"
        echo "  stderr: $(cat /run/remora/containers/${name}/stderr.log 2>/dev/null || echo '<empty>')"
        exit 1
    fi
}

start_container redis --network bridge --nat web-stack-redis:latest
start_container app --network bridge --nat --link redis:redis web-stack-app:latest
start_container proxy --network bridge --nat --link app:app web-stack-proxy:latest
sleep 1

# ── Verification ───────────────────────────────────────────────────────

echo
log "${BOLD}Running verification tests...${NC}"
echo

# Resolve the proxy container's bridge IP for direct access.
# Port forwarding (localhost:8080) requires hairpin NAT which is not yet
# implemented; for now we test via the bridge IP directly.
PROXY_IP=$($REMORA ps 2>/dev/null | awk '/^proxy / {print $3}')
PROXY_STATE="/run/remora/containers/proxy/state.json"
if [ -f "$PROXY_STATE" ]; then
    PROXY_IP=$(python3 -c "import json; print(json.load(open('$PROXY_STATE')).get('bridge_ip',''))" 2>/dev/null || true)
fi
if [ -z "$PROXY_IP" ]; then
    echo -e "${RED}Could not determine proxy bridge IP${NC}"
    exit 1
fi
log "Proxy bridge IP: ${BOLD}${PROXY_IP}${NC}"

CURL="curl -s --max-time 5"
BASE="http://${PROXY_IP}:80"

# Test 1: Static page
BODY=$($CURL "$BASE/" 2>/dev/null || true)
if echo "$BODY" | grep -q "Remora Blog"; then
    ok "GET / — contains 'Remora Blog'"
else
    fail "GET / — expected 'Remora Blog' in response"
    echo "       body: $(echo "$BODY" | head -3)"
fi

# Test 2: Health check (proxied to app:5000)
# Retry once — the first proxy_pass request can 502 if nginx hasn't connected yet.
BODY=$($CURL "$BASE/health" 2>/dev/null || true)
if ! echo "$BODY" | grep -q '"status"'; then
    sleep 1
    BODY=$($CURL "$BASE/health" 2>/dev/null || true)
fi
if echo "$BODY" | grep -q '"status"'; then
    ok "GET /health — returns status ok"
else
    fail "GET /health — expected JSON status"
    echo "       body: $(echo "$BODY" | head -3)"
fi

# Test 3: Empty notes list
BODY=$($CURL "$BASE/api/notes" 2>/dev/null || true)
if [ "$BODY" = "[]" ]; then
    ok "GET /api/notes — returns empty list"
else
    fail "GET /api/notes — expected [], got: $BODY"
    echo "       body: $(echo "$BODY" | head -3)"
fi

# Test 4: Post a note
BODY=$($CURL -X POST -H 'Content-Type: application/json' \
    -d '{"text":"hello from remora"}' \
    "$BASE/api/notes" 2>/dev/null || true)
if echo "$BODY" | grep -q '"ok"'; then
    ok "POST /api/notes — note created"
else
    fail "POST /api/notes — expected ok response, got: $BODY"
fi

# Test 5: Verify note persisted
BODY=$($CURL "$BASE/api/notes" 2>/dev/null || true)
if echo "$BODY" | grep -q "hello from remora"; then
    ok "GET /api/notes — note persisted"
else
    fail "GET /api/notes — expected note in list, got: $BODY"
fi

# ── Summary ────────────────────────────────────────────────────────────

echo
echo -e "${BOLD}Results: ${GREEN}${pass} passed${NC}, ${RED}${fail} failed${NC}"

if [ "$fail" -gt 0 ]; then
    echo -e "\n${YELLOW}Some tests failed. Check container logs:${NC}"
    echo "  $REMORA logs redis"
    echo "  $REMORA logs app"
    echo "  $REMORA logs proxy"
    # Keep containers running for debugging — cleanup runs on exit
    echo -e "\nPress Enter to clean up and exit..."
    read -r
fi
