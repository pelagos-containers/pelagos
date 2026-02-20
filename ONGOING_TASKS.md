# Ongoing Tasks

## Current Task: `remora build` — Image Build Feature

**Status:** COMPLETE

### Goal

Implement `remora build` to create custom OCI images from a "Remfile" (simplified
Dockerfile). Prerequisite for the multi-container web stack example application.
Buildah-style daemonless model — no daemon, direct filesystem + container spawn.

### CLI Interface

```
remora build -t <tag> [--file <path>] [--network bridge|pasta] [<context-dir>]
```

- `-t / --tag` (required): Name for resulting image (e.g. `myapp:latest`)
- `-f / --file` (optional): Path to Remfile. Default: `<context>/Remfile`
- `--network` (optional): Network mode for RUN steps. Default: `bridge` (root) / `pasta` (rootless)
- `<context>` (positional, default `.`): Build context directory. COPY sources relative to this.

### Remfile Instructions (MVP)

| Instruction | Syntax | Effect |
|-------------|--------|--------|
| `FROM` | `FROM alpine:latest` | Load base image layers + config |
| `RUN` | `RUN apk add curl` | Execute in container, snapshot upper dir as layer |
| `COPY` | `COPY src dest` | Copy from build context into image as new layer |
| `CMD` | `CMD ["arg1"]` or `CMD arg1` | Set default command |
| `ENV` | `ENV KEY=VALUE` | Set environment variable |
| `WORKDIR` | `WORKDIR /app` | Set working directory |
| `EXPOSE` | `EXPOSE 8080` | Metadata only (documentation) |

### New Files

1. **`src/build.rs`** — Core build engine (library module)
   - `Instruction` enum for all supported instructions
   - `parse_remfile(content) -> Vec<Instruction>` — line-by-line parser
   - `execute_build(instructions, context_dir, tag) -> ImageManifest`
   - `create_layer_from_dir(source_dir) -> String` (tar + sha256 + store)
   - `BuildError` enum via `thiserror`

2. **`src/cli/build.rs`** — CLI subcommand handler
   - `BuildArgs` struct with clap derive
   - `cmd_build(args)` — reads Remfile, calls build engine, prints progress

### Changes to Existing Files

3. **`Cargo.toml`** — Add `sha2 = "0.10"` for content-addressable digests

4. **`src/lib.rs`** — Add `pub mod build;`

5. **`src/cli/mod.rs`** — Add `pub mod build;`

6. **`src/main.rs`** — Add `Build(cli::build::BuildArgs)` to `CliCommand`, dispatch

7. **`src/container.rs`** — Add `wait_preserve_overlay(&mut self)` to `Child`
   - Does everything `wait()` does (waitpid, teardown cgroup/network/pasta/fuse/dns/hosts)
   - **Skips removing overlay base dir**, returns its path instead
   - Caller (build engine) tars upper dir then cleans up

### Build Execution Flow

**FROM:** Load base `ImageManifest` via `image::load_image()`. Init accumulated layers + config.

**RUN:**
1. Build `Command::new("/bin/sh").args(&["-c", cmd])` with `.with_image_layers(current_layers)`,
   accumulated ENV vars, WORKDIR, and network mode
2. `child.wait_preserve_overlay()` → get exit status + overlay base path
3. If exit != 0, abort build with error
4. Upper dir at `overlay_base/upper/` — if non-empty, `create_layer_from_dir(upper)` → digest
5. Append digest to layers, clean up overlay base dir

**COPY:**
1. Resolve src relative to context dir
2. Create temp dir with dest path structure, copy source into it
3. `create_layer_from_dir(temp_dir)` → digest, append to layers

**CMD/ENV/WORKDIR/EXPOSE:** Mutate accumulated `ImageConfig` (no layer creation)

**Final:** `image::save_image(manifest)` with all accumulated layers + config

### Layer Creation Algorithm

```
1. Create tempfile for tar.gz
2. tar + gzip source_dir contents → tempfile
3. sha256(tar.gz bytes) → digest string
4. If layer_exists(digest), return early (dedup)
5. Move source_dir → layers_dir/<hex>/
6. Return digest
```

Stores the extracted directory (not tar.gz) in layer store, matching existing `extract_layer()` convention.

### Example Remfile

```
FROM alpine:latest
RUN apk add --no-cache curl
COPY index.html /var/www/index.html
ENV APP_PORT=8080
WORKDIR /var/www
CMD ["httpd", "-f", "-p", "8080", "-h", "/var/www"]
EXPOSE 8080
```

### Verification

1. `cargo test --lib` — parser unit tests
2. `cargo build` — compiles clean
3. Manual test (user runs as root):
   ```bash
   mkdir /tmp/test-build && cd /tmp/test-build
   echo '<h1>Hello</h1>' > index.html
   cat > Remfile <<'EOF'
   FROM alpine:latest
   RUN apk add --no-cache curl
   COPY index.html /srv/index.html
   CMD ["/bin/sh", "-c", "echo built-ok"]
   EOF
   sudo -E remora build -t test-build:latest .
   sudo -E remora run test-build:latest
   # Should print "built-ok"
   ```

