# CRI Compliance Roadmap

## What CRI is

The Container Runtime Interface (CRI) is the gRPC API that Kubernetes' kubelet
uses to drive a container runtime. It is defined in `k8s.io/cri-api` as two
protobuf services:

- **RuntimeService** — pod and container lifecycle (create, start, stop, exec,
  attach, port-forward, stats)
- **ImageService** — image pull, list, remove, filesystem info

CRI sits *above* the OCI Runtime Specification. The typical production stack is:

```
kubelet
  └─► CRI gRPC (Unix socket)
        └─► containerd / CRI-O          (high-level runtime)
              └─► OCI Runtime Spec       (runc / crun / youki)
                    └─► Linux kernel
```

---

## Where Pelagos sits today

Pelagos implements the **OCI Runtime Spec** layer (bottom of the stack above)
and its own higher-level CLI on top of that. It is not CRI-compliant.

| Layer | Pelagos status |
|-------|---------------|
| OCI Runtime Spec (`create/start/state/kill/delete`) | Partial ✅ — `src/oci.rs` |
| OCI image pull + layer store | ✅ — `src/image.rs`, `src/cli/image.rs` |
| Container networking | ✅ — own bridge/NAT/DNS stack, not CNI |
| CRI gRPC server | ❌ |
| Pod sandbox concept | ❌ |
| CNI integration | ❌ |
| Container log management (CRI format) | ❌ |
| Daemon architecture | ❌ — Pelagos is a CLI |

### Fastest path to Kubernetes without a CRI rewrite

Write a **containerd shim** (`containerd-shim-pelagos-v1`). A shim is a small
process that containerd forks per-container; it speaks the containerd shim v2
protocol and calls an OCI runtime. Implementing the shim protocol is
significantly less work than a full CRI server, and containerd handles pod
sandboxes, CNI, and image management itself. Pelagos's existing OCI lifecycle
commands are used as-is.

---

## Full CRI implementation plan

This is a significant but tractable project. Everything listed below builds on
existing Pelagos internals rather than replacing them.

---

### Phase C1 — Daemon architecture

**Goal:** `pelagos daemon` — a long-running process that listens on a Unix socket
and dispatches commands.

**Work:**
- Add `tokio`-based main loop (already a dependency via `oci-client`)
- Expose existing container/image operations as async handlers
- Write PID file, handle SIGTERM gracefully
- Systemd unit file (`pelagos.service`)

**Why first:** everything else depends on a persistent process that kubelet can
connect to.

---

### Phase C2 — CRI gRPC server skeleton

**Goal:** implement the CRI protobuf API surface using `tonic`.

**Work:**
- Add `tonic` + `prost` to `Cargo.toml`
- Vendor or reference `k8s.io/cri-api` proto files
- Implement `RuntimeService` and `ImageService` trait stubs (return
  `Unimplemented` initially)
- Wire Unix socket listener into the daemon
- Verify kubelet can connect and enumerate capabilities (`Status` RPC)

---

### Phase C3 — ImageService

**Goal:** kubelet can pull, list, and remove images via CRI.

**Work:**
- `PullImage` → calls existing `cmd_image_pull()` machinery
- `ListImages` → wraps `list_images()`
- `RemoveImage` → wraps `remove_image()`
- `ImageStatus` → wraps `load_image()`
- `ImageFsInfo` → report disk usage of `/var/lib/pelagos/layers/`

This is largely a translation layer over existing code.

---

### Phase C4 — Pod sandbox

**Goal:** `RunPodSandbox` / `StopPodSandbox` / `RemovePodSandbox`.

A pod sandbox is a lightweight "pause" container that holds the shared network
namespace for all containers in a pod. Other containers join its netns rather
than creating their own.

**Work:**
- Implement a minimal pause process (sleeps forever, reaps zombies as PID 1)
- `RunPodSandbox`: spawn pause container, set up network namespace
- Store sandbox state (ID → PID, netns path, IP) in runtime state directory
- `StopPodSandbox` / `RemovePodSandbox`: teardown + cleanup

**Note:** this is the architecturally novel piece. Pelagos currently assumes one
container = one network namespace. Pod sandboxes require decoupling network
setup from container spawn.

---

### Phase C5 — CNI integration

