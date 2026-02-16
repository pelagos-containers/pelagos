# Remora - Linux Container Runtime

## Project Overview

Remora is a low-level Linux container runtime written in Rust. It creates lightweight containers using Linux namespaces, similar to Docker but at a much lower level. The project launches processes in isolated environments with their own process tree, mount points, and network namespaces.

## Key Features

- **Namespace Isolation**: Uses Linux namespaces (UTS, Mount, PID, Cgroup)
- **chroot Environment**: Runs processes in a custom Alpine Linux rootfs
- **Network Namespace Support**: Can attach to existing network namespaces (via `/var/run/netns/con`)
- **Filesystem Mounting**: Handles mounting of proc, sys, and cgroup filesystems
- **UID/GID Mapping**: Supports user/group ID mapping for unprivileged containers

## Architecture

### Main Components

1. **main.rs** (261 lines)
   - Entry point: parses CLI arguments, sets up environment
   - Mounts `/sys` from parent process (requires root privileges)
   - Spawns child process in isolated namespaces
   - Handles cleanup (unmounting sys) on exit

2. **child() function** (lines 42-115)
   - Creates new namespaces (UTS, Mount, PID, Cgroup)
   - Sets up chroot environment
   - Mounts proc filesystem via pre_exec callback
   - Opens and attaches to network namespace if available

3. **Setup Scripts**
   - `setup.sh`: Creates network namespace with veth pair for container networking
   - `launch.sh`: Builds Alpine rootfs and launches remora with sudo

## Dependencies

### Core Dependencies (Cargo.toml:8-17)

- **unshare** (path = "../unshare") ⚠️ **UNMAINTAINED**
  - Version: 0.7.0
  - Last updated: June 7, 2021 (4.5+ years ago)
  - Purpose: Low-level interface for Linux namespaces
  - Critical functionality: Command spawning, namespace creation, chroot
  - Issues: Some deprecation warnings, uses older Rust patterns

- **clap** (3.1.6) - CLI argument parsing
- **nix** - Unix system calls wrapper
- **libc** - Direct libc bindings for mount/umount
- **log** + **env_logger** - Logging infrastructure
- **subprocess** - Process management utilities
- **cgroups-fs** - Cgroup filesystem interface
- **palaver** - (purpose unclear, needs investigation)

## Build Status

✅ **Project currently builds successfully** (tested Feb 16, 2026)

Warnings present:
- unshare library generates 4 warnings (unused parens, missing ABI declarations)
- nom v2.2.1 will be rejected by future Rust versions
- All warnings are in dependencies, not main code

## Current Issues (from TODO.org)

1. ✅ Can mount sys from parent process (requires root)
2. ⚠️ Requires root privileges to mount sys
3. ❌ sys doesn't unmount cleanly on exit
4. ❌ Cgroups not working properly
5. ❓ Mounting sys seems to include cgroups (unclear why)

## Usage

```bash
# Setup network namespace
./setup.sh

# Launch container with Alpine Linux rootfs
./launch.sh

# Direct usage
sudo -E ./target/debug/remora \
  --exe /bin/ash \
  --rootfs ./alpine-rootfs \
  --uid 1000 \
  --gid 1000
```

## Environment Variables

- `RUST_LOG=info` - Enable info-level logging
- `RUST_BACKTRACE=full` - Full stack traces on panic

## Critical Path Dependency: unshare

### Why It's Critical

The unshare library provides the core functionality:
- Process spawning with namespace isolation
- Namespace management (unshare, setns)
- UID/GID mapping
- chroot/pivot_root operations
- File descriptor forwarding

### Replacement Considerations

The project heavily depends on unshare's `Command` builder pattern:
```rust
let mut cmd = Command::new(to_run);
cmd.unshare([Namespace::Uts, Namespace::Mount, Namespace::Pid, Namespace::Cgroup].iter())
   .chroot_dir(curdir)
   .pre_exec(&mount_proc)
   .stdin(Stdio::inherit())
   // ...
```

Potential alternatives:
1. **nix** crate - Already a dependency, has some namespace support but less comprehensive
2. **Fork unshare** - Create maintained fork with modern Rust patterns
3. **youki/libcontainer** - Modern container runtime libraries (heavier weight)
4. **Direct syscalls via nix/libc** - More work but gives full control

## Network Setup

The setup.sh script creates:
- Network namespace named "con"
- veth pair (veth1 ↔ veth2)
- veth2 placed in container namespace with IP 172.16.0.1
- veth1 remains in host namespace
- Routing configured for container ↔ host communication

## Development Notes

- Requires Linux kernel with namespace support
- Must run with sudo/CAP_SYS_ADMIN for namespace creation
- Alpine Linux rootfs expected in `./alpine-rootfs/`
- Logging controlled via RUST_LOG environment variable
- Uses unsafe blocks for direct libc calls (mount/umount)

**Note on Time Estimates:**
- We avoid time estimates in documentation
- They underestimate human effort and overestimate AI capabilities
- Focus on task complexity and dependencies instead

## TODOs / Future Work

- [ ] Investigate and fix sys unmounting issue
- [ ] Debug cgroup mounting problems
- [ ] Replace or update unshare dependency
- [ ] Consider dropping root requirements where possible
- [ ] Add tests
- [ ] Document expected rootfs structure
