# Dependency Replacement Analysis: unshare → Modern Alternatives

## Executive Summary

The `unshare` crate (v0.7.0, last updated June 2021) provides critical container functionality but uses outdated Rust patterns. For a learning project focused on modern, idiomatic Rust, **I recommend Option 3: DIY with nix crate** as the best balance of modernity, learning value, and maintainability.

---

## Current unshare Usage in remora

From `src/main.rs:9`, remora imports:
```rust
use unshare::{Child, Command, Error, GidMap, Stdio, UidMap};
```

**Actual usage:**
- `Command::new()` - Process builder with namespace support
- `.unshare([Namespace::Uts, Mount, Pid, Cgroup])` - Create namespaces
- `.chroot_dir()` - Set chroot directory
- `.pre_exec()` - Pre-exec callback for proc mounting
- `.spawn()` / `.wait()` - Process lifecycle
- `Stdio::inherit()` - Stdio redirection

**Complexity:** ~2,748 lines of code across 20 files in the unshare library

---

## Option 1: Fork & Modernize unshare

### Overview
Create a maintained fork of unshare with modern Rust patterns.

### Modernity: ⭐⭐⭐⭐ (Good)
- Update to Rust 2021 edition
- Remove deprecated patterns (unnecessary parens, missing ABI specs)
- Update to nix 0.31.1 (currently uses 0.20.0)
- Modern error handling (thiserror/anyhow)

### Cleanliness/Idiomatic: ⭐⭐⭐ (Fair)
- Still inherits architectural decisions from 2016-era Rust
- Complex internal state machine for process spawning
- Mixed abstraction levels (some direct syscalls, some via nix)
- Builder pattern is good but implementation has historical baggage

### Complexity: ⭐⭐ (High)
- Must understand 2,748 lines of existing code
- Complex unsafe code for fork/exec/clone
- Intricate file descriptor handling and pipe management
- Must maintain compatibility or refactor existing code

### Depth: Shallow (wraps existing code)
- Not learning much new—just maintenance work
- Doesn't teach modern container patterns
- Time spent on legacy code comprehension vs. learning

### Verdict: ❌ **Not Recommended**
Too much effort maintaining old code for a learning project. You'd spend time understanding historical decisions rather than learning modern approaches.

---

## Option 2: Use libcontainer (youki)

