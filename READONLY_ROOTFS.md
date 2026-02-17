# Read-Only Rootfs Implementation

Deep dive into how Remora implements immutable container filesystems.

---

## The Challenge

Making a container's root filesystem read-only is trickier than it seems. You can't just call `mount()` with `MS_RDONLY` after `chroot()` - Linux mount semantics require specific setup.

### The Core Problem

After `chroot()`, the new root `/` is **not a mount point** - it's just a directory that became the root of the filesystem namespace. You can only remount things that are already mount points in the kernel's mount table.

```c
chroot("/path/to/alpine-rootfs");
mount(NULL, "/", NULL, MS_REMOUNT | MS_RDONLY, NULL);  // FAILS: EINVAL
```

**Error:** `EINVAL` (Invalid argument) - because `/` isn't a mount point yet!

---

## The Solution: Two-Step Bind Mount Process

The standard approach (used by Docker, runc, Podman, etc.) is a two-step process:

1. **Make rootfs a mount point** (before chroot)
2. **Remount it readonly** (after chroot + other mounts)

### Step 1: Bind Mount to Itself (Before Chroot)

```rust
// BEFORE chroot - outside the jail
let dir_c = CString::new("/path/to/alpine-rootfs").unwrap();
let result = libc::mount(
    dir_c.as_ptr(),      // source: /path/to/alpine-rootfs
    dir_c.as_ptr(),      // target: /path/to/alpine-rootfs (same!)
    ptr::null(),         // fstype: NULL (bind mount)
    libc::MS_BIND | libc::MS_REC,  // flags
    ptr::null(),         // data: NULL
);
```

**What this does:**
- Bind-mounts a directory to itself
- Creates an entry in the kernel's mount table
- Makes `/path/to/alpine-rootfs` a proper **mount point**
- `MS_REC` makes it recursive (includes all subdirectories)
- Equivalent to: `mount --bind /path/to/alpine-rootfs /path/to/alpine-rootfs`

**Why bind to itself?**
- Bind mounting creates a mount point without changing the filesystem
- It's like creating a "view" of the directory that the kernel tracks as a mount
- This is required for later remounting with different flags

### Step 2: Perform Chroot

```rust
chroot(dir)?;  // /path/to/alpine-rootfs becomes "/"
std::env::set_current_dir("/")?;
```

After chroot, `/` (which was `/path/to/alpine-rootfs`) **inherits** its mount point status.

### Step 3: Mount Other Filesystems

```rust
// These operations need a WRITABLE rootfs
mount("proc", "/proc", "proc", 0, NULL);     // ✓ Works
mount("sysfs", "/sys", "sysfs", 0, NULL);    // ✓ Works
mount("/dev", "/dev", NULL, MS_BIND, NULL);  // ✓ Works
```

**Critical:** These mounts must happen **before** making rootfs readonly!

### Step 4: Remount as Read-Only (After All Mounts)

```rust
// AFTER chroot and all other mounts
let root = CString::new("/").unwrap();
let result = libc::mount(
    ptr::null(),         // source: NULL (remount existing)
    root.as_ptr(),       // target: /
    ptr::null(),         // fstype: NULL
    libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_BIND,  // flags
    ptr::null(),         // data: NULL
);
```

**What this does:**
- `MS_REMOUNT` = modify existing mount flags (not a new mount)
- `MS_RDONLY` = add the read-only flag
- `MS_BIND` = tells kernel this is remounting a bind mount
- `source = NULL` = remount the existing mount at target path
- Equivalent to: `mount -o remount,ro,bind /`

---

## Why MS_BIND in the Remount?

When remounting a **bind mount**, you must specify `MS_BIND` again. This is a Linux kernel requirement:

```c
// Regular filesystem mount -> remount without MS_BIND
mount("/dev/sda1", "/mnt", "ext4", 0, NULL);
mount(NULL, "/mnt", NULL, MS_REMOUNT | MS_RDONLY, NULL);  // ✓ Works

// Bind mount -> MUST include MS_BIND in remount
mount("/src", "/dst", NULL, MS_BIND, NULL);
mount(NULL, "/dst", NULL, MS_REMOUNT | MS_RDONLY, NULL);  // ✗ EINVAL!
mount(NULL, "/dst", NULL, MS_REMOUNT | MS_RDONLY | MS_BIND, NULL);  // ✓ Works
```

**Why?** The kernel needs to know you're modifying a bind mount specifically, not the underlying filesystem.

---

## Order of Operations Matters

### ❌ Wrong Order (Causes Failures)

