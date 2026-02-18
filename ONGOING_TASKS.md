# Ongoing Tasks

## Planned Feature 1: Container-to-Container Networking 🔜

**Priority:** High — enables multi-container workflows (web + db, microservices)
**Effort:** Moderate

### Goal

Allow containers on the same `remora0` bridge to discover and communicate with each
other by name, without needing to know IP addresses. This is Remora's equivalent of
Docker's `--link` / user-defined bridge DNS.

### Current State

- Bridge networking (N2) already assigns each container a 172.19.0.x/24 IP on the
  shared `remora0` bridge. Containers on the bridge **can already reach each other by IP**.
- DNS injection (N5) writes a static `/etc/resolv.conf` pointing to external servers.
- Container state tracks name and PID, but **not** the assigned bridge IP.
- No mechanism exists for name→IP resolution between containers.

### Design

#### Approach: `/etc/hosts` injection (simple, no daemon)

Rather than running a DNS server (like Docker's embedded DNS at 127.0.0.11), inject
`/etc/hosts` entries. This is simpler, requires no background daemon, and works with
all software that respects `/etc/hosts` (essentially everything via glibc/musl NSS).

**Why not a DNS server?**
- Requires a long-running daemon or in-process UDP listener
- Adds significant complexity (DNS protocol, caching, TTL)
- `/etc/hosts` achieves the same result for container-to-container name resolution
- Docker Compose also fell back to `/etc/hosts` for `--link` before user-defined networks

#### New API

```rust
// Link to another running container by name — resolves its bridge IP and
// adds a /etc/hosts entry inside the new container.
.with_link("db-container")

// Link with alias (container named "postgres-primary" reachable as "db")
.with_link_alias("postgres-primary", "db")
```

#### Implementation

**Step 1: Persist bridge IP in container state**

Both state systems need the container's bridge IP:

`src/cli/mod.rs` — add to `ContainerState`:
```rust
pub bridge_ip: Option<String>,   // e.g. "172.19.0.5"
```

`src/oci.rs` — add to `OciState`:
```rust
pub bridge_ip: Option<String>,
```

Populated after `setup_bridge_network()` returns the `NetworkSetup` (which already
contains `container_ip`). Written to state.json in both CLI and OCI paths.

**Step 2: Resolve links at spawn time**

`src/container.rs`:
```rust
links: Vec<(String, String)>,  // (container_name, alias)

pub fn with_link(mut self, name: &str) -> Self {
    self.links.push((name.to_string(), name.to_string()));
    self
}

pub fn with_link_alias(mut self, name: &str, alias: &str) -> Self {
    self.links.push((name.to_string(), alias.to_string()));
    self
}
```

Before fork (in `spawn()`):
1. For each link, look up the target container's state.json
2. Read its `bridge_ip` field
3. Error if target isn't running or has no bridge IP
4. Collect `Vec<(String, String)>` of `(alias, ip)` pairs

**Step 3: Inject /etc/hosts in pre_exec**

Same mechanism as DNS injection — write a temp file, bind-mount it:
1. Parent creates `/run/remora/hosts-{pid}-{n}/hosts` containing:
   ```
   127.0.0.1   localhost
   172.19.0.3  db-container
   172.19.0.5  web  web-container
   ```
2. Pre-exec bind-mounts it over `{effective_root}/etc/hosts` (before chroot)
3. Cleanup: remove temp dir in `wait()`

**Step 4: CLI integration**

`src/cli/run.rs` — add `--link` flag:
```
--link <name>          Link to another container (name resolution via /etc/hosts)
--link <name>:<alias>  Link with custom alias
```

Multiple `--link` flags allowed (clap `multiple_occurrences`).

#### Lookup Helpers

New function in `src/cli/mod.rs`:
```rust
/// Look up a running container's bridge IP by name.
/// Searches both CLI state (/run/remora/containers/) and OCI state (/run/remora/).
pub fn resolve_container_ip(name: &str) -> io::Result<String>
```

This searches:
1. `/run/remora/containers/{name}/state.json` (CLI containers)
2. `/run/remora/{name}/state.json` (OCI containers)

Returns the `bridge_ip` field, or an error if not found / not running / no bridge IP.

#### Tests (4 new integration tests)

1. **`test_container_link_hosts`** — root, rootfs: Start container A on bridge, start
   container B with `--link A`, verify B's `/etc/hosts` contains A's IP.

2. **`test_container_link_alias`** — root, rootfs: Start A, start B with
   `with_link_alias("A", "db")`, verify B can resolve "db" to A's IP in `/etc/hosts`.

3. **`test_container_link_ping`** — root, rootfs: Start A (sleep), start B with link,
   run `ping -c1 A` from B → succeeds.

4. **`test_container_link_missing`** — root: Attempt `with_link("nonexistent")`,
   verify spawn returns an error.

#### File Changes

| File | Changes |
|------|---------|
| `src/container.rs` | `links` field, `with_link()`, `with_link_alias()`, hosts file creation + bind-mount in pre_exec, cleanup in `wait()` |
| `src/cli/mod.rs` | `bridge_ip` field in `ContainerState`, `resolve_container_ip()` helper |
| `src/cli/run.rs` | `--link` CLI flag, parse and pass to builder |
| `src/oci.rs` | `bridge_ip` field in `OciState` (for future OCI container linking) |
| `tests/integration_tests.rs` | 4 new tests |
| `docs/INTEGRATION_TESTS.md` | 4 new entries |

#### Notes / Risks

- **Static resolution**: Links are resolved at spawn time. If container A restarts with
  a new IP, container B's `/etc/hosts` becomes stale. This matches Docker `--link`
  behavior. Dynamic resolution would require a DNS server (future enhancement).
- **Bridge-only**: Links only work when both containers use `NetworkMode::Bridge`.
  Error clearly if target isn't on bridge.
- **No circular dependency detection**: If A links B and B links A, both must exist
  before the other starts. This is a limitation (same as Docker `--link`).

---

## Planned Feature 2: OCI Image Layers 🔜

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

The current `with_overlay()` takes a single upper+work dir pair. The lower dir is
always the chroot path. For images, we need multiple lower layers.

**Option A: New builder method (preferred)**

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

Upper + work dirs auto-created at `/run/remora/containers/<name>/upper/` and `work/`.

**Option B: Extend existing with_overlay**

Add `lower_dirs: Vec<PathBuf>` to `OverlayConfig`. If populated, ignore chroot as
lower and use these instead. Less clean but fewer API changes.

**Decision: Option A** — cleaner separation, doesn't conflate image-based and
manual overlay workflows.

#### New Dependencies

```toml
flate2 = "1"     # gzip decompression for layer tarballs
tar = "0.4"      # tar extraction
```

`zstd` deferred (rare in practice; gzip is the default for Docker Hub images).

#### CLI Commands

**`remora image pull <reference>`**

```bash
remora image pull alpine                  # → docker.io/library/alpine:latest
remora image pull alpine:3.19             # → docker.io/library/alpine:3.19
remora image pull ghcr.io/foo/bar:v1      # → custom registry
```

Implementation:
1. Normalize reference (add `docker.io/library/` prefix if bare name)
2. Compute storage name (`alpine_latest`, `ghcr.io_foo_bar_v1`)
3. Call `skopeo copy docker://{ref} oci:/var/lib/remora/images/{name}:{tag}`
4. Parse OCI layout, extract all layers to `/var/lib/remora/layers/`
5. Print summary: image name, layer count, total size

**`remora image ls`**

List downloaded images. Walk `/var/lib/remora/images/`, parse each `index.json`
for tag and layer count.

**`remora image rm <name>`**

Remove image layout dir. Layers are NOT removed (shared, content-addressable).
Add `remora image prune` later to GC unreferenced layers.

**`remora run --image <name> [cmd]`**

1. Call `prepare_image(name, tag)` → get layer dirs + config
2. If no cmd specified, use config's `Entrypoint` + `Cmd`
3. Set env from config's `Env`
4. Set cwd from config's `WorkingDir`
5. Call `with_image_layers(layer_dirs)` on the builder
6. Proceed with normal container lifecycle

#### Implementation Phases

**Phase A: Core parsing + extraction** (`src/image.rs`)
- OCI layout structs
- `load_image()` — parse index.json → manifest → config + layer list
- `extract_layer()` — decompress gzip, extract tar to layer dir
- Layer caching (skip if dir exists)

**Phase B: Multi-layer overlayfs** (`src/container.rs`)
- `with_image_layers(layer_dirs)` builder method
- Pre_exec: mount overlayfs with multi-lower `lowerdir=` string
- Auto-create upper/work dirs
- Cleanup in `wait()`

**Phase C: CLI integration**
- `remora image pull/ls/rm` subcommands (`src/cli/image.rs`)
- `--image` flag in `remora run` (`src/cli/run.rs`)
- Apply image config defaults (env, cmd, workdir)

**Phase D: skopeo integration**
- `pull_image()` — shell out to skopeo
- Reference normalization (bare name → docker.io/library/name:latest)
- Validate skopeo is installed, clear error if not

#### Tests (5 new integration tests)

1. **`test_image_parse_oci_layout`** — no root: Create a synthetic OCI layout on
   disk (hand-crafted JSON + a tiny layer tarball), parse with `load_image()`,
   verify layer count and config fields.

2. **`test_image_extract_layer`** — no root: Create a gzipped tar, call
   `extract_layer()`, verify files appear in output dir.

3. **`test_image_layer_caching`** — no root: Extract same layer twice, verify
   second call is a no-op (dir already exists).

4. **`test_image_multi_layer_overlay`** — root, rootfs: Create two layer dirs
   with different files, use `with_image_layers()`, run `ls /` in container,
   verify files from both layers visible.

5. **`test_image_pull_and_run`** — root, requires skopeo + internet: Pull
   `alpine:latest`, run `/bin/echo hello`, verify output. Skip if skopeo
   not installed or no internet.

#### File Changes

| File | Changes |
|------|---------|
| `src/image.rs` | NEW — OCI image parsing, layer extraction, skopeo integration |
| `src/lib.rs` | Add `pub mod image;` |
| `src/container.rs` | `with_image_layers()`, multi-lower overlayfs mount in pre_exec |
| `src/cli/image.rs` | NEW — `remora image pull/ls/rm` subcommands |
| `src/cli/mod.rs` | Add `pub mod image;` |
| `src/cli/run.rs` | `--image` flag, image config defaults |
| `src/main.rs` | Add `Image { Pull | Ls | Rm }` subcommand |
| `Cargo.toml` | Add `flate2 = "1"`, `tar = "0.4"` |
| `tests/integration_tests.rs` | 5 new tests |
| `docs/INTEGRATION_TESTS.md` | 5 new entries |

#### Notes / Risks

- **skopeo dependency**: External binary, must be installed. Clear error message if
  missing. Could later add a pure-Rust registry client, but that's substantial work.
- **Whiteout files**: OCI layers use `.wh.<name>` marker files for deletions.
  First pass: skip whiteout conversion (works for most images). Enhancement: convert
  `.wh.<name>` to overlayfs character device 0/0 and `.wh..wh..opq` to
  `trusted.overlay.opaque=y` xattr during extraction.
- **Layer extraction requires root** for proper ownership (tar contains uid/gid).
  `tar` crate supports `set_preserve_permissions(true)`.
- **Multi-arch**: First pass: always pull `linux/amd64`. Enhancement: detect host
  arch and select matching platform from image index.

---

## Planned Feature 3: `remora exec` — Attach to Running Container 🔜

**Priority:** Medium — quality-of-life for debugging running containers
**Effort:** Moderate

### Goal

Run a new process inside an already-running container's namespaces, similar to
`docker exec` or `nsenter`. Supports both non-interactive (`remora exec <id> ls`)
and interactive (`remora exec -it <id> /bin/sh`) modes.

### Current State

- Namespace joining already works: `with_namespace_join(path, ns)` calls `setns()`
  in pre_exec (Step 6). Used by OCI namespace joining and bridge networking.
- PTY/interactive sessions work: `spawn_interactive()` allocates a PTY pair, relays
  stdin/stdout, handles SIGWINCH.
- Container state persists PID in state.json (both CLI and OCI).
- No mechanism to discover which namespaces a container is using.

### Design

#### How it works

1. Look up target container's PID from state.json
2. Open `/proc/{pid}/ns/*` for each namespace type
3. Build a new `Command` that joins all those namespaces via `setns()`
4. Exec the requested command inside the joined namespaces

This is essentially what `nsenter(1)` does, but integrated into remora's builder API.

#### Which namespaces to join

For exec, we want to join ALL namespaces the container has. The problem: state.json
doesn't record which namespaces were created.

**Solution**: Probe `/proc/{pid}/ns/` and compare each namespace inode to the
init process (PID 1). If the container's namespace inode differs from PID 1's,
the container has its own namespace — join it.

```rust
/// Detect which namespaces a process has that differ from the host.
fn detect_container_namespaces(pid: i32) -> io::Result<Vec<(PathBuf, Namespace)>> {
    let ns_types = [
        ("mnt", Namespace::MOUNT),
        ("uts", Namespace::UTS),
        ("ipc", Namespace::IPC),
        ("pid", Namespace::PID),
        ("net", Namespace::NET),
        ("user", Namespace::USER),
        ("cgroup", Namespace::CGROUP),
    ];
    let mut result = Vec::new();
    for (name, flag) in &ns_types {
        let container_ns = format!("/proc/{}/ns/{}", pid, name);
        let host_ns = format!("/proc/1/ns/{}", name);
        // Compare inode numbers (different inode = different namespace)
        let c_stat = fs::metadata(&container_ns)?;
        let h_stat = fs::metadata(&host_ns)?;
        if c_stat.ino() != h_stat.ino() {
            result.push((PathBuf::from(&container_ns), *flag));
        }
    }
    Ok(result)
}
```

#### PID namespace subtlety

Joining a PID namespace via `setns(CLONE_NEWPID)` affects the **children** of the
calling process, not the caller itself. So the exec'd process will be in the
container's PID namespace (seeing the container's PID tree), but the intermediate
process that calls setns is not.

