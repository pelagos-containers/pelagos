# Ongoing Tasks

## Session 2026-05-29 — completed

- **#269 (v0.64.0)** `pelagos stats` command — merged
- **#271 / #260** replace remaining ip/nft shell-outs with native netlink — merged
- **#272** native netlink teardown integration tests — merged
- **test suite serialization** — fixed 43 serial label races; 346/346 pass consistently

## Open issues (as of 2026-05-29, SHA c57e1f6)

| # | Title | Notes |
|---|-------|-------|
| 259 | verify IPv6 end-to-end on Mac+VM | depends on pelagos-mac#285 |
| 153 | embedded Wasm path never activates from `pelagos run` (piped stdio blocks) | regression |
| 141 | multiple containers binding same container port — no per-container ns isolation | concrete bug |
| 62 | minimal `--features` build for embedded/IoT | standalone |
| 67 | epic: Wasm/WASI deeper support | long-term |

See GitHub for full issue list.
