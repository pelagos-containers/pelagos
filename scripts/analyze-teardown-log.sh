#!/usr/bin/env bash
#
# Analyze a teardown log produced by:
#
#   sudo -E RUST_LOG=pelagos::teardown=info cargo test --test integration_tests \
#     2>&1 | tee teardown.log
#
# Usage: scripts/analyze-teardown-log.sh teardown.log
#
set -euo pipefail

LOG="${1:-teardown.log}"

if [ ! -f "$LOG" ]; then
    echo "Usage: $0 <teardown.log>"
    exit 1
fi

echo "=== Resource allocation vs teardown ==="
echo ""

# Extract alloc and free events keyed by netns name.
declare -A alloc_veth alloc_network alloc_nat
declare -A free_start free_veth free_netns

while IFS= read -r line; do
    if [[ "$line" =~ ALLOC\ netns=([^ ]+)\ veth=([^ ]+)\ network=([^ ]+)\ nat=([^ ]+) ]]; then
        ns="${BASH_REMATCH[1]}"
        alloc_veth["$ns"]="${BASH_REMATCH[2]}"
        alloc_network["$ns"]="${BASH_REMATCH[3]}"
        alloc_nat["$ns"]="${BASH_REMATCH[4]}"
    elif [[ "$line" =~ FREE_VETH\ veth=([^ ]+) ]]; then
        veth="${BASH_REMATCH[1]}"
        free_veth["$veth"]=1
    elif [[ "$line" =~ FREE_NETNS\ netns=([^ ]+) ]]; then
        free_netns["${BASH_REMATCH[1]}"]=1
    fi
done < "$LOG"

leaked=0
for ns in "${!alloc_veth[@]}"; do
    veth="${alloc_veth[$ns]}"
    network="${alloc_network[$ns]}"
    nat="${alloc_nat[$ns]}"

    veth_freed="${free_veth[$veth]+yes}"
    netns_freed="${free_netns[$ns]+yes}"

    if [[ "$veth_freed" != "yes" || "$netns_freed" != "yes" ]]; then
        echo "LEAKED  netns=$ns veth=$veth network=$network nat=$nat"
        [[ "$veth_freed" != "yes" ]]  && echo "        veth NOT freed"
        [[ "$netns_freed" != "yes" ]] && echo "        netns NOT freed"
        leaked=$((leaked + 1))
    fi
done

if [ "$leaked" -eq 0 ]; then
    echo "All allocated resources were freed."
else
    echo ""
    echo "$leaked leaked resource(s) found."
fi

echo ""
echo "=== NAT table kept (non-empty remaining set) ==="
grep "NAT_TABLE_KEPT.*active_nat_users" "$LOG" | sed 's/.*INFO //' || echo "  (none)"

echo ""
echo "=== Counts ==="
alloc_count=$(grep -c "ALLOC netns=" "$LOG" || true)
free_netns_count=$(grep -c "FREE_NETNS" "$LOG" || true)
nat_deleted=$(grep -c "NAT_TABLE_DELETED" "$LOG" || true)
nat_kept=$(grep -c "NAT_TABLE_KEPT" "$LOG" || true)
echo "  Allocated:      $alloc_count"
echo "  Netns freed:    $free_netns_count"
echo "  NAT table del:  $nat_deleted"
echo "  NAT table kept: $nat_kept"
