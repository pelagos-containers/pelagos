# Ongoing Tasks

All work is tracked in GitHub Issues. This file is a brief index.

## Open Issues

| # | Title | Kind |
|---|-------|------|
| #47 | track: runtime-tools pidfile.t kill-on-stopped bug (upstream) | upstream |
| #48 | track: runtime-tools process_rlimits broken by Go 1.19+ (upstream) | upstream |
| #49 | track: runtime-tools delete tests hardcoded for cgroupv1 (upstream) | upstream |
| #52 | epic: AppArmor / SELinux profile support | epic |
| #56 | epic: Wasm/WASI shim mode (WasmMode) | epic |
| #57 | feat(wasm): detect Wasm binary and select runtime (wasmtime/WasmEdge) | feat |
| #58 | feat(wasm): OCI Wasm artifact support | feat |
| #59 | feat(wasm): containerd-shim-wasm compatibility layer | feat |
| #60 | feat: io_uring opt-in seccomp profile | feat/low-pri |
| #61 | feat: CRIU checkpoint/restore support | feat/low-pri |
| #62 | feat: minimal --features build for embedded/IoT | feat/low-pri |
| #63 | feat(mac): AppArmor profile template (sub of #51) | feat |
| #64 | feat(mac): SELinux process label support (sub of #51) | feat |

## Current Baseline (2026-03-03, SHA 8fc2d21)

- Unit tests: **278/278 pass**
- Integration tests: **190/190 pass**, 8 ignored (require external registries)
- E2E tests (BATS): **12/12 pass**
- OCI conformance (runtime-tools): **33 PASS / 4 FAIL** (failures are unfixable upstream bugs — #47, #48, #49)
- **Published to crates.io as `pelagos v0.1.3`**
- GitHub Release v0.1.3: static musl binaries for x86_64 and aarch64

## Completed This Session (2026-03-03)

**Remora → Pelagos rename (exhaustive)**

The directory, repo, and package name had been renamed to `pelagos` in a prior
session, but hundreds of internal references to `remora` remained. This session
completed the rename across the entire codebase in multiple passes:

1. Binary names: `remora` → `pelagos`, `remora-dns` → `pelagos-dns` (Cargo.toml)
2. File renames: `src/bin/remora-dns.rs`, `src/lisp/remora.rs`, `examples/multi-stage/.remignore`
3. Runtime paths: `/var/lib/remora` → `/var/lib/pelagos`, `/run/remora` → `/run/pelagos`
4. Network: bridge `remora0` → `pelagos0`, nft tables `remora-*` → `pelagos-*`
5. Env vars: `REMORA_DNS_BACKEND` → `PELAGOS_DNS_BACKEND`, `REMORA_REGISTRY_*` → `PELAGOS_REGISTRY_*`
6. Linux group: `remora` → `pelagos` (setup.sh, image.rs)
7. DNS domain suffix `.remora` → `.pelagos`
8. Lisp module: `register_remora_builtins` → `register_pelagos_builtins`
9. Shell variable `$REMORA` → `$PELAGOS` in all scripts and example runners
10. All README.md files in examples/, docs/, scripts/, CLAUDE.md, CHANGELOG

Key lesson: `sed` with `\b` word boundaries silently skips compound forms
(`Remora-specific`, `$REMORA`, `Remora's`). Use `perl -pi -e 's/remora/pelagos/g'`
without word boundaries for exhaustive replacement.

**Bug fix: aarch64-musl cross-compilation**

`src/notif.rs` used `SECCOMP_IOCTL_NOTIF_RECV/SEND` (u64) directly as the
`libc::ioctl` request argument. On aarch64-musl, `libc::ioctl` expects `i32`,
causing a compile error. Fixed by casting `as _`, letting rustc infer the
correct type per target. This unblocked the GitHub Actions release workflow.

## Pending / Next Steps

Nothing blocking. The open GitHub issues above represent the next feature
work. Suggested starting points:
- #52 (AppArmor/SELinux) — highest real-world security impact
- #56–59 (Wasm) — differentiating feature for the runtime

## Session Notes

For historical session notes (completed work, design rationale) see git log.
