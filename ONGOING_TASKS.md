# Ongoing Tasks

## Current Task: Dual DNS Backend — builtin + dnsmasq

### Context

The embedded DNS server (`remora-dns`) works but is minimal — A-records only, no caching, no AAAA, blocking upstream forwarding, no EDNS/DNSSEC. Rather than building a full DNS server, support dnsmasq as an alternative backend. Keep the builtin for zero-dependency deployments.

**Default: builtin.** Users opt into dnsmasq via `--dns-backend dnsmasq` CLI flag or `REMORA_DNS_BACKEND=dnsmasq` env var.

### Implementation Steps

#### Step 1: `src/paths.rs` — New path helpers

- `dns_backend_file()` → `<runtime>/dns/backend`
- `dns_dnsmasq_conf()` → `<runtime>/dns/dnsmasq.conf`
- `dns_hosts_file(network)` → `<runtime>/dns/hosts.<network>`

#### Step 2: `src/dns.rs` — Backend abstraction + dnsmasq support

- Add `DnsBackend` enum (`Builtin`, `Dnsmasq`)
- `active_backend()` reads `REMORA_DNS_BACKEND` env var (cached in OnceLock)
- Extract existing `ensure_dns_daemon()` body → `ensure_builtin_daemon()`
- Add: `generate_dnsmasq_conf()`, `regenerate_dnsmasq_hosts()`, `ensure_dnsmasq_daemon()`, `stop_daemon()`
- Dispatch in `ensure_dns_daemon()`, `dns_add_entry()`, `dns_remove_entry()`
- Backend consistency: write/check `<runtime>/dns/backend` file
- Fallback: if dnsmasq not found, warn and use builtin

#### Step 3: `src/cli/run.rs` + `src/cli/build.rs` — CLI flag

- `--dns-backend <builtin|dnsmasq>` on RunArgs and BuildArgs
- Set `REMORA_DNS_BACKEND` env var before DNS calls

#### Step 4: Integration tests (3 new dnsmasq tests)

| Test | Asserts |
|------|---------|
| `test_dns_dnsmasq_resolves_container_name` | Same as builtin but with dnsmasq backend |
| `test_dns_dnsmasq_upstream_forward` | Upstream forwarding via dnsmasq |
| `test_dns_dnsmasq_lifecycle` | Daemon starts/stops with dnsmasq backend |

Skip if dnsmasq not on PATH.

#### Step 5: Documentation

- `docs/USER_GUIDE.md` — DNS Backend section
- `docs/INTEGRATION_TESTS.md` — Document 3 new tests
- `CLAUDE.md` — Update DNS section

### Files Changed

| File | Change |
|------|--------|
| `src/paths.rs` | Add `dns_backend_file()`, `dns_dnsmasq_conf()`, `dns_hosts_file()` |
| `src/dns.rs` | DnsBackend enum, active_backend(), dnsmasq helpers, dispatch |
| `src/cli/run.rs` | Add `--dns-backend` flag |
| `src/cli/build.rs` | Add `--dns-backend` flag |
| `tests/integration_tests.rs` | 3 new dnsmasq tests |
| `docs/USER_GUIDE.md` | DNS backend section |
| `docs/INTEGRATION_TESTS.md` | Document new tests |
| `CLAUDE.md` | Update DNS docs |

---

## Next Task: `remora compose`

Declarative multi-container stacks from a YAML file — replacing manual shell
scripts like `examples/web-stack/run.sh`.

---

## Previously Completed

### Embedded DNS Server (v0.4.x)
- `remora-dns` daemon with A-record resolution, upstream forwarding, SIGHUP reload
- Per-network config files, automatic lifecycle management
- 5 integration tests

### Multi-Network Containers (v0.4.0)
- Containers join multiple bridge networks simultaneously
- `attach_network_to_netns()` for secondary interfaces (eth1, eth2, ...)
- Smart `--link` resolution across shared networks
- 4 new integration tests

### Full feature list in CLAUDE.md
