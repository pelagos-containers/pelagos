# Ongoing Tasks

## Session 2026-05-28 (continued) — Issue #261 native nfnetlink (branch refactor/native-nfnetlink-261)

Base SHA d00fbf5 (main, v0.63.0).

### Goal

Eliminate the remaining `nft` binary shell-outs from `network.rs` (Issue #261, #260 Item 3).
All writes go via `run_nft` / `run_nft_quiet`; one read goes via `SysCmd::new("nft") -a list chain`
(`find_jump_rule_handles`). Replace all with a native NETLINK_NETFILTER (nfnetlink) client.

### Approach

New file `src/nfnetlink.rs` — raw nfnetlink using only libc, no new crate deps.
Wire format confirmed via strace. Consistent with `src/netlink.rs` (RTNETLINK).

### nfnetlink protocol notes (from strace)

- `AF_NETLINK, SOCK_RAW, NETLINK_NETFILTER` socket
- Batch: `NFNL_MSG_BATCH_BEGIN` (family=AF_UNSPEC, res_id=10) + ops + `NFNL_MSG_BATCH_END`
- Each op: `nlmsghdr` + `nfgenmsg` (family, NFNETLINK_V0, res_id=0) + nested NLA attrs
- Table: NFTA_TABLE_NAME (type 1, str)
- Chain: NFTA_CHAIN_TABLE(1,str), NFTA_CHAIN_NAME(3,str), NFTA_CHAIN_TYPE(7,str),
         NFTA_CHAIN_HOOK(4,nested: NFTA_HOOK_HOOKNUM(1,u32) + NFTA_HOOK_PRIORITY(2,u32))
- Rule: NFTA_RULE_TABLE(1,str), NFTA_RULE_CHAIN(2,str), NFTA_RULE_EXPRESSIONS(4,nested list)
- Expression list: repeated NFTA_LIST_ELEM (type 1, nested: NFTA_EXPR_NAME(1,str) + NFTA_EXPR_DATA(2,nested))

### Expressions needed

| Expression | Encoding notes |
|---|---|
| `payload_load` | dreg(1,u32), base(2,u32), offset(3,u32), len(4,u32) |
| `bitwise` | sreg(1), dreg(2), len(3), mask(4,nested data), xor(5,nested data) |
| `cmp` | sreg(1,u32), op(2,u32 0=EQ/1=NEQ), data(3,nested NFTA_DATA_VALUE) |
| `meta` | dreg(1,u32), key(2,u32 6=iifname/7=oifname) |
| `masq` | empty NFTA_EXPR_DATA |
| `verdict` | via immediate: data is NFTA_DATA_VERDICT nested {code(1,u32), chain(2,str)} |
| `immediate` | dreg(1,u32), data(2,nested NFTA_DATA_VALUE or NFTA_DATA_VERDICT) |
| `nat` (DNAT) | type(1,u32 0=DNAT), family(2,u32), addr_reg_min(3,u32), proto_reg_min(7,u32) |

CIDR match strategy: load 4 bytes + bitwise AND mask + cmp (handles any prefix length).
Interface name: null-padded to IFNAMSIZ (16) bytes in cmp DATA.
Port match: payload base=TRANSPORT offset=2 len=2, cmp against u16 big-endian.

### Functions to implement in nfnetlink.rs

Low-level:
- `open_nfnetlink() -> RawFd`
- `send_batch(fd, ops: &[u8]) -> io::Result<()>`  ← batch_begin + ops + batch_end, recv ACKs
- `nlattr_*` builders (str, u32, u64, nested, data_value)
- `nfgenmsg_header(family, type, flags, seq)` → Vec<u8>

High-level (what network.rs calls):
- `pub fn nft_create_nat_masquerade(table, bridge, cidr) -> io::Result<()>`
  - add table ip TABLE
  - add chain ip TABLE postrouting { type nat hook postrouting priority 100 }
  - add rule ip TABLE postrouting: payload(saddr) + bitwise(mask) + cmp(EQ,net) +
                                    meta(oifname) + cmp(NEQ,bridge) + masq
  - add chain ip TABLE forward { type filter hook forward priority -100 }
  - add rule ip TABLE forward: payload(saddr) + bitwise + cmp(EQ,net) + verdict(ACCEPT)
  - add rule ip TABLE forward: payload(daddr) + bitwise + cmp(EQ,net) + verdict(ACCEPT)
- `pub fn nft_flush_postrouting(table) -> io::Result<()>`
- `pub fn nft_delete_ip_table(table) -> io::Result<()>`
- `pub fn nft_delete_ip6_table(table) -> io::Result<()>`
- `pub fn nft_add_dns_input_chain(table, bridge) -> io::Result<()>`
  - add table ip TABLE
  - add chain ip TABLE input { type filter hook input priority -100 }
  - flush chain ip TABLE input
  - add rule ip TABLE input: meta(iifname) + cmp(EQ,bridge) + payload(udp.dport) + cmp(EQ,53) + verdict(ACCEPT)
- `pub fn nft_remove_dns_input_chain(table) -> io::Result<()>`
  - flush chain ip TABLE input
  - delete chain ip TABLE input
- `pub fn nft_add_filter_forward_compat(chain, cidr) -> io::Result<()>` (iptables-nft)
  - add chain ip filter CHAIN
  - flush chain ip filter CHAIN
  - add rule ip filter CHAIN: saddr+mask+cmp + accept
  - add rule ip filter CHAIN: daddr+mask+cmp + accept
  - add rule ip filter FORWARD: verdict(JUMP, CHAIN)
- `pub fn nft_del_filter_chain_and_jump(table_family, table, chain) -> io::Result<()>`
  - find handles of jump TARGET in base_chain → delete each
  - flush chain TABLE CHAIN
  - delete chain TABLE CHAIN
- `pub fn nft_add_filter_input_compat(chain, bridge) -> io::Result<()>` (iptables-nft)
- `pub fn nft_del_filter_input_compat(chain) -> io::Result<()>`
- `pub fn nft_install_dnat(table, entries: &[(Ipv4Addr,u16,u16,PortProto)]) -> io::Result<()>`
  - add table ip TABLE; add chain ip TABLE prerouting { type nat hook prerouting priority -100 }
  - flush chain ip TABLE prerouting
  - for each entry: add rule: dport match + immediate(ip) + immediate(port) + nat DNAT
- `pub fn nft_install_dnat6(table, entries: &[(Ipv6Addr,u16,u16,PortProto)]) -> io::Result<()>`
- `pub fn nft_flush_prerouting(table) -> io::Result<()>`
- `pub fn nft_find_jump_handles(family_u8, table, chain, target) -> Vec<u64>`
  - GETRULE DUMP request, parse NFT_MSG_NEWRULE responses, return handles where exprs contain jump target
- `pub fn nft_delete_rule(family_u8, table, chain, handle) -> io::Result<()>`

### network.rs changes

Replace:
- `run_nft`, `run_nft_quiet` → removed
- `find_jump_rule_handles`, `delete_jump_rules` → removed
- `build_nat_script`, `build_prerouting_script`, `build_prerouting6_script` → removed
- `add_iptables_nft_forward_compat`, `remove_iptables_nft_forward_compat` → call nfnetlink
- `add_iptables_nft_dns_compat`, `remove_iptables_nft_dns_compat` → call nfnetlink
- `add_dns_input_rule`, `remove_dns_input_rule` → call nfnetlink
- `enable_nat` (run_nft calls) → call nfnetlink
- `disable_nat` (run_nft calls) → call nfnetlink
- `enable_port_forwards` (run_nft calls) → call nfnetlink
- `disable_port_forwards` (run_nft calls) → call nfnetlink

Remove: `use std::process::Command as SysCmd` (pasta still uses std::process::Command directly in its own scope).

### Tests to add

Integration tests in `tests/integration_tests.rs` (mod `nfnetlink_native`):
- `test_nft_nat_create_delete`: create NAT table, verify with `nft list table ip`, delete, verify gone
- `test_nft_dns_input_rule`: add DNS INPUT chain, verify rule, remove, verify gone
- `test_nft_dnat_rules`: enable port-forward, verify DNAT rule in prerouting, disable, verify cleaned up
- `test_nft_iptables_filter_compat`: (guarded by `ip filter` exists) add forward compat chain, verify jump, remove

All existing NAT/port-forward/DNS integration tests serve as regression tests.

---

## Session 2026-05-28 — Issue #260 native netlink (COMPLETE ✅), v0.63.0 released

Base SHA 0e8ca13 (main) → 7769d1a (v0.63.0).

### What was done this session

**Issue #260 Items 1 & 2: eliminate all CLI shell-outs from networking code**

Item 1 (`470f32c`): replace `nft` binary calls with `nftables-sys` crate.
Item 2 (`29e029e`): add `src/netlink.rs` (RTNETLINK/ioctl) replacing all `ip` CLI calls
in `network.rs`. Removed the `run()` helper entirely.

**Pasta bind-mount race fix (`b0ff4d4`)**:
- Root cause: `in_netns` thread spawning added ~5 threads per bridge container, increasing
  parent load enough to trigger a race where fast commands (echo) exited before the parent
  could bind-mount `/proc/{pid}/ns/net` for pasta.
- First attempted fix (self-SIGSTOP in pre_exec) deadlocked: Rust's `Command::spawn()` blocks
  on an internal SEQPACKET socket until the child exec's — stopping before exec prevented exec.
- Real fix: child bind-mounts `/proc/thread-self/ns/net` → `/run/pelagos/pasta-ns/{pid}` in
  pre_exec step 1.61, before exec, while still in the host mount namespace. The bind-mount
  survives exec and process exit. `setup_pasta_network` detects it via `statfs(NSFS_MAGIC)`.

**v0.63.0 released**: merged to main, tagged, release workflow triggered.

### Test baseline

334/334 integration tests pass (352/352 unit tests).

### Remaining Issue #260 work

- [ ] **Item 3**: replace `nft` shell-outs with native nfnetlink (nftables-sys calls for
  per-container rule management, not just the bulk setup already done in Item 1).

### Other parked items

- [ ] Issue #259: IPv6 end-to-end verification on Mac (pelagos-mac branch
  `feat/bridge-networking-vm-alpine-fallback`)
