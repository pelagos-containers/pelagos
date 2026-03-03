# Volumes, Bind Mounts, and Overlay Filesystems

This document explains how Pelagos handles persistent storage (volumes), host
directory mapping (bind mounts), temporary in-memory filesystems (tmpfs), and
copy-on-write layered filesystems (overlayfs). It covers both the library API
and the CLI, and describes how these features compose with each other.

---

## Bind Mounts

A bind mount maps a host directory into the container's filesystem tree. The
container sees the host directory at the specified target path.

### API

```rust
// Read-write bind mount
Command::new("/bin/sh")
    .with_chroot(&rootfs)
    .with_namespaces(Namespace::MOUNT | Namespace::UTS)
    .with_bind_mount("/host/data", "/container/data")
    .spawn()?;

// Read-only bind mount
Command::new("/bin/sh")
    .with_chroot(&rootfs)
    .with_namespaces(Namespace::MOUNT | Namespace::UTS)
    .with_bind_mount_ro("/host/config", "/etc/myconfig")
    .spawn()?;
```

### CLI

```bash
remora run alpine -v /host/data:/container/data -- /bin/sh
```

When the source path is absolute (`/host/...`), the CLI creates a direct bind
mount. If the source is a plain name, it is treated as a named volume (see
below).

### Implementation

Bind mounts use the `MS_BIND` flag with `libc::mount()`. The mount happens in
the child's pre\_exec hook **before chroot**, so host paths are still reachable.
The target directory is created inside the effective root if it does not exist.

Read-only bind mounts use a two-step process: first a regular `MS_BIND` mount,
then a `MS_REMOUNT | MS_BIND | MS_RDONLY` remount.

---

## tmpfs Mounts

A tmpfs mount creates an in-memory writable filesystem at a target path. This
is useful for scratch space, especially when the rootfs is read-only.

### API

```rust
Command::new("/bin/sh")
    .with_chroot(&rootfs)
    .with_namespaces(Namespace::MOUNT | Namespace::UTS)
    .with_readonly_rootfs()
    .with_tmpfs("/tmp")
    .spawn()?;
```

### CLI

```bash
remora run alpine --read-only --tmpfs /tmp -- /bin/sh
```

---

## Named Volumes

Named volumes are host directories managed by Pelagos. They provide persistent
storage that survives container restarts and can be shared between containers.

### Storage Location

| Mode     | Path                                                  |
|----------|-------------------------------------------------------|
| Root     | `/var/lib/remora/volumes/<name>/`                     |
| Rootless | `$XDG_DATA_HOME/remora/volumes/<name>/` (or `~/.local/share/remora/volumes/<name>/`) |

Each volume is a plain directory — no metadata files or driver abstraction.

### API

```rust
use remora::container::Volume;

// Create a new volume (or open existing)
let vol = Volume::create("mydata")?;

// Open an existing volume (errors if not found)
let vol = Volume::open("mydata")?;

// Mount into a container
Command::new("/bin/sh")
    .with_chroot(&rootfs)
    .with_namespaces(Namespace::MOUNT | Namespace::UTS)
    .with_volume(&vol, "/data")
    .spawn()?;

// Delete a volume and all its contents
Volume::delete("mydata")?;
```

`with_volume()` is syntactic sugar for `with_bind_mount(vol.path(), target)`.

### CLI

```bash
remora volume create mydata
remora volume ls
remora volume rm mydata

# Use in a container (auto-creates if it doesn't exist)
remora run alpine -v mydata:/data -- /bin/sh
```

---

## Overlay Filesystem

Overlayfs provides a copy-on-write layered filesystem. A read-only lower layer
(or stack of layers) is combined with a writable upper layer. Reads fall through
to the lower layers; writes land in the upper layer, leaving the lower layers
untouched.

### Single-Layer Overlay

The simplest form: the rootfs is the lower layer, and you supply an upper and
work directory.

```rust
let scratch = tempfile::tempdir()?;
let upper = scratch.path().join("upper");
let work = scratch.path().join("work");
std::fs::create_dir_all(&upper)?;
std::fs::create_dir_all(&work)?;

Command::new("/bin/sh")
    .with_chroot(&rootfs)
    .with_namespaces(Namespace::MOUNT | Namespace::UTS)
    .with_overlay(&upper, &work)
    .spawn()?;
```

After the container exits, `upper/` contains only the files that were created or
modified. The rootfs is completely untouched.

### Multi-Layer Overlay (OCI Images)

OCI images consist of multiple layers stacked on top of each other. Pelagos
mounts them as a single overlayfs with multiple `lowerdir=` entries.

```rust
// layer_dirs must be in top-first order (as overlayfs expects)
let layers = vec![top_layer, middle_layer, bottom_layer];

Command::new("/bin/sh")
    .with_image_layers(layers)
    .spawn()?;
```

`with_image_layers()` automatically:
- Sets the bottom layer as the chroot directory
- Creates ephemeral `upper/` and `work/` directories at
  `/run/remora/overlay-{pid}-{n}/`
