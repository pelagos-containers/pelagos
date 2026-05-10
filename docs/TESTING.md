# Testing Pelagos

This guide is the single entry point for understanding and running Pelagos tests.
Per-test documentation lives in [`INTEGRATION_TESTS.md`](INTEGRATION_TESTS.md).

---

## Before You Start: What Testing Pelagos Involves

**Building Pelagos is trivial.** It is pure Rust — `cargo build` is the entire build step,
no C/C++ toolchain required, dependencies are statically linked, and a fresh build takes
30–60 seconds. The build has no platform quirks beyond requiring Linux.

**Testing is a different matter.** Most tests exercise real kernel features: namespaces,
cgroups v2, overlayfs, nftables, seccomp-BPF, and user-mode networking. That means:

- Nearly every test requires root. You will type `sudo -E cargo test` a lot.
- Tests that modify shared kernel state (bridge interfaces, nftables tables, cgroup trees)
  must run serially. The full suite takes 15–20 minutes.
- Before running any networking or cgroup tests, the environment must be clean. The
  `scripts/reset-test-env.sh` script handles this; forgetting to run it is the single
  most common cause of test failures that look unrelated to the code being tested.
- The test suite has graceful skip logic throughout. A test that needs `alpine-rootfs`
  and doesn't find it skips rather than fails. A test that needs `pasta` and doesn't
  find it skips rather than fails. The skip output looks like a pass to Cargo, so read
  the output if something seems suspiciously fast.

**What you need on a fresh machine:**

| Requirement | For what | How to get it |
|---|---|---|
| Root / sudo | Almost all integration tests | sudoers or `sudo` group |
| `alpine-rootfs/` in project root | Most integration tests | `sudo scripts/build-rootfs-tarball.sh` |
| `iproute2` (`ip` command) | Bridge/NAT/veth tests | distro package |
| `nftables` (`nft` command) | NAT and port-forwarding tests | distro package |
| `pasta` / `passt` binary in PATH | Rootless networking tests | `passt` distro package |
| `fuse-overlayfs` | Rootless overlay on kernels < 5.11 | distro package |
| Linux ≥ 5.11 | Rootless overlay with `userxattr` (kernel overlayfs) | kernel version |
| Linux ≥ 5.13 | Landlock tests | kernel version; tests skip on older kernels |
| C compiler (`cc`) | io_uring seccomp tests | `gcc` or `clang` package |
| `wasmtime` binary in PATH | WASM tests | github.com/bytecodealliance/wasmtime |

Kernel-version tests skip gracefully. Everything else in the table is needed for a
complete suite run, but missing items cause targeted skips, not global failures.

---

## Test Layers

There are four layers of tests, in order from fastest and least privileged to
slowest and most privileged:

### 1. API tests — no root, no rootfs, ~10 seconds

```bash
cargo test --test integration_tests api::
```

These tests never spawn a process. They verify that the builder API compiles,
that bitflag operations work correctly, and that seccomp profile methods exist and chain.
Run these during active API development without ever reaching for `sudo`.

### 2. Unit tests — no root, ~20 seconds

```bash
cargo test --lib
```

Tests internal library logic: parsers, data structures, serialization. No containers
are spawned. Run these freely during development. They are also the CI lint gate.

### 3. Integration tests — requires root + rootfs, ~15–20 minutes

```bash
sudo -E cargo test --test integration_tests
```

The main test suite. ~200 tests across 56 modules in `tests/integration_tests.rs`.
They spawn real containers, create real network interfaces, and write to the real
cgroup tree. Must run serially (tests share kernel state).

See [Running Integration Tests](#running-integration-tests) below for how to run
subsets by category.

### 4. E2E scripts — requires root + rootfs, 10–60 minutes each

Shell scripts in `scripts/` that test the CLI binary end-to-end. These are the
slowest layer and exercise paths the integration tests don't cover (e.g., the full
`run --detach` → `ps` → `stop` → `rm` lifecycle, registry authentication, stress
scenarios).