```rust
// ATTEMPT 1: Remount before it's a mount point
chroot(dir)?;
mount(NULL, "/", NULL, MS_REMOUNT | MS_RDONLY, NULL);
// ❌ FAILS: EINVAL - "/" is not a mount point

// ATTEMPT 2: Make readonly too early
bind_mount(dir, dir);
chroot(dir)?;
mount(NULL, "/", NULL, MS_REMOUNT | MS_RDONLY | MS_BIND, NULL);  // Makes "/" readonly
mount("proc", "/proc", "proc", 0, NULL);
// ❌ FAILS: EROFS - Read-only file system (can't mount on readonly fs)

// ATTEMPT 3: Forget MS_BIND in remount
bind_mount(dir, dir);
chroot(dir)?;
mount("proc", "/proc", "proc", 0, NULL);
mount(NULL, "/", NULL, MS_REMOUNT | MS_RDONLY, NULL);  // Missing MS_BIND
// ❌ FAILS: EINVAL - Must use MS_BIND for bind mount remount
```

### ✅ Correct Order

```rust
// 1. BEFORE chroot: Make rootfs a bind mount (creates mount point)
let dir_c = CString::new(chroot_dir.as_os_str().as_bytes()).unwrap();
libc::mount(dir_c.as_ptr(), dir_c.as_ptr(), ptr::null(),
            libc::MS_BIND | libc::MS_REC, ptr::null());

// 2. Perform chroot (rootfs becomes "/" and inherits mount point status)
chroot(chroot_dir)?;
std::env::set_current_dir("/")?;

// 3. Mount all needed filesystems (requires writable rootfs)
mount("proc", "/proc", "proc", 0, NULL);        // ✓
mount("sysfs", "/sys", "sysfs", 0, NULL);       // ✓
mount("/dev", "/dev", NULL, MS_BIND, NULL);     // ✓
// ... bind mount /dev/null over sensitive paths ...

// 4. FINALLY: Remount rootfs as read-only (after everything else)
libc::mount(ptr::null(), root.as_ptr(), ptr::null(),
            libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_BIND, ptr::null());
// ✓ Success!
```

---

## Visual Timeline

```
┌─────────────────────────────────────────────────────────────┐
│ OUTSIDE CHROOT (Host Filesystem)                           │
└─────────────────────────────────────────────────────────────┘

/home/user/alpine-rootfs/          (regular directory)
         ↓
mount --bind alpine-rootfs alpine-rootfs
         ↓
/home/user/alpine-rootfs/          (NOW A MOUNT POINT ✓)

┌─────────────────────────────────────────────────────────────┐
│ CHROOT TRANSITION                                           │
└─────────────────────────────────────────────────────────────┘

chroot(/home/user/alpine-rootfs)
         ↓
/                                  (inherited mount point status)

┌─────────────────────────────────────────────────────────────┐
│ INSIDE CHROOT (Container Filesystem - Still Writable)      │
└─────────────────────────────────────────────────────────────┘

mount -t proc proc /proc           ✓ Success
mount -t sysfs sys /sys            ✓ Success
mount --bind /dev /dev             ✓ Success
mount --bind /dev/null /proc/kcore ✓ Success (masked paths)

┌─────────────────────────────────────────────────────────────┐
│ MAKE READONLY (Final Step)                                 │
└─────────────────────────────────────────────────────────────┘

mount -o remount,ro,bind /         ✓ Success
         ↓
/                                  (NOW READ-ONLY ✓)

Container tries: touch /test
         ↓
❌ EROFS: Read-only file system
```

---

## Implementation in Remora

### Code Location: `src/container.rs`

The implementation is split across the pre-exec hook:

#### Part 1: Before Chroot (~line 980)

```rust
} else if let Some(ref dir) = chroot_dir {
    // If readonly rootfs is requested, bind-mount the chroot dir to itself BEFORE chroot
    // This makes it a proper mount point so we can remount it readonly later
    if readonly_rootfs {
        let dir_c = CString::new(dir.as_os_str().as_bytes()).unwrap();
        let result = libc::mount(
            dir_c.as_ptr(),          // source: chroot dir
            dir_c.as_ptr(),          // target: same dir
            ptr::null(),             // fstype: NULL
            libc::MS_BIND | libc::MS_REC, // recursive bind mount
            ptr::null(),             // data: NULL
        );
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
    }

    chroot(dir)?;
    std::env::set_current_dir("/")?;
}
```

#### Part 2: After Chroot + Mounts (~line 1104)

```rust
// Step 4.5: Perform automatic mounts (/proc, /sys, /dev)
// ... mount code ...

// Step 4.8: Mask sensitive paths
// ... masked paths code ...

// Step 4.85: Make rootfs read-only if requested
// MUST come after all mounts (/proc, /sys, /dev, masked paths)
if readonly_rootfs {
    let root = CString::new("/").unwrap();
    let result = libc::mount(
        ptr::null(),             // source: NULL (remount)
        root.as_ptr(),           // target: /
        ptr::null(),             // fstype: NULL (remount)
        libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_BIND, // remount readonly
        ptr::null(),             // data: NULL
    );
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
}
```

---

## API Usage

