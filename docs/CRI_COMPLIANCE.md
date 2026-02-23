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

## Where remora sits today

Remora implements the **OCI Runtime Spec** layer (bottom of the stack above)
and its own higher-level CLI on top of that. It is not CRI-compliant.

| Layer | Remora status |
|-------|---------------|
| OCI Runtime Spec (`create/start/state/kill/delete`) | Partial ✅ — `src/oci.rs` |
| OCI image pull + layer store | ✅ — `src/image.rs`, `src/cli/image.rs` |
| Container networking | ✅ — own bridge/NAT/DNS stack, not CNI |
| CRI gRPC server | ❌ |
| Pod sandbox concept | ❌ |
| CNI integration | ❌ |
| Container log management (CRI format) | ❌ |
| Daemon architecture | ❌ — remora is a CLI |

### Fastest path to Kubernetes without a CRI rewrite

Write a **containerd shim** (`containerd-shim-remora-v1`). A shim is a small
process that containerd forks per-container; it speaks the containerd shim v2
protocol and calls an OCI runtime. Implementing the shim protocol is
significantly less work than a full CRI server, and containerd handles pod
sandboxes, CNI, and image management itself. Remora's existing OCI lifecycle
commands are used as-is.

---

## Full CRI implementation plan

This is a significant but tractable project. Everything listed below builds on
existing remora internals rather than replacing them.

---

### Phase C1 — Daemon architecture

**Goal:** `remora daemon` — a long-running process that listens on a Unix socket
and dispatches commands.

**Work:**
- Add `tokio`-based main loop (already a dependency via `oci-client`)
- Expose existing container/image operations as async handlers
- Write PID file, handle SIGTERM gracefully
- Systemd unit file (`remora.service`)

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
- `ImageFsInfo` → report disk usage of `/var/lib/remora/layers/`

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

**Note:** this is the architecturally novel piece. Remora currently assumes one
container = one network namespace. Pod sandboxes require decoupling network
setup from container spawn.

---

### Phase C5 — CNI integration

**Goal:** replace remora's built-in bridge/NAT networking with CNI plugins for
pod networking.

**Work:**
- Implement CNI plugin invocation: fork plugin binary, pass JSON config via
  stdin, read result from stdout (CNI spec is simple)
- `RunPodSandbox` calls CNI `ADD` to configure the pod's netns
- `StopPodSandbox` calls CNI `DEL` to clean up
- Support reading CNI config from `/etc/cni/net.d/` (standard location)
- Keep remora's built-in networking for `remora run` / `remora compose` —
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

This maps closely to existing remora container machinery; the main addition is
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
# Point kubelet at the remora CRI socket
kubelet --container-runtime-endpoint=unix:///run/remora/cri.sock ...

# Or use crictl for manual testing without a full cluster
crictl --runtime-endpoint unix:///run/remora/cri.sock pull alpine
crictl --runtime-endpoint unix:///run/remora/cri.sock runp pod.json
crictl --runtime-endpoint unix:///run/remora/cri.sock create <pod-id> container.json pod.json
crictl --runtime-endpoint unix:///run/remora/cri.sock start <container-id>
```

`crictl` (from `cri-tools`) is the standard way to exercise a CRI runtime
without running a full Kubernetes cluster.
