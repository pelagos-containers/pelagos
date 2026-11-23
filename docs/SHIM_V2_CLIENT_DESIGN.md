# containerd shim v2 client — spike findings and design

Spike for #575, part of epic #574. Goal: pin down what a CRI-side **shim v2
client** (the role containerd / CRI-O play) must implement for `pelagos-cri` to
drive Kata Containers and other shim v2 runtimes via `RuntimeClass`.

## Sources and verification status

Read directly from upstream source (not from memory):

- containerd `main`: `core/runtime/v2/README.md`,
  `api/runtime/task/v3/shim.proto`, `api/runtime/sandbox/v1/sandbox.proto`
- Kata Containers **4.4.0** (latest release at time of writing), `runtime-rs`:
  `service/src/{task_service,sandbox_service}.rs`,
  `shim/src/args.rs`, `kata-sys-util/src/spec.rs`,
  `kata-types/src/annotations/*`, `resource/src/rootfs/mod.rs`
- `containerd-shim-protos` 0.12.0 (rust-extensions) `Cargo.toml` and `lib.rs`

Items marked **UNVERIFIED** were not confirmed in source and must be checked
when the corresponding work item is implemented. Not examined in this spike:
gVisor `runsc` and runwasi behavior, Kata's Go runtime, exact event transport.

Note: the epic referred to "Kata 3.x". Current upstream is 4.x; runtime-rs
implements both the Task and Sandbox services.

## 1. Task API

Service name is `containerd.task.v3.Task` (containerd 2.0+). RPCs: State,
Create, Start, Delete, Pids, Pause, Resume, Checkpoint, Kill, Exec, ResizePty,
CloseIO, Update, Wait, Stats, Connect, Shutdown.

Kata runtime-rs implements all of these **except Checkpoint**. The client must
use at least: Create, Start, State, Kill, Delete, Wait, Pids, Exec, ResizePty,
CloseIO, Update, Stats, Connect, Shutdown. Pause/Resume are optional. Per the
containerd contract a shim that cannot serve an RPC returns `ErrNotImplemented`;
the client must tolerate it.

## 2. Shim bootstrap

Two protocols exist:

- **Legacy (containerd up to 2.2):** `containerd-shim-<name>-v2 start` with
  flags `-namespace -address -publish-binary -id`, env `TTRPC_ADDRESS`,
  `GRPC_ADDRESS`, `MAX_SHIM_VERSION=2`, runtime options as protobuf on stdin,
  **cwd = the bundle directory**. The shim writes its ttrpc address to stdout
  (or JSON `{"version":2,"address":...,"protocol":"ttrpc"}`) and the client
  connects to it.
- **BootstrapParams (containerd 2.3+):** one protobuf `BootstrapParams` on
  stdin, `BootstrapResult` on stdout. `containerd-shim-protos` 0.12 has a
  `bootstrap` module. UNVERIFIED whether Kata 4.4.0 speaks it.

Kata runtime-rs `Args::validate` requires `id`, `namespace` and
`publish_binary` to be non-empty for start/run/delete, so the **legacy
protocol is the minimum**. Plan: legacy first, BootstrapParams as a follow-up.

A `delete` subcommand (flags plus `-bundle`) is invoked when the shim is
unreachable (e.g. SIGKILLed) and must clean up its resources, including
rootfs mounts. The client also calls it when re-adopting after its own restart
and the shim cannot be reached.

The bundle directory must contain `config.json`. Kata's `start` reads it.

## 3. One shim per pod — how Kata distinguishes sandbox from container

Kata's `get_container_type` reads the OCI spec annotation, checking in order:
`io.kubernetes.cri.container-type`, `io.kubernetes.cri-o.ContainerType`,
`io.kubernetes.docker.type`. Values: `sandbox` / `podsandbox` → pod sandbox;
`container` → pod container; anything else → error.

**If no key is present Kata treats the container as a pod sandbox.** A client
that forgets the annotation will silently get a new VM per container.

- sandbox: `start` launches a new shim (new VM) and returns its address.
- container: `start` reads the sandbox id from `io.kubernetes.cri.sandbox-id`
  (or `io.kubernetes.cri-o.SandboxID`, `io.kubernetes.sandbox.id`) and
  returns the **existing** shim's address. Missing sandbox id is an error.

Other CRI annotations Kata consumes: `io.kubernetes.cri.container-name`,
`sandbox-name`, `sandbox-namespace`, `sandbox-cpu-quota|period|shares`,
`sandbox-memory`.

## 4. Sandbox API (containerd 2.x)

`containerd.runtime.sandbox.v1.Sandbox`: CreateSandbox, StartSandbox, Platform,
StopSandbox, WaitSandbox, SandboxStatus, PingSandbox, ShutdownSandbox,
SandboxMetrics, UpdateSandbox. `CreateSandboxRequest` carries `bundle_path`,
`rootfs` mounts, `options`, `netns_path` and `annotations` explicitly.

Kata runtime-rs implements Create/Start/Stop/Wait/Status/Shutdown (the macro
list also has two entries consistent with Platform and Ping — UNVERIFIED).
`containerd-shim-protos` gates its bindings behind the `sandbox` feature.

