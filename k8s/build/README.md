# Cluster build + critest infrastructure

Builds pelagos **on the cluster** and runs critest **on the cluster**, so multi-MB
build artifacts never traverse a slow/remote dev-box uplink — only small git
deltas and control commands do.

## Node roles

| Node | CPU | RAM | Role |
|------|-----|-----|------|
| ipc1 | Pentium Gold G5400T (4c) | 32G | k3s control-plane |
| ipc2 / ipc3 | Pentium Gold G5400T (4c) | 32G | k3s workers (light) |
| **ipc4** | i5-12500T (12c) | 32G | **build node** (k3s Job target) |
| ipc5 | i5-12500T (12c) | 32G | spare fast 32G node (alt build target) |
| **ipc6** | i5-12500T (12c) | **16G** | **critest guinea pig** |

ipc4 (stable, 32G) builds; ipc6 (the weakest, most disposable node) runs critest.
Keeping them separate means a critest run that trashes the guinea pig never costs
us the build environment.

## Flow

```
dev box:   edit -> commit -> git push                 (KB of source delta)
ipc4 Job:  git pull -> cargo build -> stage binaries  (/srv/pelagos-build/staging)
LAN:       ipc4 -> ipc6  scp of the binaries          (gigabit, not the dev uplink)
ipc6:      pelagos-install.path -> install + restart pelagos-cri
ipc6:      critest (kubelet stopped for the sweep)
```

`scripts/cluster-build.sh <git-ref> [critest-focus]` drives all of it from the dev box.

## Guinea-pig model: **transient (Model B)**

ipc6 is a normal k3s member. The kubelet's orphan-sandbox GC corrupts a critest
run (it deletes critest's sandboxes mid-sweep — the #379/#353 flakiness), and that
GC runs regardless of `cordon`. So `cluster-build.sh` **stops `k3s-agent` on ipc6
for the duration of a critest sweep and always restarts + uncordons it afterward**
(restore runs even on failure/interrupt). Between sweeps ipc6 contributes its
capacity to the cluster.

## Builder image

The Job runs in a **derived builder image** — `rust:1-bookworm` + a modern protoc
baked in (`k8s/build/builder.Dockerfile`). protoc is a build dependency
(pelagos-cri's tonic-build) that must be newer than Debian apt's (3.21, too old
for the CRI api.proto's `debug_redact`); baking it into the image keeps it a
versioned, pullable dependency instead of node-local host state.

Built and hosted entirely with pelagos (dogfood), into the cluster Zot registry:
```bash
# on a cluster node (e.g. ipc4)
sudo pelagos build -t 192.168.89.2:5004/pelagos-builder:rust-protoc-34.1 \
  --file k8s/build/builder.Dockerfile k8s/build
sudo pelagos image push --insecure 192.168.89.2:5004/pelagos-builder:rust-protoc-34.1
```
The Job's `image:` points at that ref. `192.168.*` is auto-treated as an insecure
(HTTP) registry by pelagos's `oci_client_config`, so the pod pulls it with no
extra config. Bump the protoc version → rebuild + re-push under a new tag and
update `pelagos-build-job.yaml`.

## One-time setup

### Build node (ipc4)
Node label `kubernetes.io/hostname=ipc4` already exists (k3s sets it); the Job's
nodeAffinity uses it. hostPath dirs are auto-created (`DirectoryOrCreate`):
`/srv/pelagos-build/{cache,staging}` (cache = cargo/target/repo build data only —
protoc now comes from the builder image). Nothing else to do — the Job runs as root.

To target ipc5 instead, change the nodeAffinity value in `pelagos-build-job.yaml`.

### Guinea pig (ipc6)
Install the host-side install path-unit and create the delivery drop dir:
```bash
# on ipc6
sudo cp k8s/build/systemd/pelagos-install.{path,service} /etc/systemd/system/
sudo install -m0755 k8s/build/systemd/pelagos-install.sh /usr/local/sbin/
sudo mkdir -p /srv/pelagos-incoming && sudo chown "$USER" /srv/pelagos-incoming
sudo systemctl daemon-reload && sudo systemctl enable --now pelagos-install.path
```

### LAN delivery trust (ipc4 -> ipc6)
The delivery scp runs **on ipc4** targeting ipc6, so it stays on the LAN. Give
`cb@ipc4` key access to `cb@ipc6` once:
```bash
# on ipc4
ssh-keygen -t ed25519 -N '' -f ~/.ssh/id_ed25519   # if absent
ssh-copy-id cb@ipc6                                  # or append to ipc6 authorized_keys
```

## Usage

```bash
# build a branch and run a focused critest on the guinea pig
scripts/cluster-build.sh feat/cri-mount-propagation-rshared-341 'Mount Propagation'

# just build + deploy (no critest), e.g. for manual poking
scripts/cluster-build.sh main
```

For a quick single-node build+install (no cluster Job — build and install on the
node you're on), see `scripts/node-build-install.sh`.

## Status

**Live and validated end-to-end.** One-time setup is applied (path-unit on ipc6,
ipc4→{all nodes} SSH trust, builder image pushed to Zot 5004). `scripts/cluster-build.sh main`
has been run start to finish: Job builds on ipc4 in the builder image (~15s
incremental), delivers ipc4→ipc6 over LAN, the path-unit installs + restarts
pelagos-cri. `scripts/node-build-install.sh` remains as a single-node fallback.
