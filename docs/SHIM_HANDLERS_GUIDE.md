# Running pods in Kata micro-VMs or gVisor (shim handlers)

How-to for operators. Design: `docs/SHIM_V2_CLIENT_DESIGN.md`; behaviour and
validation results: `docs/CRI_COMPLIANCE.md` ("Shim-backed runtime handlers").
Epic #574. Pods without a RuntimeClass keep running natively; nothing here
changes them.

## 0. After installing pelagos: the checklist

Packages (`.deb`/`.rpm`, AUR) and `scripts/install.sh` put every pelagos piece in
place: `pelagos-cri`, `pelagos-cri-shim-handlers`, the `pelagos-cri.service` unit
(packages only), `cri.env.example` and the RuntimeClass manifests. They do not
enable the service or any handler, and they cannot install Kata/gVisor themselves.
Remaining steps, in order:

1. `sudo systemctl enable --now pelagos-cri` (packages; with `install.sh` copy
   `pelagos-cri/pelagos-cri-agent.service` and adjust paths). Point the kubelet at
   `unix:///run/pelagos/cri.sock`.
2. Install the runtime(s): section 1.
3. Enable handlers: section 2 (`pelagos-cri-shim-handlers add ...`).
4. Create RuntimeClasses: section 3.
5. Verify the node: section 4.

## 1. Prerequisites (not installed by pelagos)

| Need | Notes |
|---|---|
| A shim binary per runtime | Kata: `containerd-shim-kata-v2` (runtime-rs 4.x tested). gVisor: `containerd-shim-runsc-v1` **and** `runsc`. |
| Kata location | Kata only accepts its configuration from its shipped paths, so install the release tree at `/opt/kata` (shim at `/opt/kata/bin/containerd-shim-kata-v2`). |
| gVisor `runsc` | Must be on `pelagos-cri`'s `PATH` (the shim looks it up). Put it in `/usr/local/bin`, or set `PATH=` in `/etc/pelagos/cri.env`. |
| KVM for Kata | `/dev/kvm` must exist on the node. |
| A CNI config | Shim pods get their netns + CNI ADD before the VM starts; nodes without CNI config and `hostNetwork` pods are rejected. |
| Pause image | Pulled automatically on first use (`registry.k8s.io/pause:3.10`; `--shim-sandbox-image` to change, empty to disable and pre-pull yourself). |

## 2. Enable on a node

```
sudo pelagos-cri-shim-handlers add \
    kata=/opt/kata/bin/containerd-shim-kata-v2 \
    gvisor=/usr/local/bin/containerd-shim-runsc-v1
sudo pelagos-cri-shim-handlers list
```

This writes `PELAGOS_CRI_ARGS="--shim-handler kata=... --shim-handler gvisor=..."`
to `/etc/pelagos/cri.env` (read by the shipped `pelagos-cri` units) and restarts
`pelagos-cri`. A restart re-adopts running shim pods; it does not disturb them.
Undo: `pelagos-cri-shim-handlers remove kata gvisor` (no handlers = native only).
`/usr/share/pelagos/cri.env.example` is a commented template.

Cluster installs using `k3s-experiments/scripts/install-pelagos.sh` get the
same `EnvironmentFile` line in the unit it writes.

Which installs can enable handlers (#639): the `.deb`/`.rpm` packages and both
AUR packages ship `pelagos-cri`, `pelagos-cri-shim-handlers`, the
`pelagos-cri.service` unit, `cri.env.example` and the RuntimeClass manifests
(release CI asserts the `.deb` has them); `scripts/install.sh` installs
`pelagos-cri` and the helper to `INSTALL_DIR` (no unit); `install-pelagos.sh`
writes a unit that reads `cri.env` (nodes installed before that change keep
their old unit until the script is re-run).

## 3. Create the RuntimeClasses and run a pod

```
pelagos-cri-shim-handlers runtimeclass kata gvisor     # or kubectl apply -f /usr/share/pelagos/runtimeclass/
```

```yaml
apiVersion: v1
kind: Pod
metadata: {name: secure}
spec:
  runtimeClassName: kata        # or gvisor
  containers: [{name: app, image: busybox, command: [sleep, "3600"]}]
```

Check isolation: `kubectl exec secure -- uname -r` shows the guest kernel
(Kata) or `4.19.0-gvisor`, not the host's. Schedule shim pods only on nodes
that have the handler, e.g. with `scheduling.nodeSelector` on the RuntimeClass.

## 4. Verify a node end to end

CI runs this on every shim-related change and at release time
(`.github/workflows/shim-e2e.yml`, `scripts/ci-shim-e2e.sh`): real Kata and gVisor
on a one-node k3s. To check an actual node by hand:

`scripts/test-shim-runtimeclass-e2e.sh` runs two-container pods per runtime
through the real kubelet: Ready, distinct guest kernel, ConfigMap, service
account token, shared emptyDir, logs, pod IP, cross-runtime ping, clean delete.

```
NODE=<node> KUBECTL="sudo kubectl" RUNTIMES="kata gvisor" scripts/test-shim-runtimeclass-e2e.sh
```

It creates and removes its own namespace and RuntimeClasses. Run it on a node
with handlers enabled; see the script header for variables.

## 5. Operating notes

- Each shim runs in its own systemd scope (`pelagos-shim-<id>.scope` in
  `pelagos.slice`) so restarting `pelagos-cri` does not kill VMs.
  Inspect with `systemctl list-units 'pelagos-shim-*'`.
- Shim bundles: `/run/pelagos-cri/shim/k8s.io/<id>/`; per-container logs follow normal CRI
  paths. Leftover check after deleting all shim pods: no `pelagos-shim-*`
  scopes, no `qemu`/`runsc` processes.
- **Removing a handler:** delete its pods first and let the kubelet garbage-collect
  them (stopped sandboxes linger until GC). A CRI without the handler can no
  longer manage those sandboxes, and their shim scope is left running; stop it
  with `systemctl stop pelagos-shim-<id>.scope`.
- Wasm via shim handlers is **not supported** (#614).

## 6. Sizing (Kata VM memory and CPU)

The kubelet sends the sum of the pod's container limits plus RuntimeClass
`overhead` in the sandbox config; `pelagos-cri` passes it to Kata as the
sandbox cpu/memory annotations. Observed on a 256Mi/500m pod: a 256 MiB, 1 vCPU
guest; 1Gi/2 CPU: 1011 MiB, 3 vCPUs; no limits at all: Kata's default
(about 2 GiB, 1 vCPU, set by `default_memory`/`default_vcpus` in Kata's own
`configuration.toml`). If any container lacks a limit the kubelet sends zero, and
the default applies. To get predictable sizes, set limits on every container, or
add `overhead` to the RuntimeClass. Limits are also enforced inside the guest,
and Kata hot-plugs resources as containers start. Exact bin-packing accuracy is
not a goal. gVisor has no VM; its pod cgroup limits apply on the host.

## 7. Troubleshooting

| Symptom | Cause |
|---|---|
| Sandbox fails "only shipped Kata configuration files are accepted" | Kata tree is not at `/opt/kata`. |
| Sandbox fails with `/pause was not found` | Pause image missing and auto-pull disabled. |
| gVisor shim exits at start | `runsc` not on `pelagos-cri`'s PATH. |
| `image "busybox:latest" not found in the local store` | Fixed in #630 (short names are expanded). Older releases need a fully-qualified image. |
| Pod stuck, CNI error | Node has no CNI config, or the CNI plugin needs `IgnoreUnknown` (#611). |
