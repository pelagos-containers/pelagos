# IPv6 Forwarding: Bridge-Scoped, Not Global

**Status: fix committed (19373ec), not yet verified on T-Mobile**

See also: GitHub issue #224

---

## The Bug (pre-commit 19373ec)

The old `setup_ipv6_container()` code wrote:

```rust
std::fs::write("/proc/sys/net/ipv6/conf/all/accept_ra", "2\n");
std::fs::write("/proc/sys/net/ipv6/conf/all/forwarding", "1\n");
```

Writing to `net.ipv6.conf.all.forwarding` propagates to **every host interface**
including `wlan0` and `eth0`.  systemd-networkd monitors sysctl changes and
interprets `wlan0/forwarding=1` as "this interface entered router mode."
It responds by shutting down its RA client for that interface and removing the
SLAAC-managed global IPv6 address and default route.

When the container exited, `teardown_network` never reset `all/forwarding` to
`0`.  Even after the value was eventually cleared (e.g. system restart),
networkd did not automatically restart its RA client — so the host remained
without IPv6 until networkd was restarted manually.

**Observed symptom on T-Mobile tethering (and any SLAAC-based network):**

```
$ ping -6 2001:4860:4860::8888
ping: connect: Network is unreachable

$ sudo pelagos run alpine ping -6 -c3 2001:4860:4860::8888
3 packets transmitted, 0 packets received, 100% packet loss
```

The networkd journal exposed the conflict:

```
systemd-networkd[748]: Foreign process 'sysctl[…]' changed sysctl
  '/proc/sys/net/ipv6/conf/wlan0/accept_ra' from '0' to '1',
  conflicting with our setting to '0'.
```

### Why `accept_ra=2` on `all` didn't help

The `all/accept_ra` sysctl does **not** propagate to existing per-interface
configurations.  It only updates `devconf_all->accept_ra`, which controls the
default for newly-created interfaces.  The kernel's `ipv6_accept_ra()` checks
`idev->cnf.accept_ra` (per-interface), not the `all` value.

Additionally, modern systemd-networkd (v249+) handles RAs entirely in
userspace via a raw socket and deliberately sets `wlan0/accept_ra=0` to
prevent the kernel from double-processing them.  Any write to `wlan0/accept_ra`
by a "foreign process" is detected and immediately reverted.

## The Fix

Write forwarding only to the **bridge interface**:

```rust
let bridge_fwd = format!("/proc/sys/net/ipv6/conf/{}/forwarding", net.name);
let _ = std::fs::write(&bridge_fwd, "1\n");
```

### Why bridge-scoped is sufficient

For the container → internet IPv6 path:

1. Container sends packet out `eth0` → arrives at `vh-xxxx` (veth host side)
2. Bridge processes it at L2 → passes to `pelagos0` for L3 routing
3. `ip6_forward()` in the kernel checks the **incoming L3 device's** forwarding
   flag: `skb->dev` at this point is the bridge device (`pelagos0`), not the
   originating veth port
4. `pelagos0/forwarding=1` → kernel will forward the packet
5. Host looks up a route for the destination and forwards via NAT66

The individual veth interfaces do not need forwarding enabled.  No host
wireless or ethernet interface is touched.

## Kernel Behavior Reference

| sysctl write | Propagates to existing interfaces? |
|---|---|
| `all/forwarding` | **Yes** — kernel iterates all netdevs and calls `dev_forward_change()` |
| `all/accept_ra` | **No** — only updates `devconf_all`; per-interface `idev->cnf` unchanged |
| `<iface>/forwarding` | Affects that interface only |

When `dev_forward_change()` fires on an interface managed by networkd, networkd
may disable its RA client and remove SLAAC-managed state for that interface.

## Verification Steps (TODO)

This fix has not yet been tested end-to-end on T-Mobile tethering.  Before
closing the issue:

- [ ] Install fixed binary: `sudo cargo install --path .`
- [ ] One-time state repair: `sudo systemctl restart systemd-networkd`
- [ ] Connect to T-Mobile hotspot
- [ ] Confirm host has IPv6: `ip -6 addr show wlan0` — expect a global (`scope global`) address
- [ ] If no global address, T-Mobile hotspot may not deliver IPv6 via SLAAC on this device/plan — the fix is still correct but containers won't have internet IPv6 because the host doesn't either
- [ ] If host has IPv6: `sudo pelagos run alpine ping -6 -c3 2001:4860:4860::8888` — expect 0% loss
- [ ] Reconnect to T-Mobile a second time (without restarting networkd) to confirm no manual intervention is needed after the fix

## Diagnosing a Corrupted Host

```bash
for iface in all wlan0 pelagos0; do
  echo "$iface: forwarding=$(cat /proc/sys/net/ipv6/conf/$iface/forwarding) \
accept_ra=$(cat /proc/sys/net/ipv6/conf/$iface/accept_ra)"
done
```

Expected healthy state on a SLAAC network with the fix:
- `all/forwarding=0`
- `wlan0/forwarding=0`, `wlan0/accept_ra=0` (networkd owns this — correct)
- `pelagos0/forwarding=1` (set by pelagos — correct)

One-time recovery for hosts corrupted by old code:

```bash
sudo systemctl restart systemd-networkd
```

## Commit

Fixed in commit `19373ec` — "fix(network): scope IPv6 forwarding to bridge only, not all interfaces"
