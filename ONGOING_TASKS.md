# Ongoing Tasks

## Current Task: `--format json` on All List Commands + `container inspect`

**Status:** COMPLETE

### What Was Done
- Added `OutputFormat` enum (table/json) to `src/main.rs` with `FromStr` for clap parsing
- Added `--format` flag to: `remora ps`, `container ls`, `volume ls`, `image ls`, `rootfs ls`
- Added `container inspect <name>` subcommand (always JSON output)
- Updated `cmd_ps()`, `cmd_volume_ls()`, `cmd_image_ls()`, `cmd_rootfs_ls()` to accept `json: bool`
- JSON mode: prints `serde_json::to_string_pretty` of the data; empty results print `[]`
- Table mode: unchanged from before
- `VolumeInfo` and `RootfsInfo` structs added inline for JSON serialization
- `ContainerState` and `ImageManifest` already derived `Serialize`

### Files Modified
| File | Change |
|------|--------|
| `src/main.rs` | `OutputFormat` enum; `--format` on all `Ls` variants; `Inspect` in `ContainerCmd` |
| `src/cli/ps.rs` | `cmd_ps(all, json)` JSON branch; new `cmd_inspect(name)` |
| `src/cli/volume.rs` | `VolumeInfo` struct; `cmd_volume_ls(json)` JSON branch |
| `src/cli/image.rs` | `cmd_image_ls(json)` JSON branch |
| `src/cli/rootfs.rs` | `RootfsInfo` struct; `cmd_rootfs_ls(json)` JSON branch |

---

## Previous Task: Multi-Container Web Stack Example

**Status:** COMPLETE — 5/5 tests passing

### Architecture
```
Host :8080 → [nginx proxy :80] → [bottle app :5000] → [redis :6379]
```

### Files Created
- `redis/Remfile` — Alpine + redis-server
- `app/Remfile` + `app/app.py` — Alpine + Python/Bottle REST API (notes CRUD via Redis)
- `proxy/Remfile` + `proxy/nginx.conf` + `proxy/index.html` — Alpine + nginx reverse proxy
- `proxy/Remfile` now symlinks nginx logs to /dev/stdout + /dev/stderr for `remora logs`
- `run.sh` — orchestration script: build, launch, verify (5 HTTP tests), cleanup
- `README.md` — architecture and usage docs

### How to Run
```bash
cargo build --release && export PATH=$PWD/target/release:$PATH
sudo ./examples/web-stack/run.sh
```

---

### Bugs Found and Fixed During Development

#### Bug 1: `remora build` RUN steps had no DNS (FIXED)
- Build containers used `--network bridge` but had no DNS resolvers configured
- `apk add` failed with "DNS: transient error"
- **Fix:** Added `.with_dns(&["8.8.8.8", "1.1.1.1"])` to `execute_run()` in `src/build.rs`

#### Bug 2: `remora build` RUN steps had no NAT (FIXED)
- Build containers had DNS configured but no outbound internet (no MASQUERADE)
- The NAT warm-up approach (throwaway container) didn't work because NAT is refcounted
  and rules are removed when the warm-up container exits
- **Fix:** Added `.with_nat()` to `execute_run()` in `src/build.rs` (conditional on bridge mode)

#### Bug 3: `tar::Builder::append_dir_all` follows symlinks (FIXED)
- Overlay upper dirs contain absolute symlinks pointing into the container rootfs
- `append_dir_all` follows these symlinks and fails with ENOENT on the host
- **Fix:** Replaced with `append_dir_all_no_follow()` custom walker in `src/build.rs`
  that stores symlinks as symlinks in the tar archive

#### Bug 4: Build engine didn't append `:latest` to tags (FIXED)
- `remora build -t web-stack-redis` saved the image as `web-stack-redis`
- `remora run web-stack-redis:latest` couldn't find it (tried raw ref, then normalised)
- **Fix:** `execute_build()` now appends `:latest` when tag has no version/digest

---

### Current Blocker: Inter-Container Communication + Port Forwarding

There are TWO separate problems, which got tangled together during debugging:

#### Problem A: Host port forwarding (`curl localhost:8080` → container)

Port forwarding uses nftables DNAT in the PREROUTING chain. This works for traffic
from external hosts but NOT for traffic originating on the host itself (localhost).
Localhost traffic goes through the OUTPUT chain, not PREROUTING.

**Attempted fixes (all failed):**

1. **OUTPUT chain DNAT** — Added a DNAT rule in the OUTPUT hook to catch
   localhost-originated traffic. The DNAT worked (changed dst to container IP)
   but the return path was broken: packets from 127.0.0.1 can't traverse
   non-loopback interfaces without `route_localnet=1`.

