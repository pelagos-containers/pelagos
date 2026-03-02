# Ongoing Tasks

## Active: Epic #29 — OCI linux.resources → cgroup controller wiring (2026-03-01)

PR #38: https://github.com/skeptomai/remora/pull/38 (open, pending merge)

### Context

Epic: https://github.com/skeptomai/remora/issues/29

**Critical discovery:** `opencontainers/runtime-tools` v0.9.0's `runtimetest` binary has
stub cgroupv2 implementations — every cgroup validation function returns
`"cgroupv2 is not supported yet"`. This system is cgroupv2-only. Therefore
**all linux_cgroups_* runtime-tools tests cannot pass on this system regardless of remora
behavior**. Conformance is verified instead via remora's own integration test suite.

### Sub-issues (created under epic #29)

| Issue | Feature | Status |
|-------|---------|--------|
| #30 | `--pid-file` on `remora create` | closed (already implemented; pidfile.t race is watcher design issue #37) |
| #31 | Extended memory resources (swap, reservation, swappiness) | ✅ closed — PR #38 |
| #32 | CPU cpuset resources (cpus, mems) | ✅ closed — PR #38 |
| #33 | Block I/O resources (weight + throttle) | ✅ closed — PR #38 |
| #34 | Device cgroup (v1 allow/deny, graceful v2 skip) | ✅ closed — PR #38 |
| #35 | Network cgroup (net_cls classID + net_prio priorities, v1 only) | ✅ closed — PR #38 |
| #36 | Document runtime-tools cgroupv2 limitation | ✅ closed — documented in ONGOING_TASKS.md |
| #37 | Watcher zombie-keeper design for pidfile.t | open |

### Architecture

All resource wiring follows the same three-layer pattern:

```
OCI config.json (oci.rs structs)
    → build_command() maps fields → CgroupConfig (cgroup.rs)
        → setup_cgroup() applies via cgroups-rs builder
```

**cgroups-rs v2 support status:**
- Memory (swap, reservation, swappiness): ✅ supported (memory.swap.max, memory.low, memory.swappiness)
- CPU cpuset (cpus, mems): ✅ supported (cpuset.cpus, cpuset.mems)
- Block I/O (weight, throttle): ✅ supported via io.max on v2
- Device cgroup: ❌ v1-only in cgroups-rs (uses devices.allow/deny); graceful skip on v2
- Network (net_cls, net_prio): ❌ v1-only controllers; graceful skip on v2
- Hugepages: ✅ hugetlb controller exists in cgroups-rs (deferred to later)

### Implementation plan per sub-issue

#### Issue #38: --pid-file
- `src/main.rs`: add `--pid-file <path>` Option<String> to OciCreateArgs
- In `cmd_create()`: after state is written, if pid-file is set, write `state.pid\n` to path
- Integration test: create with --pid-file, verify file contents match state.pid
- runtime-tools test: `pidfile.t` (1 test)

#### Issue #39: Extended memory resources
**OCI fields:** memory.swap, memory.reservation, memory.swappiness, memory.disableOomKiller
- `src/oci.rs` OciMemoryResources: add swap, reservation, swappiness, disable_oom_killer fields
- `src/cgroup.rs` CgroupConfig: add memory_swap, memory_reservation, memory_swappiness
- `src/cgroup.rs` setup_cgroup(): wire via memory builder `.memory_swap_limit()`, `.memory_soft_limit()`, `.swappiness()`
- `src/cgroup_rootless.rs`: add memory.swap.max, memory.low writes
- `src/oci.rs` build_command(): map new fields
- Integration test: OCI bundle with memory.swap set, verify cgroup memory.swap.max

#### Issue #40: CPU cpuset resources
**OCI fields:** cpu.cpus (cpuset string), cpu.mems
- `src/oci.rs` OciCpuResources: add cpus, mems fields
- `src/cgroup.rs` CgroupConfig: add cpuset_cpus, cpuset_mems
- `src/cgroup.rs` setup_cgroup(): post-build, use `cg.controller_of::<CpuSetController>()` to call set_cpus/set_mems
- Note: CpuSetController not in CgroupBuilder directly, must use controller_of after build
- `src/oci.rs` build_command(): map cpus/mems fields
- Integration test: OCI bundle with cpu.cpus="0", verify container stays on CPU 0