- [ ] Issue #141: multiple containers binding the same container port
- [ ] `kubectl exec` CRI streaming (unimplemented in pelagos-cri; returns protocol error)
- [ ] pasta not installed on ipc2/ipc3 (falls back to loopback): `sudo apt-get install passt`

---

## Session 2026-05-25/26 — multi-node k3s validation (issue #243) COMPLETE ✅

Base SHA 2995e8d → 601c2f0 (main). Continued from prior session where issue #239 (single-node
nginx acceptance criterion) was met and merged as PR #242.

### What was done this session

Deployed pelagos-cri to ipc2 (192.168.88.52) and ipc3 (192.168.88.54), joined them as
k3s agents. This exposed 6 bugs in pelagos-cri fixed in commit 184f39b:

1. **CRI entrypoint semantics** — when `container.entrypoint` is empty, must fall back to
   image ENTRYPOINT. Fixes coredns (scratch image with `/coredns` entrypoint).
2. **Better error logging** — log full stdout+stderr from pelagos run on failure.
3. **Stale sandbox purge on startup** — AppState::new() checks pause_pid liveness; purges
   dead sandboxes + their containers. Fixes "sandbox not found" after pelagos-cri restart.
4. **Sandbox netns sysctl** — set `net.ipv4.ip_unprivileged_port_start=0` in CNI netns via
   `nsenter --net=<path>`. Required for coredns (nonroot user, binds port 53).