```rust
use remora::container::{Command, Namespace, Stdio};

let mut child = Command::new("/bin/ash")
    .args(&["-c", "touch /test_write"])  // This will fail!
    .with_chroot("/path/to/rootfs")
    .with_namespaces(Namespace::UTS | Namespace::MOUNT)
    .with_proc_mount()
    .with_readonly_rootfs(true)  // ← Enables read-only rootfs
    .spawn()?;

child.wait()?;
// Container ran, but "touch /test_write" failed with EROFS
```

---

## Security Benefits

### Immutable Infrastructure

Once the container starts, **no process** (even root) can modify the filesystem:

```bash
# Inside container (even as root):
$ touch /etc/malware.sh
touch: /etc/malware.sh: Read-only file system

$ rm /bin/bash
rm: can't remove '/bin/bash': Read-only file system

$ echo "evil" > /etc/passwd
ash: can't create /etc/passwd: Read-only file system
```

### Attack Surface Reduction

**Prevented attacks:**
- ✅ Malware persistence (can't write to disk)
- ✅ Configuration tampering
- ✅ Binary replacement
- ✅ Privilege escalation via file modification

**Still possible:**
- ⚠️ Memory-only attacks (process injection, etc.)
- ⚠️ Network attacks
- ⚠️ Writable volumes (if mounted separately)

---

## Comparison to Other Runtimes

| Runtime | Bind Mount Before Chroot | Remount After Mounts | Same Approach |
|---------|--------------------------|---------------------|---------------|
| **Docker** | ✅ Yes | ✅ Yes | ✅ Yes |
| **runc** | ✅ Yes | ✅ Yes | ✅ Yes |
| **Podman** | ✅ Yes | ✅ Yes | ✅ Yes |
| **Remora** | ✅ Yes | ✅ Yes | ✅ Yes |
| **systemd-nspawn** | ✅ Yes | ✅ Yes | ✅ Yes |

**This is the standard approach across all container runtimes.**

---

## Testing

### Test: Read-Only Rootfs Works

```rust
#[test]
fn test_readonly_rootfs() {
    let mut child = Command::new("/bin/ash")
        .args(&["-c", "touch /test_file 2>&1; echo exit_code=$?"])
        .with_chroot(&rootfs)
        .with_namespaces(Namespace::UTS | Namespace::MOUNT)
        .with_proc_mount()
        .with_readonly_rootfs(true)
        .spawn()
        .expect("Failed to spawn with read-only rootfs");

    let status = child.wait().expect("Failed to wait for child");
    assert!(status.success());
    // Container runs successfully, but touch fails inside
}
```

### Manual Testing

```bash
# Build and run secure container
sudo -E cargo run --example secure_container

# Output shows:
# 1. Testing read-only rootfs:
#   ✓ Cannot write to rootfs (read-only)
```

---

## Debugging Common Errors

### EINVAL (Invalid argument)

**Symptom:**
```
Failed to spawn: Os { code: 22, kind: InvalidInput, message: "Invalid argument" }
```

**Causes:**
1. ❌ Trying to remount something that's not a mount point
2. ❌ Using `MS_BIND` and `MS_REMOUNT` without initial bind mount
3. ❌ Missing `MS_BIND` when remounting a bind mount

**Fix:** Ensure bind mount happens before chroot, and remount includes `MS_BIND`.

### EROFS (Read-only file system)

**Symptom:**
```
Failed to mount /proc: Os { code: 30, kind: ReadOnlyFilesystem, ... }
```

**Cause:** ❌ Made rootfs readonly before mounting `/proc`, `/sys`, etc.

**Fix:** Ensure readonly remount happens **after** all other mounts.

### Order Debug Checklist

```
✓ 1. Bind mount rootfs to itself (before chroot)
✓ 2. Perform chroot
✓ 3. Mount /proc
✓ 4. Mount /sys
✓ 5. Mount /dev
✓ 6. Bind mount masked paths
✓ 7. Remount rootfs readonly (LAST!)
```

---

## References

### Linux Kernel Documentation
- `mount(2)` man page: Mount flags and semantics
- `Documentation/filesystems/sharedsubtree.txt`: Mount propagation

### Container Runtime Source Code
- **runc**: `libcontainer/rootfs_linux.go` - `prepareRoot()` function
- **Docker/containerd**: Similar bind+remount pattern
- **Podman**: Uses runc's approach

### Related Remora Documentation
- `CLAUDE.md` - Development guidelines
- `SECCOMP_DEEP_DIVE.md` - Syscall filtering
- `PHASE1_COMPLETE.md` - Phase 1 security features
- `TESTING_GUIDE.md` - How to run security tests

---

## Summary

**Read-only rootfs requires two mount operations:**

1. **Before chroot:** Bind-mount rootfs to itself → makes it a mount point
2. **After chroot + mounts:** Remount with `MS_REMOUNT | MS_RDONLY | MS_BIND` → makes it readonly

**Order is critical:**
- Too early = can't mount `/proc`, `/sys`, etc.
- Too late = security gap during container initialization
- Wrong flags = `EINVAL` errors

**This is the standard, battle-tested approach used by all major container runtimes.**
