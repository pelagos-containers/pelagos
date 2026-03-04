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

## Current Baseline (2026-03-03, SHA b151b02)

- Unit tests: **286/286 pass**
- Integration tests: **198/198 pass**, 8 ignored (require external registries)
- E2E tests (BATS): **12/12 pass**
- OCI conformance (runtime-tools): **33 PASS / 4 FAIL** (failures are unfixable upstream bugs — #47, #48, #49)
- **Published to crates.io as `pelagos v0.1.3`**
- GitHub Release v0.1.3: static musl binaries for x86_64 and aarch64

## Completed This Session (2026-03-03)

**Wasm/WASI runtime support (epic #56, sub-issues #57, #58, #59)**

All three sub-issues implemented, tested, and merged to `main` in commit b151b02.

### #57 — Binary detection + runtime dispatch (src/wasm.rs)

New module `src/wasm.rs` providing the Wasm layer:

- `is_wasm_binary(path)` — reads 4-byte magic (`\0asm`); returns `Ok(false)` for
  missing/too-short files, never errors on absent file
- `WasmRuntime` enum — `Wasmtime | WasmEdge | Auto` (default)
- `WasiConfig` struct — `{ runtime, env: Vec<(String,String)>, preopened_dirs: Vec<PathBuf> }`
- `find_wasm_runtime(preferred)` — PATH search, preference order configurable
- `spawn_wasm()` — dispatches to wasmtime or wasmedge subprocess
- `build_wasmtime_cmd()` — wasmtime 14+ `--dir host::guest` identity-mapping syntax
- `build_wasmedge_cmd()` — wasmedge `--dir` / `--env` syntax

`Command` struct in `container.rs` gained three builder methods:
`with_wasm_runtime()`, `with_wasi_env()`, `with_wasi_preopened_dir()`.
`spawn()` auto-detects Wasm (magic bytes or explicit config) and calls
`spawn_wasm_impl()` instead of the Linux fork/namespace path.
`Stdio` enum gained `Copy + Clone + PartialEq + Eq` to allow caching by value.

### #58 — OCI Wasm artifact support (src/image.rs, src/cli/image.rs, src/cli/run.rs)

- `WASM_LAYER_MEDIA_TYPES` constant — three recognised Wasm OCI media types
- `is_wasm_media_type()` — predicate on media type string
- `ImageManifest.layer_types: Vec<String>` — new `#[serde(default)]` field;
  backward-compatible with existing manifests on disk
- `ImageManifest::is_wasm_image()` — true if any layer has a Wasm media type
- `ImageManifest::wasm_module_path()` — path to the extracted `module.wasm` blob
- `extract_wasm_layer()` — copies OCI blob as `<layer_dir>/module.wasm`
  (no decompression needed; Wasm layers are raw blobs)
- `cmd_image_ls()` — TYPE column now shows "wasm" or "linux"
- `build_image_run()` — fast-path for Wasm images; skips overlayfs, calls
  `spawn_wasm()` directly with preopened dirs from bind-mounts and env from image config

### #59 — containerd-shim-wasm (src/bin/pelagos-shim-wasm.rs)

New binary `containerd-shim-pelagos-wasm-v1` implementing the containerd shim v2
protocol (ttrpc) via the `containerd-shim = "0.10"` crate:

- Registers as `io.containerd.pelagos.wasm.v1`
- `WasmState` — bundle path, spawned child, exit code
- `WasmShim` — implements `shim::Shim` + `shim::Task`
- Lifecycle: `create` records bundle, `start` calls `spawn_wasm()`, `state`
  polls liveness, `kill` forwards signal via nix, `wait` blocks on child,
  `delete` cleans up, `shutdown` signals shim exit
- `parse_oci_config()` — extracts wasm path, argv, and WASI env from OCI
  `config.json`

Install (or symlink) as `containerd-shim-pelagos-wasm-v1` in PATH and add to
containerd config:
```toml
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.wasm]
  runtime_type = "io.containerd.pelagos.wasm.v1"
```

### Tests

8 new unit tests in `src/wasm.rs` (magic bytes, media types, PATH search).
8 new integration tests in `tests/integration_tests.rs` (wasm_tests module).
All documented in `docs/INTEGRATION_TESTS.md`.

## Pending / Next Steps

Suggested starting points:
- #52 (AppArmor/SELinux) — highest real-world security impact
- #60 (io_uring seccomp profile) — useful complement to existing seccomp work
- #61 (CRIU) — complex but differentiating checkpoint/restore feature

## Session Notes

For historical session notes (completed work, design rationale) see git log.
