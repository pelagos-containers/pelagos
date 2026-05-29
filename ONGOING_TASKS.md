# Ongoing Tasks

## Session 2026-05-29 — Issue #269: pelagos stats (feat/stats-269)

### Goal

Add `pelagos stats [--no-stream] [name...]` — Docker-compatible container
resource snapshot / live stream (CPU%, memory, PIDs).

### Design

- `StatsArgs { no_stream: bool, names: Vec<String> }`
- No `names` → all running containers
- `--no-stream`: one sample after a 500 ms warmup, print, exit
- Streaming: loop every 1 s, `\x1b[2J\x1b[H` to clear, print header + rows
- Columns: `NAME | CPU% | MEM USAGE / LIMIT | MEM% | PIDS`

### CPU% formula

```
delta_cpu_usec / (delta_wall_us * nprocs) * 100
```

Use `libc::sysconf(_SC_NPROCESSORS_ONLN)` for nprocs.

### Files to change

1. `src/cgroup.rs`
   - Add `memory_limit_bytes: Option<u64>` to `ResourceStats`
   - Add `pub fn open_cgroup(name: &str) -> Option<Cgroup>` — `Cgroup::load` on existing path
   - Update `read_stats` to also populate `memory_limit_bytes` from `memory.max`

2. `src/cli/stats.rs` — new module: `StatsArgs`, `cmd_stats()`

3. `src/main.rs`
   - Add `Stats(cli::stats::StatsArgs)` variant to `CliCommand`
   - Dispatch to `cli::stats::cmd_stats(args)`
   - Also add to `ContainerCmd`

4. `src/cli/mod.rs` — add `pub mod stats;`

5. `tests/integration_tests.rs` — `test_stats_no_stream`: start container, run
   `cargo run -- stats --no-stream <name>`, verify output parses and MEM > 0

6. `docs/INTEGRATION_TESTS.md` — document `test_stats_no_stream`

---

## Previous: Issue #267: fix errnoRet in OCI seccomp — COMPLETE
## Previous: Issue #265: OCI seccomp args — COMPLETE

---

## Previous: Issue #267: fix errnoRet in OCI seccomp (fix/seccomp-errno-ret-267)

### Problem

`filter_from_oci` maps both `SCMP_ACT_ERRNO` and `SCMP_ACT_ENOSYS` to
`SeccompAction::Errno(libc::EPERM as u32)`. Two OCI spec fields are ignored:

- `OciSeccomp.defaultErrnoRet` — errno for the default action when it is ERRNO
- `OciSyscallRule.errnoRet` — per-rule errno override

Docker's profile uses `SCMP_ACT_ENOSYS` (errno 38) extensively to signal
"not implemented" vs "not permitted". Containers get EPERM (1) where they
should get ENOSYS (38).

### Changes

#### `src/oci.rs`

Add `default_errno_ret` to `OciSeccomp` and `errno_ret` to `OciSyscallRule`:

```rust
pub struct OciSeccomp {
    pub default_action: String,
    #[serde(default, rename = "defaultErrnoRet")]
    pub default_errno_ret: Option<u32>,
    ...
}

pub struct OciSyscallRule {
    pub names: Vec<String>,
    pub action: String,
    #[serde(default, rename = "errnoRet")]
    pub errno_ret: Option<u32>,
    pub args: Vec<OciSyscallArg>,
}
```

Note: `OciSeccomp` uses `#[serde(rename_all = "camelCase")]` so `default_errno_ret`
would auto-rename to `defaultErrnoRet` — but be explicit with `rename` to be safe.
Actually since `rename_all = "camelCase"` is on the struct, `default_errno_ret` → 
`defaultErrnoRet` automatically. Add explicit `rename` anyway for clarity.

#### `src/seccomp.rs`

Update `oci_action_to_seccomp` to accept an `errno_ret: Option<u32>` parameter:

```rust
fn oci_action_to_seccomp(action: &str, errno_ret: Option<u32>) -> Option<SeccompAction> {
    match action {
        "SCMP_ACT_ALLOW" => Some(SeccompAction::Allow),
        "SCMP_ACT_ERRNO" => Some(SeccompAction::Errno(
            errno_ret.unwrap_or(libc::EPERM as u32)
        )),
        "SCMP_ACT_ENOSYS" => Some(SeccompAction::Errno(libc::ENOSYS as u32)),
        "SCMP_ACT_KILL" | "SCMP_ACT_KILL_THREAD" => Some(SeccompAction::KillThread),
        "SCMP_ACT_KILL_PROCESS" => Some(SeccompAction::KillProcess),
        "SCMP_ACT_LOG" => Some(SeccompAction::Log),
        "SCMP_ACT_TRAP" => Some(SeccompAction::Trap),
        _ => None,
    }
}
```

Update all three call sites:
- `oci_action_to_seccomp(&config.default_action, config.default_errno_ret)`
- `oci_action_to_seccomp(&rule.action, rule.errno_ret)` (appears twice in the rule loop)

### Test plan

#### Unit test

`test_filter_from_oci_errno_ret` — build `OciSeccomp` with `SCMP_ACT_ERRNO` and
`errno_ret: Some(38)` (ENOSYS). Verify it compiles. Also verify default (no
`errno_ret`) maps to EPERM. Pure compilation check, no kernel needed.

#### Integration test `test_oci_seccomp_errno_ret`

Compile a static C binary that:
1. Calls a blocked syscall (e.g. `personality(0xffffffff)`)
2. Prints `ERRNO:<n>` where n is the errno value
3. Runs with two OCI seccomp configs:
   - `errnoRet: 38` → assert output is `ERRNO:38`
   - No `errnoRet` → assert output is `ERRNO:1` (EPERM)

Also verify `SCMP_ACT_ENOSYS` returns 38 without needing `errnoRet`.

`personality` is a good test syscall — it's in the Docker blocked list, takes a
single argument, and doesn't require any special privileges or setup.

### Files to change

1. `src/oci.rs` — add `default_errno_ret` to `OciSeccomp`, `errno_ret` to `OciSyscallRule`
2. `src/seccomp.rs` — update `oci_action_to_seccomp` + three call sites
3. `tests/integration_tests.rs` — `test_oci_seccomp_errno_ret`
4. `docs/INTEGRATION_TESTS.md` — document new test

---

## Previous: Issue #265 COMPLETE

PR #266 merged. OCI seccomp arg conditions fully implemented and kernel-verified.

## Previous: Issue #263 CRI cleanup COMPLETE

PR #264 merged. pasta installed on ipc1/ipc2/ipc3.

## Previous: Issue #261 COMPLETE; v0.64.0 RELEASED

Final tag: `b93d473` — https://github.com/pelagos-containers/pelagos/releases/tag/v0.64.0
