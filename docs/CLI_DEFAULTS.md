# Pelagos CLI Defaults — Opinionated Choices

This document explains the policy choices made at the **CLI layer** of pelagos.
These are distinct from the library layer: `pelagos::container::Command` is
unopinionated by design (callers set every flag explicitly). The CLI exists to
make the common case obvious and safe.

The guiding principle: **match Docker's defaults where Docker is right, deviate
where pelagos has reason to.**

---

## Namespace defaults for `pelagos run`

The OCI Runtime Specification does not mandate any default namespace set — it
is implementation-defined. Every major container runtime (Docker, Podman,
nerdctl/containerd) has independently converged on the same set:

| Namespace | Docker | Podman | pelagos CLI | Rationale |
|---|---|---|---|---|
| PID | ✓ | ✓ | ✓ | Process tree isolation; init reaping |
| MOUNT | ✓ | ✓ | ✓ | Filesystem isolation; auto-added with `--rootfs` |
| UTS | ✓ | ✓ | ✓ | Per-container hostname |
| IPC | ✓ | ✓ | ✓ | Prevent SysV shm/sem leakage between containers |
| NET | ✓ | ✓ | ✓ | Network isolation (see §Networking below) |
| CGROUP | ✓ | ✓ | ✓ | Container sees only its own cgroup subtree |
| USER | ✗ (rootful) | ✗ (rootful) | ✗ (rootful) | Root-mode containers don't need user ns |

The library (`Command::new()`) starts with **no namespaces**. The CLI layer
adds the full set above. This separation is intentional: library callers
control isolation explicitly; CLI users get safe defaults automatically.

### Why IPC matters

SysV shared memory and POSIX message queues are keyed by integers. Two
processes in the same IPC namespace that happen to choose the same key can
attach to each other's segments. Without `CLONE_NEWIPC`, any two running
containers share the root IPC namespace and can interfere with each other.
Docker has isolated IPC by default since its first release.

### Why CGROUP matters

Without `CLONE_NEWCGROUP`, the container sees the full host cgroup hierarchy
at `/sys/fs/cgroup`. A container that reads `/sys/fs/cgroup` can enumerate
all cgroups on the system, including those of other containers. With a cgroup
namespace the container sees only its own subtree as `/sys/fs/cgroup`.

---

## Networking defaults

### The core rule

| Invocation | Network mode | Rationale |
|---|---|---|
| `pelagos run IMAGE` | `Loopback` | Isolated loopback-only ns; safe default with no external access |
| `pelagos run -p HOST:CONTAINER IMAGE` | `Bridge` (auto-promoted) | Port publishing requires per-container IP; bridge is the efficient path |
| `pelagos run --network bridge IMAGE` | `Bridge` | Explicit |
| `pelagos run --network none IMAGE` | `None` (host ns) | Explicit opt-in to host networking; no isolation |
| `pelagos run --network pasta IMAGE` | `Pasta` | Explicit; rootless-compatible userspace relay |

### Why not `NetworkMode::None` by default

`None` means the container runs in the root VM network namespace. All
containers share a single network stack. Two containers publishing the same
container-side port (e.g., both `nginx` on port 80) produce `EADDRINUSE` on
the second container — there is nothing to fix at the pelagos-mac layer; the
conflict is real and in the kernel.

### Why loopback (not bridge) as the no-flags default

A new bridge namespace requires the `pelagos0` bridge to be pre-configured
and a free IP from IPAM. If `pelagos run` is the very first command on a fresh
install, the bridge may not exist yet. Loopback is always available and
provides full isolation — it just has no external connectivity, which is
correct when no `-p` flags were given.

### Why bridge (not pasta) for `-p` auto-promotion

| | Bridge | Pasta |
|---|---|---|
| Packet path | kernel DNAT → container netns | userspace relay process → TAP |
| Per-container overhead | veth pair + IPAM entry | additional process |
| Root required | yes | no (but works as root too) |
| Suitable in pelagos VM | yes | yes |

Both provide isolation. Bridge uses kernel routing after the first hop and
avoids an extra userspace copy per packet. In pelagos's VM environment (always
root, bridge pre-configured) bridge is the efficient choice. Pasta remains
available via explicit `--network pasta`.

### The relay fix (pelagos-mac)

The smoltcp NAT relay in pelagos-mac (`nat_relay.rs`) currently sends
`container_port` in its 2-byte handshake to the VM-side proxy. With bridge
mode, pelagos's userspace proxy listens on `host_port` in the root namespace —
the relay must send `host_port` instead. This is a coordinated change in
pelagos-mac tracked in pelagos-mac#57.

---

## Escape hatches

All defaults can be overridden explicitly:

```bash
# No network namespace at all (host networking):
pelagos run --network none nginx

# Loopback only (explicit, same as default without -p):
pelagos run --network loopback nginx

# Bridge with port publishing:
pelagos run --network bridge --nat --publish 8080:80 nginx

# Pasta (rootless-compatible):
pelagos run --network pasta --publish 8080:80 nginx
```

---

## Consistency with compose and the lisp runtime

`compose.rs` and `lisp/runtime.rs` both add `Namespace::IPC` on top of
whatever the CLI passes. `run.rs` previously did not. This inconsistency is
fixed: `run.rs` now includes IPC in its baseline, so all three entry points
produce the same namespace set.

---

## References

- [OCI Runtime Specification — Linux Namespaces](https://github.com/opencontainers/runtime-spec/blob/main/config-linux.md#namespaces)
- [Docker default networking](https://docs.docker.com/network/)
- [Podman run defaults](https://docs.podman.io/en/latest/markdown/podman-run.1.html)
- pelagos#142 — tracking issue for implementing these defaults
- pelagos-mac#57 — relay protocol fix required for bridge-mode port forwarding
