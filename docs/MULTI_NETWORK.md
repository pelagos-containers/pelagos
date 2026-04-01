# Multi-Network Support — Design Plan

> **Status: IMPLEMENTED** (v0.59+).
> This document is the original design plan; it is preserved for historical context.
> For current usage, see the [User Guide — Named Networks](USER_GUIDE.md#named-networks) section.

## Motivation

Today Pelagos has exactly one bridge: `pelagos0` (172.19.0.0/24). Every container
using `--network bridge` lands on the same L2 segment with the same IP pool.
This works, but it means:

- **No network isolation between container groups.** A "frontend" container can
  freely reach a "backend" container at the IP level. The only isolation
  boundary is "on the bridge" vs "not on the bridge."

- **A single flat IP pool.** All containers share 172.19.0.2–254 (253 addresses).
  With multiple tenants or projects this is tight and opaque.

- **No scoped service discovery.** If we ever add DNS-based container name
  resolution, the discovery domain should be the network, not "all containers
  everywhere."

Docker solves this with `docker network create` — user-defined bridge networks
with per-network subnets, isolation, and DNS. This document plans Pelagos's
equivalent.

---

## Current State (What Exists)

### Hardcoded constants (`src/network.rs`)

| Constant       | Value             | Used by                                   |
|----------------|-------------------|-------------------------------------------|
| `BRIDGE_NAME`  | `"pelagos0"`       | `ensure_bridge()`, veth attach, nftables  |
| `BRIDGE_GW`    | `"172.19.0.1"`    | container default route                   |
| `BRIDGE_CIDR`  | `"172.19.0.1/24"` | bridge IP assignment                      |

### Global state files (`src/paths.rs` → `/run/pelagos/`)

| File              | Purpose                    | Scope   |
|-------------------|----------------------------|---------|
| `next_ip`         | IPAM counter (2–254)       | Global  |
| `nat_refcount`    | NAT enable/disable trigger | Global  |
| `port_forwards`   | DNAT rule list             | Global  |

### Functions that assume a single bridge

- `ensure_bridge()` — creates `pelagos0` with hardcoded CIDR
- `allocate_ip()` — always returns 172.19.0.x
- `setup_bridge_network()` — no bridge name parameter; uses constants
- `teardown_network()` — only knows about one bridge
- `enable_nat()` / `disable_nat()` — nftables rules hardcode subnet + bridge name
- `enable_port_forwards()` / `disable_port_forwards()` — same
- `NFT_ADD_SCRIPT` — hardcoded `172.19.0.0/24` and `oifname != "pelagos0"`

### CLI (`src/cli/run.rs`)

`--network bridge` is a mode flag with no further parameters. No way to
specify which network a container should join.

### Container builder API (`src/container.rs`)

`with_network(NetworkMode::Bridge)` — the enum variant carries no data.

---

## Proposed Design

### Core Concept: Named Networks

A **network** is a named bridge with its own subnet, IPAM pool, NAT refcount,
and port-forward list. The default network is called `"pelagos0"` and behaves
exactly as today (backwards compatible).

### Network Definition

```rust
// src/network.rs

pub struct NetworkDef {
    pub name: String,           // bridge interface name, e.g. "pelagos0", "frontend"
    pub subnet: Ipv4Net,        // e.g. 10.0.1.0/24
    pub gateway: Ipv4Addr,      // e.g. 10.0.1.1 (always .1 of the subnet)
}
```

Where `Ipv4Net` is a simple struct holding a base address and prefix length
(we can use the `ipnet` crate or a small hand-rolled type).

### Persistence: `/var/lib/pelagos/networks/<name>/`

```
/var/lib/pelagos/networks/
  pelagos0/
    config.json          # { "name": "pelagos0", "subnet": "172.19.0.0/24", "gateway": "172.19.0.1" }
  frontend/
    config.json          # { "name": "frontend", "subnet": "10.0.1.0/24", "gateway": "10.0.1.1" }
```

This directory is the source of truth for "what networks exist." Created by
`pelagos network create`, read by `setup_bridge_network()`.

### Runtime State: `/run/pelagos/networks/<name>/`

```
/run/pelagos/networks/
  pelagos0/
    next_ip              # IPAM counter (per-network)
    nat_refcount         # NAT refcount (per-network)
    port_forwards        # DNAT entries (per-network)
  frontend/
    next_ip
    nat_refcount
    port_forwards
```

This replaces the current global `/run/pelagos/next_ip`, etc. Each network
has its own independent pool and refcount.

### CLI: `pelagos network` Subcommand

```
pelagos network create <name> --subnet <CIDR>
pelagos network ls [--format json]
pelagos network rm <name>
pelagos network inspect <name>
```

**`create`:**
- Validates subnet doesn't overlap with any existing network
- Writes `config.json` to `/var/lib/pelagos/networks/<name>/`
- Does NOT create the bridge yet (lazy — created on first container)

**`ls`:**
- Lists all networks from `/var/lib/pelagos/networks/*/config.json`
- Shows name, subnet, gateway, active container count (optional)

**`rm`:**
- Refuses if any container is currently on the network
- Deletes the bridge interface if it exists
- Removes `/var/lib/pelagos/networks/<name>/` and `/run/pelagos/networks/<name>/`

**`inspect`:**
- Shows network config + current IPAM state + active containers

### CLI: `--network` Flag Changes

```
# Current (unchanged — uses default pelagos0 bridge):
pelagos run --network bridge alpine /bin/sh

# New — join a named network:
pelagos run --network frontend alpine /bin/sh

# Explicit default:
pelagos run --network pelagos0 alpine /bin/sh
```

Parse logic: if the value is `"none"`, `"loopback"`, `"bridge"`, or `"pasta"`,
use the current mode semantics. Otherwise, treat it as a network name and
look up the network definition. This is backwards-compatible.

### Builder API Changes

```rust
// Current (unchanged):
Command::new("/bin/sh").with_network(NetworkMode::Bridge)

// New — named network:
Command::new("/bin/sh").with_network(NetworkMode::BridgeNamed("frontend".into()))

// Or alternatively, keep Bridge and add a separate method:
Command::new("/bin/sh")
    .with_network(NetworkMode::Bridge)
    .with_bridge_network("frontend")
```

**Recommendation:** Add a `BridgeNamed(String)` variant to `NetworkMode`.
`NetworkMode::Bridge` becomes sugar for `BridgeNamed("pelagos0".into())`.
Internally, both paths go through the same code.

### Default Network Bootstrap

On first use (if `/var/lib/pelagos/networks/pelagos0/config.json` doesn't exist),
the default network is auto-created with the current hardcoded values:
- name: `pelagos0`
- subnet: `172.19.0.0/24`
- gateway: `172.19.0.1`

This ensures zero behavior change for existing users.

---

## Implementation: Function-by-Function Changes

### `src/paths.rs`

Add per-network path helpers:

```rust
pub fn network_config_dir(name: &str) -> PathBuf {
    data_dir().join("networks").join(name)
}

pub fn network_runtime_dir(name: &str) -> PathBuf {
    runtime_dir().join("networks").join(name)
}

pub fn network_ipam_file(name: &str) -> PathBuf {
    network_runtime_dir(name).join("next_ip")
}

pub fn network_nat_refcount_file(name: &str) -> PathBuf {
    network_runtime_dir(name).join("nat_refcount")
}

pub fn network_port_forwards_file(name: &str) -> PathBuf {
    network_runtime_dir(name).join("port_forwards")
}
```

The old global `ipam_file()`, `nat_refcount_file()`, `port_forwards_file()`
can either delegate to `network_*("pelagos0")` or be deprecated.

### `src/network.rs`

**`ensure_bridge()`** → **`ensure_bridge(net: &NetworkDef)`**

Takes the bridge name and CIDR from the network definition instead of
constants.

**`allocate_ip()`** → **`allocate_ip(net: &NetworkDef)`**

Reads/writes `network_ipam_file(&net.name)`. Derives the IP pool range from
`net.subnet` (e.g. for 10.0.1.0/24: allocate 10.0.1.2–10.0.1.254).

**`setup_bridge_network()`** → add `net: &NetworkDef` parameter

Plumbs the network definition through to `ensure_bridge()`, `allocate_ip()`,
route setup (gateway from `net.gateway`), and veth attachment (bridge name
from `net.name`).

**`NFT_ADD_SCRIPT`** → **`build_nat_script(net: &NetworkDef)`**

Generate the nftables script dynamically with the network's subnet and bridge
name. Use a per-network nftables table name (e.g. `ip pelagos-frontend`) to
avoid rule collisions between networks.

**`enable_nat()` / `disable_nat()`** → accept `&NetworkDef`

Use per-network refcount file and per-network nftables table.

**`enable_port_forwards()` / `disable_port_forwards()`** → accept `&NetworkDef`

Use per-network port-forwards file and per-network nftables prerouting chain.

**`teardown_network()`**

`NetworkSetup` already stores the network name (today it stores `ns_name`).
Add the `NetworkDef` (or at least bridge name + subnet) so teardown knows
which refcount files and nftables tables to update.

### `src/container.rs`

Where `setup_bridge_network()` is called (~line 1698), load the `NetworkDef`
from disk based on the network name in the `NetworkMode` variant, then pass
it through.

### `src/cli/run.rs`

Update `parse_network_mode()` to handle named networks:

```rust
fn parse_network_mode(s: &str) -> Result<NetworkMode, ...> {
    match s.to_ascii_lowercase().as_str() {
        "none" | "" => Ok(NetworkMode::None),
        "loopback" => Ok(NetworkMode::Loopback),
        "bridge" => Ok(NetworkMode::BridgeNamed("pelagos0".into())),
        "pasta" => Ok(NetworkMode::Pasta),
        name => {
            // Verify network exists
            let config_path = paths::network_config_dir(name).join("config.json");
            if config_path.exists() {
                Ok(NetworkMode::BridgeNamed(name.into()))
            } else {
                Err(format!("network '{}' not found (create with: pelagos network create {})", name, name))
            }
        }
    }
}
```

### New file: `src/cli/network.rs`

Implements `cmd_network_create()`, `cmd_network_ls()`, `cmd_network_rm()`,
`cmd_network_inspect()`.

### `src/main.rs`

Add the `network` subcommand to clap.

---

## nftables Table Strategy

Each network gets its own nftables table to avoid rule collisions:

```
table ip pelagos-pelagos0 {
    chain postrouting { type nat hook postrouting priority 100; }
    chain forward { type filter hook forward priority 0; }
    chain prerouting { type nat hook prerouting priority -100; }
}

table ip pelagos-frontend {
    chain postrouting { ... }
    chain forward { ... }
    chain prerouting { ... }
}
```

This means NAT and port-forwarding for network A don't interfere with
network B. Each has independent refcounting and rule management.

The iptables FORWARD fallback rules also need to be per-subnet.

---

## Subnet Overlap Validation

`pelagos network create` must reject subnets that overlap with existing
networks. For two CIDRs A and B, they overlap if A contains B's network
address or B contains A's network address. This is straightforward with
bitwise comparison of the masked addresses.

---

## Migration / Backwards Compatibility

1. If `/var/lib/pelagos/networks/` doesn't exist (pre-multi-network install),
   the first `--network bridge` or `pelagos network ls` auto-creates the
   default `pelagos0` network definition.

2. `--network bridge` continues to mean "use the default pelagos0 bridge."
   No CLI change required for existing users.

3. Old global state files (`/run/pelagos/next_ip`, etc.) are migrated or
   ignored once per-network state takes over. Simplest approach: if
   `/run/pelagos/networks/pelagos0/next_ip` doesn't exist but
   `/run/pelagos/next_ip` does, copy it over on first access.

---

## Container-to-Container Isolation

Containers on different bridges are isolated at L2 — they cannot reach each
other without explicit routing. This is the primary value of multi-network.

Containers on the **same** bridge can reach each other freely (same as today).
If intra-network isolation is ever needed (Docker's `--icc=false`), that would
be a separate feature using ebtables/nftables bridge filtering rules.

---

## Scope and Effort

| Component                   | Effort    |
|-----------------------------|-----------|
| `NetworkDef` struct + serde | Quick     |
| `src/paths.rs` additions    | Quick     |
| `pelagos network create/ls/rm/inspect` CLI | Moderate |
| Parameterize `ensure_bridge` + `allocate_ip` | Moderate |
| Parameterize `setup_bridge_network` + nftables | Moderate |
| Update container builder + `NetworkMode` enum | Quick |
| Update `parse_network_mode` | Quick |
| Subnet overlap validation   | Quick     |
| Migration / default bootstrap | Quick   |
| Integration tests           | Moderate  |
| Documentation updates       | Quick     |

**Overall: Moderate effort.** The changes are pervasive (many functions gain
a `&NetworkDef` parameter) but individually straightforward. The main risk
is the nftables rule generation — per-network tables need careful testing
to ensure they don't interfere with each other or with external firewall
rules.

---

## Open Questions (resolved in implementation)

1. **Bridge name length limit.** ✅ User-supplied names are used as-is;
   `pelagos network create` rejects names that would exceed 15 bytes (IFNAMSIZ − 1).

2. **Subnet defaults.** ✅ Auto-assignment implemented via `--alloc-from` and
   `auto_alloc_pool` in `config.toml`. `--subnet` is optional; omitting it
   carves the next free /24 from the configured pool.

3. **Multi-network containers.** ✅ Implemented. `--network` is repeatable;
   additional networks are attached as eth1, eth2, …

4. **Network-scoped DNS.** ✅ Implemented. The builtin `pelagos-dns` daemon
   resolves container names within the network they are attached to.
