# Ongoing Tasks

## Last Completed: REML Orchestration DSL Refinement (Feb 26, 2026)

### What shipped in v0.14.0

A significant refinement of the `.reml` imperative orchestration DSL.  The
API is now cleaner, more concise, and produces better error messages.

**Renamed primitives (breaking change):**

| Old | New |
|-----|-----|
| `derive` | `then` |
| `derive-all` | `then-all` |
| `:after` (in `start`) | `:needs` |

**New stdlib macros:**

| Macro | Purpose |
|-------|---------|
| `define-nodes (var svc) ...` | Declare multiple lazy start nodes at once |
| `define-then name upstream (param) body...` | Combined define + then with binding-name future |
| `define-results results (var "key") ...` | Destructure a `run` alist into named bindings |

**`run` now discovers transitive dependencies automatically.**  The terminal
list states intent ("I need these handles") rather than enumerating the full
graph.  `db-url` and `cache-url` no longer need to appear in the run list.

**Error messages now reference binding names.**  `define-then` passes the
binding name as `:name` to `then`, so future names in errors match the Lisp
source (`"db-url"` not `"db-then"`).

**Docs fully updated:** `REML_EXECUTOR_MODEL.md` rewritten, `USER_GUIDE.md`
imperative section replaced.  All old vocabulary (`container-start-async`,
`run-all`, `:after`, `:inject`) removed from docs.

### State as of v0.14.0

- Git SHA (remora): see `git log -1`
- 245 unit tests passing
- `compose.reml` and `compose-chain.reml` examples updated and correct

### Next steps

No active tasks.  Candidates for future work:

- Streaming results from `run` (push to channel as futures complete)
- Per-`run` cancellation scope (SIGTERM on failure, scoped to that run)
- `define-then-all` macro for multi-upstream joins
- Integration test for transitive dependency discovery with real containers
