#!/bin/bash
# IPv6 regression test:
#   - bridge+NAT containers don't corrupt host SLAAC
#   - pasta (default) provides IPv6 internet for both root and rootless
#   - container-to-container IPv6 works on bridge
set -euo pipefail

IFACE=${1:-wlan0}
PASS=0
FAIL=0

pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

echo "=== IPv6 regression test (interface: $IFACE) ==="
echo

# 1. Baseline host IPv6
echo "--- Baseline ---"
BEFORE=$(ip -6 addr show "$IFACE" | grep "scope global" | awk '{print $2}' | head -1)
if [ -n "$BEFORE" ]; then
    pass "host has IPv6: $BEFORE"
else
    echo "  SKIP: $IFACE has no global IPv6 address — connect to an IPv6 network first"
    exit 1
fi
echo

# 2. bridge+NAT container (historically the corruption vector)
echo "--- Bridge+NAT container (IPv4) ---"
if sudo pelagos run --network pelagos0 --nat alpine ping -4 -c3 -W2 8.8.8.8; then
    pass "bridge+NAT IPv4 internet"
else
    fail "bridge+NAT IPv4 internet"
fi
echo

# 3. Host IPv6 still intact after bridge run?
echo "--- Host IPv6 after bridge container ---"
AFTER=$(ip -6 addr show "$IFACE" | grep "scope global" | awk '{print $2}' | head -1)
if [ "$AFTER" = "$BEFORE" ]; then
    pass "host IPv6 unchanged: $AFTER"
else
    fail "host IPv6 changed: was $BEFORE, now ${AFTER:-GONE}"
fi

# 4. Forwarding sysctls untouched?
for iface in all "$IFACE" pelagos0; do
    path="/proc/sys/net/ipv6/conf/$iface/forwarding"
    [ -f "$path" ] || continue
    val=$(cat "$path")
    if [ "$val" = "0" ]; then
        pass "$iface/forwarding=0 (untouched)"
    else
        fail "$iface/forwarding=$val (should be 0)"
    fi
done
echo

# 5. Host IPv6 internet
echo "--- Host IPv6 internet ---"
if ping -6 -c3 -W2 2001:4860:4860::8888 > /dev/null 2>&1; then
    pass "host ping6 2001:4860:4860::8888"
else
    fail "host ping6 2001:4860:4860::8888"
fi
echo

# 6. IPv6 internet — pasta, rootless (auto-default)
echo "--- Container IPv6 internet (pasta, rootless, auto-default) ---"
if pelagos run alpine ping -6 -c3 -W2 2001:4860:4860::8888; then
    pass "rootless auto-default: IPv6 internet via pasta"
else
    fail "rootless auto-default: IPv6 internet via pasta"
fi
echo

# 7. IPv6 internet — pasta, root (auto-default)
echo "--- Container IPv6 internet (pasta, root, auto-default) ---"
if sudo pelagos run alpine ping -6 -c3 -W2 2001:4860:4860::8888; then
    pass "root auto-default: IPv6 internet via pasta"
else
    fail "root auto-default: IPv6 internet via pasta"
fi
echo

# 8. Container-to-container IPv6 on bridge
echo "--- Container-to-container IPv6 (bridge) ---"
sudo pelagos run --name c2c-server --detach --network pelagos0 --nat alpine sleep 30
sleep 1
NS=$(cat /run/pelagos/containers/c2c-server/state.json \
    | python3 -c "import sys,json; print(json.load(sys.stdin)['network_ns_name'])")
C1_IP6=$(sudo ip netns exec "$NS" ip -6 addr show eth0 \
    | grep "scope global" | awk '{print $2}' | cut -d/ -f1)
if [ -n "$C1_IP6" ]; then
    pass "server has ULA IPv6: $C1_IP6"
    if sudo pelagos run --network pelagos0 --nat alpine ping -6 -c3 -W2 "$C1_IP6"; then
        pass "c2c ping6 to $C1_IP6"
    else
        fail "c2c ping6 to $C1_IP6"
    fi
else
    fail "server has no ULA IPv6 address"
fi
sudo pelagos stop c2c-server 2>/dev/null; sudo pelagos rm c2c-server 2>/dev/null
echo

echo "=== Results: $PASS passed, $FAIL failed ==="
[ $FAIL -eq 0 ]
