# Ongoing Tasks

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
