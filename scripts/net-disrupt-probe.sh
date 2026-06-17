#!/usr/bin/env bash
# Probe: does pelagos sandbox create/teardown disrupt HOST conntrack / nftables?
#
# Symptom this investigates (issue #NNN): on a node that also runs Tailscale,
# running pelagos-cri sandbox workloads kills the node's *NAT-traversed* Tailscale
# connections (omen<->ipc2 direct path falls back to DERP), while LAN-local
# connectivity (ipc1<->ipc2) is unaffected. That signature = something flushing
# the host's conntrack/nftables state (the classic "container runtime nukes
# iptables and breaks Tailscale" footgun).
#
# This script snapshots the host network state BEFORE a sandbox create, AFTER
# create, and AFTER teardown — so we can localize whether the disruption happens
# at runp (create) or rmp (teardown), and whether it's conntrack, nft, or routes.
#
# Run ON the pelagos-cri node (e.g. ipc2), as root, self-contained so it survives
# the SSH session dropping mid-run:
#   sudo bash scripts/net-disrupt-probe.sh /tmp/netprobe.result [tailscale-peer]
set -u
SOCK="unix:///run/pelagos/cri.sock"
OUT="${1:-/tmp/netprobe.result}"
PEER="${2:-omen}"
: > "$OUT"
log(){ echo "$@" >> "$OUT"; }

have(){ command -v "$1" >/dev/null 2>&1; }

snap(){ # $1 = label
  log ""
  log "===== SNAPSHOT: $1  ($(date -u +%FT%TZ)) ====="
  # Tailscale's view of the remote peer: a non-empty CurAddr == direct path;
  # a Relay value == fell back to DERP. The whole point is to catch this flip.
  if have tailscale; then
    tailscale status --json 2>/dev/null | python3 -c "
import sys,json
try: d=json.load(sys.stdin)
except Exception: sys.exit()
for _,p in d.get('Peer',{}).items():
    if p.get('HostName','').lower().startswith('$PEER'.lower()):
        print('  tailscale[%s]: CurAddr=%r  Relay=%r  Active=%s' % (p.get('HostName'), p.get('CurAddr'), p.get('Relay'), p.get('Active')))
" >> "$OUT" 2>/dev/null || log "  tailscale: (json parse n/a)"
  fi
  if have conntrack; then
    log "  conntrack total entries : $(conntrack -C 2>/dev/null)"
    log "  conntrack udp/41641 (ts): $(conntrack -L -p udp 2>/dev/null | grep -c 41641)"
    log "  conntrack udp/51820 (fl): $(conntrack -L -p udp 2>/dev/null | grep -c 51820)"
  else
    log "  conntrack: (not installed) — /proc count: $(cat /proc/sys/net/netfilter/nf_conntrack_count 2>/dev/null)"
  fi
  log "  nft ruleset sha256 : $(nft list ruleset 2>/dev/null | sha256sum | cut -c1-16)"
  log "  nft table list     : $(nft list tables 2>/dev/null | tr '\n' '|' | cut -c1-200)"
  log "  default route      : $(ip route show default 2>/dev/null | head -1)"
  log "  tailscale0 mtu/flags: $(ip -o link show tailscale0 2>/dev/null | sed -E 's/.*mtu ([0-9]+).*state ([A-Z]+).*/mtu=\1 state=\2/')"
}

POD=$(mktemp /tmp/netprobe-pod-XXXXXX.json)
cat > "$POD" <<JSON
{ "metadata": {"name":"netprobe","namespace":"default","uid":"netprobe-uid","attempt":1},
  "log_directory":"/tmp",
  "linux": {} }
JSON

snap "BEFORE (baseline)"

log ""
log ">>> crictl runp  (create ONE sandbox)"
SB=$(crictl -r "$SOCK" runp "$POD" 2>>"$OUT")
log "    sandbox_id=$SB"
sleep 2
snap "AFTER runp (sandbox created)"

log ""
log ">>> crictl stopp + rmp  (teardown)"
[ -n "$SB" ] && crictl -r "$SOCK" stopp "$SB" >>"$OUT" 2>&1
[ -n "$SB" ] && crictl -r "$SOCK" rmp "$SB" >>"$OUT" 2>&1
sleep 2
snap "AFTER rmp (torn down)"

rm -f "$POD"
log ""
log "===== INTERPRETATION HINTS ====="
log "  - If 'conntrack total' drops sharply or 'nft ruleset sha256' changes between"
log "    BEFORE and AFTER-runp  -> the disruption is at sandbox CREATE."
log "  - If it only changes between AFTER-runp and AFTER-rmp -> it's TEARDOWN."
log "  - If tailscale[peer] CurAddr goes non-empty -> empty (Relay set) across a step,"
log "    that step broke the NAT-traversed direct path (the Tailscale symptom)."
log "===== DONE ====="
