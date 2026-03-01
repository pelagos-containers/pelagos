# Ongoing Tasks

## Completed: remora exec PID namespace join (2026-02-28)

### Context

`remora exec` did not join the container's PID namespace. When a PID namespace is
active, `state.pid` is the intermediate process P whose `/proc/P/ns/pid` is the host
PID namespace. The fix uses `/proc/P/ns/pid_for_children` as a fallback in
`discover_namespaces`, and implements a double-fork in `container.rs` step 1.65 to
actually enter the target namespace (since `setns(CLONE_NEWPID)` alone only updates
`pid_for_children` — the calling process is not moved; only a subsequent fork enters
the new namespace, followed by exec).

GitHub issue: #1 (closed by this work).

### Files changed

- `src/cli/exec.rs`: `discover_namespaces` — `pid_for_children` fallback
- `src/container.rs`: step 1.65 Case B — PID namespace join double-fork (both
  `spawn()` and `spawn_interactive()` pre-exec hooks)
- `tests/integration_tests.rs`:
  - `build_exec_command` helper updated with `pid_for_children` fallback
  - new test `exec::test_exec_joins_pid_namespace`
- `docs/WATCHER_PROCESS_MODEL.md`: updated caveat section, marked limitation fixed
- `docs/INTEGRATION_TESTS.md`: added `test_exec_joins_pid_namespace` entry

---

## Completed: watcher subreaper (2026-02-28)

### Context

When a container uses a PID namespace, the watcher forks an intermediate process P
which then forks the container C.  If the watcher was killed unexpectedly (OOM, etc.),
P was re-parented to host PID 1 rather than the watcher.  P's `PR_SET_PDEATHSIG`
(SIGKILL to C) depends on P's parent dying — but after re-parenting to init, that
signal never fires and C becomes an orphan.

The fix calls `prctl(PR_SET_CHILD_SUBREAPER, 1)` in the watcher (and compose
supervisor) immediately after `setsid()`.  This makes the watcher the reaper for all
orphaned descendants; if the watcher is killed, P is re-parented to the watcher not to
init, and P's pdeathsig fires in one hop when the watcher exits.

GitHub issue: #5 (closed by this work).

### Files changed

- `src/cli/run.rs`: added `prctl(PR_SET_CHILD_SUBREAPER, 1)` after `setsid()` in
  the watcher child branch
- `src/cli/compose.rs`: added `prctl(PR_SET_CHILD_SUBREAPER, 1)` after `setsid()` in
  both the daemonize path (line ~220) and the foreground-with-hooks path (line ~347)
- `tests/integration_tests.rs`: new module `watcher`, new test
  `test_watcher_kill_propagates_to_container`
- `docs/WATCHER_PROCESS_MODEL.md`: marked limitation fixed, updated signal propagation
  prose and known-limitations table
- `docs/INTEGRATION_TESTS.md`: added `test_watcher_kill_propagates_to_container` entry

---

## Completed: health probe timeout SIGKILL (2026-02-28)

### Context

When a health probe timed out, the probe child process was abandoned — the probe
thread was left running until the OS cleaned it up. This left a stray process and
consumed a thread slot in the watcher indefinitely.

The fix introduces `exec_in_container_with_pid_sink` in `src/cli/exec.rs`, which
stores the spawned child's host PID into an `Arc<AtomicI32>` immediately after
`spawn()` (before blocking on `wait()`). `run_probe` in `health.rs` passes this
sink to the probe thread. On `recv_timeout`, the monitor reads the PID from the
shared atomic and sends `SIGKILL`, ensuring the child is cleaned up immediately.

GitHub issue: #2 (closed by this work).

### Files changed

- `src/cli/exec.rs`: new `exec_in_container_with_pid_sink` + `Arc<AtomicI32>` import
- `src/cli/health.rs`: `run_probe` updated to pass pid sink + SIGKILL on timeout
- `tests/integration_tests.rs`: new test
  `healthcheck_tests::test_probe_child_pid_is_killable`
- `docs/WATCHER_PROCESS_MODEL.md`: marked probe-timeout limitation as fixed
- `docs/INTEGRATION_TESTS.md`: added `test_probe_child_pid_is_killable` entry

---

## Completed: epoll log relay (2026-02-28)

### Context

Each watcher previously spawned two dedicated relay threads (one for stdout, one
for stderr). This cost 2 threads per container at steady state. The fix replaces
both with a single `epoll`-based relay thread in `src/cli/relay.rs` that
multiplexes both pipe fds via `epoll_wait`, reducing the static thread count per
container from 3 to 2 (main + relay, down from main + stdout relay + stderr relay).

GitHub issue: #3 (closed by this work).

### Files changed

- `src/cli/relay.rs`: new module — `start_log_relay`, `relay_loop` (epoll), 3 unit
  tests
- `src/cli/mod.rs`: added `pub mod relay;`
- `src/cli/run.rs`: replaced two relay `thread::spawn` + two `join` calls with
  `super::relay::start_log_relay`; removed unused `Read` import
