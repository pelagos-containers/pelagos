# Running pods in Kata micro-VMs or gVisor (shim handlers)

How-to for operators. Design: `docs/SHIM_V2_CLIENT_DESIGN.md`; behaviour and
validation results: `docs/CRI_COMPLIANCE.md` ("Shim-backed runtime handlers").
Epic #574. Pods without a RuntimeClass keep running natively; nothing here
changes them.

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
- Wasm via shim handlers is **not supported** (#614).

## 6. Troubleshooting

| Symptom | Cause |
|---|---|
| Sandbox fails "only shipped Kata configuration files are accepted" | Kata tree is not at `/opt/kata`. |
| Sandbox fails with `/pause was not found` | Pause image missing and auto-pull disabled. |
| gVisor shim exits at start | `runsc` not on `pelagos-cri`'s PATH. |
| Pod stuck, CNI error | Node has no CNI config, or the CNI plugin needs `IgnoreUnknown` (#611). |
