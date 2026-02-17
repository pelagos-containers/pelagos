# Namespace Joining Issue and Resolution

**Date:** 2026-02-16
**Issue:** Network namespace joining with setns() causes filesystem paths to become inaccessible
**Status:** ✅ RESOLVED

---

## The Problem

When implementing `setns()` support to join existing network namespaces, we encountered a critical issue where filesystem paths became inaccessible after joining the namespace.

### Symptoms

- After calling `setns()` to join a network namespace created by `ip netns add con`
- Then calling `unshare(CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWUTS | CLONE_NEWCGROUP)`
- The path `/home/cb/Projects/remora/alpine-rootfs` became inaccessible
- Specifically: `/` existed, `/home` existed, but `/home/cb` did NOT exist (ENOENT)
- `chroot()` failed with "No such file or directory"

### Debug Output

```
DEBUG: / EXISTS
DEBUG: /home EXISTS
DEBUG: /home/cb DOES NOT EXIST - No such file or directory (os error 2)
DEBUG: /home/cb/Projects DOES NOT EXIST - No such file or directory (os error 2)
DEBUG: chroot target "/home/cb/Projects/remora/./alpine-rootfs" DOES NOT EXIST
DEBUG: Current directory: Ok("/")
DEBUG: Calling chroot to "/home/cb/Projects/remora/./alpine-rootfs"
DEBUG: chroot FAILED: ENOENT: No such file or directory
```

### Initial Attempts (Trial and Error)

1. **Attempt 1:** Used `BorrowedFd` to satisfy AsFd trait - compiled but same error
2. **Attempt 2:** Switched to `libc::setns()` directly - compiled but same error
3. **Attempt 3:** Added extensive debug logging - revealed setns() was succeeding but paths disappeared afterward
4. **Attempt 4:** Moved setns() from pre_exec to parent process - same error (paths still inaccessible)

At this point, we switched from trial-and-error to research.

---

## Research Findings

### What `ip netns add` Does

