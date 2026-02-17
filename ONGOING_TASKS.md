# Ongoing Tasks

## Current Task: OCI Phase 2 — capabilities + maskedPaths/readonlyPaths

### Context

OCI (Open Container Initiative) compliance allows Remora to interoperate with standard
container tooling: Kubernetes, containerd, BuildKit, and anything that speaks the
OCI Runtime Specification v1.0.2.

An OCI runtime must implement five lifecycle CLI subcommands against a **bundle** — a
directory containing `config.json` and `rootfs/`. The spec is explicit about required
vs. optional fields; we implement the minimum viable set first.

---

### The Hardest Part: create/start Split

Currently, Remora forks a child and immediately execs the user program (pre_exec →
exec in one shot). OCI requires that `create` sets up the container environment but
suspends execution until `start` is called — potentially by a separate process.

This requires a synchronization mechanism that persists across two separate process
invocations (`remora create` then later `remora start`). We use a Unix socket:

```
remora create mycontainer /bundle
  │
  ├─ parse config.json
  ├─ create /run/remora/mycontainer/
  ├─ create "ready" pipe (parent reads, child writes)
  ├─ fork child
  │    │
  │    └─ pre_exec:
  │         set up namespaces, mounts, etc.  (same as today)
  │         create + listen on /run/remora/mycontainer/exec.sock
  │         write byte to ready-pipe  ← signals "created" state
  │         accept connection on exec.sock
  │         read byte from connection  ← blocks here until "start"
  │         return from pre_exec → exec happens
  │
  ├─ parent reads ready byte
  ├─ write state.json (status=created, pid=child_pid)
  └─ exit

remora start mycontainer
  ├─ read state.json, verify status=created
  ├─ connect to /run/remora/mycontainer/exec.sock
  ├─ write one byte  ← unblocks the child
  ├─ update state.json (status=running)
  └─ exit
```

The child is alive but blocked between `create` and `start`. Once `start` writes the
byte, the child returns from pre_exec and exec is called.

**Process liveness / stopped state:** `remora state` determines liveness dynamically:
- `kill(pid, 0) == 0` → process is alive → status is "running"
- `ESRCH` → process exited → status is "stopped"
- state.json "created" + process gone → status is "stopped"

---

### API Shape

```bash
# OCI CLI interface (implemented in main.rs)
remora create  <id> <bundle>   # set up container, suspend before exec
remora start   <id>            # signal child to exec
remora state   <id>            # print JSON state to stdout
remora kill    <id> <signal>   # send signal to container process
remora delete  <id>            # tear down resources, remove state dir
```

Existing `remora` interactive CLI flags remain for non-OCI use; OCI subcommands are
a new command group (add `clap` subcommands).

---

### config.json Fields — First Pass

#### Must implement

| Field | Notes |
|-------|-------|
| `ociVersion` | Validate format; reject unknown major versions |
| `root.path` | Relative to bundle dir; chroot target |
| `root.readonly` | Map to `with_readonly_rootfs(true)` |
| `process.args` | The executable + arguments |
| `process.cwd` | Set working directory after exec |
| `process.env` | Environment variables |
| `process.user.uid` / `.gid` | Map to `with_uid` / `with_gid` |
| `process.noNewPrivileges` | Map to `with_no_new_privileges` |
| `process.terminal` | Map to `spawn_interactive()` |
| `hostname` | Map to UTS namespace + `sethostname` (already done via namespaces) |
| `linux.namespaces` | `type` → our `Namespace` flags; `path` → `with_namespace_join` |
| `linux.uidMappings` / `gidMappings` | Map to `with_uid_maps` / `with_gid_maps` |
| `mounts` | Process in order; map to `with_bind_mount` / `with_tmpfs` |

#### Defer (phase 2)

- `hooks` — all lifecycle hook types
- `linux.resources` — cgroup limits (Remora has them; OCI format differs)
- `linux.seccomp` — OCI seccomp format differs from Remora's native API
- `linux.devices` — custom device nodes
- `linux.sysctl` — kernel parameters
- `linux.maskedPaths` / `readonlyPaths` — trivial to add later
- `process.capabilities` — capability sets
- `process.rlimits` — resource limits
- `annotations` — store but do not act on

