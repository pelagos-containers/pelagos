# Ongoing Tasks

All work is tracked in GitHub Issues. This file is a brief index.

## Open Issues

| # | Title | Kind |
|---|-------|------|
| #47 | track: runtime-tools pidfile.t kill-on-stopped bug (upstream) | upstream |
| #48 | track: runtime-tools process_rlimits broken by Go 1.19+ (upstream) | upstream |
| #49 | track: runtime-tools delete tests hardcoded for cgroupv1 (upstream) | upstream |
| #52 | epic: AppArmor / SELinux profile support | epic |
| #60 | feat: io_uring opt-in seccomp profile | feat/low-pri |
| #61 | feat: CRIU checkpoint/restore support | feat/low-pri |
| #62 | feat: minimal --features build for embedded/IoT | feat/low-pri |
| #63 | feat(mac): AppArmor profile template (sub of #51) | feat |
| #64 | feat(mac): SELinux process label support (sub of #51) | feat |
| #67 | epic: deeper Wasm/WASI support | epic |
| #70 | feat(wasm): mixed Linux+Wasm compose validation (P1) | feat |
| #71 | feat(wasm): WASI preview 2 socket passthrough (P2) | feat |
| #72 | feat(wasm): Component Model via embedded wasmtime (P3) | feat/CLOSED |
| #73 | feat(wasm): persistent Wasm VM pool (P4) | feat/low-pri |
| #69 | fix: integration test suite hangs locally (DNS tests) | bug/CLOSED |

## Current Baseline (2026-03-04, SHA pending)

- Unit tests: **296/296 pass** (embedded-wasm feature), **294/294** (default)
- Integration tests: **205/205 pass, 8 ignored** (`--test-threads=1`)
- CI (GitHub Actions): **all 6 jobs pass** (lint, unit-tests, integration-tests, e2e-tests, wasm-e2e-tests, wasm-embedded-e2e-tests)
- E2E tests: **7 Wasm subprocess e2e tests pass** (`scripts/test-wasm-e2e.sh`)
- Embedded e2e tests: **14/14 pass** (`scripts/test-wasm-embedded-e2e.sh`, includes P3b component tests)

**Note for next session:** Run integration tests with `--test-threads=1` to avoid
network-state races between DNS tests. Always `sudo scripts/reset-test-env.sh`
if starting from a possibly dirty environment.

## Completed This Session (2026-03-03)

