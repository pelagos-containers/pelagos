# Ongoing Tasks

## Recent: New Integration Tests for NAT + TCP Linking — COMPLETE ✅

### What was done

Added 3 new tests to close gaps found during the web pipeline debugging session:

1. **`test_container_link_tcp`** (linking module) — Cross-container TCP via nc,
   proving TCP works over bridge links (not just ICMP/ping).
2. **`test_nat_iptables_forward_rules`** (networking module) — Verifies iptables
   FORWARD rules exist while NAT is active and are cleaned up afterwards. Catches
   the UFW/Docker `FORWARD policy DROP` regression.
3. **`test_rootfs_path_accepts_filesystem_dir`** + **`test_rootfs_path_rejects_nonexistent`**
   (unit tests in `src/cli/mod.rs`) — Verify `rootfs_path()` accepts filesystem
   directories and rejects nonexistent names.

**Files changed:**
- `tests/integration_tests.rs` — 2 new integration tests (now 76 total)
- `src/cli/mod.rs` — 2 new unit tests
- `docs/INTEGRATION_TESTS.md` — documentation for new tests

---

## Recent: Multi-Container Web Pipeline Example — COMPLETE ✅

### What was done

Created a 3-container web pipeline example demonstrating bridge networking,
container linking, and multi-container orchestration. Fixed NAT to add iptables
FORWARD rules for hosts with UFW/Docker.

**Files changed:**
- Moved `examples/seccomp_demo.rs` → `examples/seccomp_demo/main.rs`
- Moved `examples/secure_container.rs` → `examples/secure_container/main.rs`
- Created `examples/web_pipeline/main.rs` — 3-container pipeline demo (httpd-based)
- Created `examples/net_debug/main.rs` — 2-container network diagnostic
- Created `scripts/install-httpd.sh` — Install busybox-extras via remora
- Fixed `src/network.rs` — Added iptables FORWARD rules for NAT
- Fixed `src/cli/mod.rs` — `rootfs_path()` accepts filesystem paths

**Key lessons:**
- Bridge mode: do NOT pass `Namespace::NET` — the child joins a pre-configured
  named netns via `setns()`, not `unshare(CLONE_NEWNET)`
- Always set `.env("PATH", ALPINE_PATH)` for containers using the Alpine rootfs
- Use `Stdio::Null` for long-running containers to avoid pipe-hang on teardown
- Hosts with UFW/Docker have `iptables FORWARD policy DROP` — must add explicit
  iptables ACCEPT rules in addition to nftables MASQUERADE
- `httpd` requires `busybox-extras` package (not in minimal Alpine rootfs)

---

## Recent: Container Linking + Test Reorganization — COMPLETE ✅

### Commit
`22ec972` — Add container linking and reorganize integration tests into modules

### What was done

**Container-to-container networking via `/etc/hosts` injection:**
- `with_link()` and `with_link_alias()` builder methods on `Command`
- `resolve_container_ip()` looks up bridge IP from CLI/OCI state files
- Hosts temp file + bind-mount in pre_exec (same pattern as DNS)
- `--link` CLI flag for `remora run`
- `bridge_ip` field on `ContainerState` and `OciState`

**Integration test reorganization:**
- All 72 tests wrapped in 11 categorized `mod` blocks
- Categories: `api`, `core`, `capabilities`, `resources`, `security`, `filesystem`, `cgroups`, `networking`, `oci_lifecycle`, `rootless`, `linking`
- Run any category independently: `cargo test --test integration_tests <mod>::`
- Full testing guide at `docs/TESTING.md`

---

## Planned Feature 1: OCI Image Layers 🔜

**Priority:** High — enables `remora pull alpine` instead of manual rootfs setup
**Effort:** Significant Work

### Goal

Pull OCI/Docker images from registries, unpack their layers, and run containers
from them using overlayfs. This replaces the manual rootfs download workflow with
`remora image pull alpine` → `remora run --image alpine /bin/sh`.

### Current State

- Overlay filesystem works: `with_overlay(upper, work)` mounts a single lower dir
  (the chroot path) + writable upper layer.
- Rootfs management: `remora rootfs import/ls/rm` creates symlinks to local dirs.
- No image parsing, no layer extraction, no registry interaction.
- `OverlayConfig` has `upper_dir` and `work_dir` — lower dir is always the chroot path.

### Design

#### Architecture: External download + native unpack

- **Download**: Shell out to `skopeo` (available on Arch: `pacman -S skopeo`). No daemon
  required. Handles registry auth, manifest negotiation, format conversion.
- **Parse + unpack**: Native Rust. Parse OCI layout JSON, extract layer tarballs.
- **Mount**: Extend existing overlayfs to support multiple lower layers.

This avoids implementing HTTP registry protocol, auth tokens, and manifest content
negotiation — all of which skopeo handles perfectly.

#### Storage Layout

```
/var/lib/remora/images/
  <name>_<tag>/                    # OCI image layout (from skopeo)
    oci-layout
    index.json
    blobs/sha256/...

/var/lib/remora/layers/
  <sha256-hex>/                    # Extracted layer (content-addressable, shared)
    bin/
    etc/
    usr/
    ...

/run/remora/containers/<name>/
  upper/                           # Per-container writable layer
  work/                            # overlayfs work dir
```

Layers are content-addressable (keyed by compressed digest). If two images share a
base layer, it's stored once.

#### OCI Image Layout Parsing

New file: `src/image.rs`

Key structs (all serde `Deserialize`):