5. **runAsUser/runAsGroup from CRI security context** — pass `--user uid:gid` to pelagos
   run. Required for projected volumes (serviceaccount token mode 600, owned by runAsUser).
6. **SystemD unit fix** — remove `RuntimeDirectory=pelagos` (wiped /run/pelagos/ on restart).
   Use `ExecStartPre=/usr/bin/mkdir -p /run/pelagos` instead.

### Formal acceptance test results (issue #243 CLOSED)

Cross-node Service reachability verified with `xnode-test` deployment (3 replicas, one per node,
ECR-mirrored nginx:alpine) + ClusterIP service `xnode-test-svc` (10.43.214.132:80):

| Source | Destination | Result |
|---|---|---|
| ipc2 pod (10.42.5.7) | ClusterIP 10.43.214.132 | HTTP 200 ✅ |
| ipc2 pod (10.42.5.7) | ipc1 pod 10.42.0.80 | HTTP 200 ✅ |
| ipc2 pod (10.42.5.7) | ipc3 pod 10.42.6.231 | HTTP 200 ✅ |
| ipc3 pod (10.42.6.231) | ClusterIP 10.43.214.132 | HTTP 200 ✅ |
| ipc3 pod (10.42.6.231) | ipc1 pod 10.42.0.80 | HTTP 200 ✅ |
| ipc3 pod (10.42.6.231) | ipc2 pod 10.42.5.7 | HTTP 200 ✅ |
| ipc1 host | ClusterIP 10.43.214.132 | HTTP 200 ✅ |
| ipc2 host | ClusterIP 10.43.214.132 | HTTP 200 ✅ |
| ipc3 host | ClusterIP 10.43.214.132 | HTTP 200 ✅ |

Flannel VXLAN cross-node pod networking is fully functional with pelagos-cri.

### Remaining known gaps (not blocking #243)

- [ ] `kubectl exec` — streaming Exec API unimplemented in pelagos-cri; returns
  "error stream protocol error: unknown error" — next major CRI work item
- [ ] pasta not installed on ipc2/ipc3 — `sudo apt-get install passt` (containers fall back to loopback)