**Wasm/WASI follow-on work (issues #65, #66, #68)**

### #65 — Wasm e2e test script + host→guest dir mapping bug fix

- Fixed `WasiConfig.preopened_dirs`: changed from `Vec<PathBuf>` (identity-only)
  to `Vec<(PathBuf, PathBuf)>` (host, guest pairs)
- Updated `build_wasmtime_cmd`: `--dir host::guest` (double colon)
- Updated `build_wasmedge_cmd`: `--dir host:guest` (single colon)
- Added `with_wasi_preopened_dir_mapped(host, guest)` builder on `Command`
- Fixed `src/cli/run.rs` fast-path to use mapped version for `--bind`
- Fixed `src/bin/pelagos-shim-wasm.rs` rootfs identity tuple
- Created `scripts/test-wasm-e2e.sh` — 7 tests covering: image ls TYPE column,
  run basic output, env passthrough, --bind dir mapping, magic bytes
- Added 4 unit regression tests in `src/wasm.rs`:
  `test_wasmtime_cmd_identity_dir_mapping`, `test_wasmtime_cmd_mapped_dir`
  (regression guard — asserts identity form NOT produced), `test_wasmedge_cmd_mapped_dir`,
  `test_wasmtime_cmd_env_vars`

### #66 — CI integration for e2e tests

- Added `e2e-tests` job to `.github/workflows/ci.yml` (nftables, iproute2, passt,
  rootfs build, reset, then `scripts/test-e2e.sh`)
- Added `wasm-e2e-tests` job (wasm32-wasip1 target, wasmtime install, `scripts/test-wasm-e2e.sh`)
- All 5 CI jobs green on first push

### #68 — `pelagos build` Wasm target (P5-option-B)

- `FROM scratch` support in `execute_stage()`: starts with empty layers + default
  `ImageConfig` instead of pulling a base image
- `detect_wasm_layers(layers)`: post-build scan of each layer dir; if a layer
  contains exactly one `.wasm` file with valid magic bytes, renames it to
  `module.wasm` and records `"application/wasm"` as the layer media type
- Helper functions: `find_sole_wasm_file()`, `collect_layer_files()`
- Used in `execute_build()`: `layer_types: detect_wasm_layers(&layers)`
- 4 new integration tests in `wasm_build_tests` module:
  `test_build_wasm_from_scratch_detects_mediatype`,
  `test_build_wasm_second_layer_only`,
  `test_build_non_wasm_layer_not_detected`,
  `test_build_elf_with_wasm_extension_not_detected`
- CI: all 5 jobs pass (SHA df18ca1)

Also: `docs/WASM_SUPPORT.md` created — three-layer architecture, CLI examples,
comparison table vs runc/runwasi/Spin, limitations, roadmap pointing to epic #67.

## Bug #69 — RESOLVED

Root cause was dirty local environment state from a previous crashed session
(orphaned `pelagos-dns` processes, stale network namespaces). Post-reboot +
`sudo scripts/reset-test-env.sh` + `--test-threads=1` → 202/202 pass in 37s.
No code changes required. Issue closed.

## Completed This Session (2026-03-04)

### P3b — Wasm Component Model via embedded wasmtime (#72)

**Feature gate:** `--features embedded-wasm` (same as P3a)

- `src/wasm.rs`:
  - `WASM_MODULE_VERSION`: const `[0x01, 0x00, 0x00, 0x00]` to distinguish modules from components
  - `is_wasm_component_binary(path)`: reads bytes 4-7; returns `true` if magic matches but version ≠ module version
  - `run_embedded_inner`: now detects component vs plain module, routes to P2 or P1 path
  - `run_embedded_component_file`: private helper — loads component from disk with `component-model` engine, calls `run_embedded_component`
  - `run_embedded_component(engine, component, extra_args, wasi)`: `pub` entry point — implements `WasiView` via `WasiState { ctx: WasiCtx, table: ResourceTable }`, builds P2 context, calls `wasmtime_wasi::p2::add_to_linker_sync` + `Command::instantiate` + `call_run`, handles `I32Exit` in error chain
  - Fixed P3a argv TODO: `run_embedded_module` now calls `builder.arg("module.wasm")` + `builder.arg(arg)` for each extra arg
  - 4 new unit tests: `test_is_wasm_component_binary_{module_is_false,component_is_true,too_short_is_false,non_wasm_is_false}`
- `Cargo.toml`:
  - wasmtime: added `"component-model"` to features
  - wasmtime-wasi: added `"p2"` to features
- `src/build.rs`:
  - `detect_wasm_layers`: after renaming to `module.wasm`, checks `is_wasm_component_binary`; emits `"application/vnd.bytecodealliance.wasm.component.layer.v0+wasm"` for components, `"application/wasm"` for plain modules
- `tests/integration_tests.rs`:
  - `test_wasm_component_detection_from_bytes`: asserts module/component byte detection
  - `test_wasm_embedded_component_exit_code`: compiles hello.rs → wasm32-wasip2 at test time, runs component in-process, asserts exit code 0; skips if wasm32-wasip2 unavailable
- `scripts/test-wasm-embedded-e2e.sh`:
  - Component section: checks/installs `wasm32-wasip2`, compiles to component, builds image, runs 3 tests (output, env, bind); 14/14 total pass
- `.github/workflows/ci.yml`:
  - `wasm-embedded-e2e-tests` job: added `wasm32-wasip2` to targets list
- `docs/INTEGRATION_TESTS.md`: entries for 2 new integration tests

### P3a — Embedded wasmtime for plain Wasm modules

**Feature gate:** `--features embedded-wasm`

- Added `[features]` to `Cargo.toml`; optional deps `wasmtime 42` + `wasmtime-wasi 42`
- `src/wasm.rs`:
  - `run_wasm_embedded(program, extra_args, wasi)` — public entry point, spawns a thread
  - `run_embedded_module(engine, module, extra_args, wasi)` — inner function (pub for tests);
    uses `wasmtime_wasi::p1::add_to_linker_sync` + `WasiCtxBuilder::build_p1()`
  - I32Exit detection: traverses the full anyhow error chain (proc_exit wraps it in a backtrace context)
  - 2 unit tests: `test_embedded_exit_zero`, `test_embedded_exit_nonzero`
- `src/container.rs`:
  - `ChildInner` enum: `Process(std::process::Child)` + `#[cfg] Embedded(Option<JoinHandle<i32>>)`
  - `Child.inner` changed to `ChildInner`; `wait_inner()` helper handles both variants via `ExitStatusExt::from_raw`
  - `spawn_wasm_impl()`: uses embedded path when all stdio is Inherit and feature is on
  - Updated `pid()`, `take_stdout()`, `take_stderr()`, `wait()`, `wait_preserve_overlay()`,
    `wait_with_output()`, `Drop` to dispatch on `ChildInner`
- `tests/integration_tests.rs`: `wasm_embedded_tests::test_wasm_embedded_exit_code` (no root, no PATH runtime)
- `docs/INTEGRATION_TESTS.md`: entry for the new integration test
- **E2E test** (`scripts/test-wasm-embedded-e2e.sh`, 8/8 pass):
  - Builds pelagos with `--features embedded-wasm`
  - Compiles `scripts/wasm-embedded-context/hello.rs` → wasm32-wasip1
  - Runs `pelagos build` on `scripts/wasm-embedded-context/Remfile` (FROM scratch + COPY)
  - Strips wasmtime/wasmedge from PATH → proves in-process execution
  - Tests: basic output, `--env` passthrough, `--bind` preopened dir

## Wasm Epic #67 — Sub-issues

| # | GH | Title | Priority |
|---|----|-------|----------|
| P1 | #70 | Mixed Linux+Wasm compose validation | Medium |
| P2 | #71 | WASI preview 2 socket passthrough | Medium |
| P3 | #72 | Wasm Component Model via embedded wasmtime | **DONE** |
| P4 | #73 | Persistent Wasm VM pool | Low (depends on P3) |
| P5 | #68 | `pelagos build` Wasm target | **DONE** |

**P2/P3 socket note:** P2 threads socket flags through the subprocess CLI path
(`build_wasmtime_cmd`). P3 adds a separate embedded path via `WasiCtxBuilder`.
They coexist — P2 work is not thrown away. P3 must include socket support in
its `WasiCtxBuilder` setup for parity.

## Next Session: Start Here

1. **#52 — AppArmor/SELinux profile support** (highest real-world security impact)
   - Sub-issues: #63 (AppArmor template), #64 (SELinux process label)
   - Design choice to resolve: generate profiles at build time vs ship canned profiles
2. #60 (io_uring seccomp profile) — useful complement to existing seccomp work
3. #61 (CRIU checkpoint/restore) — complex but differentiating feature

## Session Notes

For historical session notes (completed work, design rationale) see git log.
