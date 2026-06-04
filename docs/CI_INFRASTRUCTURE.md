# CI Infrastructure Notes

Options and trade-offs evaluated for self-hosted CI, with focus on native
arm64 builds for pelagos release artifacts.

## The Problem

pelagos ships musl-static binaries for both x86_64 and aarch64. The current
GitHub Actions workflow handles aarch64 via an Actions-hosted runner, which
likely uses QEMU emulation under the hood. Native arm64 builds are faster and
eliminate emulation risk.

Bringing CI in-house would remove the GitHub Actions dependency and cost,
enable faster builds, and allow CI jobs to run against the actual k3s cluster
(integration tests, package tests, etc.).

## ARM64 Hardware Options

### SBCs (Orange Pi, Raspberry Pi, RK3588 boards)

Not recommended at current prices. The SBC market has gotten expensive relative
to what you get — a fully equipped RK3588 board (case, PSU, NVMe) runs
$200-250 per node, and you still get inconsistent Linux support, thermal issues,
and hobbyist-grade reliability. Three nodes costs as much as a Mac Mini and
delivers less performance.

### Purpose-built ARM servers (Ampere Altra, Solidrun HoneyComb)

Server-class reliability and performance but expensive ($500-700+). Overkill
for a small home lab CI setup.

### Oracle Free Tier (Ampere A1)

4 Ampere cores, 24GB RAM, permanently free. Legitimate arm64 CI node at zero
cost. The right answer if you want arm64 builds now without hardware spend.
Joins the cluster via Tailscale. The main downside is it's a cloud dependency,
not in-house.

### Mac Mini (M4, 2026)

The best value for native arm64 if you want physical hardware.

- M4 is significantly faster per core than any SBC
- Runs Linux ARM64 VMs at near-native speed via Apple's Virtualization.framework
- Each VM registers as a k3s agent node — cluster sees them as normal nodes
- Low power, quiet, reliable, designed for 24/7 operation

**Current pricing (as of May 2026):**
Apple discontinued the $599/16GB/256GB base model in May 2026 due to high
demand (driven largely by local AI use). The line now starts at $799 with
512GB. Units are backordered.

The discontinued 16GB/256GB config appears on eBay around $600 — below
current new pricing and a reasonable deal for CI use, where 16GB is sufficient
and 256GB is manageable with discipline. Buyers of that config likely upgraded
for AI workloads where 16GB is limiting; for Rust builds and k3s VMs it's fine.

## VM Strategy on Mac Mini

Run 2-3 lightweight Ubuntu or Alpine ARM64 VMs via Virtualization.framework
(UTM is a good UI wrapper). Each VM:

- Gets a static LAN IP or Tailscale address
- Runs k3s agent, joins the existing cluster
- Is labeled `kubernetes.io/arch: arm64`
- CI jobs targeting arm64 use `nodeSelector` to land on these nodes

VM lifecycle (auto-start on host reboot, clean k3s rejoin) needs explicit
setup but is straightforward.

### Control plane placement

If the Mac Mini hosts the arm64 VMs, consider running the k3s control plane
there too (server nodes rather than agent nodes). This makes the beefiest
always-on hardware responsible for scheduling, and existing nodes (ipc1, etc.)
become workers.

## Storage for CI Builds

**Local NVMe: yes. NFS: no.**

Rust incremental compilation generates thousands of small files in `target/`
per build. NFS latency on metadata operations makes this 5-10x slower than
local, with occasional corruption risk on connection hiccups.

256GB local SSD is workable with discipline:
- Set `CARGO_HOME` to a known path; share the registry cache (downloaded crate
  sources) across VMs via a local bind mount — this part is read-heavy and
  safe to share
- Keep build `target/` directories per-VM, per-project
- `cargo clean` as part of CI job teardown, or periodically via cron
- Don't accumulate stale toolchain versions in `CARGO_HOME`

## CI System Options

If self-hosting CI on the cluster, reasonable options:

- **Gitea + Gitea Actions** — GitHub Actions-compatible syntax, lowest
  friction to migrate `.github/workflows/`. Self-hosted runners run as k3s
  pods or directly on nodes.
- **Tekton** — Kubernetes-native, more powerful, more complex.
- **Argo Workflows** — similar to Tekton, good UI.

Gitea Actions is probably the right starting point given existing GitHub
Actions workflow investment.
