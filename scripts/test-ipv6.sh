#!/bin/bash
# Manual verification: pelagos bridge+NAT containers don't corrupt host IPv6
set -euo pipefail

IFACE=${1:-wlan0}
PASS=0
FAIL=0

pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

echo "=== IPv6 regression test (interface: $IFACE) ==="
echo

# 1. Baseline
echo "--- Before container run ---"
BEFORE=$(ip -6 addr show "$IFACE" | grep "scope global" | awk '{print $2}' | head -1)
if [ -n "$BEFORE" ]; then
    pass "host has IPv6: $BEFORE"
else
    echo "  SKIP: $IFACE has no global IPv6 address — connect to an IPv6 network first"
    exit 1
fi
echo

# 2. Run bridge+NAT container (the historically corruption vector)
echo "--- Running bridge+NAT container ---"
sudo pelagos run --network pelagos0 --nat alpine ping -4 -c3 8.8.8.8
echo

# 3. Host IPv6 still intact?
echo "--- After container run ---"
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

# 5. Host can still reach IPv6 internet?
echo "--- Host IPv6 internet ---"
if ping -6 -c3 -W2 2001:4860:4860::8888 > /dev/null 2>&1; then
    pass "host ping6 2001:4860:4860::8888"
else
    fail "host ping6 2001:4860:4860::8888"
fi
echo

# 6. Container IPv6 internet via pasta?
echo "--- Container IPv6 internet (pasta) ---"
pelagos run alpine ping -6 -c3 -W2 2001:4860:4860::8888
if [ $? -eq 0 ]; then
    pass "container ping6 via pasta"
else
    fail "container ping6 via pasta"
fi
echo

echo "=== Results: $PASS passed, $FAIL failed ==="
[ $FAIL -eq 0 ]
