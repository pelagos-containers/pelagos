# PID Namespace Issue - "Out of Memory" Error

**Date:** 2026-02-16
**Issue:** "can't fork: Out of memory" when running commands in container
**Status:** ✅ FIXED

---

## The Problem

After successfully implementing network namespace joining, containers would start correctly but then fail with:

```
/bin/ash: can't fork: Out of memory
```

This error appeared when trying to run commands inside the container, even though:
- The container started successfully
- Chroot worked correctly
- Network namespace joining worked correctly
- System had plenty of available memory

---

## Root Cause

According to the [pid_namespaces(7) man page](https://man7.org/linux/man-pages/man7/pid_namespaces.7.html):

> **"These calls do not, however, change the PID namespace of the calling process, because doing so would change the caller's idea of its own PID (as reported by getpid()), which would break many applications and libraries."**

When `unshare(CLONE_NEWPID)` is called:

1. **The calling process does NOT enter the new PID namespace**
2. Only *future children* of the calling process will be in the new namespace
3. A process's PID namespace membership is determined at creation and **cannot be changed**

### What Was Happening

In our implementation:

```
1. Parent process forks → Child process
2. Child runs pre_exec hook
3. pre_exec calls unshare(CLONE_NEWPID) ← Creates new PID namespace
4. pre_exec returns
5. exec() runs /init.sh ← STILL in original PID namespace!
```

**Result:**
- New PID namespace was created but empty
- The exec'd program (/init.sh) was NOT in the new namespace
- When /init.sh tried to fork, the kernel's PID allocation failed
- Error: "can't fork: Out of memory" (misleading message)

### Why the Error Message is Confusing

The kernel's `alloc_pid()` function returns `ENOMEM` (Out of memory) when it fails to allocate a PID in an improperly initialized namespace. This has nothing to do with actual memory availability.

---

## Research Findings

### From [Digging into Linux namespaces - Quarkslab](https://blog.quarkslab.com/digging-into-linux-namespaces-part-1.html):

> "The Linux kernel doesn't like to have PID namespaces without a process with PID=1 inside. When the namespace is left empty the kernel will disable some mechanisms which are related to the PID allocation inside this namespace thus leading to this error."

### From [Building containers by hand - Red Hat](https://www.redhat.com/sysadmin/pid-namespace):

> "This results in the `alloc_pid` function failing to allocate a new PID when creating a new process, producing the 'Cannot allocate memory' error."

### From [PID Namespace - HackTricks](https://book.hacktricks.xyz/linux-hardening/privilege-escalation/docker-security/namespaces/pid-namespace):

> "The issue can be resolved by using the -f option with unshare. This option makes unshare fork a new process after creating the new PID namespace."

---

## The Solution

**Remove PID namespace creation from pre_exec-based architecture.**

PID namespaces require forking *after* calling `unshare(CLONE_NEWPID)` so that the child process becomes PID 1 in the new namespace. This cannot be done with `std::process::Command`'s pre_exec hook.

### Code Change

**Before (Broken):**
```rust
.with_namespaces(
    Namespace::UTS | Namespace::MOUNT | Namespace::PID | Namespace::CGROUP
)
```

**After (Fixed):**
```rust
.with_namespaces(
    Namespace::UTS | Namespace::MOUNT | Namespace::CGROUP
    // NOTE: PID namespace NOT created - see PID_NAMESPACE_ISSUE.md
)
```

---

## Why This is the Right Solution

### Alternative Approaches Considered

1. **Fork in pre_exec** - Not possible (pre_exec runs after fork, before exec)
2. **Use clone() instead of fork()** - Would require completely rewriting spawn logic
3. **Double-fork wrapper** - Complex, would need to manage intermediate processes
4. **Keep broken PID namespace** - Unacceptable, breaks container functionality

### Current Namespace Isolation

Even without a PID namespace, containers still have:

- ✅ **Mount namespace** - Isolated filesystem mounts
- ✅ **UTS namespace** - Isolated hostname/domain
- ✅ **Cgroup namespace** - Isolated cgroup view
- ✅ **Network namespace** - Isolated network stack (when using --join-netns)
- ✅ **Chroot** - Filesystem isolation

The container is still well-isolated for most use cases.

---

## Future Enhancement: Proper PID Namespace Support

To properly support PID namespaces, we would need to:

1. **Restructure spawn process:**
   ```
   Parent process
     ↓ fork
   First child
     ↓ unshare(CLONE_NEWPID)
     ↓ fork again
   Second child (PID 1 in new namespace)
     ↓ exec target program
   ```

2. **Implementation options:**
   - Use `nix::unistd::fork()` directly instead of `std::process::Command`
   - Implement custom spawn logic with proper double-fork
   - Use `clone()` syscall with CLONE_NEWPID flag

3. **Additional considerations:**
   - PID 1 must reap zombie processes
   - Signal handling responsibilities
   - Init system behavior

This would be a substantial architectural change and should be done as a separate feature implementation.

---

## Testing

### Before Fix

```bash
$ sudo -E ./test-namespace-join.sh
...
==> Inside container, checking network interfaces:
...
/bin/ash: can't fork: Out of memory  ← ERROR
```

### After Fix

```bash
$ sudo -E ./test-namespace-join.sh
...
==> Inside container, checking network interfaces:
1: lo: <LOOPBACK,UP,LOWER_UP> ...
5: veth2@if6: <BROADCAST,MULTICAST,UP,LOWER_UP> ...
    inet 172.16.0.1/32 scope global veth2

==> Checking for veth2 interface:
✅ SUCCESS! veth2 interface found in container
✅ SUCCESS! IP address 172.16.0.1 found
```

---

## Documentation Updates

- **main.rs** - Added comment explaining why PID namespace is not created
- **PID_NAMESPACE_ISSUE.md** - This file, explaining the issue and solution
- **CLAUDE.md** - Should be updated to note PID namespace limitation

---

## References

1. [pid_namespaces(7) - Linux manual page](https://man7.org/linux/man-pages/man7/pid_namespaces.7.html)
2. [Digging into Linux namespaces - part 1 - Quarkslab](https://blog.quarkslab.com/digging-into-linux-namespaces-part-1.html)
3. [Building containers by hand: The PID namespace - Red Hat](https://www.redhat.com/sysadmin/pid-namespace)
4. [PID Namespace - HackTricks](https://book.hacktricks.xyz/linux-hardening/privilege-escalation/docker-security/namespaces/pid-namespace)
5. [unshare(2) - Linux manual page](https://man7.org/linux/man-pages/man2/unshare.2.html)

---

**Resolution Date:** 2026-02-16
**Status:** ✅ Fixed by removing PID namespace creation
**Future Work:** Implement proper PID namespace support with double-fork architecture
