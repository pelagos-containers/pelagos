# Ongoing Tasks

## Last Completed: Eager Async Model (Feb 26, 2026)

### What shipped (post-v0.14.0, unreleased)

**Restored the original async contract**: `container-start-bg` + `container-join`
let a `.reml` script kick off multiple containers simultaneously without the
declarative graph, then collect the handles when their values are actually needed.

**New primitives:**

| Primitive | Signature | Description |
|-----------|-----------|-------------|
| `container-start` (updated) | `(svc [:env list])` → ContainerHandle | Now accepts `:env` (list of dotted pairs) for dynamic env injection |
| `container-start-bg` | `(svc [:env list])` → PendingContainer | Spawns in background thread; returns immediately |
| `container-join` | `(pending)` → ContainerHandle | Blocks until background start completes |

**New `Value::PendingContainer`** — wraps `Arc<Mutex<Option<Receiver>>>`, is
`Clone`-safe (Arc), and allows double-join detection via the `Option::take()`.

**New example:** `examples/compose/imperative/compose-eager.reml` — shows both
sequential eager (db → url → app) and parallel eager (start-bg + join).

**Docs updated:** `REML_EXECUTOR_MODEL.md` (new "Eager Execution" section +
updated executor table), `USER_GUIDE.md` (eager model prose + table).

### State as of post-v0.14.0

- Git SHA (remora): see `git log -1`
- 248 unit tests passing
- Both executor models documented and exemplified

### Next steps

No active tasks.  Candidates for future work:

- Cut v0.15.0 release with the eager model
- Integration test for `container-start-bg` + `container-join` with real containers
  (verifies parallel startup overlap and error propagation)
- `define-then-all` macro for multi-upstream joins in the graph model
- Per-`run` cancellation scope (SIGTERM on failure, scoped to that run)
- Streaming results from `run` (push to channel as futures complete)
