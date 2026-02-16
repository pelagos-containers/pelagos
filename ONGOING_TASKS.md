# Remora: Container Runtime Development Roadmap

**Last Updated:** 2026-02-16
**Current Status:** Phase 2 Complete, Phase 3 Ready

---

## Phase Progress

- ✅ **Phase 1:** Minimal Viable Replacement (Complete)
- ✅ **Phase 2:** Clean, Modern API (Complete)
- 🔄 **Phase 3:** Advanced Features (Ready to start)

---

## ✅ Phase 1: Minimal Viable Replacement (COMPLETE)

**Goal:** Replace unmaintained `unshare` crate with modern `nix`-based implementation.

**Status:** ✅ Complete (2026-02-16)

### Completed Tasks
- ✅ Created `src/container.rs` module
- ✅ Implemented `Namespace`, `Stdio`, `Command`, `Child`, `Error` types
- ✅ Updated `Cargo.toml` (removed unshare, updated nix to 0.31.1)
- ✅ Updated `src/main.rs` to use new container module
- ✅ Fixed Alpine rootfs architecture (ARM → x86_64)
- ✅ Created launch scripts and documentation
- ✅ Automatic cleanup for /sys and /proc mounts

### Results
- 🎯 ~300 lines of clean, modern code (vs 2,748 lines unmaintained)
- 🎯 Zero compiler warnings from our code
- 🎯 Container launches successfully
- 🎯 x86_64 Alpine 3.23.3 rootfs working

---

## ✅ Phase 2: Clean, Modern API (COMPLETE)

**Goal:** Refactor to idiomatic Rust 2021 with excellent ergonomics.

**Status:** ✅ Complete (2026-02-16)

### Completed Tasks
- ✅ Enhanced error handling with `thiserror`
  - Context-rich error messages
  - Error source chaining
  - Automatic Display/Error trait impls
- ✅ Consuming builder pattern (`self` instead of `&mut self`)
  - Modern `with_*` method naming
  - Fluent method chaining
  - Deprecated old API gracefully
- ✅ Bitflags for namespace combinations
  - Ergonomic `Namespace::UTS | Namespace::PID` syntax
  - Set operations (union, intersection, difference)
  - Efficient u32 representation
- ✅ Comprehensive documentation
  - Module-level overview
  - Code examples throughout
  - Safety documentation
  - ~200 lines of docs
- ✅ Unit tests
  - 10 tests passing
  - 2 integration tests (require root)
  - Namespace, builder, error coverage
- ✅ Code organization
  - Created `src/lib.rs` for library use
  - Well-structured module layout

### Results
- 🎯 Idiomatic Rust 2021 patterns
- 🎯 10 unit tests passing
- 🎯 Comprehensive documentation
- 🎯 Backward compatible
- 🎯 Production-ready API

---

## 🔄 Phase 3: Advanced Features (READY TO START)

**Goal:** Add advanced container features and fix remaining issues.

**Estimated Time:** 4-6 hours
**Priority:** High priority items marked with ⭐

### Critical Fixes ⭐

#### 1. Fix Mount Propagation Issue ⭐⭐⭐ (CRITICAL)

**Problem:** `/proc` mounts are leaking from container to parent namespace.

**Current Status:**
```bash
$ mount | grep alpine-rootfs
proc on /home/cb/Projects/remora/alpine-rootfs/proc type proc (rw,relatime)
proc on /home/cb/Projects/remora/alpine-rootfs/proc type proc (rw,relatime)
```

**Root Cause:**
- Mount namespace not fully isolated
- Mounts are shared between parent and child
- Need `MS_PRIVATE` or `MS_SLAVE` propagation flags

**Solution:**
```rust
// In container.rs pre_exec callback, before mounting proc:

// Make all mounts private (don't propagate)
unsafe {
    libc::mount(
        ptr::null(),
        c"/".as_ptr(),
        ptr::null(),
        libc::MS_REC | libc::MS_PRIVATE,
        ptr::null(),
    );
}

// Now mount proc - won't leak to parent
mount_proc()?;
```

