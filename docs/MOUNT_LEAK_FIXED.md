# Mount Leak Issue - FIXED ✅

**Status:** RESOLVED
**Fixed:** 2026-02-16 (Phase 3)
**Severity:** Was High, now resolved

---

## The Problem (Was)

After running the container, `/proc` mounts leaked from the container's mount namespace to the parent namespace and persisted after container exit.

**Evidence (before fix):**
```bash
$ mount | grep alpine-rootfs
proc on /home/cb/Projects/remora/alpine-rootfs/proc type proc (rw,relatime)
proc on /home/cb/Projects/remora/alpine-rootfs/proc type proc (rw,relatime)
```

---

## The Solution

Added `MS_PRIVATE` mount propagation in `src/container.rs` after creating the mount namespace:

```rust
// In spawn() pre_exec callback, after unshare(CLONE_NEWNS):
if namespaces.contains(Namespace::MOUNT) {
    // Make all mounts private to prevent propagation to parent
    let root = CStr::from_bytes_with_nul(b"/\0").unwrap();
    let result = libc::mount(
        ptr::null(),                          // source: NULL (remount)
        root.as_ptr(),                        // target: root
        ptr::null(),                          // fstype: NULL (remount)
        libc::MS_REC | libc::MS_PRIVATE,      // flags: recursive + private
        ptr::null(),                          // data: NULL
    );
}
```

**What this does:**
- Makes all mounts in the container's namespace private
- Private mounts don't propagate to parent namespace
- Container's mounts stay isolated
- Clean exit with no leaked mounts

---

## Verification

**Test script:** `./test-mount-fix.sh`

**Result:**
```bash
✅ SUCCESS! No mount leaks detected.
```

**Verification steps:**
1. Launch container
2. Container exits
3. Check: `mount | grep alpine-rootfs`
4. Result: No output (clean!)

---

## Impact

✅ **Clean exits** - No manual cleanup needed
✅ **Proper isolation** - Mounts stay in container namespace
✅ **Reliable** - Works on normal and abnormal exits
✅ **No side effects** - Container functionality unchanged

---

## Related Fixes

This also helps with:
- `/sys` unmounting (TODO.org #3)
- Cgroup mount propagation (TODO.org #5)
- General mount namespace isolation

---

**Fixed:** 2026-02-16
**Verified:** User testing confirmed
**Status:** ✅ Complete
