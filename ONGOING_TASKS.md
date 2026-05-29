# Ongoing Tasks

## Session 2026-05-29 — Issue #265: OCI seccomp arg conditions (feat/oci-seccomp-args-devices-265)

### Scope adjustment

After reading the code, `linux.devices` node creation is **already implemented** —
`oci.rs` translates each `linux.devices[]` entry to a `DeviceNode` and calls
`cmd.with_device()`, which does `mknod` in pre_exec (container.rs). No work needed there.

The sole remaining work is **`linux.seccomp` argument conditions** (`args` field per
syscall rule). The comment in `src/seccomp.rs:454` explicitly says:
> "Argument conditions (`args`) are ignored in this first-pass implementation."

### Root cause

`filter_from_oci` in `src/seccomp.rs` builds `filtered_rules` by calling
`filtered_rules.entry(num).or_default()` — this creates an empty `Vec<SeccompRule>`,
meaning "match any call to this syscall → match_action". The `rule.args` field is
parsed (already in `OciSyscallArg`) but never translated to `SeccompCondition`s.

### What needs to change

#### 1. `src/oci.rs` — add `value_two` to `OciSyscallArg`

`SCMP_CMP_MASKED_EQ` requires two values: `value` is the mask, `valueTwo` is the
expected masked result. The field is missing from our struct.

```rust
#[derive(Debug, Deserialize)]
pub struct OciSyscallArg {
    pub index: u32,
    pub value: u64,
    #[serde(default, rename = "valueTwo")]
    pub value_two: u64,
    pub op: String,
}
```

#### 2. `src/seccomp.rs` — translate args to SeccompConditions

Update imports:
```rust
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp,
    SeccompCondition, SeccompFilter, SeccompRule,
};
```

Add helper `oci_arg_to_condition(arg: &OciSyscallArg) -> Option<SeccompCondition>`:
- Maps OCI op strings to `SeccompCmpOp` variants
- `SCMP_CMP_MASKED_EQ`: op = `MaskedEq(arg.value)`, comparison value = `arg.value_two`
- All other ops: use `arg.value` as the comparison value
- Uses `SeccompCmpArgLen::Qword` for all conditions (64-bit, safe for all syscall args)

In the rule-building loop, replace `entry.or_default()` with:
- If `rule.args` is empty → clear the entry (unconditional match wins over any
  prior arg-conditional rules for the same syscall)
- If `rule.args` is non-empty → build `SeccompCondition`s (ANDed), push a
  `SeccompRule::new(conditions)` to the entry (ORed with other rules for same syscall)

OCI semantics map directly to seccompiler:
- Multiple `args` within one rule → AND (all conditions in one `SeccompRule`)
- Multiple `OciSyscallRule` entries for same syscall → OR (multiple `SeccompRule`s in Vec)

Also remove the dead first-pass loop (lines 478-499) that built `rules` but was
never used. Update the doc comment to remove "args are ignored".

### Test plan

#### Unit test in `src/seccomp.rs`

`test_filter_from_oci_with_args` — build an `OciSeccomp` directly in code with:
- `defaultAction: SCMP_ACT_ALLOW`
- One syscall rule: `socket`, action `SCMP_ACT_ERRNO`, args `[{index:0, op:SCMP_CMP_EQ, value:16}]`

Assert the BpfProgram compiles without error (no kernel required).

#### Integration test `test_oci_seccomp_args`

OCI bundle with `defaultAction: SCMP_ACT_ALLOW` and one rule:
- block `socket` when `arg[0] == 16` (AF_NETLINK = 16)

Run a shell command inside the container that:
1. Opens an AF_INET socket (must succeed → prints `INET_OK`)
2. Opens an AF_NETLINK socket (must fail with EPERM → prints `NETLINK_FAIL`)

Use a small static C program compiled inside the test or a direct syscall via
`busybox` approach. Simplest: compile a small C source via `cc` in the test tmpdir,
copy into the bundle rootfs overlay, and run it via the OCI lifecycle.

Assert stdout contains `INET_OK` and `NETLINK_FAIL`.

Document in `docs/INTEGRATION_TESTS.md`.

### Files to change

1. `src/oci.rs` — add `value_two` to `OciSyscallArg`
2. `src/seccomp.rs` — implement arg conditions in `filter_from_oci`
3. `tests/integration_tests.rs` — `test_oci_seccomp_args`
4. `docs/INTEGRATION_TESTS.md` — document new test

---

## Previous: Session 2026-05-29 — Issue #263 CRI cleanup COMPLETE

PR #264 merged. Issue #263 closed. pasta installed on ipc1/ipc2/ipc3.

## Previous: Session 2026-05-29 — Issue #261 COMPLETE; v0.64.0 RELEASED

Final tag: `b93d473` — https://github.com/pelagos-containers/pelagos/releases/tag/v0.64.0