**Tasks:**
- [ ] Add `MS_PRIVATE` mount propagation in pre_exec
- [ ] Test that mounts don't leak
- [ ] Update cleanup code to handle multiple mounts
- [ ] Document mount propagation behavior

**Success Criteria:**
- ✅ `mount | grep alpine-rootfs` shows nothing after container exits
- ✅ No manual cleanup needed
- ✅ Works on abnormal exits (kill -9)

---

### Feature Additions

#### 2. UID/GID Mapping (Phase 3a)

**Goal:** Enable unprivileged containers with user namespace mapping.

**Current Code:** Lines 76-93 in main.rs are commented out
```rust
/* .set_id_maps(
    vec![UidMap { inside_uid: 0, outside_uid: 1000, count: 1 }],
    vec![GidMap { inside_gid: 0, outside_gid: 1000, count: 1 }],
)
.uid(0)
.gid(0); */
```

**Tasks:**
- [ ] Define `UidMap` and `GidMap` structs in container.rs
- [ ] Implement `with_uid_maps()` and `with_gid_maps()` methods
- [ ] Write to `/proc/self/uid_map` and `/proc/self/gid_map` in pre_exec
- [ ] Handle `setgroups` deny for unprivileged containers
- [ ] Add `with_uid()` and `with_gid()` methods
- [ ] Test with unprivileged user
- [ ] Document security implications

**API Design:**
```rust
let child = Command::new("/bin/sh")
    .with_namespaces(Namespace::USER | Namespace::PID)
    .with_uid_maps(&[UidMap { inside: 0, outside: 1000, count: 1 }])
    .with_gid_maps(&[GidMap { inside: 0, outside: 1000, count: 1 }])
    .with_uid(0)  // Inside container
    .with_gid(0)  // Inside container
    .spawn()?;
```

**Success Criteria:**
- ✅ Can run containers as unprivileged user
- ✅ Inside container, user appears as root (uid 0)
- ✅ Outside container, process runs as uid 1000
- ✅ File ownership mapping works correctly

---

#### 3. Namespace Joining with setns() (Phase 3b)

**Goal:** Join existing namespaces instead of creating new ones.

**Current Code:** Lines 91-100 in main.rs are commented out
```rust
/* match cmd.set_namespace(&nsf, Namespace::NET){
    Ok(c) => {info!("set network namespace in {:?}",c);},
    Err(e) => {warn!("failed to set namespace {:?}", e);}
}; */
```

**Tasks:**
- [ ] Add `with_namespace_fd(File, Namespace)` method
- [ ] Call `nix::sched::setns(fd, flags)` in pre_exec
- [ ] Support joining multiple namespace types
- [ ] Handle namespace file descriptors properly
- [ ] Test with network namespace (`/var/run/netns/con`)
- [ ] Document namespace joining workflow

**API Design:**
```rust
let netns = File::open("/var/run/netns/con")?;

let child = Command::new("/bin/sh")
    .with_namespace_fd(&netns, Namespace::NET)
    .spawn()?;
```

**Success Criteria:**
- ✅ Can join existing network namespace
- ✅ Container has network connectivity
- ✅ Can join multiple namespaces simultaneously

---

#### 4. Enhanced Mount Support (Phase 3c)

**Goal:** Better mount operations with pivot_root support.

**Current State:**
- Using `chroot` (simple but less secure)
- Manual mount operations in main.rs
- No mount propagation control

**Tasks:**
- [ ] Add `with_pivot_root(new_root, put_old)` method
- [ ] Implement pivot_root syscall wrapper
- [ ] Add mount propagation flags (MS_PRIVATE, MS_SLAVE, MS_SHARED)
- [ ] Move mount operations into container module
- [ ] Add helper methods: `with_proc_mount()`, `with_sys_mount()`, `with_dev_mount()`
- [ ] Support bind mounts
- [ ] Document mount vs pivot_root differences

**API Design:**
```rust
let child = Command::new("/bin/sh")
    .with_pivot_root("/path/to/rootfs", "/path/to/rootfs/old_root")
    .with_mount_propagation(MountFlags::MS_PRIVATE)
    .with_proc_mount()  // Helper: automatically mount /proc
    .with_sys_mount()   // Helper: automatically mount /sys
    .spawn()?;
```