- Adds `Namespace::MOUNT` and proc mount
- Cleans up the ephemeral directories on `wait()`

### Rootless Overlay

- **Kernel 5.11+**: Native overlay with the `userxattr` mount option. Pelagos
  auto-detects this capability.
- **Fallback**: `fuse-overlayfs` is spawned by the parent process before fork.
  The child uses the pre-mounted FUSE filesystem transparently.

### How the Build Engine Uses Overlay

`remora build` uses overlay to snapshot each `RUN` instruction:

1. Mount all accumulated layers via `with_image_layers()`
2. Run the command — writes land in the ephemeral upper directory
3. Call `wait_preserve_overlay()` instead of `wait()` — this skips cleanup so
   the build engine can inspect the upper directory
4. Tar + sha256-hash the upper directory contents → store as a new
   content-addressable layer at `/var/lib/remora/layers/<sha256>/`
5. The next `RUN` instruction stacks this new layer on top

Layers are deduplicated by sha256 digest.

---

## How These Features Compose

### The `effective_root` Concept

The key to understanding mount composition is `effective_root`. In the child's
pre\_exec hook:

- **Without overlay**: `effective_root` = the chroot directory
- **With overlay**: `effective_root` = the overlay's merged directory

All subsequent mounts — bind mounts, volumes, tmpfs, DNS — target paths inside
`effective_root`. This means they always appear correctly in the container's
view regardless of whether an overlay is active.

### Mount Ordering in Pre-exec

The pre\_exec hook sets up mounts in this order:

```
1. Unshare namespaces
2. Make mounts private (MS_PRIVATE)
3. UID/GID mappings
4. Mount overlayfs → merged/ becomes effective_root
5. DNS bind mount (/etc/resolv.conf inside effective_root)
6. User bind mounts and volumes (inside effective_root)
7. chroot(effective_root)
8. Mount /proc, /sys, /dev (inside container)
9. Apply read-only rootfs remount (if requested)
10. Drop capabilities, apply seccomp (last)
```

### Volumes on Top of Overlay

When a volume is mounted into a container that uses overlay, the volume's bind
mount is applied **on top of the merged overlay view**. This means:

- The volume's content takes precedence over any file from the image layers at
  that path
- Writes to the volume go directly to the host directory, not to the overlay's
  upper layer
- The volume survives container exit independently of the ephemeral overlay

```rust
let layers = image::layer_dirs(&manifest);
let vol = Volume::open("persistent")?;

Command::new("/bin/sh")
    .with_image_layers(layers)       // multi-layer overlay
    .with_volume(&vol, "/data")      // bind-mounted on top of merged view
    .with_tmpfs("/tmp")              // tmpfs on top too
    .spawn()?;
```

In this example:
- `/data` comes from the volume (host directory), not from any image layer
- `/tmp` is a fresh tmpfs, even if image layers contain a `/tmp`
- Everything else comes from the overlay (merged image layers + upper)

### Read-Only Rootfs with Overlay

When `with_readonly_rootfs()` is combined with overlay, the overlay mount itself
is already a proper mount point, so the self-bind step is skipped. The read-only
remount applies to the merged view — the upper directory is still writable at
the kernel level, but the container cannot write through the merged mount.

Use `with_tmpfs()` to provide writable scratch space on a read-only overlay
container.

### Practical Composition Example

```rust
Command::new("/bin/sh")
    .with_image_layers(layers)           // OCI image as overlay
    .with_volume(&db_vol, "/var/lib/db") // persistent database storage
    .with_bind_mount_ro("/host/cfg", "/etc/app/config")  // read-only config
    .with_tmpfs("/tmp")                  // writable scratch space
    .with_readonly_rootfs()              // immutable base image
    .with_dns(&["8.8.8.8"])             // DNS on top of merged /etc
    .spawn()?;
```

Mount stack from the container's perspective:

```
/              ← overlay merged (read-only remount)
/var/lib/db    ← volume bind mount (read-write, persistent)
/etc/app/config ← host bind mount (read-only)
/etc/resolv.conf ← DNS bind mount
/tmp           ← tmpfs (read-write, ephemeral)
/proc          ← procfs
/sys           ← sysfs
/dev           ← devtmpfs
```

---

## Summary Table

| Feature        | Persistent? | Writable? | Requires Root? | Works with Overlay? |
|----------------|-------------|-----------|----------------|---------------------|
| Bind mount RW  | Yes (host)  | Yes       | Yes*           | Yes (on top)        |
| Bind mount RO  | Yes (host)  | No        | Yes*           | Yes (on top)        |
| Named volume   | Yes (host)  | Yes       | Yes*           | Yes (on top)        |
| tmpfs          | No          | Yes       | Yes*           | Yes (on top)        |
| Overlay upper  | Optional**  | Yes       | Yes*           | N/A (is overlay)    |

\* Requires `Namespace::MOUNT`, which needs root or a user namespace.
\** Upper directory persists on disk but is treated as ephemeral for OCI image containers.
