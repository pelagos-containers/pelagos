#!/usr/bin/env bash
# Network diagnostics dump for pelagos connectivity debugging

section() { echo; echo "=== $1 ==="; }

section "ip link show"
ip link show

section "ip route show"
ip route show

section "ip -6 route show"
ip -6 route show

section "ip addr show"
ip addr show

section "nft list ruleset"
sudo nft list ruleset

section "resolv.conf"
cat /etc/resolv.conf

section "systemd-resolved status"
resolvectl status 2>/dev/null || echo "(resolvectl not available)"

section "pelagos bridges"
ip link show type bridge

section "pelagos network state files"
ls -la /var/lib/pelagos/networks/ 2>/dev/null || echo "(no pelagos network state)"

section "IPv6 sysctl (forwarding + accept_ra)"
sysctl net.ipv6.conf.all.forwarding
sysctl net.ipv6.conf.all.accept_ra
sysctl net.ipv6.conf.wlan0.forwarding
sysctl net.ipv6.conf.wlan0.accept_ra
sysctl net.ipv6.conf.default.accept_ra
