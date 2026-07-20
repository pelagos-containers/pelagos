# Issue #361 — eramax feature request context

**Issue:** [#361 — Feature Request: PID 1 Init, Kubernetes API Server, Multi-Node Cluster, CRDs, and rustix Migration](https://github.com/pelagos-containers/pelagos/issues/361)
**Opened by:** eramax (Ahmed Morsi)

---

## Who is eramax?

Ahmed Morsi is the author of [eramax/z8s](https://github.com/eramax/z8s), a standalone
single-node Kubernetes-compatible orchestrator + PID 1 init written in Rust. He built z8s
independently (first commit May 25, 2026; ~186 commits as of July 2026), reviewed pelagos,
concluded pelagos's foundation is cleaner, and filed this issue.

**z8s has zero references to pelagos** — the two projects developed in parallel. He is not
a pelagos contributor and z8s is not built on pelagos.

---

## What z8s is

z8s does exactly what #361 requests:

- PID 1 init with `signalfd`-based zombie reaping and graceful shutdown
- kubectl-compatible REST API on port 6443 (Axum, typed k8s resources)
- WebSocket exec with PTY support
- In-cluster DNS for service resolution
- NodePort/ClusterIP services
- Pod and Deployment lifecycle management
- Cgroups v2 resource limits
- Seccomp filtering, capabilities, Landlock
- Gossip-based multi-node clustering (in progress)
- redb-backed persistent state (in progress)

Stack: Rust, tokio, axum, hickory DNS, nix crate, landlock. No etcd, no kubelet, no k3s.
Single-file binary. Aggressive build optimization (LTO, strip, panic-abort).

---

## What the feature request is really asking

The issue is less "here are gaps I need filled" and more "I built this in z8s; pelagos's
runtime is better — can you become z8s so I can use pelagos instead?"

The implicit ask is one of:
- **Contributor pitch**: upstream z8s's orchestration layer into pelagos and collaborate
- **Migration request**: abandon z8s in favour of pelagos once pelagos covers the same ground

---

## Alignment analysis

### Tractable / independently valuable pieces
- **rustix migration** (#7 in the issue) — removes `libc`/`nix` unsafe FFI, eliminates
  `#[cfg(target_env = "gnu")]` branching for `RlimitResource`, type-safe signal constants.
  Self-contained, valuable regardless of the orchestration question.
- **PID 1 init** (#1) — zombie reaping via `signalfd` + subreaper would genuinely help
  `FROM scratch` images and compose stacks. Complements the existing pause/watcher design.

### Out of scope given pelagos's current direction
- **Kubernetes API server** (#2) — pelagos already runs *under* k3s/kubelet via CRI.
  Building a competing kubectl-compatible API server duplicates what k3s provides and adds
  enormous scope. This is z8s's core identity, not a gap in pelagos.
- **Gossip multi-node cluster + distributed scheduler** (#4) — essentially reimplementing
  etcd + kube-scheduler. Same issue: k3s already handles this on top of pelagos-cri.
- **redb persistent state** (#3) — medium scope; current JSON-file state works well. Worth
  revisiting if state grows more complex, but not urgent.
- **Networking CRDs** (#6) — interesting idea but depends on the API server question.

### Already done
- **System service** (#5) — pelagos already ships systemd unit files.

---

## Suggested response to eramax

Acknowledge z8s directly, note the overlap, and distinguish:
1. Thank him for the detailed request and the z8s reference
2. Flag that rustix + PID 1 init stand on their own merits and are worth separate issues
3. Be direct that the k8s API server / gossip cluster layer isn't on the roadmap since
   pelagos is designed to run *under* k3s, not replace it
4. Ask what problem he's actually trying to solve — single-node kubectl without k3s? Edge/embedded?
   That would clarify whether there's real alignment
5. Suggest z8s could build on top of pelagos as a library rather than merging the whole thing in

---

## Status
- No action taken on #361 yet
- Last comment on the issue is eramax asking for attention (July 2026)
