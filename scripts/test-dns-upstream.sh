#!/usr/bin/env bash
# Diagnose the test_dns_upstream_forward integration test hang.
#
# Steps:
#   1. Check host can reach upstream DNS (8.8.8.8:53)
#   2. Create a test bridge network
#   3. Start a holder container on that network
#   4. Register a dummy DNS entry (starts pelagos-dns daemon)
#   5. Probe gateway:53 from the HOST to verify daemon is bound
#   6. Run nslookup for example.com from INSIDE a container (10s timeout)
#   7. Check nftables rules that might be intercepting port-53 traffic
#   8. Clean up
#
# Usage: sudo bash scripts/test-dns-upstream.sh

set -euo pipefail
PELAGOS=${PELAGOS:-pelagos}
NET=dnsdiag
SUBNET=10.91.7.0/24
GW=10.91.7.1
die() { echo "ERROR: $*" >&2; exit 1; }

cleanup() {
    echo "--- cleanup ---"
    # Kill holder if running
    if [[ -n "${HOLDER_NAME:-}" ]]; then
        "$PELAGOS" stop "$HOLDER_NAME" 2>/dev/null || true
        "$PELAGOS" rm   "$HOLDER_NAME" 2>/dev/null || true
    fi
    # Kill pelagos-dns daemon
    if [[ -f /run/pelagos/dns/pid ]]; then
        pid=$(cat /run/pelagos/dns/pid)
        kill "$pid" 2>/dev/null || true
        sleep 0.2
    fi
    rm -rf /run/pelagos/dns 2>/dev/null || true
    # Remove test network
    ip link del "rm-$NET" 2>/dev/null || true
    rm -rf "/run/pelagos/networks/$NET" "/var/lib/pelagos/networks/$NET" \
           "/var/lib/pelagos/ipam/$NET" 2>/dev/null || true
}
trap cleanup EXIT

[[ $EUID -eq 0 ]] || die "run as root (sudo bash scripts/test-dns-upstream.sh)"

# ── 1. Upstream reachability ──────────────────────────────────────────────────
echo "=== 1. upstream reachability (8.8.8.8:53, 1s timeout) ==="
python3 - <<'PY'
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.settimeout(1)
# Minimal A query for "example.com"
q = (b'\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00'
     b'\x07example\x03com\x00\x00\x01\x00\x01')
s.sendto(q, ('8.8.8.8', 53))
try:
    data, _ = s.recvfrom(512)
    print(f"8.8.8.8:53 reachable OK, response {len(data)} bytes")
except Exception as e:
    print(f"8.8.8.8:53 UNREACHABLE: {e}", file=sys.stderr)
    sys.exit(1)
PY

# ── 2. Create network ─────────────────────────────────────────────────────────
echo ""
echo "=== 2. creating network $NET ($SUBNET) ==="
"$PELAGOS" network create "$NET" --subnet "$SUBNET"
ip -br addr show "rm-$NET" || true

# ── 3. Holder container ───────────────────────────────────────────────────────
echo ""
echo "=== 3. starting holder container ==="
HOLDER_NAME="dns-holder"
"$PELAGOS" run -d --name "$HOLDER_NAME" --network "$NET" \
    alpine /bin/sleep 60
sleep 1   # let it settle
ip -br addr show "rm-$NET"

# ── 4. Confirm daemon is running (started automatically by pelagos run) ───────
echo ""
echo "=== 4. daemon status ==="
sleep 0.5
echo -n "pelagos-dns PID: "; cat /run/pelagos/dns/pid 2>/dev/null || echo "(no pid file)"
ss -ulnp | grep ':53' || echo "(no UDP :53 found)"

# ── 5. Probe from HOST ────────────────────────────────────────────────────────
echo ""
echo "=== 5. probing $GW:53 from HOST ==="
python3 - "$GW" <<'PY'
import socket, sys
gw = sys.argv[1]
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.settimeout(2)
q = (b'\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00'
     b'\x07example\x03com\x00\x00\x01\x00\x01')
s.sendto(q, (gw, 53))
try:
    data, addr = s.recvfrom(512)
    print(f"daemon at {gw}:53 responded OK, {len(data)} bytes from {addr}")
except Exception as e:
    print(f"daemon at {gw}:53 DID NOT RESPOND: {e}", file=sys.stderr)
    sys.exit(1)
PY

# ── 6. nslookup from inside a container ──────────────────────────────────────
echo ""
echo "=== 6a. ping gateway from inside container ==="
"$PELAGOS" run --network "$NET" \
    alpine /bin/sh -c "ping -c3 -W2 $GW 2>&1 || echo PING_FAILED"

echo ""
echo "=== 6b. can container reach gateway port 53 (netcat UDP, 3s) ==="
# ash's built-in /dev/udp or busybox nc
"$PELAGOS" run --network "$NET" \
    alpine /bin/sh -c \
    "echo '' | timeout 3 nc -u -w3 $GW 53 2>&1 && echo NC_OK || echo NC_FAILED"

echo ""
echo "=== 6c. nslookup example.com $GW from inside container (timeout 10s) ==="
"$PELAGOS" run --network "$NET" \
    alpine /bin/sh -c "timeout 10 nslookup example.com $GW 2>&1 || echo NSLOOKUP_FAILED"

# ── 7. firewall / bridge-filter audit ────────────────────────────────────────
echo ""
echo "=== 7a. nftables rules (full) ==="
nft list ruleset 2>/dev/null || echo "(nft not available)"

echo ""
echo "=== 7b. iptables INPUT/FORWARD rules ==="
iptables -L INPUT   -n -v 2>/dev/null | head -20 || echo "(iptables unavailable)"
iptables -L FORWARD -n -v 2>/dev/null | head -20 || echo "(iptables unavailable)"

echo ""
echo "=== 7c. br_netfilter loaded? ==="
lsmod | grep br_netfilter || echo "br_netfilter NOT loaded"
cat /proc/sys/net/bridge/bridge-nf-call-iptables 2>/dev/null || echo "(bridge-nf-call-iptables not set)"

echo ""
echo "=== 7d. iptables nat PREROUTING (redirect/intercept rules) ==="
iptables -t nat -L PREROUTING -n -v 2>/dev/null | head -30 || echo "(unavailable)"

echo ""
echo "=== 7e. bridge slave interfaces ==="
ip link show master "rm-$NET" 2>/dev/null || echo "(none found)"

echo ""
echo "=== 7f. tcpdump on bridge while container probes port 53 ==="
# tcpdump in background; container probe; stop after 3s
tcpdump -i "rm-$NET" -n -l udp port 53 2>&1 &
TCPPID=$!
sleep 0.2
"$PELAGOS" run --rm --network "$NET" \
    alpine /bin/sh -c "echo | nc -u -w1 $GW 53 2>&1; true" &
sleep 3
kill $TCPPID 2>/dev/null || true
wait 2>/dev/null || true

echo ""
echo "=== 7g. iptables INPUT counters snapshot (compare with earlier) ==="
iptables -L INPUT -n -v 2>/dev/null | head -10 || echo "(unavailable)"

echo ""
echo "=== done ==="
