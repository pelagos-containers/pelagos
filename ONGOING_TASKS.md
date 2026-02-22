# Ongoing Tasks

## Completed: `remora compose` — S-Expression Multi-Service Orchestration

### Summary

Added `remora compose up/down/ps/logs` with S-expression compose files. Includes
a zero-dependency recursive descent parser, typed compose model with validation
and topological sort, and a full CLI orchestrator with supervisor process,
TCP readiness polling, scoped naming, DNS registration, and log relay.

### Changes Made

1. **`src/sexpr.rs`** (NEW): Zero-dependency S-expression parser — `SExpr::Atom`/`List`, `parse()`, full test suite (atoms, quoted strings, nested lists, comments, errors)
2. **`src/compose.rs`** (NEW): Compose model — `ComposeFile`, `ServiceSpec`, `NetworkSpec`, `Dependency`, `VolumeMount`, `PortMapping`; `parse_compose()` AST-to-struct; `validate()` cross-references; `topo_sort()` Kahn's algorithm with cycle detection
3. **`src/cli/compose.rs`** (NEW): CLI orchestrator — `compose up` (parse, create scoped networks/volumes, supervisor, topo-order start, TCP readiness, DNS, log relay, monitor); `compose down` (reverse-order SIGTERM/SIGKILL, network/volume/state cleanup); `compose ps` (status table); `compose logs` (prefixed output)
4. **`src/paths.rs`**: Added `compose_dir()`, `compose_project_dir()`, `compose_state_file()` + tests
5. **`src/lib.rs`**: Added `pub mod sexpr` and `pub mod compose`
6. **`src/cli/mod.rs`**: Added `pub mod compose`
7. **`src/main.rs`**: Added `Compose` subcommand variant + dispatch
8. **`tests/integration_tests.rs`**: 5 no-root parser/model tests + 1 root test
9. **`docs/INTEGRATION_TESTS.md`**: Documented all 6 new tests

## Next Task

(No next task planned — awaiting user direction.)