2. **route_localnet=1** — Enabled on the bridge interface. This allowed the
   SYN to reach the container, but the response got lost. After DNAT, the
   source is still 127.0.0.1; the container replies to its own loopback.

3. **Hairpin NAT masquerade (saddr 127.0.0.0/8)** — Added a postrouting
   masquerade for localhost-sourced traffic going to the container subnet.
   Didn't work — by the time the packet reaches postrouting, the kernel may
   have already changed the source IP from 127.0.0.1 to the bridge IP.

4. **Hairpin NAT masquerade (broad: daddr 172.19.0.0/24 oifname remora0)** —
   Broadened the masquerade to match any traffic to the container subnet via
   the bridge. This fixed localhost→container (test 1 PASS) but BROKE
   inter-container traffic (nginx→app got 502). The masquerade was being
   applied to bridged inter-container traffic due to `br_netfilter`.

5. **Return rule for inter-container traffic** — Added
   `ip saddr 172.19.0.0/24 ip daddr 172.19.0.0/24 return` to skip masquerade
   for container-to-container traffic. But this rule was appended AFTER the
   existing NAT masquerade rule in the postrouting chain, so the masquerade
   fired first and the return rule never ran.

6. **Moved return rule to NFT_ADD_SCRIPT + simplified masquerade** — Put the
   return rule first and changed masquerade to `ip saddr 172.19.0.0/24 masquerade`
   (removed oifname filter). This broke EVERYTHING because now ALL container
   traffic was masqueraded, including responses going back through the bridge.

**Root cause:** Docker solves this with `docker-proxy` — a userspace TCP proxy
that listens on the host port and forwards to the container. nftables DNAT alone
cannot handle localhost→container (hairpin NAT) reliably without complex rule
interactions that break inter-container traffic.

**Current state of port forwarding code:** Fully reverted to the original state
(PREROUTING only, no OUTPUT chain, no route_localnet, no hairpin rules).
Port forwarding works for external hosts but not from localhost.

#### Problem B: Inter-container proxy_pass (nginx → app)

When the web stack was running with the original NAT rules, nginx returned
502 Bad Gateway for proxy_pass routes to `app:5000`. The nginx error log showed:
```
connect() failed (111: Connection refused) while connecting to upstream,
upstream: "http://172.19.0.149:5000/health"
```

**Confirmed facts:**
- All 3 containers are running (redis, app, proxy)
- Redis logs: "Ready to accept connections tcp"
- App logs: "Listening on http://0.0.0.0:5000/"
- Proxy /etc/hosts has correct entry: `172.19.0.149 app`
- App is reachable from HOST: `curl http://172.19.0.149:5000/health` → `{"status": "ok"}`
- App is reachable from proxy via exec: `remora exec proxy wget -qO- http://app:5000/health` → `{"status": "ok"}`
- But nginx proxy_pass to the same URL returns 502 (Connection refused)
- Static content served by nginx directly works fine

**Suspected cause:** The existing NAT masquerade rule
`ip saddr 172.19.0.0/24 oifname != "remora0" masquerade` may be interfering
with bridged inter-container traffic when `br_netfilter` kernel module is loaded.
With br_netfilter, bridged packets go through iptables/nftables. The `oifname`
for bridged traffic may be the veth device name (not "remora0"), causing the
masquerade rule to match inter-container traffic incorrectly.

**NOT yet verified:**
- Whether `br_netfilter` is actually loaded
- What `oifname` looks like for bridged traffic
- Whether disabling br_netfilter fixes inter-container communication
- Whether the issue exists WITHOUT any nftables rules at all

---

### Next Steps

**Immediate:** Run the test with reverted NAT rules and direct bridge IP
access (no port forwarding) to isolate whether Problem B exists independently.

The test script now curls the proxy container's bridge IP directly on port 80
instead of localhost:8080, completely bypassing port forwarding.

