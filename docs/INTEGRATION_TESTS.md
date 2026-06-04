# Pelagos Integration Test Reference

Every integration test in `tests/integration_tests.rs` is documented here.
**When adding a new integration test, add its entry to this file in the same commit.**

Run with:
```bash
sudo -E cargo test --test integration_tests
```

Tests that do not require root skip themselves with `eprintln!("Skipping ...")` and return.
Tests that require `alpine-rootfs` skip themselves if it is absent.

---

## Conventions

- **Requires root**: must be run via `sudo -E cargo test ...`
- **Requires rootfs**: skips if `alpine-rootfs/bin/busybox` is not found
- **API-only**: compiles/runs without root or rootfs — just checks builder API shape

---

## No-Root / API Tests

These exercise the type system and builder API. They never spawn a process.

### `test_uid_gid_api`
**Type:** API-only

Verifies that `with_uid()`, `with_gid()`, `with_uid_maps()`, and `with_gid_maps()` exist,
accept the right types, and chain correctly. No process is spawned.

### `test_namespace_bitflags`
**Type:** API-only

Confirms that `Namespace` bitflags compose correctly via `|` and that `contains()` and
`!contains()` return expected results for `UTS`, `MOUNT`, and `PID`.

### `test_capability_bitflags`
**Type:** API-only

Same as above for `Capability` flags: `CHOWN`, `NET_BIND_SERVICE`, and `SYS_ADMIN`.

### `test_command_builder_pattern`
**Type:** API-only

Chains several builder methods (`args`, `stdin`, `stdout`, `stderr`, `with_namespaces`,
`with_chroot`, `env`, `with_proc_mount`, `with_max_fds`) and verifies it compiles.

### `test_seccomp_profile_api`
**Type:** API-only

Verifies all four seccomp builder methods compile and chain:
`with_seccomp_default()`, `with_seccomp_minimal()`, `with_seccomp_profile(Docker)`,
`without_seccomp()`.

---

## Core Container Tests

### `test_basic_namespace_creation`
**Requires:** root, rootfs

Spawns `ash -c "exit 0"` inside `UTS | MOUNT` namespaces with `chroot`. Verifies
that `spawn()` and `wait()` succeed — the baseline test that unshare + chroot works.

### `test_proc_mount`
**Requires:** root, rootfs

Runs `test -f /proc/self/status` inside a container with `with_proc_mount()`. Verifies
that procfs is mounted correctly so the container can see its own kernel metadata.

### `test_capability_dropping`
**Requires:** root, rootfs

Calls `drop_all_capabilities()` and verifies `ash -c "exit 0"` still exits cleanly.
Proves the capability drop itself doesn't prevent a minimal shell from running.

### `test_selective_capabilities`
**Requires:** root, rootfs

Calls `with_capabilities(NET_BIND_SERVICE | CHOWN)` — keeps only two capabilities,
drops all others — and verifies the container exits cleanly.

### `test_resource_limits_fds`
**Requires:** root, rootfs

Sets `with_max_fds(100)` (RLIMIT_NOFILE) and runs `test "$(ulimit -n)" = 100` inside
the container. Verifies the rlimit is visible to the process as expected.

### `test_resource_limits_memory`
**Requires:** root, rootfs

Sets `with_memory_limit(512MB)` (RLIMIT_AS) and runs `exit 0`. Smoke-tests that the
rlimit can be applied without preventing the process from starting.

### `test_resource_limits_cpu`
**Requires:** root, rootfs

Sets `with_cpu_time_limit(60)` (RLIMIT_CPU) and runs `exit 0`. Smoke-tests that a
60-second CPU time limit can be applied without breaking a trivial process.

### `test_combined_features`
**Requires:** root, rootfs

Combines `MOUNT | UTS | CGROUP` namespaces, `with_proc_mount()`,
`with_capabilities(NET_BIND_SERVICE)`, `with_max_fds(500)`, and `with_memory_limit(256MB)`.
Verifies that multiple features coexist without conflict.

---

## Seccomp Filter Tests

### `test_seccomp_docker_blocks_reboot`
**Requires:** root, rootfs

Applies the Docker seccomp profile and runs `reboot` inside the container. Verifies
the process exits (with code 0 or 1) without actually rebooting — proving the `reboot`
syscall is blocked by the BPF filter.

### `test_seccomp_docker_allows_normal_syscalls`
**Requires:** root, rootfs

Applies the Docker seccomp profile and runs `echo`. Verifies that read, write, brk,
and other everyday syscalls are not blocked — the filter should only restrict dangerous ones.

### `test_seccomp_minimal_is_restrictive`
**Requires:** root, rootfs

Applies the minimal seccomp profile and attempts `exit 0`. Does not assert success or
failure — the minimal profile may be too strict for even `ash` to start. Verifies
only that the filter compiles and can be applied without a Rust error.

### `test_seccomp_profile_api`
**Type:** API-only

Verifies the four seccomp builder methods exist and compile (no process spawned). See
the API-only section.

### `test_seccomp_without_flag_works`
**Requires:** root, rootfs

Runs `echo` with no seccomp configuration at all. Confirms baseline operation is
unaffected when seccomp is not used.

### `test_seccomp_docker_blocks_io_uring`
**Requires:** root, rootfs, C compiler (`cc`/`gcc`)

Compiles `scripts/iouring-test-context/iouring_probe.c` as a static binary, bind-mounts
it into the container, and runs it under the Docker default seccomp profile. The probe
calls `io_uring_setup(0, NULL)` directly via `syscall(2)` and exits 1 if it receives
`EPERM`. Test asserts exit code 1, confirming the three io_uring syscalls
(`io_uring_setup`, `io_uring_enter`, `io_uring_register`) are blocked by the default
profile. Skipped if no C compiler is available.

### `test_seccomp_iouring_profile_allows_io_uring`
**Requires:** root, rootfs, C compiler (`cc`/`gcc`)

Same workload binary run under `SeccompProfile::DockerWithIoUring` (via
`with_seccomp_allow_io_uring()`). Asserts exit code 0, confirming the opt-in profile
correctly removes the io_uring restriction and the kernel accepts the call. Skipped if
no C compiler is available.

### `test_seccomp_iouring_e2e`
**Requires:** root, rootfs, C compiler (`cc`/`gcc`)

Runs `scripts/iouring-test-context/iouring_workload.c` under `DockerWithIoUring`. The
workload performs a complete io_uring round-trip: `io_uring_setup` to create the ring,
mmap of the SQ/CQ rings and SQE array, submission of an `IORING_OP_NOP` SQE, and
`io_uring_enter(IORING_ENTER_GETEVENTS)` to wait for its CQE. Asserts exit 0, meaning
the NOP CQE was received with `result == 0`. This is the definitive proof that io_uring
works end-to-end inside a pelagos container — not just that the syscall is unblocked,
but that the kernel io_uring machinery is fully functional. Skipped if no C compiler is
available.

---

## Phase 1 Security Tests

### `test_no_new_privileges`
**Requires:** root, rootfs

Calls `with_no_new_privileges(true)` and reads `/proc/self/status` inside the container.
Greps for `NoNewPrivs:\s*1` — the kernel sets this field when `PR_SET_NO_NEW_PRIVS` has
been applied, preventing privilege escalation via setuid binaries.

### `test_readonly_rootfs`
**Requires:** root, rootfs

Calls `with_readonly_rootfs(true)` and runs `touch /test_file`. Verifies the container
process runs cleanly (ash exits 0) even though the `touch` fails — the rootfs is
immutable via a bind-remount with `MS_RDONLY`.

### `test_masked_paths_default`
**Requires:** root, rootfs

Calls `with_masked_paths_default()` (which masks `/proc/kcore`, `/sys/firmware`, etc.)
and attempts `cat /proc/kcore`. Verifies the container completes without error — the
masked path is replaced with a bind mount of `/dev/null`, so reads return nothing
or an error that the shell handles gracefully.

### `test_masked_paths_custom`
**Requires:** root, rootfs

Calls `with_masked_paths(&["/proc/kcore", "/sys/firmware"])` with a custom list and
runs `echo`. Verifies that specifying masked paths manually doesn't prevent the
container from executing.

### `test_combined_phase1_security`
**Requires:** root, rootfs

Stacks all Phase 1 security features: `with_seccomp_default()`,
`with_no_new_privileges(true)`, `with_readonly_rootfs(true)`,
`with_masked_paths_default()`, `drop_all_capabilities()`. Verifies they coexist
and the container can still run `echo`.

### `test_landlock_read_only_allows_read`
**Requires:** root, rootfs, Linux ≥ 5.13

Applies `with_landlock_ro("/")` (read-only Landlock rule on the entire container root)
and runs `cat /etc/hostname`. Asserts the command succeeds. Skips if the kernel does
not support Landlock (ABI version 0). Failure indicates `FS_ACCESS_RO` does not include
`READ_FILE`/`READ_DIR`, or `apply_landlock` failed silently.

### `test_landlock_denies_write`
**Requires:** root, rootfs, Linux ≥ 5.13

Applies `with_landlock_ro("/")` on a container with a tmpfs at `/tmp` and runs
`touch /tmp/landlock_test`. Asserts the touch exits non-zero or the shell reports
`exit=1`. Skips if the kernel does not support Landlock. Failure indicates the
read-only rule is not blocking `WRITE_FILE`/`MAKE_REG` access as expected.

### `test_landlock_rw_allows_write`
**Requires:** root, rootfs, Linux ≥ 5.13

Applies `with_landlock_rw("/")` (all Landlock rights) and runs
`touch /tmp/landlock_rw_test && echo ok`. Asserts `ok` appears on stdout. Skips if
the kernel does not support Landlock. Failure indicates `FS_ACCESS_RW` is missing
write rights, or `apply_landlock` incorrectly denies writes when `MAKE_REG` is included.

### `test_landlock_no_rules_no_effect`
**Requires:** root, rootfs

Spawns a container with no Landlock rules and runs both a read (`cat /etc/hostname`)
and a write (`touch /tmp/noll`). Asserts both succeed. Does not skip on old kernels
because it does not call Landlock at all. Failure indicates that `apply_landlock(&[])`
is not a true no-op — a bug where an empty rule set applies a deny-all policy.

### `test_landlock_partial_path_allow`
**Requires:** root, rootfs, Linux ≥ 5.13

Grants read-only access to `/etc`, `/bin`, `/lib`, and `/usr` only (no rule for `/tmp`).
Runs `cat /etc/hostname && touch /tmp/partial_test; echo write_exit=$?`. Asserts
`write_exit=1` — the `/tmp` write is denied because `/tmp` is not covered by any rule.
Skips if the kernel does not support Landlock. Failure indicates Landlock rules are not
scoped to the declared subtrees, or `/tmp` is inadvertently inheriting access.

---

## MAC (AppArmor / SELinux) Tests

### `test_apparmor_profile_unconfined`
**Requires:** root, rootfs

Calls `.with_apparmor_profile("unconfined")`. When AppArmor is not running (detected via
`is_apparmor_enabled()` in the parent) the profile field is silently cleared and the
container starts normally.  When AppArmor IS running, "unconfined" is written to
`/proc/self/attr/apparmor/exec` before chroot and the container runs unconfined.
Asserts exit 0 and "ok" in stdout.  Failure indicates the MAC fd-open/write path
broke container startup in either the AppArmor-on or AppArmor-off case.

### `test_apparmor_profile_applied`
**Requires:** root, rootfs, AppArmor enabled, `apparmor_parser` in PATH

Loads `scripts/apparmor-profiles/pelagos-test` into the kernel via `apparmor_parser -r`,
runs a container that prints `/proc/self/attr/current`, and asserts the output contains
`"pelagos-test"`.  Unloads the profile afterwards.  Skips when AppArmor is not enabled or
`apparmor_parser` is absent.  Failure indicates the exec-attr fd technique (open before
chroot, write before seccomp) is not correctly transitioning the process into the named
profile at exec time.

### `test_selinux_label_no_selinux`
**Requires:** root, rootfs

Calls `.with_selinux_label("system_u:system_r:container_t:s0")`.  Because SELinux is not
running, `is_selinux_enabled()` returns false in the parent, the label is cleared, and the
container starts normally.  Asserts exit 0 and "ok" in stdout.  This test always runs
(it does not skip on systems without SELinux) to confirm that the graceful-degradation path
works: a misconfigured or production host without SELinux must not fail container startup
simply because a label was specified.

---

## `SECCOMP_RET_USER_NOTIF` Supervisor Tests

### `test_user_notif_handler_invoked`
**Requires:** root, rootfs, Linux ≥ 5.0

Installs a `with_seccomp_user_notif` handler that intercepts `SYS_getuid` and
allows all calls while incrementing an `AtomicUsize` counter. Runs `/usr/bin/id -u`
inside the container. Asserts: (1) the container exits successfully, (2) stdout
contains `"0"` (uid 0 returned normally), and (3) the counter is ≥ 1. Failure
indicates the user_notif filter was not installed, or the supervisor thread did not
receive notifications, or `SyscallResponse::Allow` is not passing the syscall through.

### `test_user_notif_deny_syscall`
**Requires:** root, rootfs, Linux ≥ 5.0

Installs a handler that intercepts `SYS_chmod` and responds with
`SyscallResponse::Deny(EPERM)` for all invocations. Runs a container that creates
`/tmp/x` and then calls `chmod 700 /tmp/x`, printing the exit code. Asserts output
contains `"exit=1"` — the chmod call returns EPERM. Failure indicates the deny
response is not being delivered to the container thread, or the wrong syscall number
was intercepted.

### `test_user_notif_allow_passthrough`
**Requires:** root, rootfs, Linux ≥ 5.0

Installs a counting handler that intercepts `SYS_chmod` and responds with
`SyscallResponse::Allow` for all calls. Runs the same chmod sequence. Asserts:
(1) output contains `"exit=0"` (chmod succeeded), and (2) the handler counter is
≥ 1. Failure indicates the allow response is not continuing the syscall through the
filter chain, or the supervisor was not invoked.

---

## Phase 4: Filesystem Flexibility Tests

### `test_bind_mount_rw`
**Requires:** root, rootfs

Creates a temporary host directory, writes `hello.txt` into it, and mounts it
read-write at `/mnt/hostdir` via `with_bind_mount()`. Runs `cat /mnt/hostdir/hello.txt`
inside the container. Verifies that host files are accessible to the container.

### `test_bind_mount_ro`
**Requires:** root, rootfs

Mounts a temporary host directory read-only at `/mnt/ro` via `with_bind_mount_ro()`.
Runs `touch /mnt/ro/newfile` and captures the exit code. Verifies `exit=1` — the
write is rejected because the mount is read-only. The `MS_BIND | MS_RDONLY` remount
is required by the Linux kernel (two calls: bind, then remount-ro).

### `test_cli_volume_flag_ro`
**Requires:** root, rootfs

Verifies that the CLI `-v host:container:ro` and `-v host:container:rw` suffixes are
parsed correctly and produce the expected mount behaviour. Runs `pelagos run -v ...:ro`
and asserts that a write inside the container fails (`exit=1`); then runs with `:rw`
and asserts the write succeeds (`exit=0`).

This tests the `run.rs` parser path (distinct from `test_bind_mount_ro` which calls
`with_bind_mount_ro()` directly). Failure means the `rsplit_once(':')` fix that strips
`:ro`/`:rw` from the mount-target path has regressed, causing the suffix to be treated
as part of the filesystem path instead of a mount option.

### `test_bind_mount_into_dev`
**Requires:** root, rootfs

Bind-mounts a host-side temp file at `/dev/termination-log` inside the container, then
writes to it and reads it back. Also verifies that the written content is visible on the
host-side file. Guards against the regression where user bind mounts were applied BEFORE
the `/dev/` tmpfs setup: the tmpfs would cover the bind mount, making `/dev/termination-log`
appear to not exist. This is the path taken by the Kubernetes CRI kubelet when it passes a
termination log mount for `terminationMessagePath`.

### `test_tmpfs_mount`
**Requires:** root, rootfs

Configures a readonly rootfs via `with_readonly_rootfs(true)` and mounts a tmpfs at
`/tmp` via `with_tmpfs("/tmp", "size=10m,mode=1777")`. Runs `touch /tmp/testfile`.
Verifies that tmpfs can provide a writable island inside an otherwise immutable
container filesystem.

### `test_named_volume`
**Requires:** root, rootfs

Creates a named volume (`Volume::create("testvol")`), mounts it at `/data`, and runs
`echo persistent > /data/file.txt`. After `wait()`, reads `vol.path()/file.txt` on
the host and verifies the content persists. Confirms that named volumes survive
container exit. Cleans up with `Volume::delete("testvol")`.

---

## Phase 5: Cgroups v2 Resource Management Tests

### `test_cgroup_memory_limit`
**Requires:** root, rootfs

Creates a cgroup with `with_cgroup_memory(32MB)` and runs `dd if=/dev/urandom of=/dev/null bs=1M count=64`.
Because `dd` streams data without accumulating RSS, it typically won't OOM, but the
important thing is that the cgroup is created and the container runs under it without
error. Verifies the cgroup setup path works end-to-end.

### `test_cgroup_memory_limit_pid_namespace`
**Requires:** root, rootfs

Regression test for the cgroup race condition when `Namespace::PID` is used. With a PID
namespace, `spawn()` performs a double-fork: an intermediate waiter process (B) forks the
real container process (C). The parent previously set up the cgroup after `spawn()` returned,
creating a window where C ran unconstrained and could exhaust memory before being added to
the cgroup.

This test verifies the fix: the cgroup is now pre-created before fork and the container
process adds its own PID to `cgroup.procs` during pre_exec before exec, eliminating the
race entirely.

Sets `with_cgroup_memory(32MB)` and `with_cgroup_memory_swap(0)` (to prevent swap escape),
uses `with_dev_mount()` for `/dev/zero`, and runs `dd if=/dev/zero of=/tmp/fill bs=1M count=100`
writing 100 MB into a tmpfs. If the memory limit is not enforced, dd completes successfully
(exit 0). A correctly working limit OOM-kills the container (non-zero exit / signal). Failure
would indicate the cgroup setup race has regressed.

### `test_cgroup_pids_limit`
**Requires:** root, rootfs

Sets `with_cgroup_pids_limit(4)` and runs a shell loop that forks 10 background
`sleep 2` jobs then calls the `wait` builtin. With `pids.max=4`, at most 3 background
sleeps can start (shell = 1 slot); further forks are denied by the kernel. After 500 ms
the test reads `pids.max` from the cgroup file to assert the limit was applied, then
reads `pids.events` and checks that the `max` counter is greater than zero — kernel
proof that at least one fork was denied. Failure would indicate the pids cgroup was
never applied or the `pids.events` counter was not incremented.

### `test_cgroup_pids_limit_pid_namespace`
**Requires:** root, rootfs

Same enforcement proof as `test_cgroup_pids_limit` but with `Namespace::PID` enabled,
which triggers the double-fork code path. Uses `wait_for_grandchild()` to locate the
real container process (grandchild, PID 1 in the namespace) via
`/proc/{waiter}/task/{waiter}/children`.

Reads `pids.max` from the grandchild's cgroup immediately to assert the limit was
applied to the correct process, then sleeps 200 ms to allow the fork-bomb to run.
ash (as PID 1 in the namespace) exits once a fork fails, taking its children with it;
however the cgroup persists until `child.wait()` so `pids.events` is still readable.
Asserts `pids.events max > 0` — kernel proof the limit was enforced on the container
process even via the double-fork path. Failure would indicate the pre-fork cgroup
race regression.

### `test_cgroup_cpu_quota_pid_namespace`
**Requires:** root, rootfs

Sets `with_cgroup_cpu_quota(50_000, 1_000_000)` (5% CPU) with `Namespace::PID` and
spawns `sleep 3`. Uses `wait_for_grandchild()` to find the real container process,
reads its cgroup path from `/proc/{grandchild}/cgroup`, then reads
`/sys/fs/cgroup/{cg}/cpu.max` from the host. Asserts the file starts with `"50000 "`,
proving the CPU quota was applied to the actual container process (not just the
intermediate waiter). Failure would indicate the cpu quota is either not applied or
applied to the wrong process in the double-fork path.

### `test_cgroup_cpuset_pid_namespace`
**Requires:** root, rootfs

Sets `with_cgroup_cpuset_cpus("0")` and `with_cgroup_cpuset_mems("0")` with
`Namespace::PID` and spawns `sleep 3`. Uses `wait_for_grandchild()` to find the real
container process, then reads `/proc/{grandchild}/status` from the HOST and checks the
`Cpus_allowed_list` field. Asserts it equals `"0"`, proving the cpuset was applied
to the actual container PID via the kernel scheduler. Failure would indicate the
cpuset cgroup was not applied to the grandchild in the double-fork path.

### `test_cgroup_resource_stats_pid_namespace`
**Requires:** root, rootfs

Spawns `sleep 3` with `with_cgroup_memory(32MB)` and `with_cgroup_pids_limit(16)`
plus `Namespace::PID`. After 200 ms, calls `child.resource_stats()` and asserts
`pids_current >= 1`. Verifies that `resource_stats()` can locate and read the cgroup
of the grandchild process in the double-fork path. Failure would indicate the stats
API cannot find the cgroup when a PID namespace is in use.

### `test_cgroup_cpu_shares`
**Requires:** root, rootfs

Sets `with_cgroup_cpu_shares(512)` (writes `cpu.weight`) and runs `echo ok`.
Smoke-tests that CPU weight configuration doesn't interfere with container execution.
Does not verify proportional scheduling behaviour (would need a concurrent reference
process).

### `test_resource_stats`
**Requires:** root, rootfs

Spawns a container with `with_cgroup_memory(128MB)` and `with_cgroup_pids_limit(64)`,
then calls `child.resource_stats()` while the container may still be running.
Verifies the call returns a valid `ResourceStats` struct with `memory_current_bytes`,
`cpu_usage_ns`, and `pids_current` fields (all `u64`, so always ≥ 0).

### `test_cgroup_cleanup`
**Requires:** root, rootfs

Spawns with `with_cgroup_memory(64MB)`, records the child PID, calls `wait()`, then
checks that `/sys/fs/cgroup/pelagos-{pid}` no longer exists. Verifies that
`teardown_cgroup()` deletes the cgroup directory after the container exits.

### `test_cgroup_memory_swap`
**Requires:** root, rootfs

