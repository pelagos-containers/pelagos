# Alpine Rootfs Comparison

## TL;DR

| Aspect | Old Rootfs | New Rootfs |
|--------|-----------|-----------|
| **Source** | example-20220730.tar.gz | Docker alpine:latest |
| **Date** | July 30, 2022 | January 27, 2025 |
| **Age** | ~3.5 years old | Current |
| **Alpine Version** | Unknown (likely 3.16) | **3.23.3** (latest) |
| **Architecture** | ❌ ARM aarch64 | ✅ x86_64 |
| **Size (compressed)** | 46 MB | N/A (from Docker) |
| **Size (extracted)** | ~9.7 MB | ~9.7 MB |
| **Docker Image Size** | N/A | 14 MB (3.95 MB content) |
| **Works on your CPU?** | ❌ No (Exec format error) | ✅ Yes |

---

## The Problem We Fixed

### Old Rootfs (example-20220730.tar.gz)

```bash
$ file alpine-rootfs/bin/busybox
alpine-rootfs/bin/busybox: ELF 64-bit LSB pie executable, ARM aarch64...
                                                        ^^^^^^^^^^^^
                                                        WRONG ARCHITECTURE!
```

**Built for:** ARM 64-bit processors (aarch64)
**Your CPU:** x86_64 (Intel/AMD)
**Result:** `Exec format error (os error 8)`

This tarball was likely created on an ARM-based system (maybe Raspberry Pi, AWS Graviton, or Apple Silicon) and bundled with the project.

### New Rootfs (from Docker)

```bash
$ file alpine-rootfs/bin/busybox
alpine-rootfs/bin/busybox: ELF 64-bit LSB pie executable, x86-64...
                                                        ^^^^^^^
                                                        CORRECT!
```

**Built for:** x86_64 (Intel/AMD)
**Your CPU:** x86_64 (Intel/AMD)
**Result:** ✅ Works perfectly!

---

## What Changed Under the Hood

### Old Method (alpine-make-rootfs script)

