# Embedded DNS: How Docker Does It and Future Options for Pelagos

**Status:** Research notes — not currently planned for implementation.
Pelagos uses `/etc/hosts` injection for container-to-container name resolution.
This document captures how Docker's embedded DNS works, in case we want to
revisit a dynamic DNS approach later.

---

## Docker's Embedded DNS (127.0.0.11)

Docker runs a real DNS resolver/forwarder inside every container on user-defined
networks. It's not just `/etc/hosts` — it's a full UDP/TCP DNS listener.

### Architecture

1. **The daemon (`dockerd`) listens on a random ephemeral port** (not port 53)
   inside each container's network namespace. It avoids binding to port 53 to
   prevent conflicts with user services.

2. **iptables DNAT inside the container** redirects port 53 to the ephemeral port:
   - `DOCKER_OUTPUT` chain: DNAT rules rewrite `127.0.0.11:53` →
     `127.0.0.11:<random-port>`
   - `DOCKER_POSTROUTING` chain: SNAT rules rewrite responses back to appear
     from port 53
   - This is invisible to applications — they see a normal DNS server at
     `127.0.0.11:53`

3. **Container's `/etc/resolv.conf`** contains only `nameserver 127.0.0.11`.
   External DNS servers (from host config or `--dns` flags) are stored
   internally by the daemon, never exposed to the container.

4. **Resolution logic:**
   - Query for a container name → daemon looks up its internal registry,
     returns the container's bridge IP
   - Query for anything else → forwarded to upstream DNS servers (host's
     resolv.conf or `--dns` overrides)

5. **One DNS goroutine per container endpoint** — spawned when the container
   joins a user-defined network.

### What This Enables (Beyond /etc/hosts)

- **Dynamic updates**: If container A restarts with a new IP, Docker's DNS
  returns the new IP immediately. `/etc/hosts` is static — written at spawn
  time and never updated.
- **Service discovery**: DNS round-robin for multiple containers with the same
  service name (used by Docker Compose and Swarm).
- **Upstream forwarding**: Docker's DNS is a full forwarder. With `/etc/hosts`,
  you still need a separate `nameserver` entry for external resolution.
- **SRV records**: Potential for service port discovery (not widely used).

### Why It's Complex

- Requires a long-running daemon with a DNS protocol implementation (UDP + TCP)
- Needs iptables/nftables manipulation inside each container's netns
- Needs to track container name-to-IP mappings dynamically (handles restarts,
  IP changes, container removal)
- Must correctly forward unknown queries upstream
- One listener per container endpoint adds resource overhead

---

## Pelagos's Current Approach: /etc/hosts Injection

Pelagos injects `/etc/hosts` entries via bind-mount at container start time.
This is the same mechanism Docker used for the legacy `--link` feature before
user-defined networks existed.

### Advantages

- Zero infrastructure — no daemon, no DNS protocol, no iptables rules
- Works everywhere — glibc, musl, and busybox all respect `/etc/hosts`
- No runtime overhead — no background process, no port binding
- Sufficient for the common case: "web container needs to reach db container"

### Limitations

- **Static**: Links resolved at spawn time. If a target container restarts
  with a new IP, the `/etc/hosts` entry becomes stale.
- **No upstream forwarding**: External DNS still requires a separate
  `nameserver` entry (handled by Pelagos's existing `with_dns()`)
- **No round-robin**: Can't load-balance across multiple containers with
  the same service name
- **No dynamic discovery**: New containers aren't visible to already-running
  containers

---

## If We Ever Want Embedded DNS in Pelagos

### Minimal viable approach

A lightweight UDP forwarder bound to `127.0.0.11` inside each container's
network namespace. Does not need to be a full DNS server — just enough to:

1. Parse incoming DNS queries (extract the QNAME)
2. Check an in-memory map of container name → IP
3. If found: synthesize an A record response
4. If not found: forward the raw packet to an upstream resolver and relay
   the response back

### Implementation sketch

**Listener setup (parent, after fork):**
1. Open a UDP socket bound to `127.0.0.11:53` inside the container's netns
   (via `setns(CLONE_NEWNET)` or by passing the fd through the fork)
2. Spawn a background thread that polls the socket

**Resolution:**
- Container names: read from a shared registry file
  (`/run/pelagos/dns-registry.json`) protected by flock
- External queries: forward to upstream nameservers from `with_dns()` config

**Container's resolv.conf:**
```
nameserver 127.0.0.11
```

**Cleanup:**
- Kill the listener thread/process in `wait()`

### Rust crates that could help

- [`trust-dns-server`](https://crates.io/crates/trust-dns-server) — full DNS
  server library (likely overkill)
- [`simple-dns`](https://crates.io/crates/simple-dns) — lightweight DNS
  packet parsing
- Raw UDP socket with manual DNS packet construction (minimal, ~200 lines
  for A record queries)

### Effort estimate

Moderate-to-significant. The DNS protocol parsing is straightforward for basic
A queries, but handling edge cases (TCP fallback, EDNS, truncation, timeouts,
multiple upstream servers) adds up. The iptables/nftables plumbing for port
redirection adds another layer.

Worth revisiting if Pelagos ever needs orchestration-level service discovery
or dynamic container-to-container resolution.

---

## Sources

- [Docker Embedded DNS Server - Original libnetwork PR #841](https://github.com/moby/libnetwork/pull/841)
- [Docker Embedded DNS Documentation (v17.09)](https://docs.docker.com/v17.09/engine/userguide/networking/configure-dns/)
- [How Docker Embedded DNS Resolver Works - Docker Forums](https://forums.docker.com/t/how-does-docker-embedded-dns-resolver-work/27282)
- [Fun DNS Facts from the KIND Environment - HungWei Chiu](https://hwchiu.medium.com/fun-dns-facts-learned-from-the-kind-environment-241e0ea8c6d4)
- [Understanding Docker DNS - Prajwal Chin](https://medium.com/@prajwal.chin/understanding-docker-dns-2ed4b070a0)
- [DNS and Docker - Eric Abell](https://medium.com/@eric_abell/dns-and-docker-d839479109ac)