### Implementation Order

1. Add `sha2` to `Cargo.toml`
2. Add `wait_preserve_overlay()` to `Child` in `container.rs`
3. Create `src/build.rs` — parser + build engine + layer creation
4. Create `src/cli/build.rs` — CLI handler
5. Wire up in `lib.rs`, `cli/mod.rs`, `main.rs`
6. Add parser unit tests in `src/build.rs`
7. Update `ONGOING_TASKS.md` and `CLAUDE.md`

### Key Existing Code to Reuse

- `src/image.rs`: `ImageManifest`, `ImageConfig`, `layer_dir()`, `layer_exists()`, `save_image()`, `load_image()`, `layer_dirs()`
- `src/cli/run.rs`: `normalise_image_reference()`, `build_image_run()` pattern for loading images + building Commands
- `src/container.rs`: `Command` builder with `.with_image_layers()`, `Child` struct with overlay management
- `src/paths.rs`: `layers_dir()`, `images_dir()`, `is_rootless()`

---

## Example Applications (After Build Feature)

Three demo apps to deeply test and showcase remora's capabilities:

**1. Multi-Container Web Stack** (needs `remora build`)
- Bridge-networked: container A (web server) + container B (backend)
- `--link` for service discovery, `--port` for host exposure
- Named volume for shared data persistence

**2. Build Sandbox**
- Rootless container that compiles user-provided code
- Read-only rootfs + tmpfs /tmp, resource limits, seccomp + cap-drop ALL

**3. CI Test Runner**
- Pull image, run test suite, collect exit code
- `--env`, `--bind`, `--workdir`, detached mode + `logs --follow`

---

## Current Capabilities (v0.2.1)

### Fully Working (E2E Tested — 81 pass, 0 fail, 1 skip)

| Category | Features |
|----------|----------|
| Lifecycle | foreground, detached, ps, stop, rm, logs, name collision |
| Images | pull (anonymous, Docker Hub), multi-layer overlay, ls, rm |
| Exec | command in running container, PTY (-i), env/workdir/user |
| Networking | loopback, bridge+IPAM, NAT+MASQUERADE, port forwarding, DNS, pasta |
| Filesystem | overlay CoW, bind RW/RO, tmpfs, named volumes, read-only rootfs |
| Security | seccomp (default+minimal), capabilities, no-new-privs, masked paths, sysctl |
| Resources | cgroups v2 (memory, CPU quota/shares, PIDs), rlimits |
| OCI | create/start/state/kill/delete lifecycle, config.json parsing |
| Rootless | images, overlay (native userxattr + fuse-overlayfs fallback), pasta, cgroups v2 |

### Known Limitations

- **PID namespace**: works in library API, architectural limitation in CLI foreground mode
- **No daemon mode**: CLI tool and library only, no background service
- **No AppArmor/SELinux**: MAC profile support deferred; seccomp+caps stack is solid
- **No authenticated registry pulls**: anonymous only (Docker Hub public images)
- **No I/O bandwidth cgroups**: no block device throttling
- **No CNI plugins**: intentional — native networking approach instead
- **Rootless overlay**: requires kernel 5.11+ (userxattr) or fuse-overlayfs installed
- **Alpine binary paths**: utilities like `id`, `env`, `wc` live in `/usr/bin/`, not `/bin/`

---

## Completed Phases

### Stress Tests (v0.2.1)
**COMPLETE** — 18 pass, 0 fail, 0 skip. All 7 sections passing.

### E2E Bug Fixes (v0.2.1)
**COMPLETE** — Fixed 4 bugs found by E2E suite.

### Phase A+B: Storage Path Abstraction + Rootless Overlay (v0.2.0)
**RELEASED** — rootless image pull and container run with single-UID mapping.

### Phase D: Minimal `/dev` Setup
**COMPLETE** — tmpfs + safe devices replacing host /dev bind-mount.

### Phase C: Multi-UID Mapping via Subordinate Ranges
**COMPLETE** — `newuidmap`/`newgidmap` helpers with pipe+thread sync.

### Phase E: Rootless Cgroup v2 Delegation
**COMPLETE** — direct cgroupfs writes under user's delegated cgroup scope.

### Rootless E2E Test Script
**COMPLETE** — `scripts/test-rootless.sh` covering all rootless phases.

---

## Previous Releases

### v0.2.1 — E2E Bug Fixes
Pre_exec ordering, proc mount path, seccomp minimal, exec workdir. E2E suite + stress tests.

### v0.2.0 — Rootless Mode
Storage path abstraction, rootless overlay, multi-UID mapping, cgroup delegation.

### v0.1.0 — Initial Release
Full feature set: namespaces, seccomp, capabilities, cgroups v2, overlay, networking,
OCI image pull, container exec, OCI runtime compliance, interactive PTY.
