# Ongoing Tasks

## Session 2026-05-29 — Issue #261 COMPLETE; v0.64.0 RELEASED

Issue #261 (replace all `nft` shell-outs with native NETLINK_NETFILTER client) is complete.
PR #262 merged to main. Tag v0.64.0 pushed, release workflow in progress.

### What was done

- New `src/nfnetlink.rs` (~1540 lines): raw nfnetlink socket client, no new dependencies
- `src/network.rs`: all `run_nft`/`run_nft_quiet` call sites replaced with nfnetlink API
- Fixed two protocol bugs found during implementation:
  - `NFNL_SUBSYS_NFTABLES` constant was 12 (HOOK subsystem); correct value is 10
  - Verdict immediates (accept/jump) must use `REG_VERDICT=0` as dreg, not REG1
- 4 new `nfnetlink_native` integration tests (all `#[serial(nat)]`)
- All 10 dockerd tests serialized with the `nat` group (`serial(nat, dockerd)`)
- Improved error visibility: non-ENOENT failures from `nft_delete_ip_table` and
  `nft_remove_filter_forward_compat` now emit `log::warn`

### Test baseline (main, SHA 111d766, 2026-05-29)

- 360/360 unit tests pass
- Integration tests: ~337/349 pass (11 ignored, 2 pre-existing flaky tests:
  - `test_port_forward_independent_teardown` — race in parallel NAT cleanup, pre-dates #261
  - `auto_pull::test_run_auto_pulls_missing_image` — docker.io rate limiting)

### Release fixes (same session)

- `msghdr` struct literal init fails on aarch64-musl (private `__pad1`/`__pad2` fields);
  fixed with `zeroed() + field-assign` at 4 call sites in `src/nfnetlink.rs`
- ECR rate limit caused transient test failure in `ensure_alpine()`; added 3-attempt
  retry with 30s/60s backoff in both ECR-based `ensure_alpine` functions

Final tag: `b93d473` — release at https://github.com/pelagos-containers/pelagos/releases/tag/v0.64.0

### Next steps

Nothing pending. v0.64.0 released successfully.