**Success Criteria:**
- ✅ pivot_root works correctly
- ✅ No mount leaks (verified by Phase 3.1 fix)
- ✅ Helper methods make common operations easy

---

#### 5. Capability Management (Phase 3d)

**Goal:** Drop unnecessary capabilities for security.

**Tasks:**
- [ ] Add `with_capabilities(caps)` method
- [ ] Use `nix::sys::prctl` for capability operations
- [ ] Support dropping all capabilities except specified
- [ ] Add common capability sets (network, filesystem, etc.)
- [ ] Test capability restrictions
- [ ] Document security best practices

**API Design:**
```rust
let child = Command::new("/bin/sh")
    .with_capabilities(Capability::NET_BIND_SERVICE | Capability::CHOWN)
    .spawn()?;
```

---

#### 6. Resource Limits (Phase 3e)

**Goal:** Control container resource usage.

**Tasks:**
- [ ] Add `with_rlimit(resource, soft, hard)` method
- [ ] Use `nix::sys::resource::setrlimit`
- [ ] Support common limits (CPU, memory, file descriptors)
- [ ] Add convenience methods for common scenarios
- [ ] Test resource enforcement
- [ ] Document limit types

**API Design:**
```rust
let child = Command::new("/bin/sh")
    .with_memory_limit(512_000_000)  // 512 MB
    .with_cpu_limit(50)               // 50% of one core
    .with_max_fds(1024)
    .spawn()?;
```

---

#### 7. Better Process Management (Phase 3f)

**Goal:** Improve container lifecycle management.

**Tasks:**
- [ ] Add `Child::kill(signal)` method
- [ ] Add `Child::wait_timeout(duration)` method
- [ ] Support graceful shutdown (SIGTERM → wait → SIGKILL)
- [ ] Handle zombie reaping (PID 1 duties)
- [ ] Add process group management
- [ ] Document signal handling

**API Design:**
```rust
let mut child = Command::new("/bin/sh").spawn()?;

// Try graceful shutdown
child.kill(Signal::SIGTERM)?;
if let Ok(status) = child.wait_timeout(Duration::from_secs(5)) {
    println!("Exited gracefully");
} else {
    child.kill(Signal::SIGKILL)?;
    child.wait()?;
}
```

---

### Integration Tests (Phase 3g)

**Goal:** Comprehensive integration test suite.

**Tasks:**
- [ ] Test namespace isolation
  - [ ] PID namespace (verify PID 1 inside)
  - [ ] Mount namespace (private mounts)
  - [ ] UTS namespace (hostname isolation)
  - [ ] Network namespace (connectivity)
- [ ] Test chroot/pivot_root isolation
- [ ] Test UID/GID mapping
- [ ] Test capability dropping
- [ ] Test resource limits
- [ ] Test cleanup on abnormal exits
- [ ] Set up CI for automated testing

**Test Structure:**
```rust
#[test]
#[ignore] // Requires root/CAP_SYS_ADMIN
fn test_pid_namespace_isolation() {
    // Spawn container with PID namespace
    // Inside: check if PID is 1
    // Inside: check if can only see own processes
    assert!(/* validation */);
}
```

---

## Phase 3 Priorities

### Must-Have (P0)
1. ⭐⭐⭐ Fix mount propagation issue
2. ⭐⭐ UID/GID mapping
3. ⭐⭐ Namespace joining (setns)

### Should-Have (P1)
4. ⭐ Enhanced mount support (pivot_root)
5. ⭐ Integration test suite
6. Capability management

### Nice-to-Have (P2)
7. Resource limits
8. Better process management

---

## Implementation Plan

### Week 1: Critical Fixes
- Day 1: Fix mount propagation (2-3 hours)
- Day 2: UID/GID mapping (3-4 hours)
- Day 3: Namespace joining (2-3 hours)
- Day 4: Testing and bug fixes

### Week 2: Advanced Features
- Day 5: Enhanced mount support (3-4 hours)
- Day 6: Capability management (2-3 hours)
- Day 7: Integration tests (3-4 hours)

