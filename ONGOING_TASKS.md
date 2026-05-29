# Ongoing Tasks

## Session 2026-05-29 — Issue #263 CRI cleanup (in progress)

Branch: `fix/cri-cleanup-263`

### Root cause analysis

Three CRI issues found during v0.64.0 deployment to ipc cluster:

1. **Port-forward broken** (`kubectl port-forward` → "unable to negotiate protocol: client
   supports 'portforward.k8s.io', server returned ''")
2. **Exit code propagation for non-1 values broken** (`exit 42` propagates as exit 1)
3. **pasta not installed on ipc2/ipc3**

Issues 1 and 2 share the same root cause: `send_upgrade_response` in
`spdystream-rs/src/server.rs` always sends a fixed 101 response without echoing
`X-Stream-Protocol-Version` back to the client.

- For **exec/attach**: kubectl sends `X-Stream-Protocol-Version: v4.channel.k8s.io` (among
  others for negotiation); server must echo the chosen version. Without v4 being confirmed,
  kubectl falls back to v1/v2/v3 which don't use the error stream for exit codes → exit code
  always appears as 1 (kubectl's own generic failure code).
- For **port-forward**: kubectl sends `X-Stream-Protocol-Version: portforward.k8s.io/v1`;
  server must echo it; without the echo kubectl rejects the upgrade entirely.

### Fix plan

#### spdystream-rs/src/server.rs

Change `send_upgrade_response` signature to accept an optional protocol string to
include as `X-Stream-Protocol-Version`:

```rust
pub async fn send_upgrade_response_with_protocol<S: AsyncWrite + Unpin>(
    stream: &mut S,
    protocol: Option<&str>,
) -> Result<()>
```

Keep the original `send_upgrade_response` calling the new one with `None` (backward compat
for tests). The response with a protocol:

```
HTTP/1.1 101 Switching Protocols\r\n
Connection: Upgrade\r\n
Upgrade: spdy/3.1\r\n
X-Stream-Protocol-Version: <protocol>\r\n
\r\n
```

#### spdystream-rs/src/server.rs — parse_upgrade_request

Already captures all headers in `req.headers`. No change needed; caller reads
`X-Stream-Protocol-Version` from `req.headers`.

#### pelagos-cri/src/streaming.rs — handle_connection

After `parse_upgrade_request`, select the protocol to confirm:

```rust
// For exec/attach: prefer v4 > v3 > v2 > v1
// For port-forward: use portforward.k8s.io/v1
let protocol = select_protocol(&req, kind);
send_upgrade_response_with_protocol(&mut tcp, protocol.as_deref()).await?;
```

Where `select_protocol` inspects the path kind and `X-Stream-Protocol-Version` header(s):
- `exec`/`attach` path → pick highest `*.channel.k8s.io` version the client offers
- `portforward` path → `portforward.k8s.io/v1`

Note: the `accept()` function in spdystream-rs (used elsewhere) can keep using the
no-protocol variant — it's not in the hot path for CRI.

#### pasta on ipc2/ipc3

Simple install: `sudo apt-get install -y passt` on both agent nodes.
No code change.

### Test plan

1. `cargo test -p spdystream-rs --lib` — new unit tests for `send_upgrade_response_with_protocol`
2. Deploy to ipc cluster: `scripts/install.sh` + `sudo systemctl restart pelagos-cri`
3. On ipc1:
   - `kubectl exec pod -- /bin/sh -c 'exit 42'; echo $?` → must print 42
   - `kubectl port-forward pod/test 9090:80 &; curl localhost:9090` → must succeed
4. `scripts/test-cri.sh` on ipc1 — all assertions pass

### Session 2026-05-29 — Issue #261 COMPLETE; v0.64.0 RELEASED

Issue #261 (replace all `nft` shell-outs with native NETLINK_NETFILTER client) is complete.
PR #262 merged to main. Tag v0.64.0 pushed and released.

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

### Release fixes

- `msghdr` struct literal init fails on aarch64-musl (private `__pad1`/`__pad2` fields);
  fixed with `zeroed() + field-assign` at 4 call sites in `src/nfnetlink.rs`
- ECR rate limit caused transient test failure in `ensure_alpine()`; added 3-attempt
  retry with 30s/60s backoff in both ECR-based `ensure_alpine` functions

Final tag: `b93d473` — release at https://github.com/pelagos-containers/pelagos/releases/tag/v0.64.0