- `src/cli/compose.rs`: replaced two relay `thread::spawn` calls with
  `super::relay::start_log_relay`; removed unused `Read` import
- `docs/INTEGRATION_TESTS.md`: added entries for all three relay unit tests

---

## Completed: UDP proxy thread joining (2026-02-28)

### Context

UDP proxy threads (one per mapped port, plus one per active client session) were
never explicitly joined. `teardown_network` set the stop flag but returned
immediately; threads exited within 100ms on their own. This meant the inbound
socket was still held briefly after teardown returned, and reply threads had no
explicit synchronisation point.

The fix stores per-port `JoinHandle`s in `NetworkSetup.proxy_udp_threads`.
`teardown_network` now drains and joins them after setting the stop flag, ensuring
the inbound socket is released before the function returns.  `start_udp_proxy`
accumulates reply-thread handles and joins them all after its main loop exits (once
the stop flag causes the loop to terminate), completing the cleanup chain.

GitHub issue: #4 (closed by this work).

### Files changed

- `src/network.rs`:
  - `NetworkSetup`: added `proxy_udp_threads: Vec<JoinHandle<()>>`
  - `start_port_proxies`: changed return type to 3-tuple; collects per-port handles
  - callsite: destructured 3-tuple, stored `proxy_udp_threads` in `NetworkSetup`
  - `teardown_network`: joins per-port threads after setting stop flag
  - `start_udp_proxy`: collects reply handles, prunes finished ones, joins remainder
  - secondary `NetworkSetup` literal: added `proxy_udp_threads: Vec::new()`
- `tests/integration_tests.rs`: new test
  `networking::test_udp_proxy_threads_joined_on_teardown`
- `docs/INTEGRATION_TESTS.md`: added entry
- `docs/WATCHER_PROCESS_MODEL.md`: marked limitation as fixed

---

## All issues resolved

All four open issues are now closed.

---

## Runtime Strategy Analysis (2026-02-28)

A strategic analysis of the container runtime landscape, remora's position, and prioritized
technical opportunities has been written to:

**`docs/RUNTIME_STRATEGY_2026.md`**

Key findings:

- Remora is structurally immune to the November 2025 runc TOCTOU CVE cluster
  (CVE-2025-31133, CVE-2025-52565, CVE-2025-52881) — worth documenting loudly.
- Top gaps vs production runtimes: AppArmor/SELinux support; OCI lifecycle completeness.
- Top differentiation opportunities: Landlock LSM (first Rust runtime), crates.io
  publication for AI agent embedding, `SECCOMP_RET_USER_NOTIF` supervisor mode.
- Performance target: ≤ 180 ms median cold-start (between crun ~153 ms and youki ~198 ms).

See the doc for the full runtime comparison matrix, CVE analysis, Wasm/WASI trends,
embedded/IoT landscape, and the ranked opportunity list.

---

## Completed: OCI Runtime Spec compliance Phases 1–6 (2026-02-28)

Epic issue: #11 (closed).

### Summary

Full OCI lifecycle compliance implemented across 6 phases, merged to main as PRs #18–22.

| Phase | Content | PR |
|-------|---------|-----|
| 1 | `--bundle`, `--console-socket`, `--pid-file` CLI flags | #18 |
| 2 | Kernel mount type dispatch (proc, sysfs, devpts, mqueue, cgroup2) | #19 |
| 3+4 | Complete cap/signal tables, annotations, double-proc-mount fix, tmpfs flag fix | #20 |
| 5 | `linux.rootfsPropagation` + `linux.cgroupsPath` | #22 (rebased) |
| 6 | `createContainer`/`startContainer` hooks in container namespace | #22 |

### Key bugs fixed

- **`OciHooks` serde rename**: `OciHooks` was missing `#[serde(rename_all = "camelCase")]`,
  causing `createContainer` / `startContainer` / `createRuntime` hook arrays to be silently
  ignored on deserialization (JSON key `"createContainer"` never matched field `create_container`).
  Fixed by adding the attribute — the root cause of hook test failures.

- **Double proc mount**: `build_command` auto-added `with_proc_mount()` when a mount namespace
  was requested, but OCI bundles that already list a `proc`-type mount caused a double-mount
  failure. Fixed with an `has_explicit_proc` guard.

- **tmpfs flag vs data**: OCI mount options like `nosuid`, `strictatime` were passed as the
  `data` string argument to `mount(2)` instead of the flags argument, causing `EINVAL`.
  Fixed by parsing known MS_* flag names out of options before calling `with_kernel_mount`.

### 18 OCI lifecycle integration tests all pass.


---

## Active: OCI Full Compliance — console-socket + runtime-tools conformance (2026-03-01)

Epic issue: TBD (will be created)

### Goal

