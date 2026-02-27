# Ongoing Tasks

## Current: (nothing pending — session wrap-up Feb 27, 2026)

All work from this session has been committed and pushed.  See "Completed this
session" below for the full list.

---

## Completed this session (Feb 27, 2026)

### Container isolation hardening for compose + lisp runtime

**Context:** `compose up` and the lisp runtime (`container-start`) were spawning
containers with essentially no security isolation beyond filesystem layering.
`Command::new()` starts with `Namespace::empty()` and no seccomp, no capability
dropping, no no-new-privileges, no masked paths.  The hanging compose-chain
(postgres orphaned workers keeping pipe write-ends open) was the trigger.

**What shipped:**

#### Namespace fix
- `Namespace::PID | UTS | IPC` added unconditionally in both `spawn_service`
  (compose.rs) and `do_container_start_inner` (runtime.rs), applied just before
  `spawn()` so they OR with any flags already accumulated (MOUNT from
  `with_image_layers`, NET from bridge setup).
- `Command::namespaces()` getter added to `src/container.rs` so callers can read
  the current flag set without clobbering it.
- `std::process::exit()` restored in `main.rs`; `_exit()` was a workaround for
  the pipe hang, which is now fixed at the source (PID namespace kills orphans).

#### Security hardening defaults
All four applied unconditionally in both execution paths (compose + lisp):
- `with_seccomp_default()` — Docker's ~300-syscall allowlist
- `drop_all_capabilities()` — zeros effective/permitted/inheritable cap sets via
  `capset()` syscall (bug fixed: previous implementation only called
  `PR_CAPBSET_DROP` on the bounding set; `CapEff` remained full as root)
- `with_no_new_privileges(true)` — blocks setuid/setgid escalation
- `with_masked_paths_default()` — hides `/proc/kcore`, `/sys/firmware`, etc.

#### `:cap-add` service spec support
Services that need specific capabilities (e.g. `CAP_NET_RAW`) declare them:
- `cap_add: Vec<String>` added to `ServiceSpec` in `src/compose.rs`
- `:cap-add` keyword parsed in both the static compose parser and the lisp
  `define-service` / `service` builtin (`src/lisp/remora.rs`)
- `parse_capability_mask()` helper in `src/cli/mod.rs`
- Capability names normalised: `net-raw` → `NET_RAW`; `CAP_` prefix optional

#### Regression tests
Two new integration tests in `tests/integration_tests.rs`:

| Test | Strategy |
|------|----------|
| `test_hardening_combination` | Raw `Command` builder with all four hardening calls; reads `/proc/self/status` from inside container via stdout; asserts `Seccomp:2`, `CapEff:0`, `NoNewPrivs:1`, innermost NSpid ≤ 5, `HOSTNAME=hardening-test` |
| `test_lisp_container_spawn_hardening` | `Interpreter::new_with_runtime`; starts `sleep 30`; locates inner child via `/proc/{pid}/task/.../children`; reads its `/proc/status` from host; asserts same four properties + UTS namespace isolation; skips if `alpine:latest` not pulled |

Both documented in `docs/INTEGRATION_TESTS.md`.

---

## Previous: Eager Async Model (Feb 26, 2026)

**Restored the original async contract**: `container-start-bg` + `container-join`
let a `.reml` script kick off multiple containers simultaneously without the
declarative graph, then collect the handles when their values are actually needed.

**New primitives:**

| Primitive | Signature | Description |
|-----------|-----------|-------------|
| `container-start` (updated) | `(svc [:env list])` → ContainerHandle | Now accepts `:env` (list of dotted pairs) for dynamic env injection |
| `container-start-bg` | `(svc [:env list])` → PendingContainer | Spawns in background thread; returns immediately |
| `container-join` | `(pending)` → ContainerHandle | Blocks until background start completes |

**New example:** `examples/compose/imperative/compose-eager.reml`

**State as of post-v0.14.0:** 251 unit tests passing, both executor models documented.