### Overview
Adopt the libcontainer crate from the [youki project](https://github.com/youki-dev/youki), a modern OCI-compliant container runtime.

### Modernity: ⭐⭐⭐⭐⭐ (Excellent)
- Actively maintained (latest: v0.5.7, requires Rust 1.85.0+)
- Modern Rust patterns throughout
- Part of Linux Foundation project
- Built for 2020s container standards (OCI runtime-spec)

### Cleanliness/Idiomatic: ⭐⭐⭐⭐⭐ (Excellent)
- Clean separation of concerns (15 modules)
- Idiomatic Rust 2021 patterns
- Well-documented API surface
- Syscall abstraction layer for testability

### Complexity: ⭐ (Very High)
- **Heavy dependency** - brings in entire container ecosystem:
  - libcgroups (cgroup v1/v2 management)
  - libseccomp (syscall filtering)
  - OCI spec parsing
  - apparmor, capabilities, rootfs management
- Mid-level API—more than you need for simple namespace experiments
- Steep learning curve to understand all abstractions
- 10-20x more code than you need

### Depth: Very Shallow (uses high-level library)
- Hides most interesting details behind abstractions
- Less learning about how Linux namespaces actually work
- Focuses on OCI compliance, not namespace fundamentals
- Good for production, not for learning internals

### Verdict: ⚠️ **Overkill for Learning**
Excellent for production systems, but brings far more functionality than needed. Like using a freight truck to learn how cars work—technically better, but obscures fundamentals.

---

## Option 3: DIY with nix Crate (RECOMMENDED)

### Overview
Implement container spawning directly using the [nix crate](https://docs.rs/nix/latest/nix/) (v0.31.1) for clean syscall wrappers.

### Modernity: ⭐⭐⭐⭐⭐ (Excellent)
- nix v0.31.1 is actively maintained (2025)
- Modern, type-safe Rust syscall wrappers
- Idiomatic error handling with nix::Result
- Up-to-date with latest Linux kernel features

### Cleanliness/Idiomatic: ⭐⭐⭐⭐⭐ (Excellent)
- Direct, simple mapping to Linux syscalls
- No unnecessary abstraction layers
- You control the architecture—make it clean from day 1
- Modern Rust patterns throughout

### Complexity: ⭐⭐⭐⭐ (Low-Medium)
**What you'd need to implement (~300-500 lines):**

1. **Namespace creation** (50 lines)
   ```rust
   use nix::sched::{unshare, CloneFlags};

   unshare(
       CloneFlags::CLONE_NEWNS |
       CloneFlags::CLONE_NEWPID |
       CloneFlags::CLONE_NEWUTS |
       CloneFlags::CLONE_NEWCGROUP
   )?;
   ```

2. **Process spawning** (100 lines)
   - Use `nix::unistd::fork()` for process creation
   - Or use `std::process::Command` with pre_exec for simpler approach
   - Handle pid, stdio inheritance

3. **Chroot setup** (50 lines)
   - `nix::unistd::chroot()` - already available
   - Directory validation and setup

4. **Mount operations** (100 lines)
   - Already have this in remora (mount_proc, mount_sys)
   - Use `nix::mount::mount()` for proc/sys/dev

5. **Error handling** (50 lines)
   - Define custom error type wrapping nix::Error
   - Context for which operation failed

6. **Builder pattern** (150 lines)
   - Clean, modern builder API
   - Less complex than unshare's version

**The nix crate already provides:**
- ✅ `nix::sched::unshare()` - Create namespaces
- ✅ `nix::sched::setns()` - Join existing namespaces
- ✅ `nix::sched::CloneFlags` - All CLONE_NEW* constants
- ✅ `nix::unistd::fork()` - Process forking
- ✅ `nix::unistd::chroot()` - Chroot operations
- ✅ `nix::mount::mount()` - Filesystem mounting
- ✅ Type-safe wrappers with proper error handling

### Depth: ⭐⭐⭐⭐⭐ Deep (Maximum Learning)
- **Understand every detail** of namespace creation
- Learn fork/exec model intimately
- Master Linux process lifecycle
- Control security and isolation precisely
- Build from fundamentals up

### Code Comparison

**Current (unshare):**
```rust
let mut cmd = Command::new(to_run);
cmd.unshare([Namespace::Uts, Namespace::Mount, Namespace::Pid].iter())
   .chroot_dir(curdir)
   .pre_exec(&mount_proc)
   .spawn()?
```

**Proposed (nix + DIY):**
```rust
let mut cmd = ContainerCommand::new(to_run);
cmd.with_namespaces(Namespaces::UTS | Namespaces::MOUNT | Namespaces::PID)
   .with_chroot(curdir)
   .pre_exec(mount_proc)
   .spawn()?
```

Same level of abstraction, but you wrote it and understand it completely.

### Verdict: ✅ **STRONGLY RECOMMENDED**
Perfect for learning. Clean, modern, maintainable, and educational.

---

## Option 4: Pure Syscalls (libc)

### Overview
Use raw `libc` bindings for maximum control.

### Modernity: ⭐⭐ (Poor)
- Raw C bindings—no Rust idioms
- Unsafe everywhere
- No type safety

### Cleanliness/Idiomatic: ⭐ (Poor)
- Violates Rust best practices
- Extensive unsafe blocks
- Manual error handling from errno
- Pointer arithmetic and C string handling

### Complexity: ⭐ (Very High)
- Must handle all edge cases manually
- Complex error handling
- Memory safety is your responsibility
- 2-3x more code than nix approach

### Depth: ⭐⭐⭐⭐⭐ (Maximum)
- Learn Linux APIs at lowest level
- Complete understanding of syscalls

### Verdict: ❌ **Not Recommended**
Too much unsafe code, violates Rust principles. The nix crate provides the same learning without sacrificing safety.

---

## Recommendation: Option 3 (DIY with nix)

### Why This Is Best for Your Learning Project

1. **Modern & Clean** ✅
   - nix crate v0.31.1 (actively maintained, 2025)
   - Type-safe, idiomatic Rust
   - No deprecated patterns or warnings

2. **Right Level of Complexity** ✅
   - Not trivial (learn real skills)
   - Not overwhelming (manageable scope)
   - 300-500 lines vs 2,748 lines
   - Focused on what YOU need

3. **Deep Learning** ✅
   - Understand namespace syscalls intimately
   - Master fork/exec/wait lifecycle
   - Learn proper error handling patterns
   - Control security and isolation

4. **Maintainable** ✅
   - You wrote it—you understand it
   - Small codebase (easy to modify)
   - Well-documented nix crate as foundation
   - Can extend as you learn more

5. **Production-Ready Path** ✅
   - Start with simple implementation
   - Refine as you learn
   - Can add features incrementally:
     - UID/GID mapping (next phase)
     - Seccomp filters (advanced)
     - Cgroup limits (resource control)
   - Easy to understand for future maintenance

### Implementation Strategy

**Phase 1: Basic Namespace Support (~150 lines)**
- Replace unshare::Command with simple wrapper around std::process::Command
- Use nix::sched::unshare() in pre_exec callback
- Keep existing mount logic

**Phase 2: Clean Builder Pattern (~200 lines)**
- Create ergonomic ContainerCommand builder
- Modern error types (thiserror)
- Proper namespace abstraction

**Phase 3: Advanced Features (~150 lines)**
- UID/GID mapping via /proc/self/uid_map
- Namespace joining with setns()
- Enhanced stdio handling

**Total: ~500 lines of clean, modern, well-understood code**

---

## Migration Effort Comparison

| Option | Lines to Write | Lines to Understand | Learning Value | Maintenance |
|--------|----------------|---------------------|----------------|-------------|
| Fork unshare | ~200 | ~2,748 | Low | High |
| libcontainer | ~100 | ~5,000+ | Low | Low |
| **DIY nix** | **~500** | **~0 (docs)** | **High** | **Low** |
| Pure libc | ~800 | ~0 | High | High |

---

## Conclusion

For a learning project emphasizing modern, idiomatic Rust:

🏆 **Build your own with nix crate**

You'll learn more, write cleaner code, and have a maintainable foundation you fully understand. The implementation is straightforward, well-scoped, and teaches you exactly how Linux namespaces work without the baggage of maintaining legacy code or the opacity of heavyweight abstractions.

Plus, when someone asks "How do you create containers in Linux?", you can answer from first principles because you built it yourself.

---

## References

- [nix crate sched module](https://docs.rs/nix/latest/nix/sched/index.html) - Namespace functions
- [GitHub - youki-dev/youki](https://github.com/youki-dev/youki) - Modern container runtime
- [libcontainer crate](https://docs.rs/libcontainer/latest/libcontainer/) - youki's core library
- [GitHub - tailhook/unshare](https://github.com/tailhook/unshare) - Original unmaintained crate
- [Linux man pages](https://man7.org/linux/man-pages/man2/unshare.2.html) - unshare(2), setns(2)
