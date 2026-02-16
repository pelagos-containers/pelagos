# Cleanup and Scripts Guide

## Scripts Overview

### Active Scripts ✅

**`launch-container.sh`** - Main container launcher
- Creates `/init.sh` in rootfs with proper PATH setup
- Launches remora with full environment
- **USE THIS** for normal testing

**`fix-rootfs.sh`** - Rebuild x86_64 rootfs
- Unmounts any stale filesystems
- Removes old rootfs
- Extracts fresh Alpine Linux from Docker
- Use when rootfs is corrupted or wrong architecture

**`setup.sh`** - Network namespace setup
- Creates network namespace named "con"
- Sets up veth pair for container networking
- Configures routing (172.16.0.1)
- Run once before testing network features

**`cleanup-mounts.sh`** - Manual mount cleanup
- Unmounts any leftover /proc, /sys, /dev mounts
- Use if container exits abnormally
- Safe to run anytime

### Removed Scripts ❌

- ~~`launch.sh`~~ - Old script (removed)
- ~~`launch-with-path.sh`~~ - Intermediate script (removed)
- ~~`container-init.sh`~~ - Content now embedded in launch-container.sh (removed)

---

## Automatic Cleanup on Container Exit

The remora code now handles cleanup automatically in `src/main.rs`:

### 1. `/sys` Mount (lines 136-141, 159-162)
```rust
// Mounted before container starts (parent process)
match mount_sys(sys_mount.as_ref()) {
    Ok(_) => info!("mounted sys"),
    Err(e) => info!("failed to mount sys: {:?}", e)
}

// ... container runs ...

// Unmounted after container exits
match umount_sys(sys_mount.as_ref()) {
    Ok(_) => info!("unmounted sys"),
    Err(e) => info!("failed to unmount sys {:?}",e)
}
```

### 2. `/proc` Mount (lines 164-170)
```rust
// Try to unmount proc if it leaked out of mount namespace
let mut proc_path = std::env::current_dir().unwrap();
proc_path.push(&rootfs_path);
proc_path.push("proc");
let proc_mount = CString::new(proc_path.into_os_string().into_string().unwrap().as_bytes()).unwrap();
match umount_sys(proc_mount.as_ref()) {
    Ok(_) => info!("unmounted proc"),
    Err(_) => {} // Ignore error - proc might not be mounted
}
```

**Note:** `/proc` is mounted inside the child's mount namespace via `pre_exec` callback. When the mount namespace is destroyed (child exits), proc should auto-unmount. The explicit unmount above handles edge cases where mounts leak.

### 3. Namespaces

Linux namespaces are automatically cleaned up by the kernel when the last process in the namespace exits:

- **PID namespace** - Destroyed when init process (ash) exits
- **Mount namespace** - Destroyed, automatically unmounting private mounts
- **UTS namespace** - Destroyed (hostname changes disappear)
- **Cgroup namespace** - Destroyed

**Network namespace** (`/var/run/netns/con`) persists because it's created externally by `setup.sh`. To remove:
```bash
sudo ip netns delete con
```

---

## Cleanup Checklist

### After Normal Container Exit ✅
- [x] `/sys` unmounted automatically
- [x] `/proc` unmounted automatically
- [x] Namespaces destroyed automatically
- [x] No manual cleanup needed

### After Abnormal Exit (crash, kill -9, etc.) ⚠️

Check for leftover mounts:
```bash
mount | grep alpine-rootfs
```

If you see mounts, run:
```bash
./cleanup-mounts.sh
```

Or manually:
```bash
sudo umount alpine-rootfs/proc
sudo umount alpine-rootfs/sys
```

### Before Rebuilding Rootfs

Always clean up first:
```bash
./cleanup-mounts.sh
./fix-rootfs.sh
```

---

## Verification Commands

### Check for Leftover Mounts
```bash
mount | grep alpine-rootfs
# Should return nothing after container exits
```

### Check Network Namespace
```bash
ip netns list
# Should show: con (use 192.168.1.1)
```

### Verify Rootfs Architecture
```bash
file alpine-rootfs/bin/busybox
# Should show: ELF 64-bit LSB ... x86-64 (not ARM aarch64)
```

### Test Container Launches
```bash
./launch-container.sh
# Inside container:
ps aux        # Should only see container processes
hostname      # Try: hostname test
ls /          # Should see Alpine rootfs
exit          # Should exit cleanly
```

---

## Troubleshooting

### "Operation not permitted" when removing rootfs
**Cause:** Filesystems still mounted
**Fix:** Run `./cleanup-mounts.sh` first

### "Exec format error" when launching
**Cause:** Wrong architecture rootfs (ARM vs x86_64)
**Fix:** Run `./fix-rootfs.sh` to get correct architecture

### "ls: not found" inside container
**Cause:** PATH not set
**Fix:** Use `./launch-container.sh` (not old launch.sh)

### Container exits but mounts remain
**Cause:** Mount namespace didn't isolate properly
**Fix:** Automatic cleanup in main.rs should handle this now (lines 164-170)

### Can't delete network namespace
**Cause:** Network namespace still in use
**Fix:**
```bash
# Find processes using the namespace
sudo ip netns pids con

# If needed, delete it
sudo ip netns delete con

# Recreate if needed
./setup.sh
```

---

## Design Notes

### Why proc sometimes leaks

The `/proc` filesystem is mounted inside the container's mount namespace via a `pre_exec` callback. Normally, when the mount namespace is destroyed (container exits), all private mounts are automatically unmounted by the kernel.

However, mount propagation can cause mounts to leak into the parent namespace if:
1. The mount is marked as `MS_SHARED`
2. The parent and child namespaces have shared mount points

Our code uses `MS_BIND` for sys (line 244) which shouldn't propagate, but we now explicitly unmount both sys and proc after container exit to be safe.

### Future Improvements (Phase 2/3)

- Add mount propagation flags (`MS_PRIVATE`, `MS_SLAVE`) to prevent leaks
- Use `pivot_root` instead of `chroot` for better isolation
- Add signal handling for graceful shutdown
- Implement proper init process (reap zombies)
- Add container lifecycle management (pause/resume/kill)

---

## Summary

✅ **Automatic Cleanup Works!**
- Filesystems unmounted on exit
- Namespaces destroyed automatically
- No manual intervention needed (normally)

✅ **Manual Cleanup Available**
- `cleanup-mounts.sh` for leftover mounts
- `fix-rootfs.sh` for rootfs issues

✅ **Clean Script Organization**
- One main launcher: `launch-container.sh`
- Helper scripts for specific tasks
- No intermediate/temporary scripts
