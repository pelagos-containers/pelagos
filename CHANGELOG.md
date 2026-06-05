# Changelog

All notable changes to Pelagos will be documented in this file.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)

## [Unreleased]

### Changed

- **Breaking (rootless runtime state):** The runtime directory for rootless users
  is now always `$XDG_RUNTIME_DIR/pelagos/` (fallback `/tmp/pelagos-<uid>/`),
  regardless of whether `/run/pelagos/` exists on the system.  Previously,
  rootless users were silently redirected to `/run/pelagos/` once root had
  initialised it, accidentally sharing runtime state across privilege domains.

  The new behaviour matches Podman and containerd: images and layers remain
  shared (via `/var/lib/pelagos/` and the `pelagos` group), but running
  containers, networking state, DNS, and compose are per-UID.

  **Upgrade impact (within a single boot session only):** rootless containers
  started before upgrading will not appear in `pelagos ps` after the upgrade,
  because their state files remain in `/run/pelagos/containers/` while the CLI
  now reads from `$XDG_RUNTIME_DIR/pelagos/containers/`.  Stopped containers
  are harmless orphans; running containers must be located and killed manually
  (`pgrep -a pelagos` or inspect `/run/pelagos/containers/` directly).  After
  a reboot `/run/` is cleared and the issue cannot recur.

### Added
- Container runtime with Linux namespace isolation (UTS, Mount, IPC, User, Net, Cgroup, PID)
- CLI: `pelagos run`, `ps`, `stop`, `rm`, `logs`, `exec`
- OCI image support: `pelagos image pull/ls/rm`, `pelagos run --image`
- OCI runtime interface: `create/start/state/kill/delete` for containerd/CRI-O
- Networking: loopback, bridge (veth + pelagos0), NAT, port forwarding, DNS, pasta (rootless)
- Container linking: `--link` with /etc/hosts injection
- Storage: bind mounts, tmpfs, named volumes, overlay filesystem
- Security: seccomp-BPF (Docker default + minimal profiles), capabilities, no-new-privileges, read-only rootfs, masked paths
- Resource limits: cgroups v2 (memory, CPU, PIDs) + rlimits
- Interactive containers with PTY support and SIGWINCH forwarding
- Rootless mode with auto-detection
- `pelagos exec` to run commands in running containers
- Container exec with namespace discovery and environment inheritance
- CI pipeline with GitHub Actions (lint, unit tests, integration tests)
- Binary releases for x86_64 Linux (musl static builds supported manually)
