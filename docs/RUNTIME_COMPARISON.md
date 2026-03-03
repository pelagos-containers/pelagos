# Pelagos vs Established Container Runtimes

**Last Updated:** 2026-02-20
**Compared Against:** runc (OCI reference), Docker Engine, Podman

---

## What Pelagos Is

- ✅ Low-level container runtime **library** (like liblxc, not like Docker)
- ✅ Focused on Linux namespaces, seccomp, cgroups, and native networking
- ✅ Ergonomic Rust API for embedding containers in applications
- ✅ OCI image pull + daemonless image build (`remora build`)
- ✅ OCI Runtime Spec v1.0.2 Phase 1 (create/start/state/kill/delete + config.json parsing)

---

## Feature Matrix

### Legend
- ✅ Implemented and tested
- ⚠️ Partial — basic implementation, gaps remain
- ❌ Not implemented
- 🚫 Out of scope

| Feature | Pelagos | runc | Docker | Notes |
|---------|--------|------|--------|-------|
| **Namespaces** |
| UTS | ✅ | ✅ | ✅ | |
| Mount | ✅ | ✅ | ✅ | |
| IPC | ✅ | ✅ | ✅ | |
| Network | ✅ | ✅ | ✅ | Loopback + bridge; see Networking |
| User | ✅ | ✅ | ✅ | Rootless auto-detection + multi-UID mapping |
| Cgroup | ✅ | ✅ | ✅ | |
| PID | ⚠️ | ✅ | ✅ | Works in library; CLI limitation |
| **Filesystem** |
| chroot | ✅ | ✅ | ✅ | |
| pivot_root | ✅ | ✅ | ✅ | |
| Auto /proc /sys /dev | ✅ | ✅ | ✅ | |
| Read-only rootfs | ✅ | ✅ | ✅ | `MS_RDONLY` remount |
| Bind mounts (RW + RO) | ✅ | ✅ | ✅ | |
| tmpfs mounts | ✅ | ✅ | ✅ | |
| Named volumes | ✅ | ✅ | ✅ | Backed by `/var/lib/remora/volumes/` |
| Overlay filesystem | ✅ | ✅ | ✅ | `with_overlay(upper, work)` |
| **Security** |
| Seccomp (Docker profile) | ✅ | ✅ | ✅ | Pure-Rust via `seccompiler` |
| No-new-privileges | ✅ | ✅ | ✅ | `PR_SET_NO_NEW_PRIVS` |
| Masked paths | ✅ | ✅ | ✅ | `/proc/kcore`, `/sys/firmware`, etc. |
| Capability management | ✅ | ✅ | ✅ | Drop all or keep specific caps |
| AppArmor / SELinux | ❌ | ✅ | ✅ | Planned |
| **Resource Limits** |
| rlimits | ✅ | ✅ | ✅ | Memory, CPU, FDs, processes |
| Cgroups v2 memory | ✅ | ✅ | ✅ | `with_cgroup_memory()` |
| Cgroups v2 CPU | ✅ | ✅ | ✅ | Weight + quota |
| Cgroups v2 PIDs | ✅ | ✅ | ✅ | |
| Resource stats | ✅ | ✅ | ✅ | `child.resource_stats()` |
| I/O bandwidth limits | ❌ | ✅ | ✅ | Requires block device numbers |
| **Networking** |
| Loopback isolation | ✅ | ✅ | ✅ | ioctl in pre_exec |
| Bridge (veth + IPAM) | ✅ | ✅ | ✅ | `remora0`, 172.19.0.x/24 |
| NAT / MASQUERADE | ✅ | ✅ | ✅ | nftables, reference-counted |
| Port mapping (DNAT) | ✅ | 🚫 | ✅ | TCP; `with_port_forward()` |
| DNS configuration | ✅ | ✅ | ✅ | `with_dns()` writes resolv.conf |
| CNI plugins | ❌ | ✅ | ✅ | Not planned (native approach) |
| **Process Management** |
| Spawn + wait | ✅ | ✅ | ✅ | |
| Interactive PTY | ✅ | ✅ | ✅ | `spawn_interactive()` |
| SIGWINCH forwarding | ✅ | ✅ | ✅ | |
| Signal sending | ⚠️ | ✅ | ✅ | Via `std::process::Child::kill()` |
| Exec into container | ✅ | ✅ | ✅ | `remora exec` (ns join + PTY) |
| **OCI** |
| OCI config.json | ✅ | ✅ | ✅ | Phase 1 fields (see ROADMAP) |
| OCI bundle format | ✅ | ✅ | ✅ | create/start/state/kill/delete |
| OCI lifecycle hooks | ✅ | ✅ | ✅ | prestart/poststart/poststop |
| **Image Build** |
| Image build | ✅ | 🚫 | ✅ | `remora build` (Remfile), daemonless |
| **Rootless** |
| Unprivileged mode | ✅ | ✅ | ✅ | Auto-detection, USER ns, pasta, overlay |
| Subuid/subgid | ✅ | ✅ | ✅ | newuidmap/newgidmap helpers |
| **Testing** |
| Integration tests | ✅ | ✅ | ✅ | 72+ tests, all passing |
| Unit tests | ✅ | ✅ | ✅ | |

---

## Parity Estimate

| vs | Estimate | Notes |
|----|----------|-------|
| runc | ~80% | Missing: AppArmor/SELinux, CRIU, Intel RDT, seccomp arg conditions, I/O cgroups, some OCI hooks |
| Docker Engine | ~50% | Docker is a full platform (daemon, swarm, compose) — not a fair comparison |

---

## Pelagos Strengths

- **Rust library API** — embed containers directly in Rust applications, no daemon
- **No CNI dependency** — native loopback, bridge, and NAT without external plugins
- **Composable** — mix security, resource, filesystem, and networking features freely
- **Transparent** — all behaviour is in `src/`; nothing hidden in a daemon or shim