---

### State Object

Stored at `/run/remora/<id>/state.json`:

```json
{
  "ociVersion": "1.0.2",
  "id": "mycontainer",
  "status": "running",
  "pid": 4242,
  "bundle": "/absolute/path/to/bundle"
}
```

Valid transitions: `creating` → `created` → `running` → `stopped` → [deleted]

---

### File Changes

#### `Cargo.toml`

Add to `[dependencies]`:
```toml
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

#### `src/oci.rs` (new file)

- `OciConfig` struct — top-level config.json deserialization (serde)
- `OciState` struct — state.json read/write
- `state_dir(id) -> PathBuf` — returns `/run/remora/{id}/`
- `state_path(id) -> PathBuf` — returns `/run/remora/{id}/state.json`
- `exec_sock_path(id) -> PathBuf` — returns `/run/remora/{id}/exec.sock`
- `read_state(id) -> io::Result<OciState>`
- `write_state(id, state) -> io::Result<()>`
- `config_from_bundle(bundle: &Path) -> io::Result<OciConfig>`
- `build_command(config: &OciConfig, bundle: &Path) -> Result<Command, Error>` —
  translates config.json fields into a `remora::container::Command` builder chain

#### `src/main.rs`

Add clap subcommands:

```rust
#[derive(Subcommand)]
enum OciCmd {
    Create { id: String, bundle: PathBuf },
    Start  { id: String },
    State  { id: String },
    Kill   { id: String, signal: String },
    Delete { id: String },
}
```

Each subcommand implemented as a free function:
- `cmd_create(id, bundle)` — parse config, build Command, fork, handle sync socket
- `cmd_start(id)` — read state, connect to exec.sock, write byte, update state
- `cmd_state(id)` — read state, check liveness, print JSON
- `cmd_kill(id, signal)` — read state, `kill(pid, sig)`
- `cmd_delete(id)` — verify stopped, teardown, remove state dir

#### `src/container.rs`

Add a `with_oci_sync(sock_path: PathBuf) -> Self` builder (or handle inside `cmd_create`
using a low-level approach). The sync socket logic runs at the end of pre_exec, after
all setup.

Alternatively, implement the sync socket entirely in `src/oci.rs` via a raw
`unsafe { command.inner.pre_exec(...) }` before calling `spawn()`. Avoids leaking OCI
concerns into the core library API.

---

### Integration Tests — 4 new tests

No root-level `#[serial]` needed (containers use unique IDs).

**`test_oci_create_start_state`**
Write a minimal bundle (config.json + rootfs symlink). Run `create`, check `state`
returns `created`, run `start`, check `state` returns `running`, wait for exit, check
`state` returns `stopped`.

**`test_oci_kill`**
Spawn a long-running container (`sleep 60`), `kill` with `SIGTERM`, verify process exits.

**`test_oci_delete_cleanup`**
After a container exits, `delete` removes `/run/remora/<id>/`.

**`test_oci_bundle_mounts`**
Bundle config.json with a `mounts` entry; verify the mount is visible inside the container.

---

### New Dependencies

- `serde = { version = "1", features = ["derive"] }` — JSON deserialization
- `serde_json = "1"` — config.json / state.json parsing and output

---

### Conformance Testing (after initial implementation)

```bash
git clone https://github.com/opencontainers/runtime-tools
cd runtime-tools
make runtimetest validation-executables
sudo RUNTIME=/path/to/remora ./test_runtime.sh
```

The conformance suite injects a `runtimetest` binary into the rootfs that validates
internal container state. A first-pass implementation will pass lifecycle tests;
resource limit and seccomp tests require phase-2 work.

---

### Verification