See [E2E Scripts](#e2e-scripts) below for the full list.

---

## First-Time Setup

Do this once on a fresh machine or after re-cloning.

**1. Build the Alpine rootfs** (required for most integration tests):

```bash
# Without Docker (recommended):
sudo scripts/build-rootfs-tarball.sh

# With Docker:
sudo scripts/build-rootfs-docker.sh
```

This creates `alpine-rootfs/` in the project root. Both scripts produce the same result.

**2. Install system dependencies:**

```bash
# Arch / Manjaro
sudo pacman -S iproute2 nftables passt fuse-overlayfs

# Debian / Ubuntu
sudo apt-get install iproute2 nftables passt fuse-overlayfs
```

Install `wasmtime` separately from its GitHub releases if you need WASM tests.

**3. Initialize the Pelagos runtime directories:**

```bash
sudo scripts/setup.sh
```

Creates `/var/lib/pelagos/` with correct ownership, sets up the `pelagos` group,
and initializes network state directories.

**4. Build the binary:**

```bash
cargo build
```

**5. Reset the test environment:**

```bash
sudo scripts/reset-test-env.sh
```

Run this before every full suite execution. It flushes stale veth pairs, leftover
nftables rules, orphaned overlay mounts, and stopped DNS daemons. **Skipping this
is the most common cause of cascading test failures.**

**6. Run the full suite:**

```bash
sudo -E cargo test --test integration_tests
```

---

## Running Integration Tests

### Full suite

```bash
sudo -E cargo test --test integration_tests
```

Always run with `--test-threads=1` if you hit unexplained race failures on
shared resources (the default for this test binary is already serial for
tests that use `#[serial]`, but explicit is safer when debugging):

```bash
sudo -E cargo test --test integration_tests -- --test-threads=1
```

### By category

Cargo's filter is a substring match — append `::` to get the exact module:

```bash
sudo -E cargo test --test integration_tests core::
sudo -E cargo test --test integration_tests security::
sudo -E cargo test --test integration_tests filesystem::
sudo -E cargo test --test integration_tests cgroups::
sudo -E cargo test --test integration_tests networking::
```

Combine categories:

```bash
sudo -E cargo test --test integration_tests security:: capabilities::
sudo -E cargo test --test integration_tests networking:: linking::
```

### By individual test

```bash
sudo -E cargo test --test integration_tests test_bridge_network_ip
```

### Rootless tests — run WITHOUT sudo

```bash
cargo test --test integration_tests rootless::
```

These tests must run as a non-root user. They verify user namespace creation,
loopback in rootless mode, pasta networking, and rootless image operations.
Running them as root defeats the purpose and some will fail.

### Test categories at a glance

| Module | Tests | Root | Rootfs | What it covers |
|---|---|---|---|---|
| `api` | 5 | No | No | Builder API, bitflags, seccomp API surface |
| `core` | ~10 | Yes | Yes | Namespace creation, proc mount, combined features |
| `capabilities` | ~4 | Yes | Yes | Capability drop, selective keep |
| `resources` | ~5 | Yes | Yes | rlimits: FDs, memory, CPU time |
| `security` | ~12 | Yes | Yes | Seccomp profiles, no-new-privs, read-only rootfs, masked paths, Landlock |
| `mac` | ~3 | Yes | Yes | AppArmor and SELinux MAC profiles |
| `user_notif` | ~3 | Yes | Yes | Seccomp user-notification supervisor handlers |
| `filesystem` | ~8 | Yes | Yes | Bind mounts RW/RO, tmpfs, named volumes, overlayfs |
| `cgroups` | ~14 | Yes | Yes | Memory/CPU/PID limits, cpuset, stats, cleanup |
| `networking` | ~16 | Yes | Yes | Loopback, bridge, NAT, port forwarding, DNS, concurrent spawn |
| `ipv6` | ~4 | Yes | Yes | IPv6 ULA bridge networking |
| `oci_lifecycle` | ~11 | Yes | Yes | OCI create/start/state/kill/delete |
| `rootless` | ~10 | **No** | Yes | User namespaces, pasta, rootless image operations |
| `rootless_cgroups` | ~5 | No | Yes | Cgroup delegation from rootless containers |
| `images` | ~12 | Yes | No | OCI registry pull, caching, layer dedup |
| `exec` | ~6 | Yes | Yes | `pelagos exec` namespace join, env inheritance |
| `build_instructions` | ~15 | Yes | Yes | Remfile parsing, RUN isolation, multi-stage builds |
| `wasm_tests` | ~8 | Yes | No | WASM container execution |
| `dns` | ~6 | Yes | Yes | DNS configuration, resolv.conf binding, upstream forwarding |
| `port_proxy` | ~5 | Yes | Yes | TCP DNAT via nftables, localhost proxy |
| `multi_network` | ~6 | Yes | Yes | Multiple network modes, multi-attach |
| `system_prune` | ~5 | Yes | No | `pelagos system df`, `system prune` |
| `issue_*` | ~18 | Varies | Varies | Bug regressions (#109–#124+) |
| *(others)* | ~30 | Varies | Varies | Compose, linking, healthcheck, registry auth, etc. |

For per-test detail — what each test asserts and what failure indicates — see
[`INTEGRATION_TESTS.md`](INTEGRATION_TESTS.md).

---

## Environment Reset

Run `scripts/reset-test-env.sh` before a full suite execution and whenever
tests start behaving strangely:

```bash
sudo scripts/reset-test-env.sh
```

What it cleans up:
- Stale veth pairs (`vh-*`, `vp-*` interfaces)
- All `pelagos-*` nftables tables
- Named network namespaces created by pelagos
- Overlay mounts under `/run/pelagos/`
- The DNS daemon if it is running

If overlays are stuck and `reset-test-env.sh` doesn't fully clear them:

```bash
sudo scripts/cleanup-fuse-overlayfs.sh   # orphaned fuse-overlayfs mounts
sudo scripts/force-unmount.sh            # leftover /proc bind mounts
```

---

## Common Failures

### Tests fail immediately with "Permission denied" or "Operation not permitted"

You forgot `sudo -E`. The `-E` preserves environment variables — `CARGO_TARGET_DIR`,
`PATH`, and `RUST_LOG` all need to pass through to the test binary.

### Bridge/NAT/DNS tests fail after a previous failed run

Leftover nftables rules or veth pairs from the interrupted run are colliding.
Run `sudo scripts/reset-test-env.sh` and retry.

### "alpine-rootfs not found" / tests skip unexpectedly fast

The rootfs isn't built. Run `sudo scripts/build-rootfs-tarball.sh`. If it was
built as root and permissions are wrong, `sudo scripts/setup.sh` repairs them.

### Networking tests fail in CI / on a fresh machine but pass locally

Check that `iproute2` and `nftables` are installed. In CI, `passt` is also
required for the full suite — install the `passt` package.

### Cgroup tests fail with "cgroup hierarchy not found"

Pelagos requires cgroups v2 (unified hierarchy at `/sys/fs/cgroup`). If your
system uses cgroups v1 or a hybrid hierarchy, cgroup tests will fail. Check:

```bash
stat -fc %T /sys/fs/cgroup   # should print "cgroup2fs"
```

On systemd systems, enable unified hierarchy in the kernel command line:
`systemd.unified_cgroup_hierarchy=1`.

### Landlock tests skipped or failing

Landlock requires Linux 5.13+. On older kernels the tests skip themselves cleanly.
If they fail (rather than skip) on a recent kernel, check that Landlock is enabled:

```bash
grep LANDLOCK /boot/config-$(uname -r)
```

### io_uring seccomp tests skipped

These tests compile a small C probe binary at test time. If `cc` is not in PATH,
they skip. Install `gcc` or `clang`.

### Pasta tests skip

`pasta` (from the `passt` package) must be in PATH. Verify with `which pasta`.
On some distros the binary is named `passt` — create a symlink if needed:
`sudo ln -s /usr/bin/passt /usr/local/bin/pasta`.

### All tests in a serial group hang

The `#[serial(nat)]` attribute serializes networking tests. If a previous test in
the group panicked and didn't release the serial lock, the process hangs. Kill
the test runner (`Ctrl-C`) and restart after running the reset script.

---

## E2E Scripts

E2E scripts in `scripts/` test the compiled CLI binary as a whole rather than the
library API. Run them after `cargo build` (they use the binary from `target/debug/`).

All require root unless noted. All produce `PASS`/`FAIL`/`SKIP` output per check
and exit non-zero if any check fails.

### Core E2E

**`scripts/test-e2e.sh`** — comprehensive root-mode CLI coverage (~10 minutes):
container lifecycle (`run`, `ps`, `stop`, `rm`, `logs`), rootfs/volume/image
management, exec, networking flags, mount flags, security options, linking,
OCI lifecycle commands, and error case validation.

**`scripts/test-rootless.sh`** — rootless mode CLI (~8 minutes, no sudo):
pasta networking, rootless image pull, rootless exec, user namespace validation.

### Build and Compose

**`scripts/test-build.sh`** — `pelagos build` / Remfile parser (~12 minutes):
multi-stage builds, RUN isolation, ARG/ENV substitution, ADD with URL download,
build cache, `.remignore` patterns.

**`scripts/test-reml.sh`** — `pelagos compose` with S-expression (.reml) format
(~8 minutes): dependency ordering, TCP readiness polling, scoped naming,
`compose up/down/ps/logs`.

### WASM

**`scripts/test-wasm-e2e.sh`** — WASM container execution via external `wasmtime`
binary (~10 minutes). Requires `wasmtime` in PATH.

**`scripts/test-wasm-embedded-e2e.sh`** — embedded wasmtime feature (in-process,
no external binary) (~10 minutes). Requires `wasm32-wasip1` and `wasm32-wasip2`
Rust targets.

### Subsystem-Specific

**`scripts/test-exec.sh`** — `pelagos exec` into running containers (~3 minutes).

**`scripts/test-dev.sh`** — `/dev` device setup and device mounting (~5 minutes).

**`scripts/test-healthcheck.sh`** — HTTP health-check probes (~3 minutes).

**`scripts/test-ipv6-ping.sh`** — IPv6 bridge networking with ULA addresses.
Requires `ip` with IPv6 support.

**`scripts/test-networking-failures.sh`** — regression tests for PID namespace
veth/netns cleanup races (~2 minutes).

**`scripts/test-registry-auth-e2e.sh`** — OCI registry authentication
(Docker Hub, GitHub Packages, Basic auth) (~15 minutes, requires internet access).

### Stress and Conformance

**`scripts/test-stress.sh`** — stress and edge cases (~15 minutes): 5 concurrent
bridge containers with IPAM collision detection, NAT refcount under concurrent
load, signal propagation, cleanup after container crash, combined resource limits,
rapid sequential container cycles.

**`scripts/run-conformance.sh`** — OpenContainers runtime-tools conformance suite.
Requires the `runtime-tools` binary at `/home/cb/Projects/runtime-tools`.

### Utilities

**`scripts/bench-coldstart.sh`** — measures cold-start latency and compares against
`crun`/`runc`. Not a pass/fail test; produces timing output.

---

## Adding New Tests

Every new integration test must:

1. Be placed in the appropriate module in `tests/integration_tests.rs`.
2. Have an entry added to [`INTEGRATION_TESTS.md`](INTEGRATION_TESTS.md) in the
   same commit, documenting: function name, root/rootfs requirements, what it
   asserts, and what a failure would indicate.

This is a hard requirement — see the CLAUDE.md rule "Document Every Integration Test".

Tests that use bridge networking, NAT, or DNS must use `#[serial(nat)]`
(not just `#[serial]`) to avoid races with the unnamed serial group.
