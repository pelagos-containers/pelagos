# Building pelagos in a Kubernetes / k3s cluster

An example of building pelagos **inside the cluster** — useful when your dev
machine is remote/slow and the cluster nodes have fast disks + uplinks. A `Job`
compiles `pelagos` + `pelagos-cri` on a build node; you then install the binaries
on whichever nodes you want.

This directory is a **template**, not a turnkey deploy — replace the
`<PLACEHOLDERS>` for your environment.

## Build requirements (applies to any build method)

- **Rust** (stable) and a **C toolchain** (`cc`/`ld`) for linking.
- **protoc** — and recent enough to know the `debug_redact` option used in the
  Kubernetes CRI `api.proto`. **Debian/Ubuntu apt's protoc (3.21) is too old**;
  use **protoc ≥ 22** (the example pins 34.1). This is the most common build
  surprise: `cargo build -p pelagos-cri` fails with
  `Option "debug_redact" unknown` on an old protoc.
- `pelagos` (the binary) does **not** need protoc; only `pelagos-cri` does.

## Files

| File | What it is |
|------|------------|
| `builder.Dockerfile` | A build image: `rust:1-bookworm` + protoc baked in. |
| `build-job.yaml` | A parameterized k8s `Job` that builds pelagos from a git ref. |

## Usage

1. **Build + push the builder image** to a registry your cluster can pull from:
   ```bash
   docker build -t <REGISTRY>/pelagos-builder:rust-protoc-34.1 -f builder.Dockerfile .
   docker push  <REGISTRY>/pelagos-builder:rust-protoc-34.1
   # (pelagos can do this too: `pelagos build` + `pelagos image push`)
   ```
2. **Fill in the placeholders** in `build-job.yaml` (`<REGISTRY>`, `<BUILD_NODE>`)
   and apply:
   ```bash
   sed -e 's|<REGISTRY>|myregistry:5000|' -e 's|<BUILD_NODE>|node01|' \
     build-job.yaml | kubectl apply -f -
   kubectl wait --for=condition=complete job/pelagos-build --timeout=20m
   ```
3. The binaries land in the Job's `output` hostPath on the build node
   (`/srv/pelagos-build/out` by default). **Installing** them on your nodes is
   intentionally left to you — copy them to `/usr/local/bin` and restart
   `pelagos-cri`, however you manage that (a script, a systemd `path` unit
   watching a drop directory, a second Job, etc.).

## Running tests on the cluster

pelagos has two test tiers with very different isolation needs:

- **Unit tests** (`cargo test --lib`) are hermetic — no root, namespaces, or
  cgroups. `build-job.yaml` runs them right after `cargo build` and **before**
  populating `/out`, so a failing build never produces installable binaries. This
  is the safe, default way to test in-cluster.

- **Integration tests** (`cargo test --test integration_tests`) require **root**
  and mutate **host** state: they create and tear down a bridge, veth pairs,
  nftables tables, cgroups, and mount/network namespaces. Do **not** run them as
  an ordinary pod on a working node — they will collide with that node's real
  networking. Run them on a **dedicated node you've drained** (`kubectl cordon` +
  stop the kubelet so its pod GC doesn't interfere), as a host `cargo test` over
  SSH, and restore the node afterwards. Sketch:
  ```bash
  # compile while still in-cluster; then run the compiled binary as root
  ssh "$NODE" 'cd pelagos && cargo test --test integration_tests --no-run'
  kubectl cordon "$NODE"; ssh "$NODE" 'sudo systemctl stop kubelet'          # drain
  EXE=$(ssh "$NODE" 'ls -t pelagos/target/debug/deps/integration_tests-* | grep -v "\.d$" | head -1')
  ssh "$NODE" "sudo '$EXE'"                                                  # run as root
  ssh "$NODE" 'sudo systemctl start kubelet'; kubectl uncordon "$NODE"       # always restore
  ```
  Run the compiled *binary* as root rather than `sudo cargo test` — many nodes'
  sudo strips `-E`, and a root `cargo` can't find a toolchain installed under the
  user's home. The node needs a Rust toolchain on the host (rustup) and
  `build-essential`; protoc is not required (the integration tests don't compile
  pelagos-cri). **If the node runs pelagos as its own CRI, stop that service too**
  for the run (and restart it before the kubelet) — the suite and
  `scripts/reset-test-env.sh` operate on `/run/pelagos`, the live runtime's dir,
  and will otherwise wedge the node NotReady.

## Notes / adapt to taste

- The build cache (`/cache`: git checkout + cargo registry + target dir) is a
  `hostPath` for speed; swap it for a `PVC` if you want it node-independent.
- Pin the Job to a capable node via `nodeSelector`/`nodeAffinity`.
- If your registry is plain HTTP, pelagos auto-treats RFC1918 hosts
  (`10.*`, `172.16–31.*`, `192.168.*`) as insecure; otherwise configure it.
