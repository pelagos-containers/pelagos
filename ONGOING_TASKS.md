# Ongoing Tasks

## Last completed: PID namespace fixes + release v0.17.0 (2026-02-28, SHA c0ed571)

### What was done this session

**Bug fixes (all tests pass, shipped in v0.17.0):**

- **`src/container.rs`**: Added `pub fn add_namespaces(self, ns: Namespace) -> Self` — ORs
  into existing namespace flags rather than replacing them.

- **`src/cli/run.rs`**: `build_image_run` now calls `.add_namespaces(UTS|PID)` instead of
  `.with_namespaces(UTS|PID)`, which was silently dropping the `MOUNT` flag set by
  `with_image_layers()` and causing every image-based container to fail with
  "with_overlay requires Namespace::MOUNT".

- **`src/cli/exec.rs`**: Added `find_root_pid(pid)` — reads
  `/proc/{pid}/task/{pid}/children`; if exactly one child exists the caller is the
  PID-namespace intermediate process P (which never called `pivot_root`), so returns the
  child's PID (C = PID 1 in the container, which DID call `pivot_root`). All four
  root-fd open sites updated to use `find_root_pid`.

- **`src/network.rs`**: Extended port-forward state-file format to include `ns_name` so
  `read_port_forwards_count` can filter stale entries by liveness (`netns_exists`).
  Stale entries from crashed containers no longer block nftables table teardown.

**Docs:**

- **`docs/WATCHER_PROCESS_MODEL.md`**: Full thread inventory including dynamic threads —
  port-forward proxy threads (TCP listener, relay pairs, UDP proxy, UDP reply
  forwarder), UID/GID mapping thread, compose supervisor threads, thread-count formula.

### Current state

- All 268 unit tests pass
- All integration tests pass
- All e2e tests pass
- `cargo clippy -- -D warnings` clean
- `cargo fmt` clean
- v0.17.0 released (x86_64 + aarch64 musl static binaries)

### No pending tasks

---

## Next suggested areas

- `remora exec` PID namespace join: `discover_namespaces` could check
  `/proc/P/ns/pid_for_children` to let exec'd processes join the container's PID
  namespace (currently they see host PIDs). Documented in `WATCHER_PROCESS_MODEL.md`.

- Probe timeout SIGKILL: health probe threads are abandoned on timeout rather than
  explicitly killed. Low urgency.

- Port-forward proxy: replace thread-per-connection TCP relay with a single tokio async
  task pool (tokio is already a dep). Low urgency for typical workloads.