According to the [ip-netns man page](https://www.man7.org/linux/man-pages/man8/ip-netns.8.html):

> "A network namespace is identified by a name. Network namespace names are stored in /var/run/netns."

When `ip netns add NAME` creates a namespace:
1. Creates a new network namespace with `unshare(CLONE_NEWNET)`
2. Bind mounts `/proc/self/ns/net` to `/var/run/netns/NAME`
3. This makes the namespace persistent (survives after process exits)

### What `ip netns exec` Does

Importantly, `ip netns exec` does MORE than just join the network namespace:

> "ip netns exec automates handling of this configuration, file convention for network namespace unaware applications, **by creating a mount namespace** and bind mounting all of the per network namespace configure files into their traditional location in /etc."

So `ip netns exec` creates BOTH:
- Joins the network namespace (setns)
- **Creates a NEW mount namespace** (unshare CLONE_NEWNS)

Source: [ip-netns(8) manual](https://www.man7.org/linux/man-pages/man8/ip-netns.8.html)

### Mount Propagation and unshare(CLONE_NEWNS)

From the [mount_namespaces(7) man page](https://man7.org/linux/man-pages/man7/mount_namespaces.7.html):

When `unshare(CLONE_NEWNS)` creates a new mount namespace:
- The new namespace receives **a copy of the mount list** from the parent namespace
- By default (since util-linux 2.27), `unshare` performs: `mount --make-rprivate /`
- This makes ALL mounts `MS_PRIVATE` recursively

**Mount Propagation Types:**
- **MS_PRIVATE**: Complete isolation - mount/unmount events don't propagate
- **MS_SHARED**: Events propagate to peer group members
- **MS_SLAVE**: One-way propagation (receives but doesn't send)
- **MS_UNBINDABLE**: Can't be bind mounted

Our code explicitly does:
```rust
libc::mount(
    ptr::null(),
    "/".as_ptr(),
    ptr::null(),
    libc::MS_REC | libc::MS_PRIVATE,  // Make all mounts private recursively
    ptr::null(),
);
```

This is correct for preventing mount leaks, but doesn't explain why `/home/cb` disappears.

### The Critical Issue: Order of Operations

From the [nsenter bug discussion](https://www.spinics.net/lists/util-linux-ng/msg14759.html):

> "nsenter uses chroot(), with the command opening root-dir and cwd file descriptors **before** the setns() syscall, and then **after** the syscall calling chroot(), leaving the final process in the namespace but not in the root directory."

The fix:
> "moving root-dir and cwd open calls **after** the setns() call, so the process opens the directory references after entering the target mount namespace"

From [Namespaces in operation, part 2 [LWN.net]](https://lwn.net/Articles/531381/):

> "Since the caller will be located in the container's mount namespace after the setns() call but the source file descriptors refer to a mount located in the host's mount namespace, this check fails."

**Key Insight:** File descriptors or paths opened/resolved **before** `setns()` may not be valid **after** `setns()` if there are mount namespace interactions.

### Why Our Code Failed

Our original implementation:
1. **Parent process:** Open `/var/run/netns/con` and call `setns(fd, 0)` to join NET namespace
2. **Pre_exec (child):** Call `unshare(CLONE_NEWNS | ...)` to create new mount namespace
3. **Pre_exec:** Try to `chroot("/home/cb/Projects/remora/alpine-rootfs")`
4. **FAIL:** Path doesn't exist in the new mount namespace context

The problem: Even though we only joined the NET namespace (not MOUNT), when we subsequently created a new mount namespace with `unshare(CLONE_NEWNS)`, the copied mount table was based on the current mount namespace context, which may have been affected by the earlier setns() call or had incomplete mount information.

According to [kernel.org article on mounting into mount namespaces](https://people.kernel.org/brauner/mounting-into-mount-namespaces), calling setns() before creating new mount namespaces can cause the mount table copy to be incomplete or incorrect.

---

## Root Cause

**The root cause is the order of operations:**

When `setns(NET)` is called in the parent process, then the child process calls `unshare(CLONE_NEWNS)`, the new mount namespace's mount table may not correctly include all mounts from the original context. This is because:

1. Path resolution happens in namespace context
2. setns() changes the namespace context (even if just NET)
3. Subsequent unshare(CLONE_NEWNS) copies the mount table from the current context
4. The copied mount table may be incomplete or have different mount points visible

The fact that `/home` exists but `/home/cb` doesn't suggests that `/home/cb` may be a separate mount point (like a user home directory mount, autofs, or network mount) that wasn't properly included in the copied mount namespace.

---

## The Solution

**Move `setns()` call to AFTER `chroot()` in the pre_exec hook.**

### Why This Works

1. **Chroot happens first:** Path is resolved in the original mount namespace context where all mounts are visible
2. **Then join network namespace:** Network namespace is joined after the filesystem is already properly set up
3. **Network namespace joining doesn't affect filesystem:** Once we're inside the chroot, joining the network namespace only affects network visibility, not filesystem visibility

### Implementation Order

```rust
unsafe {
    self.inner.pre_exec(move || {
        // Step 1: Unshare namespaces (create new ones) - but NOT network
        unshare(UTS | MOUNT | PID | CGROUP)

        // Step 2: Make mounts private
        mount(..., MS_REC | MS_PRIVATE, ...)

        // Step 3: UID/GID mapping (if USER namespace)

        // Step 4: Set UID/GID

        // Step 5: Chroot (paths still accessible)
        chroot("/home/cb/Projects/remora/alpine-rootfs")

        // Step 6: User pre_exec callback (mount /proc, etc.)

        // Step 7: Join network namespace (AFTER filesystem is set up)
        setns(netns_fd, 0)

        Ok(())
    });
}
```

### Alternative Approaches Considered

1. **Don't use setns() at all** - Not viable, need namespace joining feature
2. **Keep setns() in parent** - Tried this, same issue
3. **Don't create mount namespace** - Not viable, need mount isolation
4. **Change mount propagation** - Doesn't fix the root cause

---

## Implementation Changes

### Before (Broken)

```rust
pub fn spawn(mut self) -> Result<Child, Error> {
    // Join namespaces in PARENT process
    for (path, ns) in &self.join_namespaces {
        let file = File::open(path)?;
        let fd = file.as_raw_fd();
        unsafe {
            libc::setns(fd, 0)?;  // Join in parent - WRONG
        }
    }

    // Pre_exec hook
    unsafe {
        self.inner.pre_exec(move || {
            unshare(...)?;  // Create mount namespace
            chroot(...)?;   // FAILS - path not accessible
            Ok(())
        });
    }
}
```

### After (Fixed)

```rust
pub fn spawn(mut self) -> Result<Child, Error> {
    // Open namespace files in parent (keep File alive)
    let join_ns_files: Vec<(File, Namespace)> = ...;
    let join_ns_fds: Vec<(i32, Namespace)> = ...;

    // Pre_exec hook
    unsafe {
        self.inner.pre_exec(move || {
            // Step 1: Unshare (create new namespaces)
            unshare(...)?;

            // Step 2-4: Mount setup, UID/GID

            // Step 5: Chroot (paths still accessible)
            chroot(...)?;

            // Step 6: User callback
            user_pre_exec()?;

            // Step 7: Join network namespace LAST
            for (fd, ns) in &join_ns_fds {
                libc::setns(*fd, 0)?;
            }

            Ok(())
        });
    }
}
```

---

## Testing

### Test Case

```bash
# Create network namespace
sudo ./setup.sh  # Creates 'con' network namespace with veth pair

# Run container with namespace joining
sudo -E ./target/debug/remora \
    --exe /init.sh \
    --rootfs ./alpine-rootfs \
    --uid 1000 \
    --gid 1000 \
    --join-netns con
```

### Expected Result

- Container starts successfully
- Filesystem paths are accessible
- Chroot succeeds
- Container can see veth2 interface from 'con' namespace
- IP address 172.16.0.1 is visible inside container

### Verification Script

```bash
./test-namespace-join.sh
```

Should output:
```
✅ SUCCESS! veth2 interface found in container
✅ SUCCESS! IP address 172.16.0.1 found
```

---

## Lessons Learned

1. **Order of operations is critical** when combining namespace operations
2. **Paths resolved before setns() may not be valid after** namespace transitions
3. **Web search research is more efficient than trial-and-error** for complex kernel interactions
4. **Man pages and kernel documentation are authoritative sources** - consult them first
5. **Network namespace joining should happen AFTER filesystem setup** to avoid mount namespace issues

---

## Related Issues

- Mount propagation leak: Fixed in Phase 3 with MS_PRIVATE
- UID/GID mapping: Implemented in Phase 3
- Integration tests: TODO (Task #9)

---

## References

1. [setns(2) - Linux manual page](https://man7.org/linux/man-pages/man2/setns.2.html)
2. [ip-netns(8) - Linux manual page](https://www.man7.org/linux/man-pages/man8/ip-netns.8.html)
3. [mount_namespaces(7) - Linux manual page](https://man7.org/linux/man-pages/man7/mount_namespaces.7.html)
4. [unshare(2) - Linux manual page](https://man7.org/linux/man-pages/man2/unshare.2.html)
5. [Namespaces in operation, part 2: the namespaces API [LWN.net]](https://lwn.net/Articles/531381/)
6. [Namespaces in operation, part 7: Network namespaces [LWN.net]](https://lwn.net/Articles/580893/)
7. [Util Linux NG: Bug: for mount namespaces inside a chroot](https://www.spinics.net/lists/util-linux-ng/msg14759.html)
8. [Mounting into mount namespaces — Christian Brauner](https://people.kernel.org/brauner/mounting-into-mount-namespaces)
9. [Network namespaces - Unweaving the Web](https://blogs.igalia.com/dpino/2016/04/10/network-namespaces/)
10. [Digging into Linux namespaces - part 2 - Quarkslab's blog](https://blog.quarkslab.com/digging-into-linux-namespaces-part-2.html)

---

**Resolution Date:** 2026-02-16
**Final Status:** ✅ Fixed by moving setns() to after chroot in pre_exec hook
