# Ongoing Tasks

## Completed: Fix stale netns mounts — Add Drop impl for Child

### Summary

Added `impl Drop for Child` in `src/container.rs` so that dropping a `Child`
without calling `wait()` still cleans up network namespaces, cgroups, overlay
dirs, DNS temp dirs, pasta relays, and fuse-overlayfs mounts.

### Changes Made

1. **`src/container.rs`**: Extracted shared teardown logic into `teardown_resources(&mut self, preserve_overlay: bool)` method. Refactored `wait()`, `wait_with_output()`, and `wait_preserve_overlay()` to use it. All fields use `take()`/`drain()` for idempotency. Added `impl Drop for Child` that kills+reaps the child process then calls `teardown_resources()`. Changed `wait_with_output` from `(mut self)` to `(&mut self)` since you can't move fields out of a type implementing Drop.

2. **`src/cli/cleanup.rs`**: New `remora cleanup` subcommand that scans `/run/netns/rem-*`, `/run/remora/overlay-*`, `/run/remora/dns-*`, and `/run/remora/hosts-*` for orphaned entries whose owning PID is dead, and removes them.

3. **`src/cli/mod.rs`**: Added `cleanup` module.

4. **`src/main.rs`**: Added `Cleanup` subcommand variant and wired to `cli::cleanup::cmd_cleanup()`.

5. **`tests/integration_tests.rs`**: Added `test_child_drop_cleans_up_netns` test. Updated all `wait_with_output()` callers from `let child` to `let mut child` (signature changed to `&mut self`).

6. **`docs/INTEGRATION_TESTS.md`**: Documented the new test.

7. **Examples**: Updated `mut` bindings in `web_pipeline`, `full_stack_smoke`, `net_debug`.

### Bug Fix: wait_preserve_overlay missing secondary network teardown

`wait_preserve_overlay()` was missing teardown of secondary networks — now
handled by the shared `teardown_resources()` method.

## Next Task

(No next task planned — awaiting user direction.)
