# Ongoing Tasks

## Current Task: Multi-Network Containers

Allow containers to join multiple bridge networks simultaneously. This enables
network isolation patterns like frontend/backend separation.

Target architecture:
```
frontend (10.88.1.0/24):  proxy ←→ app
backend  (10.88.2.0/24):           app ←→ redis
```
Redis is isolated from proxy — they share no network.

### Step 1: Network layer — `attach_network_to_netns()`

**File: `src/network.rs`**

New function `attach_network_to_netns(ns_name, network_name, iface_name)`:
1. Load NetworkDef, ensure bridge exists
2. Allocate IP from per-network IPAM
3. Generate unique veth pair via `veth_names_for_network(ns_name, network_name)`
4. Create veth pair, move peer into existing netns, rename to `iface_name`
5. Assign IP, bring up, add subnet route only (no default route)
6. Attach host-side veth to bridge

New `teardown_secondary_network(setup)` — deletes veth only (not the netns).

New `veth_names_for_network(ns_name, network_name)` — FNV-1a hash of
`"ns_name:network_name"` for unique veth names.

### Step 2: Container multi-network support

**File: `src/container.rs`**

- Add `additional_networks: Vec<String>` to Command
- Add `with_additional_network(network_name)` builder method
- In spawn()/spawn_interactive(): after primary bridge setup, iterate additional_networks
  and call `attach_network_to_netns()` for each
- Add `secondary_networks: Vec<NetworkSetup>` to Child
- Add `container_ips()` and `container_ip_on(network_name)` accessors
- Teardown: secondary networks torn down before primary

### Step 3: Smart link resolution

Update link resolution in spawn() to check shared networks first.
Add `resolve_container_ip_on_network(name, network_name)`.

### Step 4: Container state with multiple IPs

Add `network_ips: HashMap<String, String>` to ContainerState.
Populate from `child.container_ips()` after spawn.

### Step 5: CLI `--network` repeatable

Change `--network` from `String` to `Vec<String>`. First value is primary,
additional values are secondary bridge networks.

### Step 6: Integration tests

4 tests in `multi_network` module:
- `test_multi_network_dual_interface`
- `test_multi_network_isolation`
- `test_multi_network_teardown`
- `test_multi_network_link_resolution`

### Step 7: Update web-stack example

Create frontend/backend networks, launch with isolation, add isolation test.

### Step 8: Documentation

Update CLAUDE.md, INTEGRATION_TESTS.md.

---

## Previously Completed

### High-Impact Quick Wins (v0.3.3)
- **ENTRYPOINT/LABEL/USER** build instructions
- **Build cache**: sha256(parent_layer + instruction) keyed
- **Localhost port forwarding**: userspace TCP proxy

### Multi-Network Support (v0.3.2)
- User-defined bridge networks with per-network subnets, IPAM, NAT
- `remora network create/ls/rm/inspect` CLI
- `--network <name>` on run/build

### JSON Output + Container Inspect (v0.3.2)
- `--format json` on all list commands
- `remora container inspect <name>`

### `remora build` (v0.3.0)
- Remfile parser, overlay snapshot per RUN step, build cache

### Full feature list in CLAUDE.md