#### Issue #41: Block I/O resources
**OCI fields:** blockIO.weight, blockIO.throttleReadBpsDevice, throttleWriteBpsDevice,
                throttleReadIOPSDevice, throttleWriteIOPSDevice
- Add OciBlockIOResources + OciThrottleDevice structs to oci.rs
- Add to OciResources: block_io field
- `src/cgroup.rs` CgroupConfig: add blkio_weight, blkio_throttle_read_bps/write_bps/read_iops/write_iops
- `src/cgroup.rs` setup_cgroup(): wire via blkio builder
- `src/oci.rs` build_command(): map blockIO fields
- Integration test: OCI bundle with blockIO.weight set, verify cgroup created without error

#### Issue #42: Device cgroup
**OCI fields:** linux.resources.devices (array of {allow, type, major, minor, access})
- Note: these are device cgroup rules, distinct from linux.devices (actual device nodes)
- Add OciDeviceCgroupResource struct
- Add to OciResources: devices field
- `src/cgroup.rs` CgroupConfig: add device_rules: Vec<CgroupDeviceRule>
- `src/cgroup.rs` setup_cgroup(): use DeviceResourceBuilder.device(); graceful skip on v2
- `src/oci.rs` build_command(): map device cgroup rules
- Integration test: OCI bundle with device allow/deny, verify no error on v2 (graceful skip)

#### Issue #43: Network cgroup
**OCI fields:** network.classID, network.priorities (array of {name, priority})
- Add OciNetworkResources struct
- Add to OciResources: network field
- `src/cgroup.rs` CgroupConfig: add net_classid, net_priorities
- `src/cgroup.rs` setup_cgroup(): use NetworkResourceBuilder; graceful skip if controller unavailable
- `src/oci.rs` build_command(): map network fields
- Integration test: OCI bundle with network.classID set, verify graceful skip on v2

#### Issue #44: Runtime-tools cgroupv2 doc + conformance strategy
- Update `docs/INTEGRATION_TESTS.md` with cgroup conformance notes
- Update epic #29 and issue #25 comments
- Mark linux_cgroups_* tests as "blocked by upstream runtime-tools cgroupv2 support gap"

### Test infrastructure

All new integration tests follow the pattern in `tests/integration_tests.rs` `oci_runtime` mod:
- Build OCI bundle (config.json + Alpine rootfs symlink) in tempdir
- `cmd_create()` + `cmd_start()` + verify state + `cmd_kill()` + `cmd_delete()`
- Skip if not root or no alpine-rootfs

### Files to change

| File | Changes |
|------|---------|
| `src/oci.rs` | Extend OCI resource structs; extend build_command() resource wiring |
| `src/cgroup.rs` | Extend CgroupConfig; extend setup_cgroup() |
| `src/cgroup_rootless.rs` | Extend with memory.swap.max, memory.low |
| `src/main.rs` | Add --pid-file to OciCreateArgs |
| `tests/integration_tests.rs` | New OCI resource integration tests |
| `docs/INTEGRATION_TESTS.md` | Document new tests |

---

## Completed: OCI killsig conformance (2026-03-01)

Three bugs fixed:
1. Incomplete seccomp syscall table (rt_sigsuspend + 139 others missing)
2. No device nodes in /dev tmpfs (OCI spec §5.3 requires runtime to create them)
3. MS_NODEV hardcoded for all tmpfs mounts including /dev

GitHub issue: #27 (closed). PR: #28.

---

## Completed: remora exec PID namespace join (2026-02-28)

GitHub issue: #1 (closed).

---

## Completed: watcher subreaper (2026-02-28)

See git log for details.

---

## Completed: OCI console-socket PTY fd passthrough (2026-02-28)

GitHub issue: #24. PR: #26 (merged).
