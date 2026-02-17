# Ongoing Tasks

## Current Task: None — see Planned section

### OCI Phase 2 — COMPLETE ✅

All fields implemented and tested (61 integration tests passing):

- `process.capabilities` → `with_capabilities()`
- `linux.maskedPaths` → `with_masked_paths()`
- `linux.readonlyPaths` → `with_readonly_paths()`
- `linux.resources` → `with_cgroup_memory()` / `with_cgroup_cpu_*()` / `with_cgroup_pids_limit()`
- `process.rlimits` → `with_rlimit()`
- `linux.sysctl` → `with_sysctl()` (new builder; writes to `/proc/sys/` in pre_exec)
- `linux.devices` → `with_device()` (new builder; `mknod` in pre_exec)
- `hooks.prestart` / `poststart` / `poststop` → `run_hooks()` in `cmd_create/start/delete`
- `linux.seccomp` → `filter_from_oci()` in `src/seccomp.rs` → `with_seccomp_program()`

---

## Planned (after OCI Phase 2)

- **Rootless Mode** — unprivileged user namespaces; slirp4netns vs pasta decision needed
- **AppArmor / SELinux** — MAC profile support; defence-in-depth on top of seccomp