Reach full OCI Runtime Spec compliance:
1. Implement `console-socket` PTY fd passthrough (`process.terminal = true`)
2. Pass the `opencontainers/runtime-tools` conformance suite with zero failures

---

### Background

Phases 1–6 (epic #11) closed the known structural gaps. Two items remain:

**A. `console-socket` is a stub.**  
When `process.terminal: true`, the OCI spec requires the runtime to allocate a PTY,
wire the slave as the container's stdin/stdout/stderr, and send the PTY master fd to
the Unix socket at `--console-socket` via `sendmsg(SCM_RIGHTS)`. Currently the
`_console_socket` parameter is accepted but entirely ignored.

**B. The `opencontainers/runtime-tools` conformance suite has never been run.**  
Our 18 OCI tests exercise behaviours we wrote for; runtime-tools generates ~80 test
bundles covering the full spec and exposes edge-cases we haven't thought of.

---

### Sub-issue 1: console-socket PTY fd passthrough

**Files:** `src/container.rs`, `src/oci.rs`, `tests/integration_tests.rs`,
`docs/INTEGRATION_TESTS.md`

#### container.rs changes

1. Add field `pty_slave: Option<i32>` to `Command` struct.
2. Add builder method `with_pty_slave(fd: i32) -> Self`.
3. Capture `pty_slave` in the `pre_exec` closure of **both** `spawn()` and
   `spawn_interactive()`.
4. In pre_exec, just before the OCI-sync block (write PID + accept), if
   `pty_slave` is Some(slave_fd):
   ```
   libc::setsid();
   libc::dup2(slave_fd, 0);   // stdin
   libc::dup2(slave_fd, 1);   // stdout
   libc::dup2(slave_fd, 2);   // stderr
   libc::ioctl(slave_fd, TIOCSCTTY, 0);  // make controlling terminal
   if slave_fd > 2 { libc::close(slave_fd); }
   ```
   slave_fd must NOT be CLOEXEC (so it survives fork chains to reach pre_exec).

#### oci.rs changes

1. Add helper `fn send_fd_to_console_socket(path: &Path, fd: i32) -> io::Result<()>`:
   - `UnixStream::connect(path)` to connect to caller's socket
   - Build `msghdr` with `SCM_RIGHTS` ancillary data containing the master fd
   - `sendmsg()` the 1-byte dummy payload + ancillary data
   
2. In `cmd_create`, before building the command, detect terminal+socket:
   ```rust
   let pty = if config.process.as_ref().map_or(false, |p| p.terminal)
               && console_socket.is_some() {
       let p = nix::pty::openpty(None, None)?;
       // ensure slave NOT CLOEXEC; master CLOEXEC
       Some((p.master.into_raw_fd(), p.slave.into_raw_fd()))
   } else {
       None
   };
   ```

3. Add `with_pty_slave(slave_raw)` to command if PTY allocated.

4. In shim branch (`0 =>`): `close(master_raw)`.

5. In parent branch: `close(slave_raw)` immediately after fork; after ready pipe,
   call `send_fd_to_console_socket(sock, master_raw)` then `close(master_raw)`.

#### Integration test

`test_oci_console_socket`: Build an OCI bundle with `process.terminal: true`.
Create a Unix socket listener. Run `remora create --console-socket <path>`.
Assert that the listener receives exactly one fd via SCM_RIGHTS, and that the
received fd is readable/writable as a PTY master (write to it, container echoes back).

---

### Sub-issue 2: runtime-tools conformance suite

**Repo:** `https://github.com/opencontainers/runtime-tools`

#### Setup
```bash
git clone https://github.com/opencontainers/runtime-tools /tmp/runtime-tools
cd /tmp/runtime-tools
make runtimetest
```

#### Run
```bash
sudo RUNTIME=$(which remora) make validate
# or
sudo go test -v ./validation/... -args -runtime=$(which remora) 2>&1 | tee /tmp/rt-results.txt
```

Each failing test produces a bundle + error message. Fix each failure in its own
commit, re-run until `0 failures`.

#### Expected failures to fix

Beyond console-socket (covered in sub-issue 1), likely failures include:
- Any OciProcess fields not yet parsed (`oomScoreAdj`, etc.)
- Edge cases in mount option parsing
- Any spec requirement in hook state JSON we've missed
- Device node handling details
- Anything in the namespace path-join flow

All fixes go into the same PR as the runtime-tools run, with individual commits
per fixed failure category.

---

### Execution Order

```
1. Create epic + sub-issues in GitHub
2. Branch: fix/oci-console-socket
   - container.rs: with_pty_slave() + pre_exec setup
   - oci.rs: send_fd_to_console_socket() + cmd_create wiring
   - tests + docs
   - PR, merge
3. Branch: fix/oci-runtimetools
   - Clone + build runtime-tools
   - Run conformance suite, collect failures
   - Fix each failure, re-run, iterate until clean
   - PR, merge
4. Run full integration test suite (sudo -E cargo test --test integration_tests)
5. Resolve epic + report
```

---