Spawns with `with_cgroup_memory(64MB)` + `with_cgroup_memory_swap(128MB)`. Verifies the
container starts and exits without error. Confirms that `memory.swap.max` is accepted by
the cgroup controller (issue #31).

### `test_cgroup_memory_reservation`
**Requires:** root, rootfs

Spawns with `with_cgroup_memory_reservation(32MB)`. Verifies the container starts and exits
cleanly. Confirms `memory.low` (soft limit) is wired through correctly (issue #31).

### `test_cgroup_cpuset`
**Requires:** root, rootfs

Spawns with `with_cgroup_cpuset_cpus("0")` + `with_cgroup_cpuset_mems("0")`. Verifies no
startup error. Confirms cpuset.cpus/cpuset.mems are applied after cgroup creation (issue #32).

### `test_cgroup_blkio_weight`
**Requires:** root, rootfs

Spawns with `with_cgroup_blkio_weight(100)`. Verifies no error. Confirms the blkio weight
is accepted by the cgroup builder on cgroupv2 via `io.weight` (issue #33).

### `test_cgroup_device_rule`
**Requires:** root, rootfs

Spawns with two device cgroup rules (allow-all + deny-console). On cgroupv2 these are
gracefully skipped without error since the devices controller uses eBPF, not
`devices.allow/deny`. Verifies no container startup failure (issue #34).

### `test_cgroup_net_classid`
**Requires:** root, rootfs

Spawns with `with_cgroup_net_classid(0x10001)`. On cgroupv2 `net_cls` is unavailable;
verifies it is silently skipped without error (issue #35).

---

## Phase 6: Native Networking Tests

### `test_loopback_network` — N1
**Requires:** root, rootfs

Calls `with_network(NetworkMode::Loopback)`. Inside `pre_exec`, after
`unshare(CLONE_NEWNET)`, `bring_up_loopback()` uses `ioctl(SIOCSIFFLAGS)` to set
`IFF_UP` on `lo`. Runs `ip addr show lo | grep -q '127.0.0.1'` inside the container.
Verifies that loopback is up with its standard address in an isolated net namespace.

### `test_bridge_network_ip` — N2
**Requires:** root, rootfs

Calls `with_network(NetworkMode::Bridge)`. `setup_bridge_network()` runs before fork,
creating a named netns (`rem-{pid}-{n}`), a veth pair, assigning `172.19.0.x/24` to
`eth0`, and attaching the host-side veth to `pelagos0`. The child joins the netns via
`setns()` in `pre_exec`. Runs `ip addr show eth0 | grep -q '172.19.0'` and verifies
`BRIDGE_IP_OK` — confirming the container sees its assigned IP from the first
instruction (no polling needed because setup is pre-fork).

### `test_bridge_network_veth_exists` — N2
**Requires:** root, rootfs

Spawns a bridge container running `sleep 2`. While it sleeps, queries
`ip link show {veth_name}` on the host (using `child.veth_name()` to get the
`vh-{hash}` interface name). Verifies the host-side veth exists while the container
is alive.

### `test_bridge_network_cleanup` — N2
**Requires:** root, rootfs

Spawns a bridge container with `ash -c "exit 0"` (exits immediately). Captures the
veth name before `wait()`, then calls `wait()`, then runs `ip link show {veth_name}`.
Verifies the veth is gone — `teardown_network()` calls `ip link del` in `Child::wait()`.
The immediate exit is safe because `setup_bridge_network()` runs before fork, so
there is no race between container startup and network setup.

### `test_bridge_netns_cleanup` — N2
**Requires:** root, rootfs

Spawns a bridge container with `exit 0`. Captures the named netns name from
`child.netns_name()` and verifies `/run/netns/{ns_name}` exists before `wait()`.
After `wait()`, verifies the path is gone. Closes a gap left by
`test_bridge_network_cleanup`, which only checks the veth — this test confirms
`ip netns del` in `teardown_network()` also runs successfully.

### `test_bridge_loopback_up` — N2
**Requires:** root, rootfs

Runs `ip addr show lo | grep -q '127.0.0.1'` inside a bridge-mode container.
Verifies that `lo` is up with `127.0.0.1` in addition to `eth0`. Loopback in bridge
mode is configured by `setup_bridge_network()` via
`ip -n {ns_name} link set lo up` before fork — different from Loopback mode which
uses an in-process `ioctl`.

### `test_bridge_gateway_reachable` — N2
**Requires:** root, rootfs

Runs `ping -c 1 -W 2 172.19.0.1` inside a bridge-mode container. Verifies actual
layer-3 connectivity: ICMP echo traverses `eth0` → veth pair → `pelagos0` bridge →
host, which replies with `172.19.0.1`. This is the only test that exercises a real
packet flowing through the full network stack, catching problems like missing ARP,
misconfigured routes, or a veth not attached to the bridge.

### `test_bridge_concurrent_spawn` — N2
**Requires:** root, rootfs

Spawns two bridge containers from separate threads simultaneously. Each thread builds
a `Command`, calls `spawn()`, and collects output entirely within the thread (no
non-`Send` types cross thread boundaries). Each container runs
`ip addr show eth0 | grep -m1 'inet ' | awk '{print $2}'` and emits its assigned IP.

Asserts:
- Both IPs are non-empty and in the `172.19.0.x/24` range
- The two IPs differ (`assert_ne!`)

Exercises the `flock(LOCK_EX)` IPAM lock (concurrent writes to `/run/pelagos/next_ip`)
and the `AtomicU32` namespace-name counter under real concurrency.

---

## Phase 6 N3 — NAT / MASQUERADE Tests

These three tests share a global `NAT_TEST_LOCK` mutex so they run serially.
All three check the nftables refcount state via `nft list table ip pelagos`,
which is global per-host state. Running them concurrently would cause spurious
failures when one test's container exits and sees a non-zero refcount left by a
sibling's still-running container.

### `test_nat_rule_added` — N3
**Requires:** root, rootfs

Spawns a bridge+NAT container running `sleep 2`. While it sleeps, runs
`nft list table ip pelagos` on the host and asserts exit 0. Failure would
indicate that `enable_nat()` did not install the MASQUERADE rule set, or that
`nft` is not available on the host.

### `test_nat_cleanup` — N3
**Requires:** root, rootfs

Resets any leftover NAT refcount/nftables state (from crashed prior test runs),
then spawns a bridge+NAT container with `ash -c "exit 0"` (exits immediately).
After `wait()`, runs `nft list table ip pelagos-pelagos0` and asserts non-zero
exit. Failure would indicate that `disable_nat()` did not remove the nftables
table (refcount not decremented to zero, or `nft delete table` failed silently).

### `test_nat_refcount` — N3
**Requires:** root, rootfs

Spawns two bridge+NAT containers: A (`sleep 2`) and B (`sleep 4`). Waits for A,
then asserts `nft list table ip pelagos` exits 0 (B still running — refcount ≥ 1).
Waits for B, then asserts it exits non-zero (refcount hits 0, table removed).
Failure would indicate the reference-counting logic in `enable_nat` /
`disable_nat` is incorrect — either decrementing too eagerly (table gone while B
runs) or not decrementing at all (table present after both exit).

### `test_nat_iptables_forward_rules` — N3
**Requires:** root, rootfs
**Skipped on:** systems without `ip filter` table (no iptables-nft installed)

Spawns a bridge+NAT container running `sleep 3`. While it sleeps, asserts that
the nft compat chain `ip filter pelagos-pelagos0-fwd` exists and that a jump
rule for it is present in `ip filter FORWARD`. After `wait()`, asserts the
chain is gone.

On hosts with UFW or Docker, the iptables-nft chain `ip filter FORWARD` has
`policy drop`. An nft `accept` verdict in one base chain does not prevent
subsequent base chains from running, so pelagos writes accept rules directly
into `ip filter FORWARD` via a named jump chain (`pelagos-<net>-fwd`). This
ensures TCP/UDP flows when ICMP would work without the fix.

Failure indicates `enable_nat()` is not adding the compat rules, or
`disable_nat()` is not cleaning them up.

---

## Phase 6 N4 — Port Mapping Tests

These three tests share the `#[serial(nat)]` key with the N3 tests (port-forward
rules live in the same `table ip pelagos`). All three use dedicated port numbers
(18080–18083) to avoid collision with real services on the host.

### `test_port_forward_rule_added` — N4
**Requires:** root, rootfs

Spawns a bridge+NAT container with `with_port_forward(18080, 80)` running `sleep 2`.
While it sleeps, runs `nft list chain ip pelagos prerouting` and asserts exit 0 and
that the output contains `dport 18080`. Failure would indicate that
`enable_port_forwards()` did not install the DNAT rule, or that the prerouting chain
was not created.

### `test_port_forward_cleanup` — N4
**Requires:** root, rootfs

Spawns a bridge+NAT container with `with_port_forward(18081, 80)` that exits
immediately (`ash -c "exit 0"`). After `wait()`, runs `nft list table ip pelagos`
and asserts non-zero exit (table gone). Failure would indicate that
`disable_port_forwards()` did not clean up nftables state, or that the port-forwards
state file was not cleared.

### `test_port_forward_independent_teardown` — N4
**Requires:** root, rootfs

Spawns A (`sleep 2`, port 18082→80) and B (`sleep 4`, port 18083→80), both with NAT.
Waits for A, then checks: prerouting chain still exists, A's rule (`dport 18082`)
is gone, B's rule (`dport 18083`) is still present. Waits for B, then asserts the
table is fully removed. Failure would indicate that `disable_port_forwards()` either
removed the wrong entries, failed to rebuild the prerouting chain from survivors, or
deleted the table prematurely while B was still running.

---

## Phase 6 N5 — DNS Tests

### `test_dns_resolv_conf` — N5
**Requires:** root, rootfs

Spawns a bridge+NAT container with `with_dns(&["1.1.1.1", "8.8.8.8"])` that runs
`cat /etc/resolv.conf` and captures stdout. Asserts the output contains both
`nameserver 1.1.1.1` and `nameserver 8.8.8.8`. Failure would indicate that the
per-container temp resolv.conf was not created, the bind mount over
`effective_root/etc/resolv.conf` failed, or the content was incorrect.
This test does not perform a live DNS lookup — it only verifies the file is visible
and correct inside the container. The shared Alpine rootfs is never modified.

---

## End-to-End Traffic Tests

These tests go beyond rule/config existence checks and verify that real packets
flow through the networking stack. They were added after discovering that nftables
rules can exist while iptables FORWARD policy DROP silently blocks TCP/UDP.

### `test_port_forward_end_to_end` — N4
**Requires:** root, rootfs, `nc` on host

Container A runs `echo HELLO_FROM_CONTAINER | nc -l -p 80` with
`with_port_forward(19090, 80)`. A temporary external network namespace
(`pf-test-client`) is created with its own veth pair to the host on
10.99.0.0/24, simulating a real external client. From that namespace,
`nc -w 2 10.99.0.1 19090` connects to the host on the forwarded port.
The traffic arrives on the `pf-test-h` veth, goes through nftables PREROUTING
(DNAT → container IP:80), then gets forwarded through the bridge to A.

Note: DNAT prerouting rules only apply to traffic arriving from external
interfaces, not locally-originated host packets (which go through OUTPUT) and
not bridge-internal traffic (hairpin routing issues). So this test creates a
separate network namespace as the client rather than connecting from the host
or from another bridge container.

Unlike `test_port_forward_rule_added` (which only checks the nftables rule string),
this proves the full DNAT path works: external traffic → nftables prerouting → DNAT →
FORWARD → bridge → container netns → container process → response back via conntrack.

### `test_udp_port_forward_rule_added` — N4-UDP
**Requires:** root, rootfs

Spawns a bridge+NAT container with `with_port_forward_udp(19095, 5000)`.
After 200 ms, queries nftables (`nft list chain ip pelagos-pelagos0 prerouting`)
and asserts the chain contains `udp dport 19095 dnat to <IP>:5000` and does NOT
contain `tcp dport 19095` (UDP-only mappings must not generate TCP rules).

Failure indicates UDP port mappings are silently ignored or the wrong nft protocol
token is emitted.  Container is SIGKILLed after the nftables check.

### `test_both_port_forward_rule_added` — N4-UDP
**Requires:** root, rootfs

Spawns a bridge+NAT container with `with_port_forward_both(19096, 53)`.
After 200 ms, queries nftables and asserts the prerouting chain contains BOTH
`tcp dport 19096 dnat to <IP>:53` AND `udp dport 19096 dnat to <IP>:53`.

Failure indicates the `Both` variant does not generate the two required rules,
which would break dual-protocol services (e.g. DNS, QUIC/HTTP3).

### `test_udp_proxy_threads_joined_on_teardown` — N4-UDP
**Requires:** root, rootfs

Starts a container with `with_port_forward_udp(19097, 5000)` and verifies:
1. While running: `UdpSocket::bind(127.0.0.1:19097)` fails (proxy holds the port).
2. After `SIGKILL` + `child.wait()`: the same bind succeeds (proxy thread was joined,
   inbound socket is closed, port is released).

This directly tests that `teardown_network` joins the per-port UDP proxy threads
(via `proxy_udp_threads.drain(..)` + `handle.join()`). Without the join, the
thread keeps the socket open and the port remains unavailable for a short window,
causing the test to fail.

### `test_bridge_cleanup_after_sigkill` — N2+N3
**Requires:** root, rootfs

Spawns a bridge+NAT container (`sleep 60`), records veth name, netns name, and
(on iptables-nft systems) verifies the nft compat chain `pelagos-pelagos0-fwd`
exists in `ip filter`. Then SIGKILLs the container and calls `wait()`. Asserts
all resource types are cleaned up: veth pair, named netns, nftables table, and
the iptables-nft compat chain.

All other cleanup tests use normal container exit. This catches teardown bugs that
only manifest when the container process dies unexpectedly — e.g. if `wait()` skips
`teardown_network()` or `disable_nat()` when the child was killed.

### `test_nat_end_to_end_tcp` — N3
**Requires:** root, rootfs, outbound internet

Spawns a bridge+NAT+DNS container that runs `wget --spider http://1.1.1.1/` and
asserts exit 0. Skips gracefully if the host has no outbound internet (checked via
host-side `ping -c1 -W2 1.1.1.1`).

This is the true end-to-end NAT test — TCP packets flow from the container through
MASQUERADE to the public internet and back. Existing NAT tests only verify that
nftables/iptables rules exist. Follows the same skip-if-no-internet pattern as
`test_pasta_connectivity`.

---

## IPv6 Dual-Stack Tests (`mod ipv6`)

### `test_ipv6_container_gets_address`
**Requires:** root, rootfs

Spawns a bridge-networked container and runs `ip -6 addr show eth0 | grep -q 'fd'` inside
it. Asserts the output contains `IPV6_OK`. Failure means the container did not receive a
ULA IPv6 address (fd-prefix) from `setup_ipv6_container`, indicating the IPv6 bridge
configuration, IPAM counter, or `ip -6 addr add` failed.

Does **not** require host IPv6 internet connectivity — purely tests local bridge
configuration.

### `test_ipv6_outbound_nat`
**Requires:** root, rootfs, host IPv6 internet connectivity

Spawns a container with `with_network(Bridge)` + `with_nat()` and runs `ping6` to
`2606:4700:4700::1111` (Cloudflare's public IPv6 resolver). Asserts `NAT6_OK` appears
in output. Skipped if the host cannot reach that address (`ping6 -c1 -W2`).

Failure means NAT66 (nftables `ip6 table` MASQUERADE rule) is not routing outbound IPv6
packets from ULA space to the internet, or the container's IPv6 default route is missing.

### `test_ipv6_port_forward_localhost`
**Requires:** root, rootfs

Spawns a container with a port forward (`host 19093 → container 9080`) that runs an nc
server echoing `HELLO_IPV6`. Connects to `[::1]:19093` from the host using `TcpStream`
and asserts the response contains `HELLO_IPV6`.

Failure means the IPv6 localhost proxy (`tcp_accept_loop_v6` binding `[::1]:host_port`)
is not running, the IPv6 DNAT prerouting rule is missing, or the `[::1]` loopback TCP
relay is not correctly forwarding to the container's IPv4 address.

Does **not** require host IPv6 internet connectivity — tests the localhost proxy only.

---

## Configurable Subnet Tests (`mod config_subnet`)

### `test_ensure_network_custom_alloc_pool`
**Requires:** nothing (no root, no rootfs — pure library API)

Calls `ensure_network` twice with a custom `/16` pool (`10.202.0.0/16`) using distinct
names. Asserts both get addresses within that pool and that they receive different /24
blocks. Failure means the auto-allocation logic is ignoring the pool parameter or
assigning overlapping subnets.

### `test_config_loaded_from_xdg`
**Requires:** nothing

Writes a config TOML to a temp `$XDG_CONFIG_HOME/pelagos/config.toml` and calls
`PelagosConfig::load()`. Asserts both `default_subnet` and `auto_alloc_pool` match the
written values. Failure means the TOML parser or XDG path resolution is broken.

### `test_network_create_auto_alloc`
**Requires:** root

Runs `pelagos network create cfg-auto-net --alloc-from 10.203.0.0/16` and asserts the
printed subnet is within `10.203.x.x`. Failure means the `--alloc-from` CLI flag is not
being passed to `ensure_network`, or `ensure_network`'s pool selection is wrong.

### `test_default_subnet_bootstrap`
**Requires:** root, rootfs

Temporarily removes `pelagos0`'s `config.json`, calls `bootstrap_default_network` with
`10.201.0.0/24`, then spawns a bridge container and asserts `eth0` has a `10.201.0.x`
address. Restores the original config after. Failure means `bootstrap_default_network`
is ignoring the override or the container's IP allocation is not reading the new
`NetworkDef`.

---

## Overlay Filesystem Tests

### `test_overlay_writes_to_upper`
**Requires:** root, rootfs

Creates temporary `upper` and `work` directories. Spawns a container with
`with_overlay(upper, work)` that writes `echo hello > /newfile`. After `wait()`:
asserts that `lower_dir/newfile` does **not** exist (lower layer is untouched),
and that `upper_dir/newfile` contains `"hello\n"`. Failure would indicate that
writes inside an overlay container are reaching the lower layer instead of the
upper layer — overlayfs copy-on-write is broken or the overlay was not mounted.

### `test_overlay_with_volume`
**Requires:** root, rootfs

Spawns a container with both `with_overlay(upper, work)` and
`with_volume(&vol, "/data")`. The container writes to the volume (`/data/vol_file.txt`)
and to a regular path (`/overlay_file.txt`). After `wait()`: asserts that the volume
file persists on the host, the regular write lands in the overlay upper dir (not the
rootfs), and the volume write does **not** appear in the overlay upper dir. Failure
would indicate that volume bind mounts are not correctly layered on top of the overlay
merged view, or that volume writes are leaking into the overlay upper directory.

### `test_overlay_lower_unchanged`
**Requires:** root, rootfs

Creates temporary `upper` and `work` directories. Records the original content of
`lower_dir/etc/hostname`, then spawns a container that runs
`echo modified > /etc/hostname`. After `wait()`: asserts that `lower_dir/etc/hostname`
is unchanged (same content as before), and that `upper_dir/etc/hostname` contains
`"modified\n"`. Failure would indicate that modifying an existing lower-layer file
writes through to the lower directory instead of producing a copy-on-write in the
upper layer.

### `test_overlay_merged_cleanup`
**Requires:** root, rootfs

Spawns a container with `with_overlay(upper, work)` that runs `true` (exits
immediately). Records the specific merged dir path via `child.overlay_merged_dir()`
before calling `wait()`. After `wait()`: asserts that neither the merged dir nor its
parent (`/run/pelagos/overlay-{pid}-{n}/`) exist. Failure would indicate that `wait()`
failed to call `remove_dir` on the merged directory and its parent, leaving stale
directories on the host. The test checks the specific path rather than scanning the
whole directory to avoid false failures from other overlay tests running in parallel.

---

## OCI Lifecycle Tests

These tests exercise the five OCI Runtime Spec v1.0.2 subcommands (`create`, `start`,
`state`, `kill`, `delete`) via the `pelagos` binary. They use minimal OCI bundles with
`rootfs/` symlinked to the Alpine rootfs and inline `config.json`.

### `test_oci_create_start_state`
**Requires:** root, rootfs

Writes a minimal `config.json` running `sleep 2`. Runs `pelagos create`, asserts
`pelagos state` returns `"created"`. Runs `pelagos start`, asserts `"running"`. Polls
until the process exits, asserts `"stopped"`. Runs `pelagos delete`, asserts the state
dir is gone. Failure indicates broken create/start synchronization, incorrect
state.json transitions, or wrong liveness detection via `kill(pid, 0)`.

### `test_oci_kill`
**Requires:** root, rootfs

Spawns a long-running container (`sleep 60`), starts it, then sends `SIGKILL` via
`pelagos kill` and polls until `pelagos state` reports `"stopped"`. Uses SIGKILL because
the container is PID 1 in a PID namespace — the kernel drops unhandled signals (like
SIGTERM) for namespace-init processes. Failure indicates that `cmd_kill` is not finding
the correct host-visible PID, or that liveness detection does not detect the exit.

### `test_oci_delete_cleanup`
**Requires:** root, rootfs

Runs `/bin/true` through the full create→start→wait-for-stopped lifecycle, records
the state dir path, runs `pelagos delete`, and asserts the directory is removed. Failure
indicates `cmd_delete` is not calling `remove_dir_all`, or is checking liveness
incorrectly and refusing to delete a stopped container.

### `test_oci_state_dir_stable_until_delete`
**Requires:** root, rootfs

Starts `/bin/true`, waits 200ms for it to exit, then asserts: (1) the state directory
still exists, and (2) `pelagos state` returns `"stopped"` without error. Then calls
`pelagos delete` and verifies the state dir is removed.

Verifies the OCI spec requirement that `stopped` is a stable inspectable state owned by
the runtime until `pelagos delete` — not cleaned up automatically on process exit. An
orchestrator queries `pelagos state` after observing process exit, before calling delete.
Failure indicates the runtime is removing state too early (issue #37 / #40).

### `test_oci_kill_short_lived`
**Requires:** root, rootfs

Starts `/bin/true`, waits 200ms (process exits), then calls `pelagos kill SIGKILL`
*without* first calling `pelagos state`. Asserts kill returns success.

This is the `pidfile.t` scenario: the container process is gone but `state.json` still
says `"running"` (cmd_state hasn't been called). `cmd_kill` must succeed — it gates only
on `state.json` status, not process liveness, and treats `ESRCH` as success (process died
concurrently). Failure indicates cmd_kill is incorrectly checking liveness (issue #37 / #41).

### `test_oci_kill_stopped_fails`
**Requires:** root, rootfs

Starts `/bin/true`, waits 200ms, calls `pelagos state` (which writes `"stopped"` to
`state.json`), then asserts `pelagos kill SIGKILL` returns an error.

This is the `kill.t test 4` scenario: once `cmd_state` has persisted `"stopped"` to disk,
subsequent kill attempts must fail per the OCI spec. Failure indicates either `cmd_state`
is not persisting the stopped status (issue #37 / #40), or `cmd_kill` is not reading it
(issue #37 / #41).

### `test_oci_pid_start_time`
**Requires:** root, rootfs (integration portion); unit assertions run without root

Two-part test.

**Unit (no root required):**
- Calls `read_pid_start_time(self)` and asserts it returns `Some(>0)`.
- Asserts that two successive calls return the same value (stability).
- Asserts that `read_pid_start_time(i32::MAX)` returns `None` (non-existent PID).

**Integration (root + rootfs):**
- Creates a `sleep 30` container, runs `create` + `start`.
- Reads `state.json` directly and asserts `pidStartTime` is present and non-zero.
- Reads `/proc/<pid>/stat` directly via `read_pid_start_time()` and asserts it
  equals the stored value.

Failure indicates `pid_start_time` is not being written to `state.json` at create
time, or that `read_pid_start_time()` is parsing `/proc/pid/stat` field 22 incorrectly.
This is the foundation of the PID reuse detection fallback path in `cmd_state` and
`cmd_kill` (issue #37; pidfd-based primary path implemented in issue #44).

### `test_oci_pidfd_mgmt_socket`
**Requires:** root, rootfs, Linux ≥ 5.3

After `create` + `start` of a `sleep 30` container, asserts that:
- `mgmt.sock` exists under `/run/pelagos/<id>/` (shim created it).
- Connecting to `mgmt.sock` and calling `recvmsg(SCM_RIGHTS)` yields a valid pidfd (≥ 0).
- `is_pidfd_alive(pidfd)` returns `true` while the container is running.
- After `pelagos kill SIGKILL`, `is_pidfd_alive` transitions to `false` within 5 s.

Failure indicates the shim failed to call `pidfd_open(2)` (kernel < 5.3 would skip it
silently; a failure here means the mgmt loop exited early or the socket was never
created), `send_fd_on_socket` is broken, or `is_pidfd_alive` misreads the poll result.
This is the primary test for issue #44.

### `test_oci_pidfd_state_liveness`
**Requires:** root, rootfs, Linux ≥ 5.3

Runs a `true` container (exits immediately after start).  Polls `pelagos state` in a
loop (100 ms intervals, 5 s timeout) and asserts it reaches `"stopped"`.

Failure indicates that `cmd_state`'s pidfd-based liveness path (or its starttime
fallback) fails to detect container exit — either `try_get_pidfd_from_shim` never
finds a valid pidfd, `is_pidfd_alive` always returns `true`, or the fallback
`kill(pid,0)` path is broken.  Complements `test_oci_create_start_state` which
tests a longer-lived (`sleep 2`) container lifecycle.

### `test_oci_bundle_mounts`
**Requires:** root, rootfs

Creates a `config.json` with a `tmpfs` mount at `/scratch` and a process that writes
to `/scratch/test.txt`. Runs the full create→start→stopped lifecycle and asserts that
`pelagos delete` succeeds. Failure indicates that OCI `mounts` entries are not being
applied from `config.json`, or that tmpfs mount handling in `build_command()` is broken.

### `test_oci_capabilities`
**Requires:** root, rootfs

Creates a `config.json` with `process.capabilities` specifying only `CAP_CHOWN` in
the bounding and effective sets. The container runs `/usr/bin/id` and must exit
successfully. Asserts the full create→start→stopped lifecycle completes cleanly.
Failure indicates that OCI `process.capabilities` parsing or the
`with_capabilities()` wiring in `build_command()` is broken.

### `test_oci_masked_readonly_paths`
**Requires:** root, rootfs

Creates a `config.json` with `linux.maskedPaths: ["/proc/kcore"]` and
`linux.readonlyPaths: ["/sys/kernel"]`. The container verifies:
- `/proc/kcore` is masked (bind-mounted `/dev/null` → zero bytes readable)
- `/sys/kernel` is read-only (`touch /sys/kernel/test` is denied)

The shell command exits 0 only if both checks pass. Asserts the full lifecycle
completes cleanly. Failure indicates that `linux.maskedPaths` or
`linux.readonlyPaths` from OCI config are not being applied, or the wiring
into `with_masked_paths()` / `with_readonly_paths()` in `build_command()` is broken.

### `test_oci_resources`
**Requires:** root, rootfs

Creates a `config.json` with `linux.resources` setting a 64 MiB memory limit and a PID
limit of 50. The container reads `/sys/fs/cgroup/memory.max` and `/sys/fs/cgroup/pids.max`.
Failure indicates that `linux.resources` parsing from OCI config or the wiring into
`with_cgroup_memory()` / `with_cgroup_pids_limit()` is broken.

### `test_oci_resources_extended`
**Requires:** root, rootfs

Creates an OCI bundle with the full extended `linux.resources` set: `memory.swap`,
`memory.reservation`, `cpu.cpus/mems`, `blockIO.weight`, `linux.resources.devices`, and
`linux.resources.network`. Runs `exit 0` inside the container. Failure indicates a parsing
or `build_command()` wiring bug for any of the extended resource fields introduced in
epic #29 (issues #31–#35). On cgroupv2-only systems, device and network cgroup rules are
gracefully skipped; the test still verifies no startup error occurs.

### `test_oci_rlimits`
**Requires:** root, rootfs

Creates a `config.json` with `process.rlimits` capping `RLIMIT_NOFILE` to 128. The container
runs `ulimit -n` (exits 0 if the limit is accepted). Failure indicates that `process.rlimits`
parsing or the wiring into `with_rlimit()` in `build_command()` is broken.

### `test_oci_sysctl`
**Requires:** root, rootfs

Creates a `config.json` with `linux.sysctl: {"kernel.domainname": "testdomain.local"}`. The
container greps for that value in `/proc/sys/kernel/domainname`. The sysctl is set in the
private UTS namespace so it doesn't affect the host. Failure indicates that `linux.sysctl`
parsing or the `with_sysctl()` / pre_exec write to `/proc/sys/` is broken.

### `test_oci_hooks`
**Requires:** root, rootfs

Creates a `config.json` with a `prestart` hook that touches a sentinel file, and a `poststop`
hook that touches a different sentinel file. Asserts the prestart sentinel exists after
`pelagos create` and the poststop sentinel exists after `pelagos delete`. Failure indicates
that OCI `hooks` parsing or the `run_hooks()` placement in `cmd_create()` / `cmd_delete()`
is broken.

### `test_oci_seccomp`
**Requires:** root, rootfs

Creates a `config.json` with `linux.seccomp` using a default-allow policy that denies only
`ptrace`, `personality`, and `bpf`. The container runs `/bin/echo hello` which must succeed.
Failure indicates that `linux.seccomp` parsing from OCI config, the `filter_from_oci()`
function in `src/seccomp.rs`, or the `with_seccomp_program()` wiring is broken.

### `test_oci_seccomp_args`
**Requires:** root (no rootfs required — compiles a static binary at test time)

Builds a seccomp filter via `filter_from_oci` with an argument condition: `defaultAction=ALLOW`,
but block `socket` when `arg[0] == 16` (AF_NETLINK). Compiles a tiny static C binary at test
time that calls both `socket(AF_INET, ...)` and `socket(AF_NETLINK, ...)` and prints
`INET_OK`/`NETLINK_OK` or `INET_FAIL`/`NETLINK_FAIL`. Runs the binary in two containers:
one without any seccomp (both sockets must succeed), one with the arg-filtered filter
(AF_INET must succeed, AF_NETLINK must be blocked). Failure indicates that `OciSyscallArg`
→ `SeccompCondition` translation in `filter_from_oci` is broken or seccompiler arg
conditions are mis-wired.

### `test_oci_seccomp_errno_ret`
**Requires:** root (no rootfs required — compiles a static binary at test time)

Verifies that `errnoRet` and `SCMP_ACT_ENOSYS` are honoured at the kernel level. Compiles
a static binary that calls `personality(0xffffffff)` and prints `ERRNO:<n>`. Runs it under
three seccomp policies:

- `SCMP_ACT_ERRNO` with `errnoRet: 38` → must return errno 38 (ENOSYS), not 1 (EPERM)
- `SCMP_ACT_ERRNO` without `errnoRet` → must return errno 1 (EPERM, default)
- `SCMP_ACT_ENOSYS` action → must return errno 38 regardless of `errnoRet` field

Failure indicates `oci_action_to_seccomp` is ignoring the `errno_ret` parameter or
`SCMP_ACT_ENOSYS` is incorrectly mapped to EPERM instead of ENOSYS.

### `test_oci_seccomp_args_operators`
**Requires:** root (no rootfs required — compiles a static binary at test time)

Runtime-verifies the seccomp arg condition operators and combination semantics that
`test_oci_seccomp_args` does not cover. Compiles one static C binary that calls
`socket(AF_INET)`, `socket(AF_INET, SOCK_STREAM|SOCK_NONBLOCK)`, `socket(AF_INET6)`,
and `socket(AF_NETLINK)`. Runs it four times with different seccomp policies:

- **NE**: block `socket` when `arg[0] != AF_INET(2)` — verifies `SCMP_CMP_NE` BPF emission
  allows AF_INET and blocks AF_INET6 and AF_NETLINK.
- **MASKED_EQ**: block `socket` when `(arg[1] & 0x800) == 0x800` (SOCK_NONBLOCK bit) —
  verifies `SCMP_CMP_MASKED_EQ` BPF emission allows plain SOCK_STREAM and blocks
  SOCK_STREAM|SOCK_NONBLOCK.
- **Multi-arg AND**: single rule with two conditions (`arg[0]==2 AND arg[1]==1`) — verifies
  the kernel ANDs conditions: blocks exact `{AF_INET, SOCK_STREAM}`, allows
  `{AF_INET, SOCK_STREAM|SOCK_NONBLOCK}` and `{AF_INET6, SOCK_STREAM}`.
- **Multi-rule OR**: two `OciSyscallRule` entries for `socket` (`arg[0]==16` and `arg[0]==10`)
  — verifies the kernel ORs rules: AF_INET allowed, AF_NETLINK blocked by rule 1,
  AF_INET6 blocked by rule 2.

Failure of any scenario indicates the corresponding seccompiler BPF emission path or
OCI→SeccompCondition translation is wrong at the kernel level, not just at compile time.

### `test_oci_cap_all_known_names_round_trip` (unit)
**Requires:** nothing (unit test in `src/oci.rs`)

Asserts that all 41 Linux capability names (with `CAP_` prefix) map to a non-None value
via `oci_cap_to_flag`. Failure means an OCI bundle specifying that capability will silently
drop it rather than applying it to the container's capability set.

### `test_oci_cap_without_prefix` (unit)
**Requires:** nothing (unit test in `src/oci.rs`)

Verifies that `oci_cap_to_flag` accepts names both with and without the `CAP_` prefix,
and returns `None` for genuinely unknown names.

### `test_oci_signal_names` (unit)
**Requires:** nothing (unit test in `src/oci.rs`)

Verifies the signal name→number table covers all signal names sent by `opencontainers/runtime-tools`
including `SIGWINCH`, `SIGCHLD`, `SIGCONT`, `SIGSTOP`, `SIGQUIT`, `SIGSYS`, and numeric forms.

### `test_oci_kernel_mounts`
**Requires:** root, rootfs

Creates an OCI bundle with proc, sysfs, devpts, mqueue mounts (matching standard runc/containerd
output) and runs `ls /proc/self` inside. Failure indicates the OCI mount-type dispatch
(`oci.rs`) or the `KernelMount` pre_exec loop (`container.rs`) is broken. Primary gate for
`opencontainers/runtime-tools` conformance since nearly every test bundle uses these mounts.

### `test_oci_create_bundle_flag`
**Requires:** root, rootfs

Invokes `pelagos create --bundle <path> <id>` (named flag, OCI-standard form) and verifies the
container reaches "created" state. Failure indicates the `--bundle` CLI flag is not accepted,
which would prevent the `opencontainers/runtime-tools` conformance harness from invoking Pelagos.

### `test_oci_create_pid_file`
**Requires:** root, rootfs

Invokes `pelagos create --bundle <path> --pid-file <path> <id>` and verifies the pid file is
written with a positive integer that matches the PID reported in `state.json`. Failure indicates
`--pid-file` is not written or contains the wrong PID, which breaks containerd / CRI-O integration.

---

### `test_oci_rootfs_propagation`
**Requires:** root, rootfs

Creates an OCI bundle with `linux.rootfsPropagation: "private"` and runs `echo ok` inside it.
Verifies the container starts and completes successfully. Failure indicates the `rootfsPropagation`
field is not parsed, the mapping to `MS_PRIVATE|MS_REC` is wrong, or the `mount(2)` call fails,
which would cause the container to refuse to start whenever a runtime-tools bundle specifies
mount propagation.

---

### `test_oci_cgroups_path`
**Requires:** root, rootfs

Creates an OCI bundle with `linux.cgroupsPath` set to a unique name and runs `echo ok` inside it.
Verifies the container starts and completes successfully. Failure indicates the `cgroupsPath` field
is not wired from OCI config through to `CgroupConfig.path`, which would break runtimes that
rely on predictable cgroup hierarchy placement (e.g. systemd-managed slices).

---

### `test_oci_create_container_hook_in_ns`
**Requires:** root, rootfs

Creates an OCI bundle with a `createContainer` hook script that writes the inode of
`/proc/self/ns/mnt` to a temp file. After `pelagos create`, reads the recorded inode and compares
it to the host's mount namespace inode (`/proc/1/ns/mnt`). Asserts they differ, confirming the
hook executed inside the container's mount namespace. Failure means `createContainer` hooks run
in the host namespace, violating the OCI spec and breaking runtimes that use these hooks to
inject config (e.g. seccomp, apparmor profiles) into the container environment.

---

### `test_oci_start_container_hook_in_ns`
**Requires:** root, rootfs

Creates an OCI bundle with a `startContainer` hook script that writes the inode of
`/proc/self/ns/mnt` to a temp file. After `pelagos start`, reads the recorded inode and compares
it to the host's mount namespace inode. Asserts they differ, confirming the hook executed inside
the container's mount namespace before the user process was exec'd. Failure means `startContainer`
hooks either do not run at all or run in the host namespace, violating the OCI spec.

---

## Rootless Mode Tests

The following tests only execute when the test binary is run **without root** (no `sudo`).
When run as root (as in the standard CI invocation), they print a skip message and exit.
To run these tests:

```bash
cargo test --test integration_tests test_rootless
cargo test --test integration_tests test_user_namespace_explicit
```

### `test_rootless_basic`
**Requires:** non-root user, rootfs

Spawns a container that runs `/bin/id` without any explicit namespace configuration beyond
`MOUNT | UTS`. The rootless auto-configuration adds `Namespace::USER` and a uid_map that
maps `{container 0 → host UID}`. Asserts that the output contains `uid=0`, confirming
that the process appears as root inside the container's user namespace. Failure indicates
that rootless auto-configuration (auto-add USER namespace + uid_map) is not working.

### `test_rootless_loopback`
**Requires:** non-root user, rootfs

Spawns a container with `NetworkMode::Loopback` without root. Verifies that `ping 127.0.0.1`
succeeds inside the container. Rootless auto-config adds USER namespace; combined with
the private NET namespace the process gains the capability to bring up `lo`. Failure
indicates that rootless + loopback networking is broken.

### `test_rootless_bridge_rejected`
**Requires:** non-root user, rootfs

Calls `spawn()` with `NetworkMode::Bridge` as a non-root user. Asserts that `spawn()`
returns an `Err` whose message mentions `root` or `rootless`. Failure indicates that the
rootless bridge-mode guard is not in place.

### `test_user_namespace_explicit`
**Requires:** root

Runs `/usr/bin/id` as root with an explicit `Namespace::USER` and an identity uid/gid map
(`{inside: 0, outside: 0, count: 1}`). No chroot or MOUNT namespace is used — the rootfs
lives under `/home/cb/` which is not traversable from inside a USER namespace with a
single-uid map (DAC_OVERRIDE only applies for inodes whose uid is in the map). Asserts the
container process outputs `uid=0`. Failure indicates a regression in the uid_map writing
path or the MS_PRIVATE MNT_LOCKED skip logic.

---

## Pasta Networking Tests

The following tests verify `NetworkMode::Pasta` (user-mode networking via the `pasta`
binary from the passt project). All tests skip gracefully when `pasta` is not installed.
All require a non-root user — pasta's privilege-dropping (root→nobody via an internal
user namespace) makes it unable to access container namespace file descriptors when run
as root. pasta is designed for rootless mode.

To run these tests:

```bash
# All pasta tests — run without sudo:
cargo test --test integration_tests test_pasta
```

### `test_pasta_interface_exists`
**Requires:** non-root user, rootfs, pasta installed

Spawns a container with `NetworkMode::Pasta`, sleeps 1 second to let pasta attach, then
runs `ip addr show`. Makes two assertions:
1. A non-loopback interface exists — pasta attached its TAP to the container's netns.
2. That interface has an `inet` address that is not 127.x — pasta's `--config-net` flag
   configured the IP inside the netns (without this, the TAP would exist but have no IP).

Failure on (1) means `setup_pasta_network()` is not being called or pasta cannot attach.
Failure on (2) means `--config-net` is not being passed, so the container has a TAP
with no address — no connectivity is possible.

### `test_pasta_rootless`
**Requires:** non-root user, rootfs, pasta installed

Same assertions as `test_pasta_interface_exists` but specifically exercises the rootless
auto-detection path: `Namespace::USER` is not set explicitly — Pelagos adds it automatically
when `getuid() != 0`. Confirms that the USER+NET two-phase unshare and pasta still coexist
correctly when rootless mode is triggered implicitly.

### `test_pasta_connectivity`
**Requires:** non-root user, rootfs, pasta installed, outbound internet access

Spawns a container with `NetworkMode::Pasta` and runs `wget -q -T 5 --spider http://1.1.1.1/`
(HEAD request — no body to write, avoiding `/dev/null` which doesn't exist as a device node
in the chroot). Asserts the command exits 0 and prints `CONNECTED`. No `sleep` is needed
because `spawn()` uses SIGSTOP/SIGCONT to ensure pasta has configured the TAP before the
container runs. Failure indicates pasta's packet relay is broken or outbound internet is
unavailable in the test environment.

### `test_pasta_dns`
**Requires:** non-root user, rootfs, pasta installed

Regression test for the missing `/etc/resolv.conf` bug: pasta provides network connectivity
but no DNS configuration. `spawn()` now auto-injects the host's real upstream DNS servers
(filtered to exclude loopback stubs like 127.0.0.53) as a bind-mounted resolv.conf.
Runs `nslookup 1.1.1.1` inside a pasta container; asserts that the output contains
"Server" or the command succeeds (reverse DNS response or NXDOMAIN both confirm DNS is
configured), and asserts the error output does NOT contain "bad address" (which would
indicate resolv.conf was missing). Failure means DNS injection is broken.

---

## PID Namespace Tests

### `test_pid_namespace_repeated_fork`
**Requires:** root, rootfs

Regression test for a bug where `unshare(CLONE_NEWPID)` left the container process outside
the new PID namespace. Only the container's children entered it, so the first forked child
became PID 1. When that child exited, the kernel marked the namespace defunct and every
subsequent `fork()` failed with ENOMEM — even with abundant system memory.

Runs a shell loop that forks an external command (`sleep 0`) five times. All five forks must
succeed and the container must print `FORKS_OK`. Failure indicates the double-fork mechanism
in `pre_exec` (which makes the container process PID 1 in the new namespace) is broken.

---

## Container Linking Tests

### `test_container_link_hosts`
**Requires:** root, rootfs

Starts container A on bridge networking, writes its state (including bridge IP) to
`/run/pelagos/containers/link-test-a/state.json`, then starts container B with
`with_link("link-test-a")`. Reads B's `/etc/hosts` and verifies it contains A's bridge
IP and hostname. Failure indicates that link resolution, hosts file generation, or the
`/etc/hosts` bind-mount injection is broken.

### `test_container_link_alias`
**Requires:** root, rootfs

Same setup as `test_container_link_hosts`, but uses `with_link_alias("link-alias-a", "db")`.
Verifies B's `/etc/hosts` contains both the alias "db" and the original container name
"link-alias-a" on the same line. Failure indicates alias handling in the hosts file
generation is broken.

### `test_container_link_ping`
**Requires:** root, rootfs

Starts container A on bridge (running `sleep`), then starts container B linked to A and
runs `ping -c1 -W2 link-ping-a`. Verifies the ping succeeds, proving both `/etc/hosts`
name resolution and bridge network connectivity work end-to-end. Failure indicates that
the hosts entry is incorrect, the bridge is misconfigured, or containers can't reach each
other.

### `test_container_link_tcp`
**Requires:** root, rootfs

Starts container A on bridge running `echo HELLO_FROM_A | nc -l -p 8080` (a one-shot TCP
server). Registers A's state, then starts container B linked to A. B runs
`nc -w 2 link-tcp-a 8080` to connect by name and capture the response.

Unlike `test_container_link_ping` (ICMP only), this proves TCP connections work across
linked containers — the same protocol used by real services. This test was motivated by
a real bug where iptables `FORWARD policy DROP` (from UFW/Docker) blocked TCP/UDP while
allowing ICMP, making ping succeed but all real traffic fail.

Failure indicates TCP traffic cannot traverse the bridge between containers, possibly
due to missing iptables FORWARD rules in `enable_nat()` or bridge forwarding issues.

### `test_container_link_missing`
**Requires:** root, rootfs

Attempts to spawn a container with `with_link("nonexistent-container-xyz")`. Verifies
that spawn fails with an error message that mentions the missing container name. Failure
indicates that link resolution doesn't properly validate the target container exists before
proceeding with the spawn.

---

## Module: `images`

### `test_layer_extraction`
**Requires:** root

Creates a synthetic tar.gz layer containing two files (one in a subdirectory), extracts
it via `image::extract_layer()`, and verifies the files exist with correct content in
the content-addressable layer store. Failure indicates the tar+gzip extraction pipeline
or layer store layout is broken.

### `test_multi_layer_overlay_merge`
**Requires:** root, rootfs

Creates two temporary layers: bottom (rootfs + `/layer-bottom`) and top (`/layer-top`).
Uses `with_image_layers()` to mount them via overlayfs. Runs `cat` inside the container
to verify both files are visible. Failure indicates multi-layer overlayfs mount construction
or `lowerdir` ordering is broken.

### `test_multi_layer_overlay_shadow`
**Requires:** root, rootfs

Creates bottom layer with `/shadow-file` containing "bottom-value" and top layer with
`/shadow-file` containing "top-value". Uses `with_image_layers()` to verify the top
layer's file shadows the bottom. Failure indicates overlayfs layer ordering (top-first
lowerdir) is incorrect.

### `test_image_layers_cleanup`
**Requires:** root, rootfs

Spawns a container with `with_image_layers()`, captures the overlay merged-dir path,
waits for exit, then verifies the ephemeral overlay directory (merged + upper + work)
was cleaned up by `wait()`. Failure indicates the cleanup logic for image-layer overlay
dirs is broken.

### `test_pull_and_run_real_image`
**Requires:** root, network access
**Ignored by default** — run with `--ignored`

End-to-end test of the full OCI image pipeline. Pulls `alpine:latest` from Docker Hub
using the `pelagos` binary, loads the manifest, mounts layers via `with_image_layers()`,
and runs `cat /etc/alpine-release` inside the container. Verifies the output is a valid
Alpine version string. Failure indicates a regression anywhere in the chain: registry
pull, layer extraction, manifest persistence, multi-layer overlay mount, or container exec.

---

## Module: `exec`

Tests for `pelagos exec` — running commands inside running containers via
namespace join + `/proc/{pid}/root` chroot.

### `test_exec_basic`
**Requires:** root, rootfs

Starts a `sleep 30` container with UTS+MOUNT namespaces (no PID namespace —
see note below), then spawns an exec'd process (`/bin/cat /etc/os-release`) by
joining the container's mount namespace via `setns()` + `fchdir()` +
`chroot(".")` in a pre_exec callback. Verifies exit code 0 and non-empty output.

Failure indicates that the setns + fchdir + chroot pattern used by `pelagos exec`
is broken — either `setns()` fails, fchdir to the container root fd doesn't
work, or the exec'd process can't see the container's filesystem.

**Note:** PID namespace is omitted because `Namespace::PID` triggers a
double-fork where `container.pid()` returns the intermediate process (which
never execs and stays in the host namespaces), not the actual container. The
real `pelagos exec` CLI gets the correct PID from state.json.

### `test_exec_sees_container_filesystem`
**Requires:** root, rootfs

Starts a container that writes `EXEC_MARKER_12345` to `/tmp/exec-marker` (on
a tmpfs), then exec's `/bin/cat /tmp/exec-marker` via mount namespace join.
Verifies the output matches the marker value.

Failure indicates the exec'd process is not correctly entering the container's
mount namespace — it would see the host's `/tmp` instead of the container's
tmpfs, and the marker file would not exist. The `fchdir(root_fd) + chroot(".")`
technique (same as `nsenter(1)`) is critical here: a plain `chroot("/")` after
`setns(MOUNT)` would chroot to the host root, not the container's.

### `test_exec_environment`
**Requires:** root, rootfs

Starts a container with `FOO=bar_from_container` in its environment, reads
`/proc/{pid}/environ` to discover the env vars, applies them to the exec'd
command (`/bin/sh -c 'echo $FOO'`), and verifies the output is
`bar_from_container`.

Failure indicates that `/proc/{pid}/environ` reading or env propagation to
the exec'd process is broken.

### `test_exec_nonrunning_container_fails`
**Requires:** root

Verifies that `kill(999999, 0)` returns false (PID not alive) and
`/proc/999999/root` does not exist. This is the guard logic `pelagos exec`
uses to reject exec into stopped containers.

Failure indicates a kernel or procfs anomaly where dead PIDs still appear alive.

### `test_exec_joins_pid_namespace`
**Requires:** root, rootfs

Starts a detached container with `pelagos run -d --rootfs alpine /bin/sleep 30`.
The `--rootfs` path always enables `Namespace::PID`, so `state.pid` is the
intermediate process P whose `/proc/P/ns/pid` is the host PID namespace, but
`/proc/P/ns/pid_for_children` points to the container's PID namespace.

Runs `pelagos exec <name> /bin/echo hello-from-exec` to verify basic exec, then runs
`pelagos exec <name> readlink /proc/self/ns/mnt` and asserts it exits 0 and returns
a `mnt:[...]` string.

The `readlink /proc/self/ns/mnt` assertion is the regression test for issue #121:
before the fix, `setns(CLONE_NEWPID)` was skipped in `cmd_exec` so exec'd processes
ran in the host PID namespace. Inside the container's /proc (which is mounted for the
container's PID namespace), `/proc/self` was a dangling 0-byte symlink for processes
not in that namespace, causing `readlink` to fail with exit code 1. VS Code's
`resolveAuthority()` runs exactly this probe and rejects non-zero exit.

Failure indicates `cmd_exec` is no longer calling `setns(CLONE_NEWPID)` in the parent
before fork, or `discover_namespaces` is not finding the PID namespace via the
`pid_for_children` fallback.

---

## Watcher Process Tests (`watcher` module)

### `test_watcher_kill_propagates_to_container`
**Requires:** root, rootfs

Starts a detached container with `pelagos run -d --rootfs alpine /bin/sleep 300`.
Reads `state.pid` (the intermediate process P), then reads P's `PPid` from
`/proc/<P>/status` to find the watcher PID.  Sends `SIGKILL` to the watcher
and polls for up to 3 seconds to verify the container process P also dies.

This tests that `PR_SET_CHILD_SUBREAPER` is effective: when the watcher is
killed, P (and therefore C inside the PID namespace) is re-parented to the
watcher rather than to host init, so the watcher's death triggers P's
`PR_SET_PDEATHSIG` in one hop.

Failure means the container process survives after the watcher is killed —
either the subreaper prctl was not called, or the kernel did not honour it.
A failing test indicates containers would become orphaned on an unexpected
watcher crash (OOM kill, etc.).

### `healthcheck_tests::test_probe_child_pid_is_killable`
**Requires:** root, rootfs

Verifies that a health-probe child process can be SIGKILL'd from outside, which
is the mechanism `run_probe` uses to clean up a timed-out probe.

Starts a container, then spawns a second `Command::new("sleep").args(["300"])`
inside the container's rootfs (via `with_chroot("/proc/{pid}/root")`).  Records
the spawned probe's host PID, sends `SIGKILL` to it, calls `probe.wait()` to
reap the zombie, then asserts `kill(probe_pid, 0)` returns `ESRCH` — confirming
the PID slot was released.

Failure means that after SIGKILL + wait the process still appears alive (e.g.
because zombie reaping didn't work), which would prevent the health monitor from
detecting that a timed-out probe child was successfully cleaned up.

---

## Log Relay Tests (`cli::relay` unit tests)

These tests live directly in `src/cli/relay.rs` and run via `cargo test --bin pelagos`
(no root required).

### `cli::relay::tests::test_relay_captures_stdout_and_stderr`
**Requires:** none (no root, no rootfs)

Spawns `sh -c "printf 'hello stdout'; printf 'hello stderr' >&2"` with piped
stdio, passes the handles to `start_log_relay`, joins the relay thread after
`child.wait()`, and asserts both log files contain the expected strings.

Failure indicates the epoll relay loop is not writing pipe data to the log files
(e.g. fd registration failed, write error was silently dropped, or the thread
exited before draining the pipe).

### `cli::relay::tests::test_relay_large_output`
**Requires:** none (no root, no rootfs)

Spawns `yes x | head -c 65536` (65 536 bytes — 8× the `BUF` read size) and
relays its stdout to a log file. After the relay thread finishes, asserts the
log file is exactly 65 536 bytes.

Failure indicates that multi-cycle relay (where epoll fires multiple times because
data exceeds one read buffer) is losing or truncating data.

### `cli::relay::tests::test_relay_none_handles`
**Requires:** none (no root, no rootfs)

Calls `start_log_relay(None, None, ...)` and joins the thread. Verifies the relay
exits immediately when no pipe fds are registered.

Failure indicates the relay loop hangs or panics when given empty input.

---

## Minimal /dev Tests (`dev` module)

### `test_dev_minimal_devices`
**Requires:** root + rootfs

Spawns a container with `with_dev_mount()` and lists `/dev/`. Asserts that safe
devices (`null`, `zero`, `random`, `urandom`, `full`, `tty`) are present, and
host-specific devices (`sda`, `nvme`, `video`) are absent.

Failure indicates the minimal /dev setup is not populating safe devices, or that
host device nodes are leaking into the container.

### `test_dev_null_works`
**Requires:** root + rootfs

Runs `echo ok > /dev/null && echo pass` inside a container with `with_dev_mount()`.
Asserts that the output contains "pass", confirming `/dev/null` is a functional
device (accepts writes without error).

Failure indicates `/dev/null` is not properly bind-mounted from the host.

### `test_dev_zero_works`
**Requires:** root + rootfs

Runs `head -c 4 /dev/zero | wc -c` inside a container with `with_dev_mount()`.
Asserts that output contains "4", confirming `/dev/zero` produces zero bytes.

Failure indicates `/dev/zero` is not properly bind-mounted from the host.

### `test_dev_symlinks`
**Requires:** root + rootfs

Checks that `/dev/fd`, `/dev/stdin`, `/dev/stdout`, and `/dev/stderr` are
symlinks inside a container with `with_dev_mount()`.

Failure indicates the minimal /dev setup is not creating the standard symlinks
that many programs depend on.

### `test_dev_pts_exists`
**Requires:** root + rootfs

Checks that `/dev/pts` and `/dev/shm` directories exist inside a container
with `with_dev_mount()`.

Failure indicates the minimal /dev setup is not creating the required
subdirectories for PTY allocation and shared memory.

---

## Rootless Cgroups

These tests exercise cgroup v2 delegation for non-root users. They skip
automatically if `is_delegation_available()` returns false (no v2, no
delegated controllers, or non-writable cgroup tree).

Run without root:
```bash
cargo test --test integration_tests rootless_cgroups -- --test-threads=1
```

### `test_rootless_cgroup_memory`
**Requires:** non-root + rootfs + cgroup v2 delegation

Sets `with_cgroup_memory(64MB)` on a rootless container and reads
`/sys/fs/cgroup/memory.max` inside it. Asserts the value is `67108864`.

Failure indicates the rootless cgroup path was not created, the memory
controller is not delegated, or the child was not moved into the sub-cgroup.

### `test_rootless_cgroup_pids`
**Requires:** non-root + rootfs + cgroup v2 delegation

Sets `with_cgroup_pids_limit(16)` on a rootless container and reads
`/sys/fs/cgroup/pids.max` inside it. Asserts the value is `16`.

Failure indicates the pids controller is not delegated or the limit was
not written to the sub-cgroup.

### `test_rootless_cgroup_cleanup`
**Requires:** non-root + rootfs + cgroup v2 delegation

Spawns a rootless container with a memory cgroup, waits for it to exit,
then checks that the sub-cgroup directory (`pelagos-{pid}`) under the
user's cgroup slice has been removed.

Failure indicates `teardown_rootless_cgroup()` did not successfully
remove the directory, which would leak cgroup entries over time.

---

## Rootless ID Mapping Tests (`rootless_idmap`)

Tests for multi-UID/GID mapping via `newuidmap`/`newgidmap` helpers and
subordinate ID ranges from `/etc/subuid` and `/etc/subgid`.

```bash
cargo test --test integration_tests rootless_idmap -- --test-threads=1
```

### `test_rootless_multi_uid_maps_written`
**Requires:** non-root + rootfs + newuidmap/newgidmap + subuid/subgid ranges

Spawns a rootless container without explicitly setting UID maps, letting
auto-config detect subordinate ranges and use the helpers. Reads
`/proc/self/uid_map` inside the container and asserts at least 2 mapping
lines are present (container root → host UID, and subordinate range).

Failure indicates the auto-detection of subordinate ranges failed, the
pipe+thread sync mechanism deadlocked, or `newuidmap` did not write
the multi-range mapping.

### `test_rootless_multi_uid_file_ownership`
**Requires:** non-root + rootfs + newuidmap/newgidmap + subuid/subgid ranges

Spawns a rootless container with multi-UID auto-config and runs
`stat -c '%u' /etc/passwd`. Asserts the file is owned by UID 0 (root)
inside the container.

Failure indicates files owned by root in the image are showing up as
`nobody` (65534) due to missing subordinate UID mappings, meaning the
multi-range mapping was not applied.

### `test_rootless_single_uid_fallback`
**Requires:** non-root + rootfs

Spawns a rootless container with an explicit single-UID map (bypassing
auto-config). Runs `id -u` and asserts it prints `0`.

Failure indicates the single-UID fallback path (existing behavior) is
broken, which would be a regression from the multi-UID changes.

### `test_rootless_overlay_mode0_mkdir_succeeds`
**Requires:** non-root + alpine image in local store

Runs a rootless container with `with_image_layers()` and executes `mkdir -m 000
/tmp/mode0test && echo ok`. Asserts the container exits 0 and stdout contains `ok`.

This is the regression test for the dpkg/Debian image build failure: dpkg creates
staging directories with mode=0 as a security measure, and overlayfs copy-up of
those directories fails unless `fuse-overlayfs` has `CAP_DAC_OVERRIDE` (which it
gets when running as uid 0 inside its user namespace).

Fixed across multiple commits (issue #195):
- Removed a stale CVE-2023-0386 fast-path that incorrectly blocked native overlayfs
- Pre-seeded `resolv.conf`/`/etc/hosts` into overlay upper dir to avoid bind-mount
  EINVAL inside user namespaces (issue #112 pattern)
- Changed rootless `fuse-overlayfs` launch: instead of a pre-fork "launcher"
  process (whose user namespace was a sibling of the container's), `fuse-overlayfs`
  is now forked **inline in `pre_exec`** after `CLONE_NEWUSER+CLONE_NEWNS`, so it
  shares the container's user namespace. This fixes the `fuse_allow_current_process`
  `current_in_userns` check that returned EACCES for sibling namespaces.

Failure indicates that the inline fuse-overlayfs launch was reverted, or that the
`CLONE_NEWUSER` → fuse-overlayfs user-namespace relationship was broken.

### `test_rootless_multi_gid_chown_succeeds`
**Requires:** non-root + rootfs + newuidmap/newgidmap + subgid range

Creates a file inside a rootless container and runs `chown 0:4 /tmp/testfile`
(GID 4 = `adm` in Debian/Ubuntu). Asserts the chown succeeds.

This is the regression test for issue #194: rootless builds with Debian/Ubuntu
fail when dpkg postinst scripts call `chown root:adm` because GID 4 is not
mapped in the user namespace when multi-range GID mapping silently fails.
Failure indicates multi-range GID maps are not being applied, meaning
Debian/Ubuntu builds will fail with EINVAL on any `chown` to GID > 0.

---

## JSON Output Tests

These tests verify the `--format json` flag on all list commands and the
`container inspect` command. They exercise create→list→remove→list cycles
to ensure JSON output is correct and consistent.

### `test_volume_ls_json`
**Requires:** root

Creates a volume, runs `volume ls --format json`, and verifies the JSON array
contains an entry with the correct `name` and `path` fields. Removes the volume
and verifies the entry is gone from the JSON output.

Failure indicates JSON serialization of volumes is broken or the `--format`
flag is not wired correctly to `cmd_volume_ls`.

### `test_rootfs_ls_json`
**Requires:** root

Imports a rootfs entry (symlink to `/tmp`), runs `rootfs ls --format json`,
and verifies the JSON array contains an entry with the correct `name` and
`path` fields. Removes the entry and verifies it is gone from the JSON output.

Failure indicates JSON serialization of rootfs entries is broken or the
`--format` flag is not wired correctly to `cmd_rootfs_ls`.

### `test_ps_json_and_inspect`
**Requires:** root

Writes a synthetic container `state.json` to the containers directory, verifies
`ps -a --format json` includes the container with the correct name. Runs
`container inspect <name>` and verifies the returned JSON object has `name`,
`pid`, and `status` fields. Removes the container via `rm` and verifies it is
gone from the JSON listing.

Failure indicates JSON serialization of container state is broken, the
`--format` flag is not wired correctly, or `container inspect` does not work.

### `test_image_ls_json`
**Requires:** root

Runs `image ls --format json` and verifies the output is a valid JSON array.
If images are present, validates each entry has `reference`, `digest`, and
`layers` fields. If no images exist, verifies the output is `[]`.

Failure indicates JSON serialization of image manifests is broken or the
`--format` flag is not wired correctly to `cmd_image_ls`.

---

## Build Instructions (ENTRYPOINT, LABEL, USER)

### `test_parse_entrypoint_json`
**Requires:** neither root nor rootfs (parser-only)

Parses `ENTRYPOINT ["python3", "-m", "http.server"]` and verifies it produces
`Instruction::Entrypoint` with the correct argument list. Also checks that CMD
on the next line is parsed independently.

Failure indicates the ENTRYPOINT JSON-form parser is broken.

### `test_parse_entrypoint_shell_form`
**Requires:** neither root nor rootfs (parser-only)

Parses `ENTRYPOINT /usr/bin/myapp --flag` (shell form) and verifies it is
wrapped in `/bin/sh -c ...` like CMD shell form.

Failure indicates shell-form ENTRYPOINT wrapping is broken.

### `test_parse_label_quoted_and_unquoted`
**Requires:** neither root nor rootfs (parser-only)

Parses `LABEL maintainer="Jane Doe"` and `LABEL version=2.0`, verifying both
quoted and unquoted value forms produce correct key-value pairs.

Failure indicates LABEL value parsing or quote stripping is broken.

### `test_parse_user_with_gid`
**Requires:** neither root nor rootfs (parser-only)

Parses `USER 1000:1000` and verifies the full string is captured as-is
(parsing uid:gid is the runtime's responsibility, not the parser's).

Failure indicates USER instruction parsing is broken.

### `test_image_config_labels_serde_roundtrip`
**Requires:** neither root nor rootfs (serialization-only)

Creates an `ImageConfig` with labels, serializes to JSON, deserializes, and
verifies labels survive the round-trip. Also verifies that missing `labels`
key in JSON deserializes to an empty HashMap (serde default).

Failure indicates the `labels` field has broken serde attributes.

### `test_image_config_user_field`
**Requires:** neither root nor rootfs (serialization-only)

Verifies `ImageConfig.user` and `ImageConfig.entrypoint` round-trip through
JSON correctly, and that missing `user` key defaults to empty string.

Failure indicates the `user` or `entrypoint` field serde default is broken.

### `test_full_remfile_with_all_instructions`
**Requires:** neither root nor rootfs (parser-only)

Parses a Remfile using every supported instruction type (FROM, LABEL, ENV,
USER, WORKDIR, COPY, RUN, ENTRYPOINT, CMD, EXPOSE) and verifies the complete
instruction list has 10 entries of the correct variant types.

Failure indicates a regression in any instruction parser.

### `test_parse_arg_instruction`
**Requires:** neither root nor rootfs (parser-only)

Parses a Remfile containing ARG before FROM (Docker compat) and ARG after FROM,
verifying both produce correct `Instruction::Arg` variants with names and defaults.
Also exercises `substitute_vars` with `$VAR`, `${VAR}`, and `$$` escape sequences.

Failure indicates the ARG parser or variable substitution engine is broken.

### `test_remignore_filtering`
**Requires:** neither root nor rootfs

Creates a temporary directory with a `.remignore` file excluding `*.log` and `build/`.
Populates the directory with matching and non-matching files, then runs a filtered copy.
Verifies excluded files (`debug.log`, `build/output`) are absent and kept files
(`app.rs`, `src/lib.rs`) are present in the destination.

Failure indicates `.remignore` pattern loading or the filtered copy logic is broken.

### `test_parse_add_instruction`
**Requires:** neither root nor rootfs (parser-only)

Parses a Remfile with ADD instructions for both local archive and URL sources.
Verifies both produce correct `Instruction::Add` variants with src/dest fields.

Failure indicates the ADD parser is broken.

### `test_add_local_tar_extraction`
**Requires:** neither root nor rootfs

Creates a temporary `.tar.gz` archive containing two files (one in a subdirectory),
extracts it using the same tar+flate2 pipeline that ADD uses, and verifies both files
are present with correct contents.

Failure indicates the ADD archive extraction logic is broken.

### `test_parse_multi_stage_remfile`
**Requires:** neither root nor rootfs (parser-only)

Parses a two-stage Remfile (`FROM alpine:3.19 AS builder` + `FROM alpine:3.19` +
`COPY --from=builder`). Verifies:
- First `FROM` has alias `"builder"`
- Second `FROM` has no alias
- `COPY --from=builder` has correct `from_stage` field
- Regular `COPY` has `from_stage: None`

Failure indicates multi-stage `FROM ... AS` or `COPY --from=` parsing is broken.

---

## Port Proxy

### `test_port_proxy_localhost_connectivity`
**Requires:** root, alpine-rootfs, `nc` on host

Spawns a bridge+NAT container running a one-shot TCP server on port 80,
forwarded from host port 19190. Connects from **localhost** (127.0.0.1)
to verify the userspace TCP proxy handles localhost traffic that nftables
DNAT in PREROUTING cannot intercept.

Failure indicates the userspace TCP proxy (`start_port_proxies()`) is broken
or not relaying localhost connections to the container.

### `test_port_proxy_cleanup_on_teardown`
**Requires:** root, alpine-rootfs

Spawns a container with a port forward that exits immediately, waits for it,
then verifies the proxy port is no longer bound (a fresh `TcpListener::bind`
on the same port should succeed).

Failure indicates the proxy runtime is not shut down during teardown, leaving
orphaned listener tasks holding the port.

---

### `test_port_proxy_multiple_connections`
**Requires:** root, alpine-rootfs

Spawns a container with port 19192→8080 running a static-response server
(`while true; do echo PONG | nc -l -p 8080; done`). Makes 5 sequential
connections from the host through the async proxy; each connection reads the
response and verifies it contains "PONG".

Failure indicates the tokio accept loop exits prematurely after the first relay
task completes, or that `copy_bidirectional` does not propagate server-side EOF
cleanly (causing subsequent connections to hang or return empty data).

### `test_bridge_duplicate_host_port_rejected`
**Requires:** root, alpine-rootfs

Starts a bridge-networked container with host port 19876 forwarded to container port 80.
While that container is running, attempts to start a second container with the same host
port. Asserts the second spawn returns an error whose message names the conflicting port.

Failure indicates `enable_port_forwards` is not checking the existing port-forward registry
for live conflicts, allowing two containers to race for the same nftables DNAT rule and TCP
proxy binding (silent misbehaviour: only the first or last container gets traffic).

### `test_pasta_duplicate_host_port_rejected`
**Requires:** root, alpine-rootfs, pasta installed

Pre-binds host port 19877 with a `TcpListener`, then attempts to spawn a pasta-networked
container forwarding that port. Asserts spawn returns an error.

Failure indicates `setup_pasta_network` is not checking port availability before spawning
pasta, meaning pasta's port-bind failure ("Couldn't listen on requested ports", exit 1) is
silently swallowed and the container starts with no working port forwarding.

### `test_bridge_duplicate_host_port_rejected_cli`
**Requires:** root, alpine-rootfs

Same conflict scenario as `test_bridge_duplicate_host_port_rejected` but exercises the
`pelagos run -d` CLI path (watcher process) rather than the library API. Verifies that the
spawn error is propagated from the watcher back to the CLI parent via the sync pipe and
appears in stderr — not the generic "watcher exited before writing state" message.

Failure indicates the watcher exits silently on spawn failure without writing the error to
the sync pipe, so the parent cannot surface the actual cause.

---

## Multi-Network Tests

### `test_network_create_ls_rm`
**Requires:** root

Creates a `NetworkDef` with subnet `10.99.1.0/24`, saves it to disk, loads it
back, and verifies all fields round-trip correctly. Then cleans up and confirms
the config file is removed.

Failure indicates `NetworkDef::save()`/`load()` serialization or path helpers
are broken.

### `test_network_create_overlap_rejected`
**Requires:** root

Creates a network with subnet `10.77.0.0/16`, then checks that a second network
with `10.77.1.0/24` is detected as overlapping via `Ipv4Net::overlaps()`.

Failure indicates subnet overlap detection is broken, which would allow users
to create networks with conflicting address ranges.

### `test_network_name_validation`
**Requires:** none (API-only)

Verifies name length constraints (> 12 chars), invalid character detection
(underscores), leading-hyphen rejection, and CIDR parsing edge cases.

Failure indicates the name validation logic or `Ipv4Net::from_cidr()` parser
has a regression.

### `test_named_network_container`
**Requires:** root, alpine-rootfs

Creates a custom network `testnet2` with subnet `10.98.1.0/24`, spawns a
container on it using `NetworkMode::BridgeNamed("testnet2")`, and checks that
the container's `eth0` has an IP in the `10.98.1.x` range.

Failure indicates the full named-network pipeline is broken: `NetworkDef`
loading, bridge creation, IPAM allocation, or veth configuration.

### `test_default_network_backwards_compat`
**Requires:** root, alpine-rootfs

Spawns a container using `NetworkMode::Bridge` (the legacy enum variant) and
verifies it gets a `172.19.0.x` IP, confirming that the `Bridge` →
`BridgeNamed("pelagos0")` normalization and default network bootstrap work.

Failure indicates the backwards-compatibility path from `NetworkMode::Bridge`
to the new per-network architecture is broken.

### `test_network_rm_refuses_default`
**Requires:** root

Bootstraps the default network and verifies the config file exists. This tests
that the default `pelagos0` network is always available and cannot be removed.

Failure indicates `bootstrap_default_network()` is not persisting the config.

### `test_multi_network_dual_interface`
**Requires:** root, alpine-rootfs

Creates two test networks (`mntest1` at `10.99.1.0/24`, `mntest2` at `10.99.2.0/24`),
spawns a container on both using `with_network()` + `with_additional_network()`, and
verifies that eth0 has a `10.99.1.x` IP and eth1 has a `10.99.2.x` IP. Also checks
the `container_ip()` and `container_ip_on()` accessors return the correct IPs.

Failure indicates `attach_network_to_netns()` is not correctly configuring the secondary
interface, or the IPAM allocation is assigning IPs from the wrong subnet.

### `test_multi_network_isolation`
**Requires:** root, alpine-rootfs

Creates two isolated networks (`mniso1`, `mniso2`). Spawns container A on net1 only,
container B on net2 only, and container C on both. Verifies C can ping both A and B,
but a container on net1 alone cannot ping B (on net2).

Failure indicates network isolation is broken — traffic is leaking between bridges
that should be completely separate.

### `test_multi_network_teardown`
**Requires:** root, alpine-rootfs

Spawns a container on two networks, records the netns name and both veth interface
names, then waits for exit. Verifies that the named netns no longer exists at
`/run/netns/` and both veth pairs (primary and secondary) are removed.

Failure indicates `teardown_secondary_network()` or `teardown_network()` is not
cleaning up properly, which would leak network namespaces or veth interfaces.

### `test_multi_network_link_resolution`
**Requires:** root, alpine-rootfs

Creates two networks, starts a "server" container on both, writes its state.json
with `network_ips` map, then starts a "client" on net2 only with `--link server`.
Verifies that `/etc/hosts` contains the server's net2 IP (the shared network),
not its net1 IP.

Failure indicates `resolve_container_ip_on_shared_network()` is not correctly
matching networks, causing links to resolve to IPs on unreachable networks.

---

## DNS Service Discovery

### `test_dns_resolves_container_name`
**Requires:** root, rootfs

Spawns container A (sleep) on a bridge network, registers it with DNS, then
spawns container B on the same network and runs `nslookup`. Verifies the
resolved IP matches A's bridge IP.

Failure means the embedded DNS daemon isn't resolving container names correctly.

### `test_dns_upstream_forward`
**Requires:** root, rootfs, host internet access to 8.8.8.8:53

Registers a dummy DNS entry to start the daemon, then resolves `example.com`
from inside a container via the gateway DNS. Verifies the daemon forwards
unknown queries to upstream DNS and relays the response.

The test first waits (up to 2s) for the daemon to bind to the gateway IP, then
checks host reachability of 8.8.8.8:53 and skips if unreachable. nslookup
inside the container is capped at 10s with `timeout` to prevent hanging.

Failure means the daemon can't forward queries to upstream DNS servers, or
the nslookup inside the container can't reach the gateway.

### `test_dns_network_isolation`
**Requires:** root, rootfs

Registers "alpha" on net1 and "beta" on net2. Container on net2 tries to
resolve "alpha" — should get NXDOMAIN. Verifies DNS respects network
boundaries.

Failure means DNS is leaking names across networks.

### `test_dns_multi_network`
**Requires:** root, rootfs

Container A on net1+net2, registers on both. Container B on net2 resolves A —
should get A's net2 IP, not net1 IP.

Failure means DNS is returning the wrong IP for multi-network containers.

### `test_dns_daemon_lifecycle`
**Requires:** root + rootfs

Spawns a holder container to create the bridge, then adds a DNS entry — daemon
should start (PID file appears, process alive). Removes the entry — daemon
should auto-exit.

Failure means the daemon lifecycle management is broken.

### `test_dns_dnsmasq_resolves_container_name`
**Requires:** root, rootfs, dnsmasq installed

Same as `test_dns_resolves_container_name` but with `PELAGOS_DNS_BACKEND=dnsmasq`.
Container B resolves container A by name via dnsmasq. Verifies the backend marker
file says "dnsmasq" and the resolved IP matches A's bridge IP.

Failure means dnsmasq backend isn't resolving container names correctly or the
hosts file generation is broken.

### `test_dns_dnsmasq_upstream_forward`
**Requires:** root, rootfs, dnsmasq installed

Registers a dummy DNS entry to start dnsmasq, then resolves `example.com` via
the gateway. Verifies upstream forwarding works through dnsmasq's `server=`
directives.

Failure means dnsmasq can't forward queries to upstream DNS servers, likely a
config generation issue.

### `test_dns_dnsmasq_lifecycle`
**Requires:** root, rootfs, dnsmasq installed

Adds a DNS entry with dnsmasq backend — daemon should start (PID file appears,
process alive, backend marker says "dnsmasq"). Removes entry and sends SIGTERM.

Failure means dnsmasq lifecycle management (start/stop/PID tracking) is broken.

---

## Drop Cleanup Tests

### `test_child_drop_cleans_up_netns`
**Requires:** root, rootfs

Spawns a container with bridge networking (which creates a named network namespace
under `/run/netns/rem-{pid}-{n}`), records the netns name, then drops the `Child`
without calling `wait()`. Asserts that the netns mount is removed after drop.

Failure means the `Drop` implementation for `Child` is not properly tearing down
network namespaces, which would cause stale `/run/netns/rem-*` mounts to
accumulate over time (especially from test panics or early returns).

---

## Compose Tests

### `test_sexpr_parse_compose_file`
**Type:** No-root

Parses a full compose file example through the S-expression parser (`pelagos::sexpr::parse`).
Verifies the top-level structure: the root is a list starting with `compose`, containing
the expected number of declarations (networks, volumes, services).

Failure means the S-expression parser cannot handle the compose file syntax (comments,
nested lists, quoted strings, keyword arguments).

### `test_compose_parse_and_validate`
**Type:** No-root

Parses a compose file through the full pipeline (`pelagos::compose::parse_compose`) which
includes S-expression parsing, AST-to-struct transformation, and cross-reference validation.
Checks that all fields are correctly populated: networks with subnets, volumes, service
names/images/networks/volumes/env/ports/memory, and dependency with `:ready-port`.

Failure means the compose model parser is dropping or misinterpreting fields from the AST.

### `test_compose_topo_sort`
**Type:** No-root

Verifies topological sort of service dependencies: given web -> api -> db, the sort must
produce db before api before web. Uses `pelagos::compose::topo_sort`.

Failure means services would be started in wrong order, causing dependency failures.

### `test_compose_cycle_detection`
**Type:** No-root

Verifies that a circular dependency (a -> b -> a) is detected and reported as a
`DependencyCycle` error by the compose parser/validator.

Failure means `compose up` would hang or stack overflow on circular dependencies.

### `test_compose_unknown_dependency`
**Type:** No-root

Verifies that a `depends-on` referencing a nonexistent service produces an
`UnknownDependency` error.

Failure means typos in service names would be silently ignored, causing runtime failures.

### `test_compose_up_down_single_service`
**Requires:** root, rootfs

Verifies compose project state directory creation and cleanup. Creates a compose project
directory, asserts it exists, then cleans it up. This exercises the compose path helpers
(`compose_project_dir`, `compose_state_file`).

Failure means the compose state filesystem layout is broken.

### `test_compose_bind_mount_parse_and_validate`
**Requires:** nothing (no root, no rootfs, no image pull)

Verifies that `(bind-mount host container)` and `(bind-mount host container :ro)` parse
correctly through `parse_compose` in a realistic multi-service monitoring-stack compose file.
Asserts that `BindMount` structs carry the right `host_path`, `container_path`, and
`read_only` values, that named volumes and bind mounts coexist on the same service, and that
the topological sort still orders dependents correctly.

Failure means bind-mount entries would be silently dropped or misread, causing containers to
start without their config files and then crash or produce wrong results.

### `test_compose_tmpfs_parse_and_validate`
**Requires:** nothing (no root, no rootfs, no image pull)

Verifies that `(tmpfs "/path")` entries in a compose service spec parse into
`ServiceSpec.tmpfs_mounts` as plain path strings, in declaration order. Asserts
that a service with a single tmpfs entry carries exactly one path, that a service
with two `(tmpfs ...)` entries carries both in order, and that tmpfs mounts coexist
correctly with `depends-on` without disrupting topological sort.

Failure means `(tmpfs ...)` entries would be silently dropped by the parser,
causing containers to launch without the intended in-memory filesystems — for
example, an app writing to a read-only path would fail immediately on startup.


### `test_compose_health_check_parse`
**Requires:** nothing (no root, no rootfs, no image pull)

Verifies that all `depends-on` health-check expression forms parse into the correct
`HealthCheck` enum variants via `parse_compose`. Exercises every syntax form in a single
compose file:

- `:ready (port N)` → `HealthCheck::Port(N)`
- `:ready (http "URL")` → `HealthCheck::Http(url)`
- `:ready (cmd "str")` (single-string, split on whitespace) → `HealthCheck::Cmd(argv)`
- `:ready (and (port N) (cmd "..."))` → `HealthCheck::And([Port, Cmd])`
- `:ready (or (port N) (http "..."))` → `HealthCheck::Or([Port, Http])`
- `:ready-port N` (backward-compat sugar) → `HealthCheck::Port(N)`

Also asserts that a service with no `depends-on` has an empty `depends_on` vec.

Failure means the parser produces wrong `HealthCheck` variants, so `eval_health_check` would
evaluate incorrect conditions and the compose supervisor would start services out of order or
time out waiting for the wrong signal.


### `test_lisp_compose_basic`
**Requires:** nothing (no root, no rootfs, no container spawning)

End-to-end test of the Lisp interpreter path in the compose subsystem. Evaluates a
`.reml`-style string that:
1. Defines a parameterised service factory `(mk-service name img net)` using `define`
2. Builds three `ServiceSpec` values with `map` and a lambda over a quoted list of pairs
3. Registers an `on-ready` hook for the `"db"` service
4. Calls `compose-up` with a `ComposeSpec` that includes one named network and the three services

After evaluation, retrieves the `PendingCompose` via `Interpreter::take_pending()` and asserts:
- Exactly one network named `"backend"` with subnet `"10.90.0.0/24"`
- Exactly three services named `"db"`, `"api"`, `"web"`
- `"db"` service has image `"postgres:16"` and network `"backend"`
- At least one `on-ready` hook registered for `"db"` via `take_hooks()`

Failure indicates a regression in: parser reader macros (quote/quasiquote), `define`/`lambda`,
`map`, the `service`/`network`/`compose`/`compose-up` builtins, list flattening in `compose`,
or the `on-ready` hook registration pipeline.

### `test_compose_declarative_through_evaluator`
**Requires:** nothing (no root, no rootfs, no container spawning)

Regression test for the consolidation that dropped `.rem` compose files: all compose files
now go through the Lisp evaluator, including those that use only static declarations (no
`define`, `lambda`, or other dynamic features). Evaluates a `compose-up` form containing
a plain 2-network, 1-volume, 3-service stack and asserts:
- Correct network/volume/service counts
- Topo order respects `depends-on`: db → api → proxy
- API service has both networks, correct depends-on with port 5432
- Proxy ports round-trip correctly

Failure indicates that purely declarative compose files broke when routed through the
evaluator — the most common user-facing regression from the .rem → .reml unification.

### `test_compose_default_file_is_reml`
**Requires:** nothing (runs `pelagos compose up --help`)

Guards against the CLI default file regressing from `compose.reml` back to `compose.rem`.
Runs `pelagos compose up --help` and asserts the help text shows `compose.reml` as the
default and does not contain `compose.rem` as a default. Failure means users whose projects
contain only `compose.reml` would get a "file not found" error when running `pelagos compose up`
without `-f`.

### `test_lisp_evaluator_tco_and_higher_order`
**Requires:** nothing (no root, no rootfs, no container spawning)

Pure evaluator correctness and TCO stress test:

1. **TCO**: Defines a named-let loop `(sum-to n)` that accumulates a sum with a tail call.
   Invokes `(sum-to 10000)` — 10,000 iterations that would overflow the stack without TCO.
   Asserts the result equals `Value::Int(50005000)`.

2. **map + lambda**: Evaluates `(map (lambda (x) (* x x)) '(1 2 3 4 5))` and asserts the
   result is the Lisp list `(1 4 9 16 25)` represented as `Value::Pair` chains.

Failure means either: (a) TCO is broken and the evaluator stack-overflows on deep tail
recursion; or (b) `map`, `lambda`, arithmetic, or list construction is incorrect.


### `test_lisp_eval_file_web_stack_fixture`
**Requires:** nothing (no root, no rootfs, no container spawning)

Reads `examples/compose/web-stack/compose.reml` from disk via `Interpreter::eval_file()`.
This is the primary test of the file-read path — all previous Lisp tests used inline strings
via `eval_str()`.

Asserts the full parsed and evaluated `ComposeFile` structure:
- Two networks: `"frontend"` (subnet `10.88.1.0/24`) and `"backend"`
- One volume: `"notes-data"`
- Three services: `"redis"`, `"app"`, `"proxy"`
- `redis`: image `web-stack-redis:latest`, network `backend`, memory `64m`, no deps
- `app`: both networks, `depends-on redis` with `HealthCheck::Port(6379)`, `REDIS_HOST` env set
- `proxy`: network `frontend`, `depends-on app` with `HealthCheck::Port(5000)`,
  host port 8080 (default — `$BLOG_PORT` not set in test environment)
- `on-ready` hooks registered for both `"redis"` and `"app"`

Failure means the `eval_file()` path is broken, the `env`-with-fallback pattern
evaluates incorrectly, named `define` variables don't compose correctly, or the
`depends-on` port extension isn't wired through.

### `test_lisp_depends_on_with_port`
**Requires:** nothing (no root, no rootfs, no container spawning)

Unit test for the `(list 'depends-on "svc" N)` → `HealthCheck::Port(N)` extension
added to `apply_service_opt`. Evaluates a service with two `depends-on` options: one
with a port and one without. Asserts:
- `depends-on "db" 5432` produces `Dependency { service: "db", health_check: Some(Port(5432)) }`
- `depends-on "cache"` (no port) produces `Dependency { service: "cache", health_check: None }`

Failure means the `.reml` format cannot express TCP readiness checks on dependencies,
making the Lisp compose path weaker than the static `.rem` format.

### `test_lisp_env_fallback_and_override`
**Requires:** nothing (no root, no rootfs, no container spawning)

Tests the `(env "VAR")` builtin and the standard Lisp fallback pattern used in
`compose.reml` for environment-driven configuration:

```lisp
(let ((p (env "VAR")))
  (if (null? p) default-value (string->number p)))
```

Asserts that with the env var absent the expression returns the default, and with
the var set it returns the parsed value. Tests the full round-trip through `env`,
`null?`, `if`, `string->number`, and the `let` binding.

Failure means operators cannot reliably use environment variables to configure their
`.reml` stacks without modifying the file itself.

### `test_lisp_eval_file_jupyter_fixture`
**Requires:** nothing (no root, no rootfs, no container spawning)

Evaluates the actual `examples/compose/jupyter/compose.reml` file through the full
Lisp interpreter pipeline and asserts the resulting `ComposeFile` matches the
expected structure:

- Exactly 1 network (`jupyter-net`, subnet `10.89.0.0/24`)
- Volume `jupyter-notebooks` declared
- 2 services: `redis` and `jupyterlab`
- `redis`: image `jupyter-redis:latest`, no deps, memory `64m`
- `jupyterlab`: image `jupyter-jupyterlab:latest`, depends-on `redis:6379`
  with `HealthCheck::Port(6379)`, port mapping `8888→8888`, env vars
  `REDIS_HOST=redis` and `REDIS_PORT=6379`
- `on-ready "redis"` hook registered (1 hook in HookMap)
- `JUPYTER_PORT` absent → `string->number` fallback path produces port 8888

Exercises the full end-to-end Lisp evaluation path: `define`, `let`, `env` with
fallback, `on-ready`, `service`, `network`, `volume`, `compose`, `compose-up`, and
the `depends-on` TCP health-check option.

Failure indicates a regression in the Lisp interpreter, the `depends-on` port
parsing, the `on-ready` hook registration, or the `env`/fallback pipeline — any
of which would make the Jupyter stack silently broken before containers are even
started.

### `test_defmacro_basic` (unit test in `src/lisp/mod.rs`)
**Requires:** nothing

Defines a simple `my-swap` macro via `defmacro` and calls it. Asserts that the
two arguments are exchanged in the output list. Verifies the core macro expansion
pipeline: unevaluated args → quasiquote template → `value_to_sexpr` → re-eval.

### `test_defmacro_generates_define` (unit test in `src/lisp/mod.rs`)
**Requires:** nothing

Defines a macro `def-42` that generates a `(define ...)` form. After calling it,
asserts that the named variable is bound in the environment. This is the minimal
proof that a macro can introduce new bindings — the key capability `define-service`
relies on.

### `test_define_service_macro` (unit test in `src/lisp/mod.rs`)
**Requires:** nothing

Calls `define-service` (the stdlib macro loaded at interpreter startup) with
`:image`, `:network`, and `:memory mem` where `mem` is a variable. Asserts that
the bound `ServiceSpec` has the correct name, image, network, and that the `mem`
variable was evaluated at call-site (not captured as a symbol).

Failure means the `define-service` macro itself is broken or `stdlib.lisp` fails
to load at startup, which would make every `.reml` file using `define-service` fail.

### `test_define_service_with_port_variable` (unit test in `src/lisp/mod.rs`)
**Requires:** nothing

Calls `define-service` with `(:port my-port 80)` where `my-port` is a variable
bound to `9090`. Asserts `ports[0].host == 9090` and `ports[0].container == 80`.

Verifies that multi-argument options with variables work correctly through the
macro expansion: the variable is not quoted in the expansion, so it evaluates to
its value when the generated `(list 'port my-port 80)` is executed.

### `test_lisp_eval_file_monitoring_fixture` (unit test in `src/lisp/mod.rs`)
**Requires:** nothing

Evaluates `examples/compose/monitoring/compose.reml` using `include_str!` and
inspects the resulting `ComposeSpec`. Asserts:

- 3 services in order: prometheus, loki, grafana
- Correct image tags for all three
- Single network `monitoring-net` with subnet `10.89.1.0/24`; all services attached
- 2 volumes: `prometheus-data`, `grafana-data`
- Grafana has exactly 2 `depends_on` entries: prometheus with `Port(9090)` and loki with `Port(3100)`
- Grafana env `GF_SECURITY_ADMIN_PASSWORD` equals `"admin"` (the default fallback)
- Port mappings: prometheus→9090, loki→3100, grafana→3000
- 2 `on-ready` hooks registered for "prometheus" and "loki"

Failure indicates a regression in: multiple `depends-on` per service, dotted-pair
`:env` with variable values, `env` built-in fallback, or `on-ready` hook registration.

### `test_lisp_eval_file_rust_builder_fixture` (unit test in `src/lisp/mod.rs`)
**Requires:** nothing

Evaluates `examples/compose/rust-builder/compose.reml` using `include_str!` and
inspects the resulting `ComposeSpec`. Asserts:

- 1 service: `rust-builder` with image `rust-builder:latest`
- 0 networks (single-service stack needs no inter-service communication)
- 2 compose-level volumes: `cargo-registry`, `sccache-cache`
- Service has 2 volume mounts: `cargo-registry → /root/.cargo/registry`, `sccache-cache → /sccache-cache`
- Service command is `["sleep", "infinity"]`
- Service env: `RUSTC_WRAPPER=sccache`, `SCCACHE_DIR=/sccache-cache`, `RUST_EDITION=2021`

Failure indicates a regression in: the new `:volume` Lisp service option,
`:command` multi-value option, dotted-pair `:env` with literal values, or
`env` built-in with null fallback.

### `test_hardening_combination` (integration test in `tests/integration_tests.rs`)
**Requires:** root, alpine-rootfs

Spawns a container using the same four-call hardening block that `compose up`
and the lisp runtime apply (`with_seccomp_default`, `drop_all_capabilities`,
`with_no_new_privileges(true)`, `with_masked_paths_default`), plus
`Namespace::PID | UTS | IPC | MOUNT`.  The container runs
`grep -E '^(Seccomp|CapEff|NoNewPrivs|NSpid):' /proc/self/status` and
`echo HOSTNAME=$(hostname)` via stdout capture.

Asserts:
- `Seccomp: 2` — Docker-default BPF filter is active
- `CapEff: 0000000000000000` — all capabilities dropped
- `NoNewPrivs: 1` — setuid escalation blocked
- NSpid last field = `1` — container is PID 1 in its own PID namespace
- `HOSTNAME=hardening-test` — UTS namespace is isolated

Failure means one of the four hardening primitives regressed at the raw API
level; every regression in this test will be masked from users unless this
ground-truth test exists.

### `test_lisp_container_spawn_hardening` (integration test in `tests/integration_tests.rs`)
**Requires:** root, alpine:latest in image store

Exercises `do_container_start_inner` (the lisp runtime path) via
`Interpreter::new_with_runtime`, starts a `sleep 30` container, then inspects
the spawned process from the host via `/proc/{inner_pid}/status`.

Steps:
1. Create interpreter with `new_with_runtime("test-iso", tmpdir)`
2. Eval `(container-start ...)` with `alpine:latest` and `sleep 30`
3. Extract intermediate PID from the returned `ContainerHandle`
4. Find the inner child (PID 1 in the namespace) via `/proc/{pid}/task/{pid}/children`
5. Read inner child's `/proc/{inner}/status` from the host
6. Compare UTS namespace symlinks (`/proc/{inner}/ns/uts` vs `/proc/self/ns/uts`)

Asserts same four properties as `test_hardening_combination`.  Skips if
`alpine:latest` is not in the image store.

Failure means the lisp `do_container_start_inner` path diverged from the
security defaults applied by compose, or that a future refactor of that
function accidentally removed the hardening block.

### `test_login_logout` (unit test in `src/cli/auth.rs`)
**Requires:** nothing (no root, no network, uses a tempdir for `HOME`)

Exercises `write_docker_config` and `remove_docker_config` (via `parse_docker_config`).

Steps:
1. Write a synthetic `~/.docker/config.json` with base64-encoded credentials
2. Parse with `parse_docker_config` and assert username/password match
3. Call `write_docker_config` to overwrite an entry
4. Call `remove_docker_config` and assert the entry is gone

Failure means the login/logout lifecycle is broken; registry auth would silently
fall back to anonymous even after `pelagos image login`.

### `registry_auth::test_local_registry_push_pull_roundtrip` (`#[ignore]`)
**Requires:** root, network (Docker Hub for `registry:2`), overlay support

Starts a `registry:2` OCI registry on a random ephemeral port with no
authentication, then exercises the push → pull round-trip over plain HTTP:

1. Pull `registry:2` from Docker Hub (if not already cached)
2. Start `registry:2` with `pelagos run --detach -p <port>:5000`
3. Pull `alpine` (source image) to ensure it is in the local store
4. Push `alpine` to `127.0.0.1:<port>/library/alpine:latest` with `--insecure`
5. Assert push output contains `"Pushed"`
6. Remove the local re-tagged reference so the subsequent pull is genuine
7. Pull from the local registry with `--insecure`; assert success
8. Assert the image appears in `pelagos image ls --format json`

Failure indicates that either `--insecure` HTTP negotiation, blob upload, or
manifest PUT is broken; any regression here would prevent push/pull from
working against local or air-gapped registries.

### `registry_auth::test_local_registry_auth_roundtrip` (`#[ignore]`)
**Requires:** root, network (Docker Hub for `registry:2`), overlay support

Starts a `registry:2` container with htpasswd authentication enforced using a
hard-coded bcrypt entry (docker/distribution ≥2.8 only accepts bcrypt; APR1/MD5
is no longer supported). Uses a temporary `HOME` directory
throughout to avoid touching the real `~/.docker/config.json`. Verifies four
properties end-to-end:

1. **Unauthenticated push fails** — `pelagos image push alpine --dest <registry>/<ref>
   --insecure` exits non-zero when the registry returns 401.
2. **`pelagos image login` writes credentials** — `--password-stdin` writes a
   base64-encoded entry into `$TMPHOME/.docker/config.json`; the command prints
   `"Login Succeeded"`.
3. **Authenticated push and pull succeed** — after login, push exits 0 and
   prints `"Pushed"`; after removing the local copy, pull exits 0 and
   downloads from the registry.
4. **`pelagos image logout` removes credentials** — subsequent pull exits
   non-zero (registry returns 401 again).

Failure at step 1 means the registry isn't actually enforcing auth (test
environment problem). Failure at steps 2–3 means credential resolution or
the `~/.docker/config.json` read/write path is broken. Failure at step 4
means `logout` didn't remove the entry and the credential cache is leaking.

### `image_save_load::test_image_save_load_roundtrip` (`#[ignore]`)
**Requires:** root, network (Docker Hub for `alpine`), overlay support

Full save/load roundtrip test:

1. **Pull** `docker.io/library/alpine:latest` from Docker Hub.
2. **Save** it to `/tmp/pelagos-test-alpine-save.tar` via `pelagos image save`.
   Verifies the output file exists and contains an `oci-layout` tar entry
   (i.e., it is a valid OCI Image Layout archive).
3. **Remove** the local image with `pelagos image rm`.
4. **Load** back from the tar via `pelagos image load -i <tar>`.
   Verifies the command prints `"Loaded"`.
5. **Verify** the image appears in `pelagos image ls`.
6. **Run** `/bin/true` inside the loaded image to confirm it is fully usable.

Failure at step 2 means `save` failed to find blobs (re-pull needed to
populate the blob cache, or a regression in blob store write paths).
Failure at step 4 means `load` failed to extract layers or write the manifest.
Failure at step 6 means the overlay mount for the loaded image is broken —
layers are present in the store but the image config or layer order is wrong.

### `image_tag::test_image_tag_roundtrip` (`#[ignore]`)
**Requires:** root, network (Docker Hub for `alpine`), overlay support

1. **Pull** `docker.io/library/alpine:latest`.
2. **Tag** it to `my-alpine:tagged` via `pelagos image tag`.
3. **Verify** both references appear in `pelagos image ls`.
4. **Run** `/bin/true` in the tagged image — confirms layers and config are
   shared correctly between source and target references.
5. **Remove** the source reference, then **run** the tagged image again —
   verifies that tag creates an independent manifest entry, not an alias.

Failure at step 2 means `tag` failed to copy the manifest or OCI config.
Failure at step 4 means the shared layer store is broken after tagging.
Failure at step 5 means `tag` stored a reference to the source rather than
creating its own manifest, so removing source broke the tag.

---

## Healthcheck Tests (`healthcheck_tests` module)

### `healthcheck_tests::test_parse_healthcheck_instruction_roundtrip`
**Type:** No-root, no-rootfs (parse-only)

Parses three Remfile snippets containing `HEALTHCHECK` instructions and checks
the resulting `Instruction::Healthcheck` fields:

1. **Shell form** — `HEALTHCHECK --interval=5s --retries=2 CMD /bin/check.sh`
   → `cmd == ["/bin/sh", "-c", "/bin/check.sh"]`, `interval_secs == 5`, `retries == 2`.
2. **JSON form** — `HEALTHCHECK CMD ["pg_isready", "-U", "postgres"]`
   → `cmd == ["pg_isready", "-U", "postgres"]`.
3. **NONE form** — `HEALTHCHECK NONE`
   → `cmd` is empty (healthcheck disabled).

Failure indicates the `HEALTHCHECK` Remfile parser (`parse_healthcheck` /
`parse_duration_str` in `src/build.rs`) is broken.

### `healthcheck_tests::test_health_config_oci_json_roundtrip`
**Type:** No-root, no-rootfs (serde-only)

Creates a `HealthConfig` with non-default values, serializes it to JSON, and
deserializes back, asserting all fields survive the round-trip. Also implicitly
verifies that the default-function annotations for `interval_secs`, `timeout_secs`,
and `retries` are correct (they are only invoked when the field is absent from JSON).

Failure indicates a serde regression in `HealthConfig` — either a missing
`#[serde(default = ...)]` annotation or a broken field name.

### `healthcheck_tests::test_healthcheck_exec_true` (`#[ignore]`)
**Requires:** root + rootfs

Starts a detached container running `sleep 30` via the `pelagos` CLI, then:

1. Runs `pelagos exec <name> /bin/true` and asserts exit status 0.
2. Runs `pelagos exec <name> /bin/false` and asserts non-zero exit status.

Failure at step 1 means `pelagos exec` can't join the container's namespaces or
`/bin/true` is missing from the rootfs. Failure at step 2 means the exit code
is not being propagated correctly from the exec'd process.

### `healthcheck_tests::test_healthcheck_healthy` (`#[ignore]`)
**Requires:** root + rootfs

Starts a detached container, then patches `state.json` to inject a
`health_config` with `cmd = ["/bin/true"]` and `health = "starting"`. Verifies
that the patched JSON parses correctly (both fields present with expected types).
Then writes `health = "healthy"` and re-reads to confirm the state file correctly
stores and returns the `healthy` variant.

This test primarily validates that the `health` and `health_config` fields in
`state.json` are correctly serialized/deserialized. Failure indicates a serde
regression in `ContainerState.health` or `ContainerState.health_config`.

### `healthcheck_tests::test_healthcheck_unhealthy` (`#[ignore]`)
**Requires:** root + rootfs

Starts a detached container, writes `health = "unhealthy"` to `state.json`, and
re-reads to confirm the `unhealthy` variant round-trips correctly through the
state file.

Failure indicates the `HealthStatus::Unhealthy` serde variant is broken
(wrong serialized string or missing enum arm).


---

## Console-socket tests (`console_socket_tests`)

### `console_socket_tests::test_oci_console_socket`
**Requires:** root + rootfs

Creates an OCI bundle with `process.terminal: true` and provides a Unix socket
path via `--console-socket`. The test binds a `UnixListener` on that path before
running `pelagos create`, then accepts one connection and calls `recvmsg` to
receive the fd sent via `SCM_RIGHTS` ancillary data.

Asserts:
1. `pelagos create` exits 0.
2. A connection is accepted within 5 seconds (the runtime connected and sent the fd).
3. The received fd is `>= 0` (a valid file descriptor was transmitted).
4. `isatty(received_fd) == 1` — the fd is a TTY, confirming it is the PTY master.

Failure modes:
- If the runtime ignores `--console-socket`, no connection arrives → poll timeout.
- If no fd is sent via `SCM_RIGHTS`, `received_fd == -1`.
- If the wrong fd is sent (not a PTY), `isatty` returns 0.

---

## Wasm tests (`wasm_tests`)

### `wasm_tests::test_wasm_binary_detection_magic`
**Type:** API-only (no root, no rootfs, no runtime)

Writes a file whose first 4 bytes are the WebAssembly magic (`\0asm`) and
asserts `is_wasm_binary()` returns `true`.

Failure indicates the magic constant or byte-read offset is wrong.

### `wasm_tests::test_wasm_binary_detection_rejects_elf`
**Type:** API-only

Writes a file starting with ELF magic (`\x7fELF`) and asserts
`is_wasm_binary()` returns `false`.

Failure indicates a false positive that would misroute native binaries to the
Wasm runtime.

### `wasm_tests::test_extract_wasm_layer_stores_module`
**Type:** Requires root or pelagos-group write access to layer store

Writes a synthetic Wasm blob to a temp file, calls `extract_wasm_layer()`, and
asserts that `<layer_dir>/module.wasm` exists with identical content.

Skips if the caller cannot write to `/var/lib/pelagos/layers/`. Failure
indicates the Wasm layer extractor is not creating the output file or the atomic
rename is broken.

### `wasm_tests::test_is_wasm_image_detects_wasm_manifest`
**Type:** API-only

Constructs an `ImageManifest` with a Wasm OCI mediaType in `layer_types` and
asserts `is_wasm_image()` returns `true`.

Failure indicates the mediaType check in `ImageManifest` is not reading
`layer_types` correctly.

### `wasm_tests::test_is_wasm_image_false_for_linux_image`
**Type:** API-only

Constructs an `ImageManifest` with a standard tar+gzip mediaType and asserts
`is_wasm_image()` returns `false`.

Failure indicates a false positive that would misroute Linux containers to the
Wasm runtime.

### `wasm_tests::test_is_wasm_image_backwards_compat_empty_layer_types`
**Type:** API-only

Constructs an `ImageManifest` with an empty `layer_types` vec (simulating an
image recorded before Wasm support was added) and asserts `is_wasm_image()`
returns `false` without panicking.

Failure indicates backward-compatibility with old manifest.json files is broken.

### `wasm_tests::test_old_manifest_json_deserialises_without_layer_types`
**Type:** API-only

Deserialises a JSON manifest that lacks the `layer_types` field (as written by
older pelagos versions) and asserts it deserialises successfully with
`layer_types` defaulting to an empty vec.

Failure indicates `#[serde(default)]` is missing on `layer_types`, which would
crash on startup when loading cached images.

### `wasm_tests::test_wasm_spawn_via_command_builder`
**Type:** Skipped if no Wasm runtime installed

Writes a minimal valid Wasm module (magic + version header only, no sections) to
a temp file, spawns it via `Command::new(path).with_wasm_runtime(Auto).spawn()`,
and waits for it to exit.

Verifies that the Wasm fast-path in `spawn()` runs end-to-end without panicking.
No assertion on exit code — an empty module may trap in the runtime, which is
acceptable.

---

## E2E Wasm Tests (`scripts/test-wasm-e2e.sh`)

Shell-level end-to-end tests that drive the `pelagos` CLI with a real Wasm
module. Require root and `wasmtime` in PATH; skip automatically if either
is absent.

Run with:
```
sudo -E env PATH="$HOME/.wasmtime/bin:$PATH" scripts/test-wasm-e2e.sh
```

### `image ls — TYPE column`
**Type:** E2E, requires root + wasmtime

Seeds a synthetic Wasm image in the pelagos image store (manifest.json with
`layer_types: ["application/wasm"]`) and runs `pelagos image ls`. Asserts the
output contains the string `wasm` in the TYPE column and the image reference.

Failure indicates `cmd_image_ls()` no longer renders the TYPE column, or
`is_wasm_image()` detection is broken.

### `run — basic output`
**Type:** E2E, requires root + wasmtime

Compiles a trivial Rust program to `wasm32-wasip1` and runs it via `pelagos run
<image-ref>`. Asserts stdout contains `hello wasm` and no `error` strings.

Failure indicates the Wasm fast-path in `build_image_run()` or the
`spawn_wasm()` dispatch is broken.

### `run — env passthrough`
**Type:** E2E, requires root + wasmtime

Runs the same Wasm module with `--env WASM_TEST_VAR=testvalue42`. Asserts the
module prints `env:WASM_TEST_VAR=testvalue42`.

Failure indicates `with_wasi_env()` is not forwarding `--env` values to the
wasmtime `--env` flag, or that `WasiConfig.env` is not being populated in
`build_image_run()`.

### `run — preopened dir (--bind)`
**Type:** E2E, requires root + wasmtime

Creates a host directory containing `test.txt`, runs the Wasm module with
`--bind <host>:/data`, and asserts the module can read `/data/test.txt` and
prints `file:bind mount works`.

Failure indicates `with_wasi_preopened_dir_mapped()` is not propagating
the host→guest mapping to the wasmtime `--dir host::guest` flag.

### `Wasm magic-byte detection`
**Type:** Structural check, no runtime required

Reads the first 4 bytes of the compiled `hello.wasm` and verifies they equal
`00 61 73 6d` (`\0asm`). Confirms that `rustc --target wasm32-wasip1` produces
a valid Wasm binary that `is_wasm_binary()` would recognise.

### `wasm::tests::test_wasmtime_cmd_identity_dir_mapping`
**Type:** Unit, no runtime required

Constructs a `WasiConfig` with a single identity-mapped preopened dir
(`/data` → `/data`) and asserts `build_wasmtime_cmd` produces `--dir /data::/data`.

### `wasm::tests::test_wasmtime_cmd_mapped_dir`
**Type:** Unit, no runtime required — regression test

Constructs a `WasiConfig` with host `/host/binddata` mapped to guest `/data`
and asserts `build_wasmtime_cmd` produces `--dir /host/binddata::/data`, not
the identity form `--dir /host/binddata::/host/binddata`.

This is the direct regression guard for the bug where `--bind /host:/container`
was silently ignored: the module received the host path as both host and guest,
so any file opens at the container path failed.

### `wasm::tests::test_wasmedge_cmd_mapped_dir`
**Type:** Unit, no runtime required — regression test

Same mapping check for WasmEdge (single-colon syntax: `--dir host:guest`).
Asserts `build_wasmedge_cmd` produces `--dir /host/binddata:/data` and not
the identity form.

### `wasm::tests::test_wasmtime_cmd_env_vars`
**Type:** Unit, no runtime required

Constructs a `WasiConfig` with two env vars and asserts both appear as
`--env KEY=val` in the wasmtime command args.

## E2E Embedded Wasm Tests (`scripts/test-wasm-embedded-e2e.sh`)

Shell-level end-to-end tests that drive the `pelagos` CLI with a real embedded
Wasm module (no external wasmtime/wasmedge in PATH). Require root and the
`wasm32-wasip1` Rust target; skip automatically if either is absent.

Run with:
```
sudo -E scripts/test-wasm-embedded-e2e.sh
```

### `1. run — basic output (embedded path)`
**Type:** E2E, requires root + `--features embedded-wasm` + `wasm32-wasip1`

Builds a Wasm image from `scripts/wasm-embedded-context/hello.rs`, strips
wasmtime/wasmedge from PATH, and runs `pelagos run <image>`.  Asserts output
contains `hello embedded wasm`.  Failure means the embedded execution path isn't
activating or the module is silently erroring.

### `2. run — env passthrough`
**Type:** E2E, requires root + embedded-wasm

Runs the same image with `--env EMBED_VAR=hello42`.  Asserts `env:EMBED_VAR=hello42`
appears in output.  Failure means `WasiConfig.env` is not wired into the
embedded `WasiCtxBuilder`.

### `3. run — preopened directory (--bind)`
**Type:** E2E, requires root + embedded-wasm

Creates a host directory with `test.txt` and runs with `--bind <host>:/data`.
Asserts `file:embed test` in output.  Failure means `WasiCtxBuilder`
preopened-dir setup is broken for the embedded path.

### `4–6. Component Model (wasm32-wasip2) — basic output, env, bind`
**Type:** E2E, requires root + embedded-wasm + `wasm32-wasip2` (skipped if absent)

Same three checks for a P2 Component Model binary compiled to `wasm32-wasip2`.
Verifies the `run_embedded_component` P2 path works end-to-end.

### `7. run --detach — stdout captured via pelagos logs (issue #153)`
**Type:** E2E, requires root + embedded-wasm

Runs the Wasm image with `--detach`, polls until the container exits, then
reads output via `pelagos logs`.  Asserts `hello embedded wasm` is present.

This is the direct regression guard for issue #153: in `--detach` mode the
watcher process inherits fd 1/2 pointing at `/dev/null`, so without the fix
all Wasm module output is silently discarded.  The fix pipes wasmtime stdout
through an OS pipe to the container log file; this test confirms it works
end-to-end through the CLI.

---

### `wasm_embedded_tests::test_wasm_embedded_exit_code`
**Type:** Unit, requires `--features embedded-wasm`
**Root:** no  **Rootfs:** no

Compiles a minimal WAT module (via `wasmtime::Module::new`) that calls
`wasi_snapshot_preview1::proc_exit(7)` and runs it in-process through
`run_embedded_module`. Asserts the function returns exit code 7.

Failure indicates the embedded wasmtime execution path is broken: either
`run_embedded_module` panics, the WASI P1 linker is not set up correctly,
or `I32Exit` is not being detected in the anyhow error chain (which would
mean every `proc_exit` call is treated as an execution error).

This test confirms the in-process path works without `wasmtime` or `wasmedge`
in PATH, and that the exit code propagation round-trip is correct.

### `wasm_embedded_tests::test_wasm_component_detection_from_bytes`
**Type:** Unit, requires `--features embedded-wasm`
**Root:** no  **Rootfs:** no

Writes synthetic 8-byte Wasm headers to two temp files — one with the plain
module version tag (`01 00 00 00`) and one with the component version tag
(`0d 00 01 00`) — and asserts `is_wasm_component_binary` returns `false` and
`true` respectively.

Failure indicates the component-vs-module byte detection is broken: either the
version-byte comparison is inverted, or the function is erroring rather than
returning `Ok(bool)` for valid inputs.  This is the gating check that routes
execution to the P1 (module) or P2 (component) embedded path.

### `wasm_embedded_tests::test_wasm_embedded_component_exit_code`
**Type:** Unit, requires `--features embedded-wasm`, `wasm32-wasip2` Rust target
**Root:** no  **Rootfs:** no

Compiles a trivial Rust `fn main() { println!("component ok"); }` to a
`wasm32-wasip2` Wasm Component using `rustc` at test time, then loads and
executes it in-process via `run_embedded_component`.  Asserts the exit code is
0.  Skips gracefully if `rustc` is not found or `wasm32-wasip2` is not
installed.

Failure indicates the P2 / Component Model execution path is broken: the
`wasmtime_wasi::p2` linker setup, `Command::instantiate`, or `call_run` is not
functioning correctly.  This test verifies the full component execution round-trip
(component detection → P2 linker → WASI Command world → exit code 0).

### `wasm_embedded_tests::test_wasm_embedded_piped_stdout`
**Type:** Unit, requires `--features embedded-wasm`
**Root:** no  **Rootfs:** no

Creates an OS pipe with `libc::pipe2`, passes the write-end as the `stdout`
`Option<std::fs::File>` to `run_embedded_module`, and reads the read-end after
the module exits.  The WAT module writes `"hello wasm\n"` to WASI fd 1 via
`fd_write`, so the captured bytes must match exactly.

Failure indicates the `OutputFile` redirection path in `run_embedded_module` is
broken: embedded Wasm stdout is inherited from fd 1 instead of being routed to
the supplied file.  In `--detach` mode fd 1 is `/dev/null`, so all Wasm output
would be silently discarded.  This is the regression guard for issue #153.

---

## Build Regression Tests (`build_regression_tests`)

These tests guard against specific bugs that were found and fixed. Each test is
named after the failure mode it prevents.

### `build_regression_tests::test_build_copy_then_chmod_layer_content_preserved`
**Type:** Integration — requires root, alpine pre-pulled
**Root:** yes  **Rootfs:** no (uses pulled alpine image layers)

Regression test for the **overlayfs metacopy bug** (Linux 6.x+). When
`metacopy=on` (the kernel default on modern kernels), a `chmod` in a RUN step
only writes a *metadata inode* to the overlay upper directory — file data stays
in the lower layer. The build engine reads `upper/` directly after container
exit (the overlay mount is gone), so it gets zero bytes for any file that was
only `chmod`'d, not written.

Builds a minimal image (`FROM alpine` + `COPY script.sh` + `RUN chmod +x`),
then reads the file bytes from every layer directory in the layer store and
asserts the file is non-empty, non-zero, and starts with `#!`. This catches the
regression at the layer-storage level without needing to run the resulting image.

Failure indicates: `metacopy=off` is missing from a kernel overlay mount option
in `container.rs`, or the build engine is reading the wrong directory.

### `build_regression_tests::test_build_copy_chmod_run_produces_output`
**Type:** Integration — requires root, alpine pre-pulled
**Root:** yes  **Rootfs:** no (uses pulled alpine image layers)

Full build-then-run regression test for the **overlayfs metacopy bug**. Builds
the same `COPY script.sh + RUN chmod +x` image as above, then runs it via
`Command::new("/bin/sh").args(["/usr/local/bin/script.sh"])` with
`with_image_layers()` and asserts the expected output string appears.

If `metacopy=off` is missing, the script will contain zeros, causing an exec
format error or silent empty output instead of the expected string.

Failure indicates: the file written by a COPY instruction loses its content
after a subsequent `RUN chmod` step — the container returns no output or an
exec error instead of the expected string.

---

## healthcheck_tests

### `test_healthcheck_exec_true`
**Requires:** root, rootfs

Starts a detached container (`sleep 30`) via `pelagos run -d`, polls state.json
until the watcher child has written a non-zero PID (up to 10s), then runs
`pelagos exec <name> /bin/true` and asserts exit 0, and `pelagos exec <name>
/bin/false` and asserts non-zero exit.

Exercises the full `pelagos exec` namespace-join path against a live container.
Failure indicates exec is broken, the container binary paths are missing in
Alpine, or the watcher is not writing state.json correctly.

### `test_healthcheck_healthy`
**Requires:** root, rootfs

Starts a detached container, waits for state.json to appear, patches it to
inject a `health_config` JSON object and sets `health = "starting"`, then
manually writes `health = "healthy"` and asserts the round-trip through
`serde_json` is correct.

This test validates the state.json JSON schema for health fields (`health`,
`health_config`), not the live health-monitor execution. Failure indicates
a serde serialization regression in the health-related state.json fields.

### `test_healthcheck_unhealthy`
**Requires:** root, rootfs

Same as `test_healthcheck_healthy` but writes `health = "unhealthy"`. Asserts
the value round-trips correctly through state.json. Failure indicates a serde
regression in the unhealthy health state field.

### `test_rootless_bridge_error`
**Requires:** root (to invoke `sudo -u nobody`)

Runs `pelagos run --network bridge alpine echo hi` as UID 65534 (nobody) via
`sudo -u #65534`. Asserts that the process exits non-zero and that stderr contains
"requires root".

This validates the rootless-first guard in `src/cli/run.rs`: when a non-root user
requests bridge networking, NAT, or port publishing, Pelagos should print a friendly
error and exit immediately rather than failing deep in the network setup with a
cryptic kernel error. Failure indicates the guard was removed or is not reached.

---

## tutorial_e2e_p1 — Basic container lifecycle

### `test_tut_p1_echo`
**Requires:** rootless (group access to image store)

Runs `pelagos run alpine /bin/echo "hello from a container"` and asserts stdout
contains the expected string. The simplest possible CLI smoke test: confirms image
pull (if needed), rootless overlay setup, and basic exec all work end-to-end.

Failure indicates something is fundamentally broken with image unpacking, rootless
overlay, or process exec.

### `test_tut_p1_hostname_whoami`
**Requires:** rootless

Runs `/bin/sh -c "hostname && whoami && cat /etc/os-release"` inside an Alpine
container and asserts: hostname is non-empty, "root" appears in output (whoami),
and "Alpine" appears in output (/etc/os-release).

Failure indicates namespace setup, UTS isolation, or Alpine image layer extraction
is broken.

### `test_tut_p1_ps_logs_stop`
**Requires:** root

Starts `sleep 30` with `--detach --name tut-p1-ps`, polls `pelagos ps` until the
container appears (up to 10s), calls `pelagos logs` (asserts exit 0), calls
`pelagos stop`, then cleans up. Uses `#[serial]` to avoid concurrent name clashes.

Failure indicates detach, watcher, ps listing, log retrieval, or stop/rm are broken.

### `test_tut_p1_exec_noninteractive`
**Requires:** root

Starts `sleep 60` detached, polls until running, then calls `pelagos exec <name>
/bin/cat /etc/hostname` and asserts the output is non-empty.

Failure indicates exec namespace-join, the watcher state file, or Alpine's
/etc/hostname is broken.

### `test_rootless_exec_noninteractive`
**Requires:** rootless (no root)

Starts `sleep 60` detached (no bridge/NAT — pure rootless), polls until running,
then calls `pelagos exec <name> /bin/cat /etc/alpine-release` and asserts exit 0
and non-empty output.

Exercises the rootless namespace-join ordering fix (USER first, then MOUNT, then
UTS/IPC/NET) and the pid==0 race window fix in `cmd_exec` (polling until the watcher
writes the real container PID before proceeding). Failure indicates a regression in
either fix.

### `test_rootless_exec_sees_container_filesystem`
**Requires:** rootless (no root)

Starts a detached container that writes `EXEC_MARKER_ROOTLESS` to `/tmp/exec-marker`,
then runs `pelagos exec <name> /bin/cat /tmp/exec-marker` and asserts the output
matches exactly. Failure indicates MOUNT namespace join is broken in rootless exec
(exec'd process sees host /tmp instead of container's /tmp).

### `test_rootless_exec_environment`
**Requires:** rootless (no root)

Starts a container with `--env MY_EXEC_VAR=hello_rootless`, then:
1. Asserts `pelagos exec` inherits `MY_EXEC_VAR=hello_rootless` from the container's
   `/proc/{grandchild_pid}/environ` (exercises the grandchild-environ fix — state.pid
   is the intermediate that never exec'd, so we must read environ from its child).
2. Asserts `pelagos exec --env MY_EXEC_VAR=overridden` overrides the inherited value.

Failure indicates environ inheritance is reading from the wrong PID (intermediate
instead of actual container), or -e override is broken.

### `test_rootless_exec_nonrunning_fails`
**Requires:** rootless (no root)

Starts a detached container, stops it with `pelagos stop`, then attempts
`pelagos exec` and asserts exit non-zero with "not running" on stderr. Exercises the
pid==0 race fix in `cmd_stop` (stop must wait for the real PID before sending SIGTERM,
otherwise it races with the watcher overwriting state as Running+real_pid). Failure
indicates the stop→exec race is back.

### `test_rootless_exec_user_workdir`
**Requires:** rootless (no root); also requires `user_allow_other` in `/etc/fuse.conf`

Starts a detached container, then runs four exec sub-cases:
- `--user 1000`: asserts `id -u` prints `1000`. Verifies fuse-overlayfs `allow_other`
  is set — without it, the exec'd process (host UID 100999) cannot read the FUSE mount
  and exec fails with EACCES on `execve`.
- `--workdir /tmp`: asserts `pwd` prints `/tmp`. Verifies chdir in the user_pre_exec
  callback works.
- `--user 1000:1000`: asserts `id -u:id -g` is `1000:1000`. Verifies GID application
  via `with_gid()`.
- `--user 1000` write: writes a file to `/tmp` and reads it back. Exercises a distinct
  failure mode from exec: without `allow_other`, fuse-overlayfs returns EACCES even for
  writes on world-writable tmpfs paths when the caller's host UID doesn't match the
  FUSE mount owner.

Failure indicates either: (a) fuse-overlayfs was not mounted with `allow_other` (setup.sh
not run, or `user_allow_other` not in `/etc/fuse.conf`); or (b) `--user`/`--workdir`
flag parsing in exec.rs is broken.

### `test_tut_p1_auto_rm`
**Requires:** rootless

Runs `pelagos run --rm --name tut-p1-rm alpine /bin/echo "vanish"`, asserts exit 0
and "vanish" in stdout, then asserts that `/run/pelagos/containers/tut-p1-rm/` does
not exist after exit.

Failure indicates the `--rm` auto-cleanup path in the watcher is not removing the
container state directory on exit.

---

## tutorial_e2e_p2 — Image build

### `test_tut_p2_simple_build`
**Requires:** rootless (group access to layer store)

Builds the image from `scripts/tutorial-e2e/p2-simple/` (FROM alpine, RUN apk add,
COPY server.sh, RUN chmod, CMD). Runs the resulting image and asserts "Hello from
pelagos!" appears in stdout. Cleans up the image tag after the test.

Failure indicates the build engine (COPY, RUN chmod, CMD exec) or image run is broken.

### `test_tut_p2_image_save_load`
**Requires:** rootless

Builds `tut-p2-simple:latest`, saves it with `pelagos image save -o <tmpfile>`,
removes the local copy, then loads it back with `pelagos image load -i <tmpfile>`,
runs it, and asserts "Hello from pelagos!" in stdout.

Failure indicates the OCI archive save/load round-trip is broken — either the tar
format is wrong or the image store is not updated correctly on load.

### `test_tut_p2_multistage_go_build`
**Requires:** rootless, network access (Go module proxy) — `#[ignore]` (slow)

Builds `scripts/tutorial-e2e/p2-go/` (two-stage: `golang:1.22-alpine` builder
→ Alpine final). Runs the image and asserts "Hello from Go!" in stdout.

Failure indicates multi-stage build (`COPY --from=builder`), Go compilation inside
a container, or static binary execution in the final Alpine stage is broken.

---

## tutorial_e2e_p3 — Isolation

### `test_tut_p3_read_only`
**Requires:** root

Runs with `--read-only` and attempts `echo test > /readonly.txt`. Asserts non-zero
exit (write rejected by read-only rootfs).

Failure indicates `--read-only` is not applied or the overlayfs upper layer is still
writable.

### `test_tut_p3_memory_oom`
**Requires:** root

Runs with `--memory 64m --tmpfs /tmp` and attempts to allocate 200 MB via `dd`.
Asserts the process exits non-zero OR stdout does not contain "done".

Failure indicates the cgroup v2 memory limit is not enforced.

### `test_tut_p3_cap_drop`
**Requires:** root

Runs with `--network loopback --cap-drop ALL` and attempts `ip link set lo mtu 1280`.
Asserts the output contains "denied", "Operation not permitted", or "RTNETLINK".

Failure indicates capability dropping (`--cap-drop ALL`) is not applied correctly.

### `test_tut_p3_seccomp`
**Requires:** root

Runs with `--security-opt seccomp=default` and attempts `unshare --user echo hi`.
Asserts the output contains a permission error or "blocked by seccomp".

Failure indicates the default seccomp profile is not applied or `unshare` syscall
is missing from the blocked list.

### `test_tut_p3_network_loopback`
**Requires:** rootless

Runs with `--network loopback` and attempts `ping -c1 8.8.8.8`. Asserts the ping
fails (no external internet access in loopback mode).

Failure indicates the loopback network mode provides unintended external connectivity.

### `test_tut_p3_network_bridge_nat_port`
**Requires:** root

Starts a detached container with `--network bridge --nat --publish 18080:80` running
an nc loop that serves a static HTTP response. Polls `curl http://localhost:18080`
until it returns "Hello from pelagos" (up to ~5s). Uses `#[serial]`.

Failure indicates bridge creation, NAT (nftables MASQUERADE), or TCP DNAT port
forwarding is broken.

---

## tutorial_e2e_p4 — Compose

### `test_tut_p4_compose_lifecycle`
**Requires:** root

Runs `compose up -f stack.reml -p tut-p4-lifecycle`, polls `pelagos ps` until both
`tut-p4-lifecycle-db` and `tut-p4-lifecycle-app` appear, asserts both appear in
`compose ps`, then runs `compose down` and asserts both are gone from `ps`.
Uses `#[serial]`.

Failure indicates compose up/down, scoped container naming, or the supervisor
lifecycle is broken.

### `test_tut_p4_compose_depends_on`
**Requires:** root

Runs the same two-service stack with `depends-on (db :ready-port 6379)`. Asserts
both services are running after `compose up` completes. Uses `#[serial]`.

Failure indicates the TCP readiness polling or topological ordering (Kahn's
algorithm) in the compose supervisor is broken.

### `test_tut_p4_compose_dns`
**Requires:** root

Starts the two-service stack, then runs `pelagos exec <app-container> /bin/sh -c
"nslookup db 2>&1 || getent hosts db 2>&1 || echo DNS_FAIL"`. Asserts the output
does not contain "DNS_FAIL" and contains an IP address.

Failure indicates the DNS daemon (pelagos-dns) is not registering compose service
names, the container's /etc/resolv.conf is misconfigured, or the DNS TCP readiness
wait in depends-on is exposing a race with DNS registration.

---

## Compose cap-add Tests

### `test_compose_cap_add_chown`
**Requires:** root, rootfs

Mirrors the compose `spawn_service` hardening block (`drop_all_capabilities` +
`with_seccomp_default` + `with_no_new_privileges` + `with_masked_paths_default`)
and then restores `CAP_CHOWN` via `with_capabilities(Capability::CHOWN)`.  Runs
`chown nobody /tmp` inside the container and asserts exit 0 and `OK` on stdout.

Failure indicates that the compose `cap-add` wiring — calling `with_capabilities`
after `drop_all_capabilities` in `spawn_service` — is broken or that `CAP_CHOWN`
is not correctly parsed from the capability name string.

### `test_compose_cap_add_chown_denied_without_cap`
**Requires:** root, rootfs

Same hardening block as `test_compose_cap_add_chown` but without any
`with_capabilities` call — `CAP_CHOWN` remains dropped.  Runs
`chown nobody /tmp && echo OK || echo EPERM` and asserts the output contains
`EPERM` and does not contain `OK`.

Failure indicates that `drop_all_capabilities` is not actually dropping
`CAP_CHOWN`, meaning the security boundary that `cap-add` is supposed to opt
into is not enforced.

### `test_default_caps_hex_value`
**Requires:** root, rootfs

Runs a container with exactly `Capability::DEFAULT_CAPS` and reads `CapEff`
from `/proc/self/status`.  Asserts the value is `00000000800405fb` — the 11-cap
set (CHOWN, DAC_OVERRIDE, FOWNER, FSETID, KILL, SETGID, SETUID, SETPCAP,
NET_BIND_SERVICE, SYS_CHROOT, SETFCAP).

Failure means the `DEFAULT_CAPS` constant was modified without updating this
test — any bit added or removed changes the hex value.

### `test_default_caps_allows_chown_denies_mknod`
**Requires:** root, rootfs

Runs a container with `DEFAULT_CAPS` and executes both `chown nobody /tmp` and
`mknod /tmp/testdev c 1 1`.  Asserts `CHOWN=OK` and `MKNOD=FAIL`.

Failure indicates either: CHOWN was removed from `DEFAULT_CAPS` (postgres-style
images would break), or MKNOD was accidentally added (device-node creation
attack surface opened).

### `test_cap_drop_all_zeros_caps`
**Requires:** root, rootfs

Runs a container with `drop_all_capabilities()` (the `(cap-drop "ALL")` path)
and asserts `chown` fails — even CHOWN, which is in `DEFAULT_CAPS`, must be
absent after explicit drop-all.

Failure indicates `drop_all_capabilities()` is not zeroing the effective cap
set, so `(cap-drop "ALL")` would silently leave capabilities in place.

### `test_cap_drop_individual_removes_only_that_cap`
**Requires:** root, rootfs

Runs a container with `DEFAULT_CAPS & !Capability::CHOWN`.  Asserts `chown`
fails (`CHOWN=FAIL`) but the process completes normally (`ALIVE`), proving that
a single-cap drop removes only that capability without becoming drop-all.

Failure in either direction: if `CHOWN=OK`, the cap-drop didn't apply; if
`ALIVE` is missing, the implementation accidentally dropped all caps.

---

## `auto_resolv_conf` module

### `test_auto_resolv_conf_loopback`
**Requires:** root, alpine-rootfs

Spawns a container with `Namespace::MOUNT` + chroot but **no** `with_dns()` call
and reads `/etc/resolv.conf` inside the container.  Asserts the output contains
at least one `nameserver` line.

The auto-inject path reads the host's nameservers via `host_upstream_dns()` (which
filters loopback stub addresses like `127.0.0.53`) and writes them to a
per-container temp file bind-mounted inside the container's private MOUNT namespace.
The host file is never shared directly — container writes go to the temp copy only.

Failure indicates the auto-injection is not populating `auto_dns`, so glibc
containers (Ubuntu, Debian) would have no DNS resolution out of the box.

### `test_explicit_dns_skips_auto_resolv`
**Requires:** root, alpine-rootfs, `Namespace::MOUNT`

Spawns a container with `with_dns(&["1.2.3.4"])` and reads `/etc/resolv.conf`.
Asserts the content contains `1.2.3.4` (the explicitly configured server).

The auto-inject condition requires `auto_dns.is_empty() && dns_servers.is_empty()`,
so an explicit `with_dns()` call bypasses it entirely.

Failure indicates either: the explicit DNS path is broken, or the auto-inject
is running in addition to and overwriting the explicit configuration.

### `test_no_mount_ns_no_auto_resolv`
**Requires:** root, alpine-rootfs

Spawns a container **without** `Namespace::MOUNT`.  Asserts the container exits 0.
The auto-inject condition requires `Namespace::MOUNT` — without it no DNS injection
is attempted, and the container shares the host's mount namespace where
`/etc/resolv.conf` is already visible.

Failure indicates the auto-inject is running unconditionally, which would attempt
to create a DNS temp file and bind-mount in a shared mount namespace, potentially
corrupting the host's view of `/etc/resolv.conf`.

---

## Container Restart (`pelagos start`)

### `test_container_restart_after_exit`
**Requires:** root, alpine-rootfs

Runs `/bin/true` in detached mode, waits for the container to reach `"exited"` status,
verifies `spawn_config` was written to `state.json`, then calls `pelagos start` and
waits for the restarted container to also exit.

Failure indicates: `SpawnConfig` was not persisted on first run, `pelagos start` returns
a non-zero exit code, or the restarted container fails to launch/exit cleanly.

### `test_container_restart_runs_same_command`
**Requires:** root, alpine-rootfs

Runs `/bin/sh -c "echo run1 > /shared/marker.txt"` with a bind-mounted host directory.
After the container exits, removes the marker file, calls `pelagos start`, and asserts
the marker file is re-created by the restarted container.

Failure indicates: `SpawnConfig` did not preserve the bind mount or the command arguments,
so the restarted container ran a different command or without the correct mount.

### `test_container_start_running_fails`
**Requires:** root, alpine-rootfs

Starts a long-lived container (`/bin/sleep 30`), waits until its PID is recorded, then
asserts that `pelagos start` returns a non-zero exit code.

Failure indicates the "already running" guard in `cmd_start` is broken and `pelagos start`
incorrectly accepts or restarts a live container.

### `test_container_restart_preserves_tmpfs`
**Requires:** root, alpine-rootfs

Runs a container with `--tmpfs /tmp` in detached mode, lets it exit, then checks that
`state.json` contains `spawn_config.tmpfs = ["/tmp"]`.  Restarts the container via
`pelagos start` and waits for it to exit again.

Failure indicates: `SpawnConfig.tmpfs` was not saved by `build_spawn_config` (field was
missing from the struct or not populated), or was not passed through by
`spawn_config_to_run_args`, causing the restarted container to fail to mount tmpfs.

### `test_container_start_multiple_names`
**Requires:** root, alpine-rootfs

Runs two containers (`/bin/true`) in detached mode, waits for both to exit, then calls
`pelagos start name1 name2` and verifies both containers reach `"exited"` status again.

Failure indicates the multi-name dispatch in `main.rs` (the `Start { id: Vec<String> }` arm)
or the loop in `cmd_start` is broken — only one container would restart, both would fail,
or `pelagos start` would reject the second argument.

## Native Container Labels (`pelagos run --label`)

### `test_run_with_labels_appear_in_inspect`
**Requires:** root, alpine-rootfs

Starts a detached container with `--label env=staging --label managed=true`, waits until
the container PID is recorded, then runs `pelagos container inspect` and asserts the
JSON output contains `labels.env == "staging"` and `labels.managed == "true"`.

Failure indicates: labels are not being parsed from CLI args, not written to `state.json`,
or `container inspect` does not include them in its JSON output.

### `test_ps_filter_label`
**Requires:** root, alpine-rootfs

Starts two containers with different `tier=web` and `tier=db` labels, then runs
`pelagos ps --format json --filter label=tier=web` and asserts exactly one container
is returned and it is the one with `tier=web`.

Failure indicates the label filter in `cmd_ps` / `apply_filters` is broken — either
the wrong containers are returned or the JSON output is malformed.

## pivot_root auto-mount-namespace (`with_chroot` auto-adds `Namespace::MOUNT`)

### `test_no_mount_ns_no_auto_resolv` (updated)
**Requires:** root, alpine-rootfs

Spawns a container with `with_chroot` but without explicitly requesting `Namespace::MOUNT`
and asserts that the container succeeds and exits 0.

`with_chroot()` now automatically adds `Namespace::MOUNT` (matching runc behavior — runc
always creates a private mount namespace when rootfs is configured, regardless of whether
config.json requests one).  This means OCI bundles without a `"mount"` namespace entry in
`linux.namespaces` still work correctly.

Failure indicates the auto-add of `Namespace::MOUNT` in `spawn()` / `spawn_interactive()`
is broken — containers with rootfs configured would fail to get a private mount namespace,
causing pivot_root(2) to run in the host mount namespace.

### `test_overlay_kernel_support_detected`
**Requires:** none (no root, no rootfs)

Reads `/proc/filesystems` and asserts the string "overlay" appears in it.  This is the
same check `kernel_supports_overlayfs()` performs before forking a container with image
layers, so that a missing `CONFIG_OVERLAY_FS` produces a clear error message rather than
a cryptic "Invalid argument (os error 22)".

Failure means the running kernel lacks overlayfs support.  On a development machine this
would also mean `pelagos run image:tag` fails immediately with the message "kernel does not
support overlayfs (CONFIG_OVERLAY_FS not compiled in)".  On Alpine's `virt` kernel (common
in VM guests) this is the expected root cause of issue #100.

### `test_pivot_root_old_root_inaccessible`
**Requires:** root, alpine-rootfs

Starts a container with a chroot rootfs and asserts that `/.pivot_root_old` does not
exist inside the container after startup.  `do_pivot_root()` creates this directory
temporarily to pass to `pivot_root(2)` and immediately unmounts and removes it.  If it
persists, the old root was not properly detached.

Failure indicates `do_pivot_root()` is not cleaning up after itself — either the
`umount2(MNT_DETACH)` or `rmdir` failed silently, leaving the old root accessible.

### `test_exec_mnt_ns_inode_stored`
**Requires:** root, alpine-rootfs

Spawns a container in detached mode (`pelagos run -d --rootfs ...`) and reads the
resulting `state.json`.  Asserts that `mnt_ns_inode` is present and non-zero.  Then
calls `pelagos exec` into the running container and asserts it succeeds — the inode
check must pass transparently for a live container (stored inode equals live inode).

Failure indicates either:
- `mnt_ns_inode` is not being written to `state.json` at spawn time (storage missing)
- The inode check in `cmd_exec` is incorrectly rejecting a live container (false reject)

### `test_exec_detects_pid_reuse`
**Requires:** root, alpine-rootfs

Spawns a container in detached mode, polls until `state.json` has a real PID, then
tampers with `mnt_ns_inode` in `state.json` by setting it to the bogus value
`999_999_999`.  Calls `pelagos exec` and asserts it fails with an error message
containing "no longer running".

Simulates the scenario where a short-lived container exits and its PID is recycled by
an unrelated process: the mount-namespace inode of the recycled process will differ from
the stored inode, so `verify_pid_not_recycled` must catch this before any `setns(2)`
call is made.

Failure indicates the inode check in `cmd_exec` is not firing — `pelagos exec` would
silently enter the wrong process's namespaces.

### `test_build_pasta_dns_public_fallback`
**Requires:** rootless (non-root), pasta installed, `docker.io/library/alpine:latest` pulled

True CLI-level regression test for issue #102: `pelagos build` RUN steps with pasta
networking failing DNS resolution.

Runs `pelagos build --network pasta --no-cache -t <tag> -f <Remfile>` where the Remfile
is `FROM alpine\nRUN cat /etc/resolv.conf`. Asserts the build succeeds and the combined
stdout+stderr contains `8.8.8.8`, confirming the public DNS fallback is injected by
`execute_run()` in `build.rs`.

This is a true regression test: a revert of the `execute_run()` fix would cause the
build's `/etc/resolv.conf` to contain only the host's private DNS (e.g. `192.168.105.1`),
failing the assertion. Only the library-level mechanism test would pass in that scenario.

### `test_build_run_pasta_dns_bind_mount_works`
**Requires:** rootless (non-root), pasta installed, alpine image pulled

Library-level mechanism test for the DNS bind-mount path used by pasta-mode build RUN steps.

Constructs an explicit DNS list (public DNS 8.8.8.8/1.1.1.1), then runs a container
with `with_image_layers()` + `with_network(Pasta)` + `with_dns()` and checks that
`/etc/resolv.conf` inside the container contains `nameserver 8.8.8.8`.

Complements `test_build_pasta_dns_public_fallback`: if this passes but the CLI test fails,
the DNS bind-mount mechanism works but `execute_run()` is not injecting DNS. If this fails,
the underlying bind-mount mechanism itself is broken.

Failure indicates the DNS bind-mount mechanism isn't working for image-layer containers
with pasta networking.

### `test_copy_dot_src`
**Requires:** root, `docker.io/library/alpine:latest` pulled

Regression test for issue #103: `COPY . /dest/` (bare dot, no trailing slash) failed with ENOENT.

`Path::new(".").file_name()` returns `None`; the fallback `unwrap_or(".")` produced a resolved
destination of `/dest/.` instead of `/dest/`, and `create_dir_all` on the non-existent parent
then raised ENOENT.  The fix treats `src == "."` as contents mode, identical to `src.ends_with('/')`.

Builds a Remfile with `COPY . /tmp/ctx/`, runs the resulting image, and asserts that a sentinel
file written to the build context appears at `/tmp/ctx/sentinelfile` inside the container.

Failure indicates `execute_copy()` does not handle the bare-dot case and the ENOENT regression
has returned.

### `test_from_local_tag`
**Requires:** root, `docker.io/library/alpine:latest` pulled

Regression test for issue #104: `FROM <local-tag>` failed because `normalise_image_reference()`
unconditionally prepended `docker.io/library/`, producing a ref that does not match the on-disk
path written by `pelagos build -t <local-tag>`.

Builds a base image tagged `pelagos-test-local-base:latest` (with a sentinel file `/marker`),
then builds a derived image whose Remfile begins with `FROM pelagos-test-local-base`.  Asserts the
second build succeeds and the sentinel from the base image is visible inside the derived container.

Failure indicates the local-ref lookup is missing and `FROM <local>` unconditionally hits the
registry normalisation path, which does not know about locally built images.

### `test_from_stage_alias_with_build_arg`
**Requires:** root, `docker.io/library/alpine:latest` pulled

Regression test for issue #105: `FROM ${VAR}` where the variable value resolves to a prior
stage's alias, with the value supplied via `--build-arg`.

Before the fix, `completed_stages` was only consulted for `COPY --from` lookups; the FROM
base-image resolution always went straight to the image store.  After substitution
`base_ref = "stage0"` failed `image::load_image` because no image named `stage0` is stored,
even though `stage0` is a completed build stage.

Builds a two-stage Remfile where stage 1's `FROM ${NEXT_IMAGE}` is seeded by
`--build-arg NEXT_IMAGE=base_stage`.  Asserts the build succeeds and the file laid down in
stage 0 is visible in the final container.

Failure indicates: `FROM <stage-alias>` does not check `completed_stages` before the image
store, or `sub_vars` is not seeded from `--build-arg` at `execute_build` entry.

### `test_from_stage_alias_with_arg_default`
**Requires:** root, `docker.io/library/alpine:latest` pulled

Companion to `test_from_stage_alias_with_build_arg`: same Dockerfile pattern but without any
`--build-arg`.  The `ARG NEXT_IMAGE=base_default` instruction inside stage 0 supplies the
default.  After stage 0's instruction loop processes the ARG, `sub_vars` must contain the
key so that stage 1's `FROM ${NEXT_IMAGE}` can be substituted.

Asserts the build succeeds and the file from stage 0 is visible in the resulting image.

Failure indicates: `sub_vars` is not updated by ARG processing inside a stage's body, so
inter-stage FROM substitution fails when the caller provides no `--build-arg` override.

### `test_copy_chown_flag_parsed`
**Requires:** root, `docker.io/library/alpine:latest` pulled

Regression test for issue #106: `COPY --chown=root:root --from=<stage> <src> <dest>` failed
with "COPY source not found: --chown=root:root" because the parser consumed `--chown=` as the
source path.

The COPY parser previously handled only a single leading `--from=` flag via `strip_prefix`.
Any other flag (or flags in a different order) bypassed the check and fell through as `<src>`.
The fix replaces the single check with a loop that strips all `--key=value` flags (`--from=`,
`--chown=`, `--chmod=`) before extracting `<src> <dest>`.

Builds a two-stage Remfile where stage1 does `COPY --chown=root:root --from=stage0 /file /file`
and asserts the build succeeds and the copied file is readable in the resulting container.

Failure indicates: the flag-stripping loop is missing or does not handle `--chown=`, causing
it to be mis-parsed as the source path.

### `test_pasta_teardown_logs_output`
**Requires:** nothing (no root, no pasta, no alpine)

Regression test for issue #107: pasta's stdout and stderr were unconditionally discarded via
`Stdio::null()`, making TAP setup failures completely opaque.  pasta may write error messages
to stdout, stderr, or both depending on the error path and version, so both must be captured.

Exercises the merged output-capture infrastructure: spawns a real child process that writes
known sentinel strings to both stdout and stderr (simulating pasta writing an error on either
channel), then verifies that the merged reader thread collects both.

Does not require a pasta binary or container network namespace — it tests only the
pipe/thread mechanics that `setup_pasta_network` and `teardown_pasta_network` use.

Failure indicates: one or both pipes are still `Stdio::null()`, the merged reader thread is
not spawned correctly, or teardown does not join the thread to collect output.

### `test_pasta_root_bind_mount`
**Requires:** root, `pasta` in PATH, `unshare` in PATH, tun kernel module loaded (`/dev/net/tun` exists)

Regression test for issue #107 (root-mode pasta, v0.38.0 bind-mount fix).

**History of failures:**
- v0.36.0: `pasta <PID>` — EPERM on `/proc/<pid>/ns/user` (privilege-drop dance to nobody)
- v0.37.0: `pasta --netns /proc/<pid>/ns/net --runas 0` — EPERM on `/proc/<pid>/ns/net`
  (Yama `ptrace_scope=1` blocks cross-process `/proc/<pid>/ns/` access, confirmed on both
  Alpine linux-lts 6.12.x aarch64 and Arch Linux x86_64 with default kernel settings)
- fd-passing (`pasta --netns /proc/self/fd/N`): ENXIO — pasta cannot open namespace files
  via `/proc/self/fd` symlinks (pasta limitation; `nsenter` handles this but pasta does not)

**Fix (v0.38.0):** pelagos bind-mounts `/proc/<pid>/ns/net` onto a file in
`/run/pelagos/pasta-ns/` before spawning pasta.  The bind-mounted file lives on tmpfs and
is openable by pasta without any `/proc/<pid>/ns/` cross-process permission check.
`teardown_pasta_network` unmounts and removes the file after killing pasta.

Replicates the exact `setup_pasta_network` code path: spawns `unshare --net sleep 30`,
bind-mounts its netns, invokes pasta with the bind-mount path, polls
`/proc/<pid>/net/dev` for a non-loopback TAP interface for up to 5 seconds.

Failure indicates: `setup_pasta_network` is not using bind-mount, the bind-mount path
is not passed to pasta, or `--runas 0` is absent.

### `test_ps_json_flag_produces_valid_json`
**Requires:** root

Verifies that `pelagos ps --json` and `pelagos ps --json --all` each produce a valid JSON
array.  Does not require any containers to be running — an empty array `[]` is acceptable.

Failure indicates: the `--json` flag is not wired up to `cmd_ps`, or `cmd_ps` does not emit
JSON when the flag is set.

### `test_ps_json_and_format_json_identical`
**Requires:** root

`pelagos ps --json --all` and `pelagos ps --format json --all` must produce byte-for-byte
identical stdout.

Failure indicates: the two flags take different code paths or one of them is broken.

### `test_image_ls_json_flag_produces_valid_json`
**Requires:** root

Verifies that `pelagos image ls --json` produces a valid JSON array.  An empty array is
acceptable if no images are stored.

Failure indicates: `--json` is not wired up on `ImageCmd::Ls`.

### `test_network_ls_json_flag_produces_valid_json`
**Requires:** root

Verifies that `pelagos network ls --json` produces a valid JSON array.

Failure indicates: `--json` is not wired up on `NetworkCmd::Ls`.

### `test_volume_ls_json_flag_produces_valid_json`
**Requires:** root

Verifies that `pelagos volume ls --json` produces a valid JSON array.

Failure indicates: `--json` is not wired up on `VolumeCmd::Ls`.

### `test_run_finds_image_built_with_bare_tag`
**Requires:** root, `docker.io/library/alpine:latest` pre-pulled

Regression test for issue #109. `pelagos build -t myapp` stores the manifest as
`myapp:latest` (execute_build appends `:latest` to bare tags). Previously `pelagos run myapp`
tried only the raw ref `myapp` and the normalised registry form — never `myapp:latest` — so
the run failed immediately after a successful build.

The fix moves the `:latest` fallback into `image::load_image` itself: when a bare ref (no `:`
or `@`) is not found, `load_image` automatically retries with `<ref>:latest`.

This test calls `execute_build` with a bare tag, then asserts:
1. `manifest.reference == "myapp:latest"` — build stored the canonical form
2. `load_image("myapp")` succeeds — bare-tag lookup works
3. `load_image("myapp:latest")` succeeds — canonical form still works

Failure indicates: `load_image` no longer falls back to `<ref>:latest` for bare refs.

### `test_pasta_stdin_not_contaminated`
**Requires:** root, pasta installed, `docker.io/library/alpine:latest` pre-pulled

Regression test for issue #110 (v0.41.0 fix). During build RUN steps using `NetworkMode::Pasta`,
pelagos's own RUST_LOG debug output (written to pelagos's stderr fd 2 via env_logger) was aliasing
the container's stdin fd in certain host environments (vsock-invoked builds on pelagos-mac), causing
`curl | bash` RUN steps to fail with exit 127 as bash tried to execute the log line as a command.

The v0.40.0 fix (combining pasta's stdout+stderr into a single pipe) was insufficient because the
contaminating source was pelagos's own log output, not pasta's stdout.

The v0.41.0 fix applies two guards:
1. `container.rs` pre_exec: explicitly opens `/dev/null` from the host filesystem and dup2s it
   to fd 0 at the start of pre_exec (before namespace setup), overriding any incorrect fd
   inherited due to fd aliasing during Command setup.
2. `build.rs` execute_run: uses `Stdio::Piped` (not `Inherit`) for container stderr, so the
   container's fd 2 is a fresh write-only pipe isolated from pelagos's own fd 2. A relay thread
   forwards the pipe to pelagos's stderr so build output remains visible.

The test sets `RUST_LOG=debug` before building to maximise log output (reproducing the failure
mode), runs `cat /dev/stdin | wc -c` in a RUN step, and asserts the byte count is 0. A non-zero
count indicates pelagos's log output or pasta's pipes leaked into the container's stdin.

Failure indicates the stdin isolation fix (pre_exec null redirect or stderr Piped) was reverted.

### `test_build_run_path_isolated_from_host`
**Requires:** root, `docker.io/library/alpine:latest` pre-pulled

Regression test for issue #110 (v0.42.0 fix). Without `env_clear()` in `execute_run`, the container
process inherited the parent pelagos process's full environment. In unusual invocation environments
(vsock daemon, minimal init, macOS VM), the parent's `PATH` could be absent, incorrect, or differ
from what the image config specifies, causing "command not found" (exit 127) in RUN steps — most
visibly in the **first non-cached RUN step** of a subsequent build invocation where the cached-step
overlay stack is used.

The fix is calling `env_clear()` before applying `config.env` in `execute_run`, so the container
receives **only** the environment variables declared in the image config, matching Docker/runc
behaviour.

The test poisons the parent process `PATH` to a garbage value (`/nonexistent-poison-path`) and then
builds a one-step image that runs `ls /usr/bin/env`. If `env_clear()` is absent, the container
inherits the poisoned `PATH`, `ls` is not found (no PATH lookup), and the build fails. With the fix,
the container's `PATH` comes from the Alpine image config (`/usr/local/sbin:/usr/local/bin:...`),
`ls` is found, and `/found.txt` contains "ok".

Failure indicates `env_clear()` was removed from `execute_run` in `build.rs`.

### `test_build_run_path_fallback_when_config_env_empty`
**Requires:** root, `docker.io/library/alpine:latest` pre-pulled

Regression test for issue #110 (v0.43.0 fix). When a base image has an empty `config.env`
(e.g. ubuntu:22.04 from ECR mirrors where `parse_image_config` returns an empty `Vec` because
the OCI config JSON has a null or absent `Env` field), `execute_run` must still inject the
OCI-standard default `PATH` so that shell utilities are findable.

The test creates a fake `ImageManifest` using Alpine's layers but with `config.env = []`, then
builds a two-step image: `chmod 644 /etc/hostname && printenv PATH > /out.txt`. It asserts:
1. The build succeeds — `chmod` was found, meaning PATH was set in the container's environment.
2. `/out.txt` contains a `/`-prefixed path — `printenv PATH` printed the injected fallback PATH.

Failure (build exit 127) indicates `execute_run` does not inject the PATH fallback when
`config.env` is empty, causing every standard utility to be unfindable in the container.

### `test_env_path_expands_base_image_value`
**Requires:** root, `public.ecr.aws/docker/library/ubuntu:22.04` pre-pulled

Root-cause regression test for issue #110 (v0.44.0). The `substitute_vars` function was
expanding `${PATH}` (and other base-image env vars) to the empty string because `sub_vars`
only contained ARG/ENV values declared in the Remfile — not the base image's inherited env.

The devcontainer node feature (and many other tools) use the pattern:
```
ENV PATH="${NVM_DIR}/versions/node/v18/bin:${PATH}"
```
Before the fix, `${PATH}` → `""`, so `config.env.PATH` became `"/nvm/bin:"` with no standard
system directories. The next `RUN chmod ...` then fails with "not found" (exit 127) because
`/bin` is not in PATH.

After the fix, `sub_vars` is pre-populated with base image env vars after `FROM` is processed,
so `${PATH}` expands to the ubuntu base image's `PATH=/usr/local/sbin:...`.

The test builds a Remfile that has `ENV PATH="${NVM_DIR}/versions/node/v18/bin:${PATH}"` and
a subsequent `RUN chmod`. It asserts:
1. The build succeeds (chmod found because PATH includes `/bin`).
2. `/out.txt` contains a standard system directory like `/usr/bin` or `/bin`.

Failure indicates `sub_vars` is not seeded with base image env vars, causing `${PATH}` to
expand to empty string and breaking any `ENV PATH=...${PATH}` pattern.

### `test_build_run_tmp_is_world_writable`
**Requires:** root, `public.ecr.aws/docker/library/ubuntu:22.04` pre-pulled

Regression test for issue #111 (v0.46.0). `/tmp` inside a RUN step container must have
mode 1777 (world-writable + sticky bit). Tools like `apt-key` (invoked by `apt-get update`)
require this; if `/tmp` is mode 755, they fail with "Couldn't create temporary file
/tmp/apt.conf.*" (exit 100).

The root cause: `COPY src /tmp/dest` calls `create_dir_all` which creates the staging `/tmp`
with the process umask (755). This layer entry shadows the base image's `/tmp` (1777). Fixed
in v0.46.0 by `fix_staging_dir_perms()` in `build.rs`, which sets mode 0o1777 on the staging
`/tmp` before packaging the layer.

The test builds a Remfile that runs `stat -c '%a' /tmp` and `touch /tmp/canary.txt`.
It asserts:
1. The build succeeds (file creation in `/tmp` did not fail).
2. `/tmp-mode.txt` contains `1777` — the sticky + world-writable mode.

Failure indicates `fix_staging_dir_perms` is not setting `/tmp` to 0o1777 in `build.rs`.

### `test_build_copy_to_tmp_visible_in_run`
**Requires:** root, `public.ecr.aws/docker/library/ubuntu:22.04` pre-pulled

Regression test for issue #111 v0.45.0 regression: files COPY'd into `/tmp` must be visible
to subsequent RUN steps.

v0.45.0 fixed `/tmp` writability by mounting a fresh tmpfs over `/tmp` in `execute_run`. This
shadowed all overlayfs content in `/tmp`, making COPY'd files invisible to the next RUN step.

The correct fix (v0.46.0) removes the tmpfs mount and instead fixes permissions via
`fix_staging_dir_perms()` — the COPY layer's `/tmp` gets mode 1777, so the base image's `/tmp`
is not shadowed by a 755 entry, and RUN steps see both the correct permissions and the COPY'd content.

The test builds a Remfile that COPYs a sentinel file to `/tmp/sentinel.txt` then RUNs
`cat /tmp/sentinel.txt`. Failure would indicate a tmpfs is mounted on `/tmp` in `execute_run`,
hiding overlay content.

### `test_build_tmp_writable_after_copy`
**Requires:** root, `public.ecr.aws/docker/library/ubuntu:22.04` pre-pulled

Regression test for issue #111 v0.47.0 fix: `/tmp` must be mode 1777 (world-writable + sticky)
in RUN steps that follow a COPY instruction writing into `/tmp`.

Root cause: `copy_dir_recursive` used `create_dir_all` (mode 755 from umask) for directories and
never called `set_permissions`. When `create_layer_from_dir` stored the COPY layer via
`copy_dir_recursive`, the staging `/tmp` (set to 1777 by `fix_staging_dir_perms`) ended up as 755
in the layer store, shadowing the base image's `/tmp` (1777). This caused `apt-key`'s `mkstemp`
on `/tmp/apt.conf.*` to fail with EACCES (world-write bit missing), blocking `apt-get update` in
devcontainer feature builds.

Fix (v0.47.0): `copy_dir_recursive` now calls `set_permissions` after creating each destination
directory, preserving all 12 permission bits (including the sticky bit) from the source.

Mirrors the devcontainer feature install pattern: COPY into `/tmp/dev-features/`, then
`chmod -R 0755 /tmp/dev-features/node` followed by a write to `/tmp`. Asserts `/tmp-mode.txt`
contains `1777`. Failure indicates `copy_dir_recursive` is not preserving directory permissions.

### `test_build_copy_from_stage_tmp_writable`
**Requires:** root, `public.ecr.aws/docker/library/ubuntu:22.04` pre-pulled

Regression test for issue #111 v0.48.0 fix: `COPY --from=<stage>` into `/tmp` must also
produce a layer where `/tmp` has mode 1777.

v0.47.0 added `fix_staging_dir_perms` to `execute_copy` but not `execute_copy_from_stage`.
The devcontainer feature Dockerfile uses `COPY --from=<stage> /tmp/build-features/ /tmp/...`
which goes through `execute_copy_from_stage`. Without the fix, `/tmp` ended up at mode 755
in the layer store, blocking `apt-key` with EACCES in the subsequent `apt-get update`.

Two-stage Remfile: stage 0 (`FROM scratch`) places a file at `/tmp/build-features/probe.txt`;
stage 1 (`FROM ubuntu`) uses `COPY --from=0` to pull it out, then `stat -c '%a' /tmp`.
Asserts `/tmp-mode.txt` contains `1777`. Failure indicates `fix_staging_dir_perms` is absent
from `execute_copy_from_stage` in `build.rs`.

### `test_build_apt_install_ca_certificates`
**Requires:** root, `public.ecr.aws/docker/library/ubuntu:22.04` pre-pulled, `pasta` installed

Regression test for issue #112: `apt-get install ca-certificates` failed with EBUSY during
`update-ca-certificates`. The root cause: pelagos unconditionally bind-mounted the host's
`/etc/ssl/certs/ca-certificates.crt` into pasta overlay containers. The bind-mount target
cannot be renamed over — `update-ca-certificates` renames a `.crt.new` file onto it, which
fails with EBUSY.

Fix (v0.51.0): for overlay-based pasta containers (all build-engine RUN steps), the parent
process pre-seeds the overlay upper dir with a plain-file copy of the host CA cert before
fork. A plain file in the upper dir can be freely renamed over. The bind-mount is now only
performed for non-overlay (static rootfs) pasta containers where `update-ca-certificates`
would not be called.

Remfile: `FROM ubuntu:22.04 RUN apt-get update && apt-get install -y ca-certificates`.
Asserts exit code 0 and that the output contains "done." (the final `update-ca-certificates`
success message). Failure indicates the bind-mount-over-overlay EBUSY regression has returned.

### `test_run_applies_image_env_path`
**Requires:** root, `docker.io/library/alpine:latest` pre-pulled

Regression test for issue #114: `pelagos run` must propagate the image's OCI config
`Env` (set by Dockerfile `ENV` instructions) to the container process. Previously,
`apply_cli_options` in `run.rs` unconditionally called `cmd.env("PATH", default)` after
the image env was applied, clobbering any custom `PATH` from the image config.

Builds a one-layer alpine image with `ENV PATH=/issue-114-sentinel:...`, then spawns a
container running `echo $PATH`. Asserts the output contains `/issue-114-sentinel`.
Also verifies that `manifest.config.env` records the sentinel path after the build, which
confirms the build engine stores ENV correctly. Failure indicates the unconditional PATH
override has been re-introduced in `apply_cli_options`, or the image-config env application
order has been broken.

### `test_exec_applies_image_env_path`
**Requires:** root, `docker.io/library/alpine:latest` pre-pulled

Regression test for issue #115: `pelagos exec` must apply the image's OCI config `Env`
(Dockerfile `ENV` instructions) to the exec'd process environment, not rely solely on
reading the running container's `/proc/<pid>/environ`.

Previously, `cmd_exec` read the container's live `/proc/<pid>/environ` as the base env.
This is unreliable: containers started before issue #114 was fixed had the wrong PATH in
their environ, and in general the running container's environ may diverge from the image
config. The fix loads `manifest.config.env` from `state.spawn_config.image` and uses that
as the authoritative base (matching Docker's `exec` semantics). For rootfs-based containers
with no image manifest, the fallback to `/proc/<pid>/environ` is retained.

Flow: builds an alpine image with `ENV PATH=/issue-115-sentinel:...`, starts it in detached
mode, runs `pelagos exec <name> /bin/sh -c 'echo $PATH'`, asserts the sentinel appears.
Failure indicates `cmd_exec` no longer loads the image manifest config env.

## `-a`/`--attach` with `--detach` (issue #117)

### `test_detach_attach_stdout_streams_output`
**Requires:** root, alpine-rootfs

Runs `pelagos run -d -a STDOUT --name <n> ... /bin/sh -c 'echo sentinel-stdout'` and
asserts that "sentinel-stdout" appears in the caller's captured stdout, and that the
container name does NOT appear in stdout (it goes to stderr in attach mode).

Failure indicates: `-a STDOUT` is not setting up the attach pipe in `run_detached`, or the
tee relay in `relay.rs` is not writing to the pipe write-end, or the parent relay thread is
not reading from the pipe read-end.

### `test_detach_attach_stderr_streams_output`
**Requires:** root, alpine-rootfs

Runs `pelagos run -d -a STDERR ... /bin/sh -c 'echo sentinel-stderr >&2'` and asserts that
"sentinel-stderr" appears in the caller's captured stderr.

Failure indicates that `-a STDERR` is not teeing container stderr to the caller's stderr,
or the stderr attach pipe is not wired up in the relay.

### `test_detach_attach_sig_proxy_compat`
**Requires:** root, alpine-rootfs

Runs `pelagos run -d -a STDOUT -a STDERR --sig-proxy=false ... /bin/sh -c 'echo Container started'`
and asserts the command is accepted and "Container started" appears in stdout.

This exercises the Docker CLI compatibility pattern used by the VS Code devcontainer CLI.
Failure indicates that `--sig-proxy` is not accepted (clap parse error) or that the
combination of `-a STDOUT -a STDERR --sig-proxy=false` breaks the attach relay.

### `test_start_returns_promptly`
**Requires:** root, alpine image (pulled from registry)
**Module:** `issue_118_start_returns_promptly`

Runs `pelagos run --detach`, stops the container, then calls `pelagos start` with
`stdout(Stdio::piped())` and asserts that the process exits within 2 seconds.  After
the process exits, polls `state.json` to confirm the container reaches `status: running`
with a real PID within 5 seconds.

This test reproduces the exact mechanism of the hang described in issue #118: before the
fix the watcher child inherited the write-end of the stdout pipe and never closed it, so
the caller (SSH session, vsock relay, `Command::output()`) would block until the container
itself exited.  With the fix the watcher redirects its inherited stdio to `/dev/null` after
`setsid()`, releasing the pipe immediately when the parent exits.

Failure indicates either: (a) the watcher still inherits and holds the stdout pipe open
(reintroduction of issue #118), or (b) `spawn_config` is not being saved so `pelagos
start` cannot reconstruct the RunArgs.

### `test_etc_hosts_localhost_present`
**Requires:** root, alpine rootfs
**Module:** `issue_120_etc_hosts`

Spawns a container that runs `cat /etc/hosts` and asserts the output contains
`127.0.0.1 localhost` and `::1 ip6-localhost`.

Failure indicates that `/etc/hosts` is not being created (or is empty) in containers
with a MOUNT namespace + chroot, which causes `getaddrinfo("localhost")` to fail with
`ENOTFOUND` — breaking any software (e.g. VS Code Remote server's Node.js listener)
that resolves `localhost` at startup.

### `test_etc_hosts_hostname_alias`
**Requires:** root, alpine rootfs
**Module:** `issue_120_etc_hosts`

Spawns a container with `with_hostname("mycontainer")` that runs `cat /etc/hosts` and
asserts the output contains `127.0.1.1 mycontainer`.

Failure indicates that the hostname alias is missing from `/etc/hosts`, which would cause
`getaddrinfo(hostname)` to fail inside the container — matching Docker's behaviour of
always providing a `127.0.1.1` alias for the container's hostname.

### `test_run_foreground_state_written_before_output_issue_124`
**Requires:** root, network (alpine image pull)
**Module:** `issue_124_run_state_ordering`

Spawns `pelagos run` (foreground, no `--detach`) with stdout piped. Reads until
the container's first output line (`echo ready`) appears, then immediately reads
`state.json` and asserts `pid > 0`.

Failure indicates that container output reached the caller before `state.json`
was written with the real PID — the ordering bug described in issue #124.  Any
tool using container stdout as a readiness signal would call `pelagos exec` and
see `pid=0`, causing exec-into to fail.

### `test_run_detached_state_ready_on_return_issue_124`
**Requires:** root, network (alpine image pull)
**Module:** `issue_124_run_state_ordering`

Runs `pelagos run --detach` and immediately reads `state.json` the moment the
command returns.  Asserts `pid > 0`.

Failure indicates the parent returned (printed the container name) before the
watcher had written state with the real PID — the sync-pipe guarantee is broken.
A concurrent `pelagos exec` immediately after `pelagos run --detach` would see
`pid=0` and fail.

### `test_compose_down_kills_shell_entrypoint_descendants`
**Requires:** root, alpine image
**Module:** `compose_shutdown_fixes`

Starts a compose stack with a single service whose entrypoint is
`sh -c 'sleep 9999 & wait'` — a shell that backgrounds a child process and then
waits, simulating a shell-script wrapper that does not forward signals.  After
`compose up`, the test locates the backgrounded `sleep` process by scanning
`/proc` for any process in the container's process group (PGID == container PID).
It then calls `compose down` and asserts that the sleep process is no longer alive.

Failure indicates that `setpgid(0, 0)` in the container's pre_exec is not
making the container a process group leader, or that `compose down` is using
`kill(pid, sig)` instead of `kill(-pid, sig)` — leaving orphaned descendants
behind after shutdown (issue #169).

### `test_compose_no_pull_fails_immediately`
**Requires:** root
**Module:** `compose_shutdown_fixes`

Runs `compose up --no-pull` with a service that references an image tag that is
not present in the local layer store.  Asserts that the command exits non-zero
and that the error output contains `"not found locally"`.

Failure indicates that `--no-pull` is not wired up, the pre-fork image presence
check is not running, or the error message changed shape (issue #160).

### `test_compose_up_detects_stale_supervisor`
**Requires:** root, alpine image
**Module:** `compose_shutdown_fixes`

Runs `compose up` to start a sleep service, then SIGKILL-s the supervisor process
to simulate a crash.  Immediately re-runs `compose up` with the same config and
asserts it succeeds without requiring a manual `compose down` first.

Failure indicates that `cmd_compose_up_reml` is not detecting the dead supervisor
PID in the project state file, or that `cleanup_stale_project` is failing to
remove lingering containers and project state before starting fresh (issue #161).

### `test_dns_stale_config_removed_on_bind_failure`
**Requires:** root (writes to `/run/pelagos/dns/`)
**Module:** `dns`

Writes a DNS config file for a fictitious network whose gateway IP (192.0.2.1,
RFC 5737 TEST-NET) has no corresponding network interface on the host.  Starts
`pelagos-dns` pointing at the config dir and asserts that the stale file is
deleted and the daemon logs a "stale config" removal message rather than the
generic "failed to bind" error.

Failure indicates that EADDRNOTAVAIL on bind is not triggering the stale-config
cleanup path, meaning the daemon will spam "failed to bind" on every SIGHUP for
the lifetime of any unrelated compose stack (issue #168).

### `test_pull_nonexistent_image_error`
**Requires:** network access (no root)
**Module:** `images`
**Ignored by default:** yes (network)

Pulls two image references that do not exist on Docker Hub and asserts the error
says "image not found" rather than the opaque "Not authorized" that Docker Hub
returns for missing public-library images (issue #206).  Covers two sub-cases:

- No colon in the reference (`definitely-does-not-exist-xyz123456`): error must
  include a colon hint, because it looks like a name-tag typo.
- Explicit colon (`definitely-does-not-exist-xyz123456:latest`): "image not
  found" only, no hint.

Failure indicates `is_dockerhub_library_not_found()` is not matching, Docker Hub
changed its 401-for-nonexistent behavior, or the error text was changed.

### `test_pull_does_not_retain_blob`
**Requires:** root (writes to `/var/lib/pelagos/`)
**Module:** `images`

Extracts a synthetic layer tarball via `image::extract_layer()` and asserts
that the blob file does NOT exist in the blob store afterward.  Overlay mounts
use the unpacked layer directory directly; retaining the compressed blob would
double the on-disk cost of every pulled image.

Failure indicates the pull path is persisting the compressed tarball after
extraction, which would waste ~50% extra disk space per image (issue #127).

### `test_system_df_shows_components`
**Requires:** root (reads `/var/lib/pelagos/`)
**Module:** `system_prune`

Runs `pelagos system df` and asserts the output contains all expected component
headers: `Component`, `layers/`, `blobs/`, `images/`, `volumes/`,
`build-cache/`, and `Total`.

Failure indicates `system df` is missing rows or the column formatting is broken.

### `test_system_prune_removes_orphan_layers`
**Requires:** root (writes to `/var/lib/pelagos/layers/`)
**Module:** `system_prune`

Creates a synthetic layer directory with a digest not referenced by any local
image manifest, runs `pelagos system prune`, and asserts the directory was
removed.

Failure indicates orphan layer pruning is broken — layers from deleted images
will accumulate on disk indefinitely (issue #126).

### `test_system_prune_keeps_referenced_layers`
**Requires:** root (reads `/var/lib/pelagos/`)
**Module:** `system_prune`

Pulls alpine, runs `pelagos system prune` (without `--all`), and asserts that
the alpine image's layer directories still exist afterward.

Failure indicates the prune command is incorrectly removing layers that are
referenced by a local image manifest — running containers would fail to start.

### `test_system_prune_removes_blobs`
**Requires:** root (writes to `/var/lib/pelagos/blobs/`)
**Module:** `system_prune`

Writes a dummy file to the blob store, runs `pelagos system prune`, and asserts
the file was removed.

Failure indicates blob pruning is broken — build blobs will accumulate on disk
(issue #126).

### `cli::system::tests::test_prune_orphan_layers_keep_set` (unit test)
**Requires:** none (uses temp directory)
**Module:** `cli::system`

Creates two synthetic layer directories in a temp dir, builds a keep-set
containing only one of them, runs the orphan-removal loop, and asserts the
non-kept directory is removed while the kept one survives.

The `--all` flag's behavior (removing referenced-but-idle layers) is tested via
this unit test rather than an integration test because running `prune --all` on a
shared layer store destroys layers needed by other integration tests and requires
expensive re-pulls from external registries.

Failure indicates the keep-set filtering logic in `prune_orphan_layers` is broken.

### `test_system_prune_volumes_removes_unused_volume`
**Requires:** root (writes to `/var/lib/pelagos/volumes/`)
**Module:** `system_prune`

Creates a named volume via `pelagos volume create`, runs
`pelagos system prune --volumes`, and asserts the volume directory was removed.

Failure indicates volume pruning is broken — unused volumes will persist even
when the user explicitly requests `--volumes` cleanup (issue #126).

### `test_run_auto_pulls_missing_image`
**Requires:** root, internet access
**Module:** `auto_pull`

Removes the alpine image from the local store (all known reference forms), then
runs `pelagos run --rm alpine /bin/echo auto-pull-ok` without a prior pull.
Asserts the command succeeds, stdout contains `auto-pull-ok`, and stderr
contains the "Unable to find image" notice that confirms the auto-pull path was
taken.

Failure indicates the auto-pull path in `build_image_run` is broken — either
the pull was not attempted, the image was not re-loaded after the pull, or the
pull itself failed (network issue in CI). Covers issue #189.

### `test_compose_bind_and_network_service_stays_alive`
**Requires:** root, internet access (ECR image pull)
**Module:** `compose_bind_network`

Starts a compose project with a single service that has **both** a bind-mount
and a bridge network. Waits 2 seconds after `compose up`, then checks that the
service is still listed as `running` in `pelagos ps`.

Failure indicates the bind+network combination causes immediate container exit
(regression for issue #227). The bug caused the container to die silently
within 2 seconds with no logged error, because the waiter thread did not log
the exit status and the supervisor did not detect the early exit before printing
"All services started". This test also exercises the new warning logging added
in the waiter thread (`compose: service 'x' killed by signal N` or `exited with
code N`).

### `test_compose_bind_is_read_write`
**Requires:** root, internet access (ECR image pull)
**Module:** `compose_bind_network`

Starts a compose project whose service uses `(list 'bind ...)` (no `-rw` suffix)
and a command that writes a file to the bind-mounted path. After the service
reaches `running` state, verifies the file exists on the host side.

Failure indicates the `bind` option is read-only (regression for issue #228):
before the fix, `apply_service_opt` hardcoded `read_only: true` for `"bind"`,
causing writes inside the container to fail with "Read-only file system".

---

## pelagos-dockerd integration tests (mod `dockerd_integration`)

These tests start `pelagos-dockerd` on a temp Unix socket, launch containers
via the pelagos CLI, and hit the Docker HTTP API to verify the inotify-based
log-follow and wait handlers introduced in issue #214.

### `test_dockerd_logs_follow_terminates_on_exit`
**Requires:** root, internet (alpine pull)
**Module:** `dockerd_integration`

Starts a container that prints two lines then exits after ~1s. Issues
`GET /containers/{id}/logs?stdout=true&follow=true` and measures how long until
the HTTP body closes. Asserts:
- Stream closes within 3s (not hanging indefinitely)
- Both "before" and "after" lines are present in the received data

Failure indicates the inotify `CLOSE_WRITE` on the log file is not waking the
background follow task, causing the stream to hang until a client disconnect or
daemon shutdown. Regression for the polling implementation's behavior of never
closing the stream until the next 200ms tick saw the state change.

### `test_dockerd_logs_follow_streams_real_time`
**Requires:** root, internet (alpine pull)
**Module:** `dockerd_integration`

Starts a container that emits one line every 200ms indefinitely. Streams logs
for 2 seconds and counts received lines. Asserts ≥6 lines were delivered.

Failure indicates the follow stream is batching output with a fixed delay
(200ms+ polling sleep) rather than delivering lines as inotify `MODIFY` events
arrive. Expected ~10 new lines in 2s at 200ms/line; the ≥6 threshold absorbs
scheduling jitter.

### `test_dockerd_wait_returns_exit_code`
**Requires:** root, internet (alpine pull)
**Module:** `dockerd_integration`

Starts a container that exits after 1s with code 42. Issues
`POST /containers/{id}/wait` and measures the wall-clock time until it returns.
Asserts:
- Returns within 3s (not blocking until the next 500ms poll tick)
- Response body contains the exit code 42

Failure indicates the inotify watch on `state.json` is not firing or not being
consumed, leaving the wait handler in the 500ms polling fallback path (which
would close within ~1.5s but signals a regression in the inotify path).

### `test_dockerd_logs_partial_line_across_writes`
**Requires:** root, internet (alpine pull)
**Module:** `dockerd_integration`

Container writes "hello" (no newline), sleeps 300ms, then writes " world\n" — two writes, two MODIFY events. Decodes raw docker frames (not just text lines) and asserts that "hello world" appears in a single frame payload, and that no frame contains only "hello".

Failure indicates `frames_from_bytes` is emitting partial lines as complete frames rather than buffering in `line_buf` until the newline arrives.

### `test_dockerd_logs_no_trailing_newline`
**Requires:** root, internet (alpine pull)
**Module:** `dockerd_integration`

Container runs `printf 'no-newline'` and exits without ever writing `\n`. Asserts that the combined payload of all docker frames contains "no-newline".

Failure indicates the `line_buf` flush at the end of the background follow task is not running, causing the last (partial) line of any container's output to be silently dropped.

### `test_dockerd_logs_follow_already_exited`
**Requires:** root, internet (alpine pull)
**Module:** `dockerd_integration`

Container writes 5 numbered lines and exits. Test waits 2s (well past exit) before issuing `follow=true`. Asserts all 5 lines received and stream closes within 3s.

Failure indicates the follow task hangs indefinitely when CLOSE_WRITE fired before the inotify watch was set up — the 1s fallback timeout must detect the already-exited state.

### `test_dockerd_logs_large_initial_drain`
**Requires:** root, internet (alpine pull)
**Module:** `dockerd_integration`

Container writes exactly 500 numbered lines as fast as possible, then exits. After a 3s wait, requests `follow=true`. Asserts exactly 500 lines received, in order (`line1`…`line500`).

Failure indicates the pre-`into_event_stream` drain loop drops bytes, produces incorrect frame boundaries on large reads, or `frames_from_bytes` mishandles 65536-byte read chunks.

### `test_dockerd_logs_follow_client_disconnect`
**Requires:** root, internet (alpine pull)
**Module:** `dockerd_integration`

Starts a fast-writing container. Records pelagos-dockerd fd count (via `/proc/{pid}/fd`). Issues a 1s follow connection and disconnects. Waits 2s. Checks fd count is within 5 of baseline, and that a second follow connection still works.

Failure (fd leak) indicates `tx.send().is_err()` is not being detected, leaving the background inotify task running and its inotify fd open after client disconnect.

### `test_dockerd_logs_concurrent_follows`
**Requires:** root, internet (alpine pull)
**Module:** `dockerd_integration`

Starts a container writing one line per 150ms. Opens two simultaneous follow connections in parallel threads, each running for 2s. Asserts both receive ≥6 lines and share a common prefix sequence.

Failure indicates a resource conflict between concurrent inotify instances, one stream starving the other, or incorrect per-connection state.

### `test_dockerd_logs_follow_backpressure`
**Requires:** root, internet (alpine pull)
**Module:** `dockerd_integration`

Starts a fast-writing container (~10 KB/s). Connects with `curl --limit-rate 100` (100 bytes/sec) for 3s. Measures pelagos-dockerd RSS growth. Asserts growth <5 MB.

Failure indicates the bounded mpsc channel (capacity 8) is not blocking the producer — without backpressure the producer would buffer the full 3s of unread output in memory.

## nfnetlink_native

### `test_nft_nat_create_delete`
**Requires:** root
**Module:** `nfnetlink_native`

Calls `nft_create_nat_masquerade` to install a NAT masquerade table and chains natively via nfnetlink (no `nft` binary). Verifies via `nft list` that the table exists, the postrouting chain has a masquerade rule, and the forward chain has accept rules. Then calls `nft_delete_ip_table` and verifies the table is gone.

Failure indicates the nfnetlink message encoding for NEWTABLE/NEWCHAIN/NEWRULE (masquerade or forward accept) is wrong, or that `nft_delete_ip_table` (DELTABLE) fails silently when it shouldn't.

### `test_nft_dns_input_rule`
**Requires:** root
**Module:** `nfnetlink_native`

Calls `nft_add_dns_input_chain` to install a DNS INPUT chain that accepts UDP port 53 on a named bridge. Verifies via `nft list chain` that the chain contains a port 53 accept rule. Calls `nft_remove_dns_input_chain` and verifies the input chain no longer exists.

Failure indicates the iifname match, UDP dport match, or verdict accept expression encoding is wrong, or that flush/delete-chain operations fail.

### `test_nft_dnat_rules`
**Requires:** root
**Module:** `nfnetlink_native`

Calls `nft_install_dnat` to install a DNAT port-forward rule (TCP host:18080 → container:80) natively. Verifies via `nft list chain` that the prerouting chain contains the expected DNAT rule. Calls `nft_flush_prerouting` and verifies no rules remain. Cleans up with `nft_delete_ip_table`.

Failure indicates the DNAT expression encoding (proto match, dport match, immediate IP/port load, nat DNAT) is wrong, or that flush-chain (DELRULE without handle) fails.

### `test_nft_iptables_filter_compat`
**Requires:** root, iptables-nft `ip filter` table present
**Module:** `nfnetlink_native`

Guards on `ip filter` table existence (iptables-nft must be active). Calls `nft_add_filter_forward_compat` to add a CIDR-scoped chain to `ip filter` with a jump rule in FORWARD. Verifies the jump exists via `nft list chain` or `nft_find_jump_handles`. Calls `nft_remove_filter_forward_compat` and verifies the jump handles are empty.

Failure indicates GETRULE DUMP parsing or DELRULE-by-handle encoding is wrong, or that the iptables-nft forward chain add/remove sequence leaves stale rules.

### `test_stats_no_stream`
**Requires:** root, alpine-rootfs
**Module:** `stats`

Starts a detached container (no cgroup limits), runs `pelagos stats --no-stream <name>`,
and asserts the output contains the column header (NAME, CPU %, MEM) and the container
name on a data row. Cleans up the container after the assertions.

Failure indicates the stats command cannot discover a running container via state files,
crashes during cgroup probing, or produces output missing expected columns or the container name.

### `test_network_rm_uses_native_netlink`
**Requires:** root
**Module:** `native_netlink_teardown`

Creates a named network via `pelagos network create`, then removes it via `pelagos network rm`.
Verifies the command exits 0 and the config directory is gone. Exercises the `netlink::link_del`
and `nfnetlink::nft_delete_ip_table` paths that replaced the `ip`/`nft` shell-outs in
`cmd_network_rm`. Both calls are expected to return non-fatal "not found" errors (no container
ran on this network, so no bridge or nft table was created); the test confirms the command
handles those gracefully and still removes the config.

Failure indicates `cmd_network_rm` is crashing on non-fatal link_del/nft errors, or the
config directory cleanup is broken.

### `test_netns_del_roundtrip`
**Requires:** root
**Module:** `native_netlink_teardown`

Calls `netlink::netns_create` then `netlink::netns_del` and asserts the `/run/netns/<name>`
bind-mount is present after create and absent after del.

Failure indicates `netns_del`'s `umount2(MNT_DETACH)` or `unlink` is broken, or the path
construction is wrong.

### `test_cleanup_removes_stale_netns`
**Requires:** root
**Module:** `native_netlink_teardown`

Creates a `/run/netns/rem-0-tcln` entry (PID=0 is always dead per `pid_alive`'s `pid <= 0`
guard), runs `pelagos cleanup`, and asserts the entry is gone.

Failure indicates `cli/cleanup.rs::cleanup_netns` is not calling `netlink::netns_del`
correctly, or the stale-PID detection logic is broken.

### `test_network_rm_deletes_live_bridge`
**Requires:** root + rootfs
**Module:** `native_netlink_teardown`

Creates a named network, runs a short-lived container on it (which instantiates the kernel
bridge interface `rm-brdel`), waits for the container to exit, verifies the bridge still
exists (teardown only removes the veth, not the bridge), then calls `pelagos network rm` and
asserts the bridge interface is gone.

Failure indicates `cmd_network_rm`'s `link_del(&net.bridge_name)` call is not deleting a
live bridge, which would leave orphaned bridge interfaces after network removal.

## `rm_multi_name_and_stale_state` module

### `test_rm_accepts_multiple_names`
**Requires:** root
**Module:** `rm_multi_name_and_stale_state`

Starts two containers, then calls `pelagos rm -f name1 name2` in a single invocation.
Asserts that both containers are gone from `pelagos ps` after the command succeeds.

Failure indicates that `pelagos rm` is not accepting multiple names (like `docker rm` does),
so users who pass more than one name will silently fail to remove all but the first.

### `test_failed_spawn_no_ghost_state`
**Requires:** root
**Module:** `rm_multi_name_and_stale_state`

Starts a container on host port 19801, then attempts to start a second container binding
the same host port (which should fail). Asserts that the failed-spawn container has no
`state.json` leftover and does not appear in `pelagos ps`.

Failure indicates the watcher is not cleaning up the state directory on `cmd.spawn()` error,
leaving ghost containers with `status=running, pid=0` visible in `pelagos ps` until manually
removed.

---

## `supplemental_groups` — Supplemental GIDs / fsGroup (#289)

### `test_additional_gids_appear_in_proc_status`
**Requires:** root, rootfs
**Module:** `supplemental_groups`

Spawns a container as UID/GID 1000 with `with_additional_gids(&[1337])` and reads
`/proc/self/status` inside it. Asserts that `1337` appears in the `Groups:` line.

Failure means `setgroups(2)` is not being called in the container pre_exec hook, so
non-root container processes would fail with `EACCES` when reading files owned by
a supplemental group (e.g., Kubernetes service account tokens chowned to `root:<fsGroup>:640`).

### `test_multiple_additional_gids`
**Requires:** root, rootfs
**Module:** `supplemental_groups`

Spawns a container with three supplemental GIDs (100, 1337, 2000) and asserts all three
appear in the `Groups:` line of `/proc/self/status`.

Failure indicates that only a subset of supplemental GIDs is being applied, which would
cause selective permission failures when a workload depends on more than one supplemental group.

---

## `pelagos-cri` unit tests — CRI log relay (#290)

These run via `cargo test -p pelagos-cri` (no root, no rootfs).

### `test_relay_logs_from_dir_e2e`
**Type:** unit/e2e (no root, no rootfs)
**Module:** `pelagos-cri::runtime::tests`

Full end-to-end test of the `relay_logs_from_dir` loop. Creates a temp directory
mimicking a pelagos container dir (`stdout.log`, `stderr.log`, `state.json`).
Verifies catch-up (line written before the relay starts), streaming (line written
while the relay is running), stderr tagging, RFC3339 timestamp format, and that
the relay exits promptly after state.json is set to `"exited"`.

Failure indicates the relay loop is not polling correctly, not writing CRI log
format, or not terminating on container exit — meaning `kubectl logs` would not work.

---

## DNS resolv.conf injection (#293)

### `test_dns_search_in_resolv_conf`
**Requires:** root, rootfs
**Module:** `dns_resolv_conf`

Spawns a container with `with_dns(&["1.1.1.1"])` and `with_dns_search(&["cluster.local", "svc.cluster.local"])`.
Reads `/etc/resolv.conf` inside the container and asserts both `nameserver 1.1.1.1` and
`search cluster.local svc.cluster.local` appear.

Failure indicates the `search` line is not being written to resolv.conf, meaning cluster
DNS search domains injected by kubelet would not take effect.

### `test_dns_options_in_resolv_conf`
**Requires:** root, rootfs
**Module:** `dns_resolv_conf`

Spawns a container with `with_dns(&["8.8.8.8"])` and `with_dns_options(&["ndots:5", "timeout:2"])`.
Asserts `nameserver 8.8.8.8` and `options ndots:5 timeout:2` appear in `/etc/resolv.conf`.

Failure indicates DNS options are not being written, meaning Kubernetes `ndots:5` would not
be applied — causing all short names to bypass the search path and fail to resolve.

### `test_dns_full_resolv_conf`
**Requires:** root, rootfs
**Module:** `dns_resolv_conf`

Spawns a container with the full Kubernetes-style DNS config: nameserver `10.96.0.10`,
three search domains (`default.svc.cluster.local`, `svc.cluster.local`, `cluster.local`),
and option `ndots:5`. Asserts all three sections appear in `/etc/resolv.conf`.

This is the canonical regression test for issue #293: if any of the three fields are
missing, cluster DNS would be broken even if the others are present.

### `test_cri_sandbox_dns_fields_roundtrip`
**Type:** unit (no root, no rootfs)
**Module:** `pelagos-cri::runtime::tests`

Constructs a `CriSandbox` with dns_servers, dns_searches, and dns_options populated.
Serializes to JSON and deserializes back; asserts all three DNS fields round-trip correctly.

Failure indicates the serde `#[serde(default)]` annotations or field names are broken,
meaning persisted sandbox state would lose DNS config on restart.

### `test_cli_dns_search_option_flags`
**Requires:** root, rootfs
**Module:** `dns_resolv_conf`

Regression test for #293 (round 2). Invokes `pelagos run` via CLI with `--dns 10.43.0.10`,
`--dns-search default.svc.cluster.local`, `--dns-search cluster.local`, and `--dns-option ndots:5`.
Reads `/etc/resolv.conf` in the container and asserts all three sections appear.

This test exercises the CLI wiring, not just the library. The original #293 fix placed
`dns_search`/`dns_option` wiring inside the non-sandbox `else` block in `cmd_run`, so they were
silently ignored for CRI containers (which always use `--sandbox`). A passing library test would
not have caught this — only a CLI-level test does.

## Cgroup Placement (issue #297)

### `test_with_cgroup_path_places_process`
**Requires:** root, rootfs
**Module:** `cgroup_placement`

Spawns a library-level container (no PID or CGROUP namespace: `Namespace::UTS | Namespace::MOUNT`)
with `with_cgroup_path("pelagos-test-cgroup/cgroup-placement-test")`. Reads `/proc/self/cgroup`
from inside the container and asserts it contains "cgroup-placement-test". Without the CGROUP
namespace, the container sees the full host cgroup path — making this a simple positive test for
the path-based cgroup assignment.

Failure indicates `with_cgroup_path()` is not honoured by `create_cgroup_no_task()`.

### `test_with_cgroup_path_nested`
**Requires:** root, rootfs
**Module:** `cgroup_placement`

Same as above but with a three-level nested path
(`pelagos-test-cgroup/level2/cgroup-nested-test`) to validate slash-separated paths that mirror
the kubepods hierarchy (e.g. `kubepods/besteffort/podUID/containerID`).

Failure indicates nested cgroup paths are not being created correctly.

### `test_cli_cgroup_path_flag`
**Requires:** root, rootfs
**Module:** `cgroup_placement`

Runs `pelagos run --detach --cgroup-path pelagos-test-cgroup/cli-test /bin/sleep 30` via the
CLI. After 300ms (time for the watcher to write state.json), reads the container's intermediate
PID from state.json, finds the grandchild (real container) PID from
`/proc/<intermediate>/task/<intermediate>/children`, then reads `/proc/<grandchild>/cgroup` from
the HOST and asserts it contains "cli-test".

This test validates the full end-to-end cgroup placement path including the PID-namespace
double-fork. The CLI always adds `Namespace::PID` and `Namespace::CGROUP`. The fundamental
challenge: writing from inside the CGROUP namespace loses visibility of the host-root-anchored
cgroup path, and writing from inside the PID namespace makes the kernel resolve the PID in the
wrong namespace table (ESRCH). The fix writes from the host-side parent process after spawn(),
using `find_container_pid()` to get the grandchild's host PID.

Failure would indicate the SPIRE-conformant cgroup placement is broken and workload attestation
would fail because `/proc/<pid>/cgroup` would not match the kubepods hierarchy pattern.

## PID Namespace Mode (`pid_namespace` module)

### `test_isolated_pid_namespace_by_default`
**Requires:** root, rootfs
**Module:** `pid_namespace`

Runs `pelagos run --detach /bin/sleep 30` (no `--no-pid-ns`) and reads the grandchild's
NSpid from `/proc/<container_pid>/status` on the HOST. Asserts that NSpid has exactly two
entries and the in-namespace PID is 1.

Validates that the default behavior (Namespace::PID included) performs the PID namespace
double-fork correctly. Failure indicates Namespace::PID is being ignored.

### `test_no_pid_ns_flag_uses_host_pid_namespace`
**Requires:** root, rootfs
**Module:** `pid_namespace`

Runs `pelagos run --detach --no-pid-ns /bin/sleep 30`. Without Namespace::PID there is no
double-fork: `state.pid` IS the container process. Reads `/proc/<state.pid>/status` from the
host and asserts NSpid has exactly one entry (host namespace only, no isolation).

This is the regression test for issue #299 (hostPID: true / namespace_options.pid = NODE not
respected). Failure would mean the SPIRE agent cannot attest workloads via SO_PEERCRED because
the container is in an isolated PID namespace and PID 0 is returned instead of the real PID.

## CRI Container ID Format (`pelagos-cri` unit tests)

### `test_generate_id_is_64_char_hex`
**Requires:** nothing (unit test in pelagos-cri)
**Module:** `runtime::tests`

Calls `generate_id()` and asserts the result is exactly 64 lowercase hex characters.
This is the de facto standard used by containerd and CRI-O; SPIRE, Fluentd, Fluent Bit,
OTel, Datadog, and Falco all hardcode `[a-f0-9]{64}` regexes to extract container IDs
from cgroup paths.

Failure indicates that the ID format regressed back to a shorter form (e.g. 32-char UUID
simple string) which would silently break workload attestation and log correlation in every
Kubernetes observability/security tool.

### `test_generate_id_is_unique`
**Requires:** nothing (unit test in pelagos-cri)
**Module:** `runtime::tests`

Generates 20 IDs and inserts them into a HashSet, asserting all 20 are distinct.
Catches obvious bugs such as a zero-initialized RNG or a seeded PRNG that always
produces the same output.

## Privileged Mode and CRI Security/Resource Wiring (`mod privileged_mode`)

### `test_privileged_mode_library_api`
**Requires:** root, alpine-rootfs
**Module:** `privileged_mode`

Spawns a container using the `Command` builder with `.with_privileged()`. Reads `/proc/self/status`
inside the container and asserts that `CapEff` is non-zero and equals `CapPrm` (all capabilities
retained). This verifies that privileged mode suppresses capability dropping and leaves the full
capability set in place.

Failure indicates that `with_privileged()` is not properly bypassing capability management in the
spawn path — the CRI privileged container feature (required by tools like kubeadm, CNI plugins)
would be silently broken.

### `test_privileged_mode_cli_flag`
**Requires:** root, alpine-rootfs
**Module:** `privileged_mode`

Runs `pelagos run --privileged --network loopback --no-pid-ns /bin/ash -c "grep CapEff /proc/self/status"`.
Parses the hex `CapEff` value and asserts it is ≥ `(1<<41)-1` (all 41 kernel capabilities set).
This is the CLI-level regression test for issue #306.

Failure means the `--privileged` flag is accepted by the CLI but not actually wired to
`Command::with_privileged()`, leaving Kubernetes privileged containers running with the
default (restricted) capability set.

### `test_cap_drop_all_cap_add_net_bind_service_cli`
**Requires:** root, alpine-rootfs
**Module:** `privileged_mode`

Runs `pelagos run --cap-drop ALL --cap-add NET_BIND_SERVICE --network loopback --no-pid-ns`.
Parses `CapEff` and asserts it equals exactly `0x400` (only `CAP_NET_BIND_SERVICE`, bit 10).
This tests both the CRI capability wiring (issue #304) and the existing `--cap-drop`/`--cap-add`
CLI mechanics end-to-end.

Failure means that capabilities are not being wired correctly from CRI
`LinuxContainerSecurityContext.capabilities` through to the container process.

### `test_memory_limit_cli`
**Requires:** root, alpine-rootfs, cgroups v2
**Module:** `privileged_mode`

Runs `pelagos run --detach --memory 67108864 --network loopback --no-pid-ns /bin/sleep 30`.
Reads the container's cgroup path from state.json, then reads
`/sys/fs/cgroup/<cgroup_name>/memory.max` and asserts it equals 64 MiB exactly.
This is the CLI regression test for issue #305 (CRI resource limits wiring).

If `cgroup_name` is empty in state.json (cgroups unavailable in test environment), the test
prints a skip message and returns without failing. Failure means `--memory` is not being
passed through the CRI `LinuxContainerResources.memory_limit_in_bytes` → `--memory` path,
so kubelet-requested memory limits for Kubernetes pods are silently ignored.

### `test_cap_drop_all_with_non_root_user`
**Requires:** root, alpine-rootfs
**Module:** `privileged_mode`

Regression test for issue #308: `--cap-drop ALL --user 1000` previously failed with EINVAL.

Root cause: `capset()` ran at step 4.86 (before `setuid()`), zeroing `CAP_SETUID`.
When `setuid(1000)` then ran it got EPERM, which became EINVAL via the spawn error pipe
(`io::Error::other()` has no `raw_os_error`).

Fix: `PR_CAPBSET_DROP` only at step 4.86; `PR_SET_KEEPCAPS` set before `setuid()`
so the permitted set survives the UID switch; `capset()` moved to step 8.6
(after `setuid()`).

Runs `pelagos run --cap-drop ALL --user 1000 --network loopback --no-pid-ns`
and asserts:
1. The process exits successfully (no EINVAL).
2. `CapEff` from `/proc/self/status` is exactly 0 (all caps dropped).

Failure means the capset/setuid ordering regression has returned.

## CRI Phase 2 Security Context (`mod cri_phase2_security`)

### `test_read_only_rootfs_cli`
**Requires:** root, alpine-rootfs
**Module:** `cri_phase2_security`

Runs `pelagos run --read-only --tmpfs /tmp` and attempts to write to `/etc/`. Asserts the write
is blocked. Verifies CRI `securityContext.readOnlyRootFilesystem = true` → `--read-only` wiring
(issue #311). Failure means kubelet-requested read-only rootfs is silently ignored.

### `test_no_new_privs_cli`
**Requires:** root, alpine-rootfs
**Module:** `cri_phase2_security`

Runs `pelagos run --security-opt no-new-privileges` and reads `NoNewPrivs` from
`/proc/self/status`. Asserts value is 1. Verifies CRI `securityContext.noNewPrivs = true` →
`--security-opt no-new-privileges` wiring (issue #311).

### `test_seccomp_default_cli`
**Requires:** root, alpine-rootfs
**Module:** `cri_phase2_security`

Runs `pelagos run --security-opt seccomp=default` and reads `Seccomp` from `/proc/self/status`.
Asserts value is 2 (filter mode). Verifies CRI `securityContext.seccomp.profileType =
RuntimeDefault` → `seccomp=default` wiring (issue #311).

### `test_seccomp_unconfined_cli`
**Requires:** root, alpine-rootfs
**Module:** `cri_phase2_security`

Runs `pelagos run --security-opt seccomp=none` and reads `Seccomp` from `/proc/self/status`.
Asserts value is 0 (disabled). Verifies CRI `securityContext.seccomp.profileType = Unconfined`
→ `seccomp=none` wiring (issue #311). Note: pelagos uses `none` (not `unconfined`) to disable seccomp.

### `test_masked_path_cli`
**Requires:** root, alpine-rootfs
**Module:** `cri_phase2_security`

Runs `pelagos run --masked-path /proc/kcore` and uses `stat -c '%t:%T'` to verify the device
is `1:3` (major:minor in hex = `/dev/null`). Verifies CRI `securityContext.maskedPaths` →
`--masked-path` wiring (issue #311). Failure means sensitive kernel paths are exposed to containers.

## CRI Phase 3 Compatibility (`mod cri_phase3_compat`)

### `test_no_ipc_ns_shares_host_ipc`
**Requires:** root, alpine-rootfs
**Module:** `cri_phase3_compat`

Runs `pelagos run --no-ipc-ns` and verifies that `readlink /proc/self/ns/ipc` inside the
container matches the host's IPC namespace inode. Verifies CRI
`namespace_options.ipc = NODE (2)` → `--no-ipc-ns` wiring (issue #313).
Failure means `hostIPC: true` pods share an isolated IPC namespace instead of the host's.

### `test_isolated_ipc_ns_by_default`
**Requires:** root, alpine-rootfs
**Module:** `cri_phase3_compat`

Baseline: without `--no-ipc-ns`, the container IPC namespace inode differs from the host.
Ensures the positive test is meaningful — if this fails, IPC isolation is broken entirely.

### `test_oom_score_adj_cli`
**Requires:** root, alpine-rootfs
**Module:** `cri_phase3_compat`

Runs `pelagos run --oom-score-adj 500` and reads `/proc/self/oom_score_adj` inside the
container. Asserts value is 500. Verifies CRI `resources.oom_score_adj` → `--oom-score-adj`
wiring (issue #313). Failure means kubelet OOM priority assignments are silently ignored.

### `test_memory_swap_limit_cli`
**Requires:** root, alpine-rootfs, cgroups v2 with swap accounting
**Module:** `cri_phase3_compat`

Runs `pelagos run --detach --memory 64m --memory-swap 128m` and reads `memory.swap.max`
from the container's cgroup directory. Asserts it equals 128 MiB. Skips if
`memory.swap.max` is not present (swap accounting disabled in kernel). Verifies CRI
`resources.memory_swap_limit_in_bytes` → `--memory-swap` wiring (issue #313).

## CRI Phase 4 Compatibility (`mod cri_phase4_compat`)

### `test_cpuset_cpus_cli`
**Requires:** root, alpine-rootfs
**Module:** `cri_phase4_compat`

Runs `pelagos run --cpuset-cpus 0` and reads `Cpus_allowed_list` from `/proc/self/status`.
Asserts value is `"0"` (pinned to CPU 0). Verifies CRI `resources.cpuset_cpus` →
`--cpuset-cpus` wiring (issue #315). Failure means NUMA pinning for low-latency workloads
is silently ignored.

### `test_stop_signal_cli`
**Requires:** root, alpine-rootfs
**Module:** `cri_phase4_compat`

Starts a detached container with `--stop-signal SIGQUIT` that traps SIGQUIT and prints
`QUIT_RECEIVED`. Calls `pelagos stop` and checks logs for the trap output. Verifies CRI
`config.stop_signal` → `--stop-signal` wiring (issue #315). Failure means containers
configured with custom stop signals (nginx SIGQUIT, postgres SIGINT) receive SIGTERM and
may not shut down cleanly.

### `test_selinux_label_accepted_cli`
**Requires:** root, alpine-rootfs
**Module:** `cri_phase4_compat`

Runs `pelagos run --selinux-label system_u:system_r:container_t:s0 /bin/true`. Asserts the
command exits successfully. On non-SELinux systems the label is silently ignored; this test
verifies the flag is accepted and doesn't crash. Verifies CRI
`securityContext.selinux_options` → `--selinux-label` wiring (issue #315).

## `cri_uid_hardening::test_uid_u32_max_rejected`
**Requires root, requires alpine-rootfs.**
Passes `--user 4294967295` (u32::MAX = `(uid_t)-1`) to `pelagos run` and asserts the
command exits with an error. On Linux `setuid((uid_t)-1)` returns EINVAL; in some runtime
implementations this silently leaves the process as root (CVE-2024-40635 class). Verifies
the UID overflow guard in `resolve_uid` (issue #317).

## `cri_uid_hardening::test_negative_uid_rejected`
**Requires root, requires alpine-rootfs.**
Passes `--user -1` to `pelagos run` and asserts rejection. Negative UIDs are not valid on
Linux; `u32::parse("-1")` fails and the string is not a valid `/etc/passwd` name, so the
error is caught in `resolve_uid`. Verifies defence against malformed input (issue #317).

## `cri_uid_hardening::test_valid_uid_boundary_accepted`
**Requires root, requires alpine-rootfs.**
Passes `--user 0` and then `--user 65534` (nobody) to `pelagos run` and asserts both
succeed. Verifies that the UID overflow guard does not mistakenly reject legitimate boundary
values (issue #317).

## `registry_mirror::test_mirror_fallback_to_origin_on_404`
**No root required.**
Binds a local TCP listener, writes a `registries.toml` pointing `docker.io` at it, and
verifies that `mirrors_for("docker.io")` returns the configured endpoint. The listener
serves a single HTTP 404 to simulate an unavailable mirror. Verifies that the mirror
config is loaded from `$PELAGOS_REGISTRIES` and that the fallback path is wired up
(issue #319).

## `registry_mirror::test_rewrite_reference_substitutes_host`
**No root required.**
Unit-style test: asserts that `rewrite_reference` replaces the origin registry host with
the mirror host:port for both http and https endpoints (issue #319).

## `registry_mirror::test_mirrors_for_no_config`
**No root required.**
Points `$PELAGOS_REGISTRIES` at a nonexistent path and asserts `mirrors_for` returns an
empty vec rather than panicking (issue #319).

## `registry_mirror::test_mirrors_for_reads_toml`
**No root required.**
Writes a real TOML file with two mirror endpoints for `docker.io` and asserts both are
returned in order (issue #319).

## `registry_mirror::test_mirrors_for_unknown_registry`
**No root required.**
Configures a mirror only for `docker.io`, then queries `ghcr.io`, and asserts the result
is empty — verifies per-registry scoping (issue #319).

## `dash_prefixed_args::test_dash_prefixed_arg_echo_n`
**Requires root and rootfs.**
Runs `echo -n hello` inside a container and asserts that `-n` is passed to `echo` as an
argument (suppressing the trailing newline) rather than consumed by clap as a pelagos flag.
Verifies the `trailing_var_arg` / `allow_hyphen_values` fix for issue #322 — without the
fix, clap rejects `-n` as an unrecognised flag and the run fails entirely.

## `dash_prefixed_args::test_dash_prefixed_signal_number`
**Requires root and rootfs.**
Runs `/bin/sh -c "kill -0 1"` inside a container. Signal number `-0` must be passed through
to the shell verbatim; PID 1 always exists in a container so `kill -0 1` exits 0. Verifies
that dash-prefixed numeric arguments (common in signal-handling scripts) are not mistaken for
pelagos CLI flags (issue #322 / #323).

## `dash_prefixed_args::test_oom_score_adj_negative`
**Requires root and rootfs.**
Runs a container with `--oom-score-adj -997` (the value k3s sets for klipper-lb pods) and
verifies the value is accepted by clap and applied correctly (readable via
`/proc/self/oom_score_adj`). Without `allow_hyphen_values = true` on the arg definition,
clap parses `-997` as short flag `-9` and rejects the run. This is the actual bug that
broke klipper-lb in v0.65.22 and was fixed in v0.65.23 (issue #323).

## `dash_prefixed_args::test_memory_swap_negative_one`
**Requires root and rootfs.**
Runs a container with `--memory-swap -1` (kernel sentinel meaning "unlimited swap") and
verifies the flag is accepted without clap treating `-1` as an unknown flag. Same
`allow_hyphen_values` fix as `--oom-score-adj` (issue #323).