### Optional: Week 3
- Resource limits
- Process management improvements
- CI/CD setup
- Performance optimizations

---

## Testing Strategy

### Per-Feature Testing

**Mount Propagation Fix:**
```bash
# Before fix: mounts leak
./launch-container.sh
mount | grep alpine-rootfs  # Shows duplicate proc mounts

# After fix: clean
./launch-container.sh
mount | grep alpine-rootfs  # Shows nothing
```

**UID/GID Mapping:**
```bash
# Run as unprivileged user (no sudo)
./test-unprivileged.sh
# Inside: whoami → root
# Outside: ps aux | grep remora → shows uid 1000
```

**Namespace Joining:**
```bash
# Create network namespace
sudo ./setup.sh

# Join it
sudo -E ./target/debug/remora --join-netns con ...
# Inside: ip addr  # Shows veth2 with 172.16.0.1
```

---

## Success Metrics

### Phase 3 Completion Criteria

**Must pass all:**
- ✅ No mount leaks after container exit
- ✅ UID/GID mapping works for unprivileged containers
- ✅ Can join existing network namespace
- ✅ All integration tests pass
- ✅ Zero clippy warnings
- ✅ Documentation updated
- ✅ Example code in docs works

**Quality metrics:**
- ✅ Code coverage >80%
- ✅ All public APIs documented
- ✅ Performance: container startup <100ms
- ✅ Memory: overhead <10MB

---

## Known Issues to Address

### High Priority
1. ⚠️ **Mount propagation leak** - Proc mounts escape container
2. ⚠️ **Sys unmounting** - Sometimes fails (from TODO.org line 3)
3. ⚠️ **Cgroups not working** - From TODO.org line 4

### Medium Priority
4. 📝 Commented UID/GID code needs implementation
5. 📝 Commented netns joining needs implementation
6. 📝 No integration tests yet

### Low Priority
7. 📋 nom v2.2.1 future incompatibility (dependency)
8. 📋 Minor clippy warnings in tests

---

## Documentation Deliverables

### Phase 3 Docs to Create
- [ ] `PHASE3_COMPLETE.md` - Summary of Phase 3
- [ ] `MOUNT_PROPAGATION.md` - Deep dive on mount issues
- [ ] `SECURITY.md` - Security best practices
- [ ] `EXAMPLES.md` - Real-world usage examples
- [ ] Update `CLAUDE.md` with final architecture
- [ ] Update `README.md` with complete API reference

---

## Future Enhancements (Post-Phase 3)

**Container Orchestration:**
- Multiple container management
- Container lifecycle (start/stop/restart/pause)
- Container naming and identification
- Port mapping and networking

**OCI Compliance:**
- Parse OCI config.json
- Implement OCI runtime operations
- OCI image support

**Advanced Features:**
- Cgroup v2 support
- Seccomp filtering
- AppArmor/SELinux profiles
- Overlay filesystems
- Container snapshots

**Tooling:**
- CLI improvements (better flags, config files)
- Logging and debugging tools
- Performance profiling
- Container inspection

---

## Timeline Estimate

| Phase | Estimated Time | Actual Time |
|-------|----------------|-------------|
| Phase 1 | 2-3 hours | ✅ 3 hours |
| Phase 2 | 2-3 hours | ✅ 2.5 hours |
| Phase 3 | 4-6 hours | 🔄 TBD |
| **Total** | **8-12 hours** | **5.5 hours + Phase 3** |

---

## Current Status Summary

✅ **Completed:**
- Modern, idiomatic Rust implementation
- Zero unmaintained dependencies
- Comprehensive documentation
- Unit test suite
- Production-ready API

⚠️ **Known Issues:**
- Mount propagation leak (needs fixing)
- Sys unmounting occasional failures
- Cgroups not implemented yet

🔄 **Next Steps:**
1. Fix mount propagation (CRITICAL)
2. Implement UID/GID mapping
3. Implement namespace joining
4. Build integration test suite

---

**Last Updated:** 2026-02-16
**Status:** Phase 2 Complete, Ready for Phase 3
**Next Priority:** Fix mount propagation issue