**Goal:** replace Pelagos's built-in bridge/NAT networking with CNI plugins for
pod networking.

**Work:**
- Implement CNI plugin invocation: fork plugin binary, pass JSON config via
  stdin, read result from stdout (CNI spec is simple)
- `RunPodSandbox` calls CNI `ADD` to configure the pod's netns
- `StopPodSandbox` calls CNI `DEL` to clean up
- Support reading CNI config from `/etc/cni/net.d/` (standard location)
- Keep Pelagos's built-in networking for `pelagos run` / `pelagos compose` —
  CNI is only used when driven via CRI

Standard CNI plugins (`bridge`, `host-local`, `loopback`) from
`containernetworking/plugins` work out of the box once invocation is wired up.

---

### Phase C6 — RuntimeService: container lifecycle

**Goal:** `CreateContainer`, `StartContainer`, `StopContainer`, `RemoveContainer`,
`ListContainers`, `ContainerStatus`.

**Work:**
- `CreateContainer`: spawn container in sandbox's netns (join via `setns`);
  apply CRI `ContainerConfig` (image, env, mounts, command, working dir, user)
- `StartContainer`: transition from created → running (maps to existing OCI
  `start` flow)
- `StopContainer`: SIGTERM → SIGKILL with timeout
- `RemoveContainer`: cleanup state + overlay
- `ListContainers` / `ContainerStatus`: read runtime state directory

This maps closely to existing Pelagos container machinery; the main addition is
reading `ContainerConfig` from the CRI protobuf instead of CLI flags.

---

### Phase C7 — Exec, logs, stats

**Goal:** `ExecSync`, `Exec`, `Attach`, `PortForward`, `ContainerStats`,
`ListContainerStats`.

**Work:**
- `ExecSync`: run command in container netns + namespaces, capture stdout/stderr
  (wraps existing `cmd_exec()` logic)
- `Exec` / `Attach`: stream I/O via CRI streaming server (separate HTTP/2
  endpoint that kubelet proxies to)
- `PortForward`: TCP proxy into pod netns
- `ContainerStats`: read cgroup memory/CPU stats (wraps `resource_stats()`)
- **Container logs**: write stdout/stderr to
  `/var/log/pods/<namespace>_<pod>_<uid>/<container>/0.log` in CRI log format
  (`<timestamp> <stream> <flags> <log>`)

The streaming server (`Exec`/`Attach`) is the most complex piece — it requires
a separate goroutine-style async task per connection.

---

## Shim-backed runtime handlers (Kata, gVisor, runwasi) — epic #574

`pelagos-cri` can also act as a **containerd shim v2 client**, selected per pod
through Kubernetes `RuntimeClass`. Pods without a RuntimeClass (or with
`pelagos`) run exactly as before.

```
pelagos-cri --shim-handler kata=/opt/kata/bin/containerd-shim-kata-v2 \
            --shim-handler gvisor=/usr/local/bin/containerd-shim-runsc-v1
```

```yaml
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata: {name: kata}
handler: kata        # the NAME in --shim-handler NAME=BINARY
```

What happens for a shim-handler pod:

| Step | Behaviour |
|---|---|
| RunPodSandbox | pod netns created and CNI ADD applied **first**, then the shim is bootstrapped (`start`) and the sandbox task created in it. The sandbox task runs `/pause` from the configured pause image. `hostNetwork` pods and nodes without a CNI config are rejected. Pod sysctls go into the sandbox spec. |
| CreateContainer / StartContainer | CRI -> OCI `config.json` (always sets `io.kubernetes.cri.container-type` / `sandbox-id`); the image's overlay is passed to the shim as a `Mount` descriptor (the shim mounts it); the container joins the pod's shim, falling back to a per-container shim `start` if the pod shim rejects it. |
| Logs | shim stdout/stderr FIFOs -> CRI log format; `ReopenContainerLog` supported. |
| Exec / ExecSync / Attach | through the Task API (exec processes, FIFOs, pty resize). Port-forward dials the pod IP in the CNI netns, unchanged. |
| Exit / events | `TaskExit` via the events sink **and** the `Wait` RPC; `GetContainerEvents` is served (with pod and container statuses) only when a shim handler is configured. |
| Stats | shim `Stats` (cgroups v1/v2 metrics) mapped to container and sandbox stats. |
| Restart | shims are re-adopted from their persisted bundles; unreachable ones are reported and their pods go NotReady so the kubelet replaces them. |
| StopPodSandbox | stop shim, **then** CNI DEL, **then** delete the netns. |