This is fine for exec — the exec'd command runs in the target namespace after fork+exec.

#### Pre-exec sequence for exec (different from create)

Exec is simpler than create — we're joining existing namespaces, not creating new ones:

1. `setns()` for each detected namespace (mnt, uts, ipc, pid, net, user, cgroup)
2. `chdir(cwd)` — change to requested working directory
3. Set UID/GID if specified (default: same as container's process)
4. Apply seccomp filter if the container had one (optional, skip for v1)
5. `exec(command)`

**No chroot needed** — joining the mount namespace gives us the container's
filesystem view automatically.

**No unshare needed** — we're joining, not creating.

#### New API

```rust
/// Execute a command inside an existing container's namespaces.
/// Returns a Child (non-interactive) or InteractiveSession (interactive).
pub fn exec_in_container(
    pid: i32,
    args: &[&str],
    interactive: bool,
    user: Option<(u32, u32)>,    // (uid, gid) override
    env: Option<&[(&str, &str)]>, // additional env vars
    cwd: Option<&str>,           // working directory override
) -> io::Result<Either<Child, InteractiveSession>>
```

Internally, this:
1. Calls `detect_container_namespaces(pid)`
2. Builds a `Command::new(args[0]).args(&args[1..])`
3. Adds `.with_namespace_join(path, ns)` for each detected namespace
4. Sets uid/gid/env/cwd as specified
5. Calls `.spawn()` or `.spawn_interactive()` based on `interactive` flag

#### CLI Command

```
remora exec <container> <command> [args...]
remora exec -it <container> /bin/sh
remora exec --user 1000:1000 <container> whoami
remora exec --env FOO=bar <container> env
remora exec --workdir /tmp <container> pwd
```

`src/cli/exec.rs`:
```rust
#[derive(Parser)]
pub struct ExecArgs {
    /// Container name or ID
    container: String,
    /// Command to execute
    command: Vec<String>,
    /// Interactive mode (allocate PTY)
    #[clap(short = 'i', long = "interactive")]
    interactive: bool,
    /// Allocate a TTY
    #[clap(short = 't', long = "tty")]
    tty: bool,
    /// User (uid:gid)
    #[clap(short, long)]
    user: Option<String>,
    /// Environment variables
    #[clap(short, long, multiple_occurrences = true)]
    env: Vec<String>,
    /// Working directory
    #[clap(short, long)]
    workdir: Option<String>,
}
```

`-it` (both `-i` and `-t`) triggers `spawn_interactive()`. Either flag alone also
triggers it (matching Docker behavior where `-t` implies `-i`).

#### Container Lookup

Exec needs to find the container PID. Search order:
1. CLI state: `/run/remora/containers/{name}/state.json`
2. OCI state: `/run/remora/{name}/state.json`

Reuse `resolve_container_pid()` helper (new in `src/cli/mod.rs`):
```rust
pub fn resolve_container_pid(name: &str) -> io::Result<i32>
```

Verify the process is alive (`kill(pid, 0) == 0`) before proceeding.

#### Implementation Phases

**Phase A: Namespace detection** (`src/container.rs`)
- `detect_container_namespaces(pid)` — compare inodes against PID 1
- Unit-testable (can mock /proc paths)

**Phase B: exec_in_container function** (`src/container.rs`)
- Build Command with namespace joins
- Support interactive and non-interactive modes
- Handle uid/gid/env/cwd overrides

**Phase C: CLI integration**
- `src/cli/exec.rs` — ExecArgs, cmd_exec
- `src/main.rs` — add Exec subcommand
- Container lookup helper

**Phase D: OCI exec (stretch goal)**
- `remora exec` on OCI-lifecycle containers (lookup in `/run/remora/{id}/`)
- State tracking: record exec PIDs in state.json (optional)

#### Tests (4 new integration tests)

1. **`test_exec_basic`** — root, rootfs: Start a container (sleep 60), exec
   `echo hello` inside it, verify output is "hello".

2. **`test_exec_sees_container_fs`** — root, rootfs: Start container with a tmpfs
   at `/scratch`, write a file there, exec `cat /scratch/file`, verify contents.
   Proves exec joins the mount namespace.

3. **`test_exec_interactive`** — root, rootfs: Start container, exec interactive
   `/bin/sh`, send `echo test\n`, verify output. (May need to use PTY test harness.)

4. **`test_exec_container_not_found`** — root: Exec on nonexistent container,
   verify clear error message.

#### File Changes

| File | Changes |
|------|---------|
| `src/container.rs` | `detect_container_namespaces()`, `exec_in_container()` |
| `src/cli/exec.rs` | NEW — ExecArgs, cmd_exec |
| `src/cli/mod.rs` | `resolve_container_pid()` helper, add `pub mod exec;` |
| `src/main.rs` | Add `Exec(cli::exec::ExecArgs)` subcommand |
| `tests/integration_tests.rs` | 4 new tests |
| `docs/INTEGRATION_TESTS.md` | 4 new entries |

#### Notes / Risks

- **Requires root**: `setns()` on most namespace types requires `CAP_SYS_ADMIN`.
  Rootless exec could work if the target container uses a USER namespace and the
  caller has the right uid mapping. Defer rootless exec to later.
- **PID namespace exec**: The exec'd process joins the container's PID namespace
  but gets a new PID (not PID 1). This is correct — only the original container
  process is PID 1.
- **No seccomp inheritance**: The exec'd process does NOT inherit the container's
  seccomp filter (seccomp is per-thread, not per-namespace). Could optionally
  re-apply the same filter. Defer for v1.
- **No capability inheritance**: exec'd process runs with caller's capabilities.
  Could drop to match container. Defer for v1.
- **Mount namespace timing**: Must `setns(mnt)` before accessing container paths.
  The existing pre_exec ordering (Step 6: setns after chroot) won't work for exec
  because there's no chroot. For exec, setns should happen early in pre_exec,
  and the rest of the setup (uid/gid, cwd) happens after.

---

## Previous Task: OCI PID Namespace Fix — COMPLETE ✅

### Commit
`bff6327` — Fix OCI create PID resolution and kill test for PID namespaces

---

## Previous Task: Full-Featured Container CLI — COMPLETE ✅
## Previous Task: Rootless Phase 2 (Pasta Networking) — COMPLETE ✅

---

## Planned (Deferred)

### AppArmor / SELinux — MAC Profile Support

Deferred: the seccomp + capabilities + masked paths stack is already solid, and MAC requires
system-side setup (profile loading) that most users won't have. Revisit if there's demand.