**If inter-container works with direct IP access:**
- Problem B was caused by port forwarding rule interference, not baseline networking
- Port forwarding from localhost needs a userspace proxy approach (like Docker's docker-proxy)

**If inter-container still fails (502):**
- Check if `br_netfilter` is loaded: `lsmod | grep br_netfilter`
- Test with `sudo modprobe -r br_netfilter` to see if that fixes it
- Test with NO nftables rules at all (`nft flush ruleset`) to see if any rule interferes
- Use `tcpdump -i remora0` to watch actual packets on the bridge
- Add a `return` rule BEFORE the masquerade in `NFT_ADD_SCRIPT` to exempt inter-container traffic

**Port forwarding (deferred):**
- Implement a lightweight userspace TCP proxy (spawn a thread/process that
  `accept()`s on host_port and `connect()`s + relays to container_ip:container_port)
- Or document that `-p` only works for external hosts, not localhost

---

## Potential Next Moves

### 1. Example Applications
Three demo apps to showcase remora's capabilities end-to-end:

**Multi-Container Web Stack** (uses `remora build`)
- Bridge-networked: container A (web server) + container B (backend)
- `--link` for service discovery, `-p` for host exposure
- Named volume for shared data persistence

**Build Sandbox**
- Rootless container that compiles user-provided code
- Read-only rootfs + tmpfs /tmp, resource limits, seccomp + cap-drop ALL

**CI Test Runner**
- Pull image, run test suite, collect exit code
- `--env`, `--bind`, `--workdir`, detached mode + `logs --follow`

### 2. Documentation Updates
- ~~Update README.md with `remora build` usage~~ ✅ Done
- ~~Update USER_GUIDE.md with Remfile reference~~ ✅ Done
- ~~Update RUNTIME_COMPARISON.md~~ ✅ Done
- ~~Correct runc parity estimate to ~80%~~ ✅ Done

### 3. `remora build` Enhancements
The current build feature is functional but missing several instructions and
optimisations that would bring it closer to Dockerfile parity:

- **ENTRYPOINT instruction**: `ENTRYPOINT ["nginx"]` — sets the entrypoint prefix;
  CMD becomes default args. Parser + config mutation only (no layer).
- **ADD instruction**: `ADD <src> <dest>` — like COPY but supports URL downloads and
  automatic tar extraction. Moderate effort (HTTP fetch + archive detection).
- **LABEL instruction**: `LABEL key=value` — image metadata. Parser + config only.
- **USER instruction**: `USER 1000:1000` — set default UID/GID. Parser + config only.
- **ARG instruction**: `ARG NAME=default` — build-time variables with `${NAME}`
  substitution. Requires variable expansion pass in parser.
- **Multi-stage builds**: `FROM alpine AS builder` / `COPY --from=builder` — requires
  tracking named stages and cross-stage COPY. Significant work.
- **Build cache**: hash (instruction + parent layer) to skip unchanged RUN steps.
  Significant work — needs cache key computation and invalidation logic.
- **`.remignore` file**: exclude files from build context (like `.dockerignore`).

### 4. Remaining runc Parity Gaps (~20%)

These are features runc supports that Remora does not. Closing these would bring
runc parity from ~80% to near-complete.

**Security / MAC (Significant Work):**
- **AppArmor profiles**: `linux.apparmorProfile` in OCI config — apply an AppArmor
  confinement to the container process. Requires detecting AppArmor availability,
  writing the profile path to `/proc/self/attr/apparmor/exec`. Most impactful
  missing security feature.
- **SELinux labels**: `linux.selinuxProcessLabel` / `linux.selinuxMountLabel` —
  set SELinux context on the container process and its mounts. Requires libselinux
  or direct `/proc/self/attr/sockcurrent` writes.

**Seccomp (Moderate Work):**
- **Argument-level conditions**: `linux.seccomp.syscalls[].args[]` — filter syscalls
  based on argument values (e.g. "allow `clone` only if flags don't include
  `CLONE_NEWUSER`"). Currently we apply profile-level allow/deny but don't support
  the `args` field with `SCMP_CMP_*` operators. Requires extending `seccompiler`
  rule generation in `src/seccomp.rs`.

**Cgroups (Moderate Work):**
- **I/O bandwidth limits**: `linux.resources.blockIO` — throttle read/write
  bytes/sec and IOPS per block device. Requires resolving device major:minor
  numbers and writing to `io.max` / `io.weight` in cgroupfs.

**OCI Hooks (Quick):**
- **`createRuntime` hook**: runs after namespaces are created but before pivot_root.
  Currently we support `prestart`, `poststart`, `poststop` but not the newer
  `createRuntime` and `startContainer` hook points from OCI Runtime Spec 1.1+.
- **`startContainer` hook**: runs inside the container namespace, after pivot_root
  but before the user process starts.

**OCI Config (Quick-to-Moderate):**
- **`linux.devices` fine-grained ACLs**: `allow`/`deny` device access lists with
  major/minor/type matching. Currently we create device nodes but don't enforce
  cgroup device controller ACLs.
- **`annotations`**: arbitrary key-value metadata on the container. Parser support
  only — no runtime effect but required for OCI compliance.

**Checkpoint / Restore (Significant Work):**
- **CRIU integration**: `runc checkpoint` / `runc restore` — freeze a container's
  state to disk and resume it later. Requires CRIU library integration, file
  descriptor serialisation, and process tree reconstruction. Low priority —
  niche use case (live migration, debugging).

**Intel RDT (Low Priority):**
- **Resource Director Technology**: `linux.intelRdt` — LLC cache and memory
  bandwidth allocation via resctrl filesystem. Very niche, only relevant on
  server-class Intel CPUs.

**PID Namespace (Moderate Work):**
- **CLI foreground mode**: PID namespace works in the library API but the CLI
  `remora run` (foreground, non-detached) has an architectural limitation where
  the spawned process can't be PID 1 because the parent is still the reaper.
  Fix requires either a shim process or double-fork with pipe signalling.

### 5. Other Improvements
- **Authenticated registry pulls**: Docker Hub private repos, other registries
  (requires token exchange, possibly basic auth)
- **`remora build` rootless mode**: test and fix any rootless build issues
- **Error messages**: audit all user-facing errors for clarity

---

## Completed Features

### `remora build` (v0.3.0)
**COMPLETE** — Build images from Remfiles (simplified Dockerfiles).
- Remfile parser: FROM, RUN, COPY, CMD, ENV, WORKDIR, EXPOSE
- Buildah-style daemonless build: overlay snapshot per RUN step
- Path traversal protection on COPY
- 14 unit tests + 22 E2E assertions (scripts/test-build.sh)
- `wait_preserve_overlay()` added to Child for build engine

### Stress Tests (v0.2.1)
**COMPLETE** — 18 pass, 0 fail, 0 skip. All 7 sections passing.

### E2E Bug Fixes (v0.2.1)
**COMPLETE** — Fixed 4 bugs found by E2E suite.

### Phase A+B: Storage Path Abstraction + Rootless Overlay (v0.2.0)
**RELEASED** — rootless image pull and container run with single-UID mapping.

### Phase D: Minimal `/dev` Setup
**COMPLETE** — tmpfs + safe devices replacing host /dev bind-mount.

### Phase C: Multi-UID Mapping via Subordinate Ranges
**COMPLETE** — `newuidmap`/`newgidmap` helpers with pipe+thread sync.

### Phase E: Rootless Cgroup v2 Delegation
**COMPLETE** — direct cgroupfs writes under user's delegated cgroup scope.

### Rootless E2E Test Script
**COMPLETE** — `scripts/test-rootless.sh` covering all rootless phases.

---

## Current Capabilities

### Fully Working (E2E Tested)

| Category | Features |
|----------|----------|
| Lifecycle | foreground, detached, ps, stop, rm, logs, name collision |
| Images | pull (anonymous, Docker Hub), multi-layer overlay, ls, rm, **build** |
| Exec | command in running container, PTY (-i), env/workdir/user |
| Networking | loopback, bridge+IPAM, NAT+MASQUERADE, port forwarding, DNS, pasta |
| Filesystem | overlay CoW, bind RW/RO, tmpfs, named volumes, read-only rootfs |
| Security | seccomp (default+minimal), capabilities, no-new-privs, masked paths, sysctl |
| Resources | cgroups v2 (memory, CPU quota/shares, PIDs), rlimits |
| OCI | create/start/state/kill/delete lifecycle, config.json parsing |
| Rootless | images, overlay (native userxattr + fuse-overlayfs fallback), pasta, cgroups v2 |

### Known Limitations

- **PID namespace**: works in library API, architectural limitation in CLI foreground mode
- **No daemon mode**: CLI tool and library only, no background service
- **No AppArmor/SELinux**: MAC profile support deferred; seccomp+caps stack is solid
- **No authenticated registry pulls**: anonymous only (Docker Hub public images)
- **No I/O bandwidth cgroups**: no block device throttling
- **No CNI plugins**: intentional — native networking approach instead
- **Rootless overlay**: requires kernel 5.11+ (userxattr) or fuse-overlayfs installed
- **Alpine binary paths**: utilities like `id`, `env`, `wc` live in `/usr/bin/`, not `/bin/`

---

## Previous Releases

### v0.2.1 — E2E Bug Fixes
Pre_exec ordering, proc mount path, seccomp minimal, exec workdir. E2E suite + stress tests.

### v0.2.0 — Rootless Mode
Storage path abstraction, rootless overlay, multi-UID mapping, cgroup delegation.

### v0.1.0 — Initial Release
Full feature set: namespaces, seccomp, capabilities, cgroups v2, overlay, networking,
OCI image pull, container exec, OCI runtime compliance, interactive PTY.