1. `cargo build` — zero warnings
2. `cargo test --lib` — all unit tests pass
3. User runs: `sudo -E cargo test --test integration_tests`
4. Manual smoke test:
   ```bash
   mkdir -p /tmp/bundle/rootfs
   # (copy alpine-rootfs into /tmp/bundle/rootfs)
   cat > /tmp/bundle/config.json << 'EOF'
   {"ociVersion":"1.0.2","root":{"path":"rootfs"},"process":{"args":["/bin/sh","-c","echo hello"],"cwd":"/"},"linux":{"namespaces":[{"type":"mount"},{"type":"uts"},{"type":"pid"}]}}
   EOF
   sudo remora create test1 /tmp/bundle
   sudo remora state test1    # should show "created"
   sudo remora start test1
   sudo remora state test1    # should show "stopped"
   sudo remora delete test1
   ```

---

### Notes / Risks

- The Unix socket approach for create/start sync is simple and robust; it avoids
  needing a background monitor process
- Blocking in pre_exec is safe here: pre_exec runs after fork, so the parent is
  unblocked (it reads the ready pipe); the child blocks until `start`
- `process.cwd` must be set AFTER chroot (it's relative to the new root); currently
  we don't set cwd explicitly — the pre_exec leaves us at `/` after chroot
- For the `remora state` check: if `status` is `created` or `running` in state.json
  but `kill(pid, 0)` returns ESRCH, report `stopped` (process exited unexpectedly)
- OCI says `start` errors if the container is not in `created` state — enforce this
- OCI `delete` errors if the container is not in `stopped` state — enforce this
- The conformance test suite requires a `runtimetest` binary inside the container
  rootfs — our alpine-rootfs won't have it; we'll need to build it and copy it in

---

## Planned (after OCI)

1. **Rootless Mode** — discuss slirp4netns vs pasta before implementing

---

## Completed Tasks

### OCI Compliance (Phase 1) ✅

Implemented the five OCI Runtime Spec v1.0.2 lifecycle subcommands:

- **`src/oci.rs`** (new): `OciConfig` / `OciState` serde types; path helpers; `build_command()`
  translating `config.json` fields to `container::Command`; `cmd_create/start/state/kill/delete`
- **`src/container.rs`**: added `oci_sync: Option<(i32, i32)>`, `container_cwd: Option<PathBuf>`,
  `env_clear()`, `with_oci_sync()`, `with_cwd()` builders; Step 8 OCI sync block in pre_exec
  (after seccomp: write PID → accept → read start byte → exec)
- **`src/main.rs`**: restructured with `clap` subcommands — `create/start/state/kill/delete`
  plus legacy `run` mode
- **`Cargo.toml`**: added `serde = { version = "1", features = ["derive"] }` and `serde_json = "1"`
- **4 integration tests**: `test_oci_create_start_state`, `test_oci_kill`,
  `test_oci_delete_cleanup`, `test_oci_bundle_mounts`

**create/start synchronization:** double-fork — parent forks shim → shim calls `command.spawn()`
(forks grandchild) → grandchild pre_exec writes PID to ready pipe + blocks on `accept(exec.sock)`
→ parent reads PID, writes `state.json`, exits → `remora start` connects to socket → grandchild
unblocks, pre_exec returns, exec happens.

---


### DNS Fix ✅

Replaced the incorrect `write_dns_config()` approach (which permanently mutated the
shared rootfs) with a per-container temp file + bind mount:

- Parent writes nameservers to `/run/remora/dns-{pid}-{n}/resolv.conf` before fork
- `pre_exec` bind-mounts that file over `effective_root/etc/resolv.conf` inside the
  container's private mount namespace — the shared rootfs is never touched
- Temp dir removed in `wait()` / `wait_with_output()` via `remove_dir_all`
- Requires `Namespace::MOUNT` (so the bind mount stays in the container's namespace)
  and `with_chroot`; returns an error if either is missing

### Overlay Filesystem ✅

Implemented `with_overlay(upper_dir, work_dir)` — copy-on-write layered rootfs.

- Lower layer = `chroot_dir` (shared, never modified)
- Upper layer = user-supplied writable dir (writes land here)
- Work dir = required by overlayfs kernel driver (same fs as upper)
- Merged dir = auto-created at `/run/remora/overlay-{pid}-{n}/merged/`, cleaned up in `wait()`

Integration tests: `test_overlay_writes_to_upper`, `test_overlay_lower_unchanged`,
`test_overlay_merged_cleanup` (49 total integration tests).