Conclusion: the **legacy path works without the Sandbox API** (sandbox is just
the first Task.Create carrying `container-type=sandbox`). Implement it first;
the Sandbox API is a later item, not a prerequisite for Kata.

## 5. Root filesystem — corrects an assumption in the epic

The epic's item 4 assumed pelagos must mount the overlay on the host before
calling Create. **That is wrong per the containerd contract:** `Create` carries
`repeated Mount rootfs`, and the **shim is responsible for mounting it into
`<bundle>/rootfs` and unmounting it on delete**. The client's job is to produce
Mount descriptors (type, source, options), as containerd's snapshotters do.

Kata runtime-rs accepts: a single mount (the common case: one `overlay` mount),
block-device rootfs, nydus, multi-layer erofs, and guest-pull virtual volumes.
For a single layer it mounts into its shared fs and exports to the guest
(virtio-fs); with no shared fs and no virtual volume it errors.

So the work item becomes: **generate one `overlay` Mount (lowerdir list from
the pelagos layer store, ephemeral upperdir/workdir)**, not "mount on host".
Watch lowerdir option-length limits on images with many layers.
UNVERIFIED: exact option handling in Kata's share_fs overlay path.

## 6. Network namespace

Kata runtime-rs has netns handling (pin / `/proc/<pid>/ns/net` references,
removal of a netns it created itself). The pod netns must already exist, with
CNI applied, and be referenced from the spec's `linux.namespaces` network entry
(legacy path) or `netns_path` (Sandbox API). `pelagos-cri` already runs CNI per
sandbox, so this is an ordering and plumbing task.

## 7. Events

The containerd contract makes `TaskCreate/Start/Exit/Delete` (and exec
equivalents) MUST-publish with strict ordering (Start before Exit).

**Verified against Kata runtime-rs 4.4.0 (`service/src/event.rs`, #585):** the
shim does **not** exec `-publish-binary`. It reads `TTRPC_ADDRESS` from its
environment, connects to that unix socket and calls
`containerd.services.events.ttrpc.v1.Events/Forward` with an `Envelope`
(topic, namespace, timestamp, `Any` event). If `TTRPC_ADDRESS` is unset or
empty it only logs events. `-publish-binary` must still be non-empty
(`Args::validate`), but is never run.

So the client needs an **events sink**, implemented in
`pelagos-cri/src/shim/events.rs`: a ttrpc `Events` server on a `0600` unix
socket owned by `pelagos-cri`, exported to shims as `TTRPC_ADDRESS` (a plain
path, as containerd passes it), delivering decoded events in arrival order over
a channel. `-address` remains the separate containerd-style endpoint flag. The
publish binary is a placeholder (the running `pelagos-cri` path).

A real publish-binary subcommand is deferred: build it only if validation shows
a shim (`runsc`, Kata's Go runtime) actually execs it. **UNVERIFIED** for those
shims.

## 8. Stdio

Paths are supplied by the client on Create/Exec (`stdin`, `stdout`, `stderr`,
`terminal`). Supported schemes: fifo (Linux), `file://`, `binary://`,
`binary-v2://`. For CRI, the client creates FIFOs and copies them into CRI-format
log lines (timestamp + stream prefix), so `file://` does not fit. TTY: FIFOs plus
`ResizePty`. Stdin close: `CloseIO`. Exec: `ExecProcessRequest` with an OCI
Process spec as `Any`; the client picks the `exec_id`.

## 9. Restart survival

The shim outlives the client. The client persists the bundle directory and the
shim address, then on startup calls `Connect` (returns shim pid, task pid,
version); if unreachable it runs `delete` to clean up. This can replace the
`systemd-run` scope workaround from #336 for shim-backed pods only.

## 10. Stats

`StatsResponse.stats` is an `Any` holding cgroup v1 or v2 metrics
(`containerd-shim-protos` has `cgroups` and `cgroups_v2` types). For Kata the
numbers come from the sandbox/guest. UNVERIFIED how complete they are.

## 11. Crates

- `containerd-shim-protos` **0.12.0**: Task v3 `TaskClient`, `Events`, features
  `async` (tokio-friendly, matches `pelagos-cri`) and `sandbox`, plus
  `bootstrap`. Depends on `ttrpc` 0.10 and `protobuf` 3.7.
- The repo's `containerd-shim = "0.10"` (wasm shim, server side) is behind
  (latest 0.12). The client should use `containerd-shim-protos` 0.12 directly
  with `async`. Bumping the wasm shim to 0.12 should happen in the same change
  to avoid two copies of the protos in one build. UNVERIFIED whether 0.10
  spoke Task v2 or v3.

## 12. Test hardware

The shared build/test node for the project has `/dev/kvm` and VT-x, so Kata can
be run there. Kata itself (release tarball) must be installed on that node as
part of the validation item.

## Impact on the epic's work items

- **Item 4 (host overlay mount): reworded** — generate Mount descriptors; the
  shim mounts.
- **New item: shim bootstrap + bundle/address persistence** (legacy protocol
  first, BootstrapParams later).
- **New item: events sink + publish binary.**
- **Item 3 (OCI spec generator):** must always set `container-type` and
  `sandbox-id`, since absence means "sandbox" to Kata.
- **Sandbox API item:** confirmed *not* required for Kata; stays late.
- Bump `containerd-shim` to 0.12 alongside introducing the client.