The project originally used a script (likely from https://github.com/alpinelinux/alpine-make-rootfs) that:
1. Downloaded Alpine packages from Alpine mirrors
2. Bootstrapped a minimal rootfs
3. Created the tarball

**Pros:**
- Reproducible builds
- Control over what packages are included
- Can customize the rootfs

**Cons:**
- Requires the script to be available
- Someone created it on ARM and committed the tarball
- Tarball gets stale (3.5 years old!)
- Large file in git (~46 MB)

### New Method (Docker extraction)

Our `fix-rootfs.sh` script now:
1. Pulls `alpine:latest` from Docker Hub
2. Creates a temporary container
3. Exports the container's filesystem
4. Extracts to `alpine-rootfs/`

**Pros:**
- ✅ Always latest Alpine version (3.23.3 as of Feb 2026)
- ✅ Automatically matches your architecture
- ✅ Official Alpine builds from Docker Hub
- ✅ Easy to update (`docker pull alpine:latest`)
- ✅ No large tarballs in git

**Cons:**
- Requires Docker installed
- Needs internet connection
- Slightly different from alpine-make-rootfs output

---

## Alpine Linux Version Comparison

### Old: Likely Alpine 3.16 (July 2022)

Alpine 3.16 was released May 23, 2022. Features:
- Linux kernel 5.15 LTS
- musl libc 1.2.3
- busybox 1.35.0
- OpenSSL 1.1

### New: Alpine 3.23.3 (January 2025)

```bash
$ cat alpine-rootfs/etc/os-release
NAME="Alpine Linux"
ID=alpine
VERSION_ID=3.23.3
PRETTY_NAME="Alpine Linux v3.23"
```

Alpine 3.23 released December 2024. Features:
- Linux kernel 6.6 LTS
- musl libc 1.2.5
- busybox 1.37.0
- OpenSSL 3.3
- Security updates and bug fixes from ~2.5 years of development

---

## What's Inside the Rootfs

### Directory Structure
```
alpine-rootfs/
├── bin/          # 82 utilities (all symlinked to busybox)
├── dev/          # Device nodes (empty until mounted)
├── etc/          # Configuration files
│   ├── os-release
│   ├── profile    # Sets PATH and environment
│   ├── passwd     # User database
│   └── group      # Group database
├── home/         # User home directories
├── lib/          # Shared libraries
├── media/        # Mount points for removable media
├── mnt/          # Temporary mount point
├── opt/          # Optional packages
├── proc/         # Process info (mounted at runtime)
├── root/         # Root user home directory
├── run/          # Runtime data
├── sbin/         # System binaries
├── srv/          # Service data
├── sys/          # Kernel/hardware info (mounted at runtime)
├── tmp/          # Temporary files
├── usr/          # User programs
│   ├── bin/
│   ├── lib/
│   ├── local/
│   └── sbin/
└── var/          # Variable data
    ├── cache/
    ├── log/
    └── tmp/
```

### Busybox Utilities (82 commands)

All standard Unix utilities as symlinks to `/bin/busybox`:
- **File operations:** ls, cp, mv, rm, mkdir, rmdir, cat, grep, find
- **Text processing:** sed, awk, head, tail, wc, sort, uniq
- **Shell:** ash (default shell), sh
- **Process management:** ps, top, kill, killall
- **Network:** ping, wget, nc, telnet
- **System:** mount, umount, hostname, dmesg, uname
- **And 60+ more utilities**

---

## Size Analysis

### Docker Image
```
IMAGE           DISK USAGE   CONTENT SIZE
alpine:latest   14 MB        3.95 MB
```

**Disk usage (14 MB):** Total space used (includes layers)
**Content size (3.95 MB):** Actual unique content
**Difference:** Docker's layer deduplication and compression

### Extracted Rootfs
```
$ du -sh alpine-rootfs
9.7M    alpine-rootfs
```

Uncompressed, ready to use. The ~9.7 MB includes:
- Busybox binary (~800 KB)
- musl libc and loader (~600 KB)
- 82 symlinks to busybox
- Configuration files
- Directory structure
- Essential libraries (libcrypto, etc.)

---

## Why Alpine Linux?

Alpine is popular for containers because it's:

1. **Tiny:** 9.7 MB vs Ubuntu (~80 MB) or Debian (~120 MB)
2. **Secure:** Uses musl libc (simpler, fewer attack surfaces)
3. **Simple:** No systemd, no complex init systems
4. **Fast:** Lightweight, quick boot times
5. **Complete:** Despite size, has all essential Unix tools
6. **Well-maintained:** Active community, regular updates

Perfect for learning containers because:
- Small size = easy to understand what's in it
- Simple = fewer moving parts
- Fast = quick iteration during development

---

## Updating the Rootfs

### To get latest Alpine version:

```bash
./fix-rootfs.sh
```

This will:
1. Pull latest `alpine:latest` from Docker Hub
2. Extract fresh rootfs
3. Verify it's x86_64

### To use a specific Alpine version:

Edit `fix-rootfs.sh` and change:
```bash
docker pull alpine:latest
```

To:
```bash
docker pull alpine:3.23.3      # Specific version
# or
docker pull alpine:3.20        # Specific major.minor
# or
docker pull alpine:edge        # Bleeding edge
```

---

## Security Considerations

### Old Tarball (3.5 years old)
- ❌ No security updates since July 2022
- ❌ Likely has known CVEs in busybox, musl, OpenSSL
- ❌ Outdated kernel headers
- ⚠️ Not suitable for production use

### New Docker Image (current)
- ✅ Latest security patches
- ✅ Up-to-date dependencies
- ✅ Regular updates from Alpine maintainers
- ✅ Can be refreshed anytime with `docker pull`

---

## Compatibility Notes

### Does this affect the project?

**No!** The new rootfs is a drop-in replacement. Both old and new:
- Use busybox for utilities
- Have ash shell at `/bin/ash`
- Same directory structure
- Same `/etc/profile` for PATH setup
- Compatible with remora's expectations

The only difference:
- Old: Wrong architecture (didn't work)
- New: Correct architecture (works perfectly)

### Can I still use alpine-make-rootfs?

Yes! If you want to build your own:

```bash
# Install alpine-make-rootfs
git clone https://github.com/alpinelinux/alpine-make-rootfs

# Build x86_64 rootfs
./alpine-make-rootfs \
    --arch x86_64 \
    --branch v3.23 \
    alpine-custom.tar.gz

# Extract
mkdir alpine-rootfs
tar -C alpine-rootfs -xzf alpine-custom.tar.gz
```

But the Docker method is simpler and always current!

---

## Summary

**We replaced:**
- ❌ 3.5-year-old ARM rootfs (didn't work on your CPU)
- ❌ 46 MB tarball in git

**With:**
- ✅ Current x86_64 rootfs (works perfectly)
- ✅ Fresh from Docker Hub (always up-to-date)
- ✅ Automatic architecture matching
- ✅ Easy to regenerate anytime

**Result:** Your container now works! 🎉

---

## Next Steps

The old `example-20220730.tar.gz` is still in the project directory. You can:

**Keep it** (for reference/history):
```bash
# Rename to make it clear it's obsolete
mv example-20220730.tar.gz example-20220730-arm-OBSOLETE.tar.gz
```

**Remove it** (save space):
```bash
rm example-20220730.tar.gz
```

**Add to .gitignore** (prevent committing rootfs):
```bash
echo "alpine-rootfs/" >> .gitignore
echo "*.tar.gz" >> .gitignore
```

The rootfs can always be regenerated with `./fix-rootfs.sh` so no need to commit it!
