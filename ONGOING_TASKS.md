# Ongoing Tasks

All work is tracked in GitHub Issues. This file is a brief index.

## Open Issues

| # | Title | Kind |
|---|-------|------|
| #44 | pidfd-based process identity for cmd_state/cmd_kill | feat/hardening |
| #47 | track: runtime-tools pidfile.t kill-on-stopped bug (upstream) | upstream |
| #48 | track: runtime-tools process_rlimits broken by Go 1.19+ (upstream) | upstream |
| #49 | track: runtime-tools delete tests hardcoded for cgroupv1 (upstream) | upstream |
| #50 | docs: document structural CVE immunity (TOCTOU class) | docs/quick |
| #51 | epic: AppArmor / SELinux profile support | epic |
| #52 | feat: Landlock LSM integration | feat/moderate |
| #53 | chore: publish remora as a crate on crates.io | chore/quick |
| #54 | feat: SECCOMP_RET_USER_NOTIF supervisor mode | feat/significant |
| #55 | chore: submit remora to OCI runtime benchmark suite | chore/quick |
| #56 | epic: Wasm/WASI shim mode (WasmMode) | epic |
| #60 | feat: CRIU checkpoint/restore support | feat/low-pri |
| #61 | feat: io_uring opt-in seccomp profile | feat/low-pri |
| #62 | feat: minimal --features build for embedded/IoT | feat/low-pri |
| #63 | feat(mac): AppArmor profile template (sub of #51) | feat |
| #64 | feat(mac): SELinux process label support (sub of #51) | feat |

## Conformance Baseline (as of 2026-03-02, v0.19.0)

- Integration tests: **180/180 pass**
- OCI conformance (runtime-tools): **33 PASS / 4 FAIL** (4 are unfixable upstream bugs — #47, #48, #49)

## Session Notes

For historical session notes (completed work, design rationale) see git log.