```rust
pub struct ImageIndex {
    pub schema_version: u32,
    pub manifests: Vec<ImageDescriptor>,
}

pub struct ImageDescriptor {
    pub media_type: String,
    pub digest: String,          // "sha256:abcdef..."
    pub size: u64,
    pub platform: Option<ImagePlatform>,
}

pub struct ImagePlatform {
    pub architecture: String,    // "amd64"
    pub os: String,              // "linux"
}

pub struct ImageManifest {
    pub schema_version: u32,
    pub config: ImageDescriptor,
    pub layers: Vec<ImageDescriptor>,
}

pub struct ImageConfig {
    pub architecture: String,
    pub os: String,
    pub config: Option<ContainerConfig>,
    pub rootfs: ImageRootfs,
}

pub struct ContainerConfig {
    #[serde(rename = "Env")]
    pub env: Option<Vec<String>>,
    #[serde(rename = "Cmd")]
    pub cmd: Option<Vec<String>>,
    #[serde(rename = "Entrypoint")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(rename = "WorkingDir")]
    pub working_dir: Option<String>,
}

pub struct ImageRootfs {
    #[serde(rename = "type")]
    pub fs_type: String,         // "layers"
    pub diff_ids: Vec<String>,
}
```

Public functions:

```rust
/// Pull an image from a registry using skopeo.
pub fn pull_image(reference: &str, name: &str, tag: &str) -> io::Result<PathBuf>

/// Parse an OCI image layout and return layer paths + config.
pub fn load_image(image_dir: &Path) -> io::Result<LoadedImage>

/// Extract a single layer tarball into /var/lib/remora/layers/<digest>/.
/// Skips if already extracted (content-addressable cache).
fn extract_layer(blob_path: &Path, digest: &str, media_type: &str) -> io::Result<PathBuf>

/// Resolve an image reference to extracted layer directories (ordered base-first).
pub fn prepare_image(name: &str, tag: &str) -> io::Result<PreparedImage>
```

`PreparedImage`:
```rust
pub struct PreparedImage {
    pub layer_dirs: Vec<PathBuf>,     // Ordered base-first
    pub config: Option<ContainerConfig>,
}
```

#### Multi-Layer Overlayfs

New builder method:

```rust
/// Mount an image's layers as overlayfs. The layer_dirs are ordered base-first.
/// A per-container upper and work dir are auto-created under /run/remora/.
pub fn with_image_layers(mut self, layer_dirs: Vec<PathBuf>) -> Self
```

This sets `chroot` to the merged dir and configures overlayfs with:
```
lowerdir=<layer_n>:<layer_n-1>:...:<layer_0>
```
(overlayfs `lowerdir` is ordered top-first, so we reverse the base-first order.)

#### New Dependencies

```toml
flate2 = "1"     # gzip decompression for layer tarballs
tar = "0.4"      # tar extraction
```

#### CLI Commands

- `remora image pull <reference>` — download via skopeo
- `remora image ls` — list downloaded images
- `remora image rm <name>` — remove image layout
- `remora run --image <name> [cmd]` — run container from image

#### Tests (5 new integration tests)

1. **`test_image_parse_oci_layout`** — no root: synthetic OCI layout parsing
2. **`test_image_extract_layer`** — no root: gzipped tar extraction
3. **`test_image_layer_caching`** — no root: idempotent extraction
4. **`test_image_multi_layer_overlay`** — root, rootfs: multi-layer mount
5. **`test_image_pull_and_run`** — root, skopeo, internet: end-to-end

#### Notes / Risks

- **skopeo dependency**: External binary, clear error if missing
- **Whiteout files**: `.wh.<name>` markers deferred to enhancement pass
- **Layer extraction requires root** for proper uid/gid ownership
- **Multi-arch**: First pass always pulls `linux/amd64`

---

## Planned Feature 2: `remora exec` — Attach to Running Container 🔜

**Priority:** Medium — quality-of-life for debugging running containers
**Effort:** Moderate

### Goal

Run a new process inside an already-running container's namespaces, similar to
`docker exec` or `nsenter`. Supports both non-interactive (`remora exec <id> ls`)
and interactive (`remora exec -it <id> /bin/sh`) modes.

### Design

#### How it works

1. Look up target container's PID from state.json
2. Probe `/proc/{pid}/ns/*` — compare each inode to PID 1's to detect container namespaces
3. Build a `Command` that joins all detected namespaces via `setns()`
4. Exec the requested command

#### New API

```rust
pub fn exec_in_container(
    pid: i32,
    args: &[&str],
    interactive: bool,
    user: Option<(u32, u32)>,
    env: Option<&[(&str, &str)]>,
    cwd: Option<&str>,
) -> io::Result<Either<Child, InteractiveSession>>
```

#### CLI Command

```
remora exec <container> <command> [args...]
remora exec -it <container> /bin/sh
remora exec --user 1000:1000 <container> whoami
```

#### Tests (4 new integration tests)

1. **`test_exec_basic`** — exec `echo hello` in running container
2. **`test_exec_sees_container_fs`** — exec sees container's mount namespace
3. **`test_exec_interactive`** — interactive PTY exec
4. **`test_exec_container_not_found`** — clear error for missing container

#### Notes / Risks

- Requires root (`setns()` needs `CAP_SYS_ADMIN`)
- No seccomp/capability inheritance in v1
- PID namespace join affects children, not caller (correct behavior)

---

## Previous Tasks — COMPLETE ✅

- `22ec972` — Container linking + test reorganization (72 tests, 11 modules)
- `bff6327` — Fix OCI create PID resolution and kill test for PID namespaces
- `41b78ce` — Full-featured CLI and PID namespace double-fork bug
- Rootless Phase 2 (Pasta networking)

---

## Planned (Deferred)

### AppArmor / SELinux — MAC Profile Support

Deferred: the seccomp + capabilities + masked paths stack is already solid, and MAC requires
system-side setup (profile loading) that most users won't have. Revisit if there's demand.
