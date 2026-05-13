# IPv6 Forwarding and NAT66: Why We Removed It

**Status: NAT66 removed; ULA container-to-container IPv6 verified; T-Mobile internet IPv6 via pasta (pending)**

See also: GitHub issue #224

---

## The Original Bug (pre-commit 19373ec)

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

## Why NAT66 Is Fundamentally Incompatible with networkd-managed SLAAC

NAT66 (IPv6 masquerade) requires the kernel's `ip6_forward()` path.
`ip6_forward()` checks `net->ipv6.devconf_all->forwarding` at the very top
and returns immediately if it is 0 — per-interface settings alone are not
enough.

The only way to set `devconf_all->forwarding` is to write `all/forwarding=1`.
That write propagates to **every host interface** via `dev_forward_change()`,
which causes systemd-networkd to disable its RA client and remove
SLAAC-managed IPv6 state.

An intermediate "hack" (commit 7bbddb8) wrote `all/forwarding=1` then
immediately reset non-bridge interfaces to 0.  This races against networkd's
inotify watcher and works in practice on a LAN with 5–50 ms networkd latency,
but it is a timing-dependent hack, not a correct solution.

**The root problem:** the two operations — "set devconf_all->forwarding=1" and
"propagate forwarding=1 to every interface" — cannot be decoupled.  There is no
kernel API to set `devconf_all->forwarding` without the propagation side-effect.

## The Correct Fix: Remove NAT66

NAT66 is not a viable feature for bridge networks on SLAAC-managed hosts.

The correct design is:

- **Container-to-container IPv6:** Bridge networking is L2 (MAC-addressed).
  Traffic between two containers on the same bridge never calls `ip6_forward()`.
  It works without any sysctl changes.  Containers get ULA addresses
  (`fd7e:73ca:9801::/64`) and can reach each other and the bridge gateway
  with zero impact on host networking.

- **Internet IPv6 from containers:** Use `pasta` (`--network pasta`), which
  provides full internet access (including IPv6) via user-mode networking.
  `pasta` needs no kernel forwarding and does not touch host sysctls.

## Verified Behavior (post NAT66 removal)

- Container-to-container IPv6: **0% packet loss** (5/5 pings, ~0.06 ms avg)
- Bridge gateway ping: **0% packet loss** (fd7e:73ca:9801::1)
- Host `wlan0/forwarding`: unmodified — networkd SLAAC unaffected
- Host `all/forwarding`: 0 at all times during bridge container lifetime

## Kernel Behavior Reference

| sysctl write | Propagates to existing interfaces? | Affects `devconf_all`? |
|---|---|---|
| `all/forwarding` | **Yes** — iterates all netdevs, calls `dev_forward_change()` | Yes |
| `<iface>/forwarding` | Affects that interface's `idev->cnf` only | No |
| `all/accept_ra` | **No** — only updates `devconf_all`; per-interface `idev->cnf` unchanged | Yes |

`ip6_forward()` checks `net->ipv6.devconf_all->forwarding` — not the incoming
interface's `idev->cnf.forwarding`.  Setting only a per-interface value leaves
`devconf_all` at 0 and `ip6_forward` bails immediately.

When `dev_forward_change()` fires on an interface managed by networkd, networkd
may disable its RA client and remove SLAAC-managed state for that interface.

## Verification Steps for T-Mobile (issue #224)

Container-to-container IPv6 is verified.  Internet IPv6 via pasta on T-Mobile
still needs testing:

- [ ] Connect to T-Mobile hotspot
- [ ] Confirm host has IPv6: `ip -6 addr show wlan0` — expect a `scope global` address
- [ ] Run `pelagos run alpine ping -6 -c3 2001:4860:4860::8888` (pasta mode, rootless)
- [ ] Confirm host IPv6 is still intact after container exits: `ping -6 2001:4860:4860::8888`
- [ ] Reconnect to T-Mobile a second time (without restarting networkd) to confirm
      no manual intervention needed

## Diagnosing a Host Corrupted by Old Code

```bash
for iface in all wlan0 pelagos0; do
  echo "$iface: forwarding=$(cat /proc/sys/net/ipv6/conf/$iface/forwarding) \
accept_ra=$(cat /proc/sys/net/ipv6/conf/$iface/accept_ra)"
done
```

Expected healthy state (with fix):
- `all/forwarding=0`
- `wlan0/forwarding=0`, `wlan0/accept_ra=0` (networkd owns this — correct)
- `pelagos0/forwarding=0` (no longer set by pelagos — correct)

One-time recovery for hosts corrupted by old code:

```bash
sudo systemctl restart systemd-networkd
```

## Commit History

- `19373ec` — bridge-scoped forwarding (WRONG: kernel ignores per-iface for ip6_forward)
- `7bbddb8` — timing hack: write all/forwarding=1 then reset non-bridge ifaces (FRAGILE)
- NAT66 removed entirely — see current `src/network.rs`