Status of validation (2026-11-24, real shims on an x86_64 node):

- **Kata 4.4.0 (runtime-rs) and gVisor runsc: validated.** Sandbox and
  container lifecycle, logs, exec (including stdin), stats, a 44-layer image
  (the symlinked `lowerdir` alias path), clean teardown (no leaked VM, netns,
  mounts or IPAM) and CRI-restart re-adoption (Kata) all work. Kata accepts
  containers on the pod shim's address and its stats arrive as cgroups v2.
  Requirements this surfaced: the shim client speaks `containerd.task.v3.Task`
  first and falls back to `v2` (all these shims register v2); the sandbox task
  needs a pause image rootfs (`--shim-sandbox-image`, default
  `registry.k8s.io/pause:3.10`, pulled automatically on first use); every
  bundle gets a `log` FIFO (Go shims exit without it); and each shim is
  started in its own `pelagos.slice` systemd scope so a CRI restart does not
  kill it (Go shims such as runsc do not escape the service cgroup on their own).
- **Wasm is NOT supported through shim handlers.**
  - runwasi's `containerd-shim-wasmtime` calls containerd's own
    Containers/Images/Content/Leases gRPC services at its `-address` socket, so
    it cannot run without a containerd daemon (`Create` fails with
    `Others("transport error")`).
  - `pelagos-shim-wasm` ignores `CreateTask.rootfs` and the stdio FIFOs, runs
    the sandbox's `/pause` as a wasm module, and its daemon dies at startup
    (it initialises a logger twice).
  - `pelagos-cri` itself rejects Wasm images on the shim path ("image is a Wasm
    image; it has no overlay rootfs"): their layers are modules, not an overlay.
  - Supporting it would take a sandbox-less handler mode, delivery of the
    module to the shim, and a shim that honours rootfs and stdio. Tracked in
    the wasm follow-up issue; native Wasm pods are unaffected by all of this.
- **Through the real kubelet:** `scripts/test-shim-runtimeclass-e2e.sh` runs
  two-container Kata and gVisor pods (ConfigMap, service-account and emptyDir
  volumes, resource limits, Cilium CNI) under k3s on a node: 16/16, plus a CRI
  restart with the pods running (VM uptime continued, shims re-adopted) and
  clean deletion. This found four bugs `crictl` could not (kubelet image *ids*,
  zero hugepage limits on cgroup v2, leaked per-container shims, a gVisor shim
  surviving pod removal). Known gap: pod resource limits do not size the Kata
  VM (#626).
- **CRI conformance:** the critest `Streaming` bucket passes 5/5 against a
  build including the shim streaming changes (native handler).

Design and upstream findings: `docs/SHIM_V2_CLIENT_DESIGN.md`. Not
implemented: the containerd Sandbox API (not needed for Kata), an explicit
`RuntimeDefault` seccomp profile for shim containers, and a generalised pelagos shim v2 *server*.

## Dependency summary

```
C1 (daemon)
  └─► C2 (gRPC skeleton)
        ├─► C3 (ImageService)   ← independent, low risk
        └─► C4 (pod sandbox)
              ├─► C5 (CNI)
              └─► C6 (container lifecycle)
                    └─► C7 (exec/logs/stats)
```

C3 can be done in parallel with C4. C4 is the critical path item that requires
the most new design work.

---

## Testing against Kubernetes

Once C6 is complete, the runtime can be tested with:

```bash
# Point kubelet at the pelagos CRI socket
kubelet --container-runtime-endpoint=unix:///run/pelagos/cri.sock ...

# Or use crictl for manual testing without a full cluster
crictl --runtime-endpoint unix:///run/pelagos/cri.sock pull alpine
crictl --runtime-endpoint unix:///run/pelagos/cri.sock runp pod.json
crictl --runtime-endpoint unix:///run/pelagos/cri.sock create <pod-id> container.json pod.json
crictl --runtime-endpoint unix:///run/pelagos/cri.sock start <container-id>
```

`crictl` (from `cri-tools`) is the standard way to exercise a CRI runtime
without running a full Kubernetes cluster.
