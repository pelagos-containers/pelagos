# Compose Web Stack Example

The same 3-container blog stack as `examples/web-stack/`, but orchestrated
with `remora compose` instead of imperative shell scripting.

## The Compose File

Everything that matters is in **`compose.rem`** — 50 lines of declarative
S-expressions replacing 224 lines of bash:

```
frontend (10.88.1.0/24):  proxy ←→ app
backend  (10.88.2.0/24):           app ←→ redis
```

| Service | Networks | Depends On | Ports | Role |
|---------|----------|------------|-------|------|
| **redis** | backend | — | — | Redis data store |
| **app** | frontend + backend | redis:6379 | — | Python/Bottle REST API |
| **proxy** | frontend | app:5000 | 8080:80 | nginx reverse proxy |

The proxy and redis share no network — network isolation is enforced by
the compose topology, not by firewall rules.

## What Compose Handles Automatically

Things the old `run.sh` did manually that `compose.rem` declares:

- **Network creation** — `(network frontend (subnet ...))` creates scoped
  networks (`blog-frontend`, `blog-backend`)
- **Volume creation** — `(volume notes-data)` creates `blog-notes-data`
- **Dependency ordering** — redis starts first, app waits for redis:6379,
  proxy waits for app:5000
- **TCP readiness** — `:ready-port` polls until the port accepts connections
- **DNS registration** — services find each other by name (`redis`, `app`)
- **Multi-network attachment** — app bridges both networks automatically
- **Resource limits** — memory and CPU per service
- **Teardown** — `compose down -v` stops everything in reverse order and
  removes networks, volumes, and state

## Running

```bash
# Build remora first
cargo build --release
export PATH=$PWD/target/release:$PATH

# Run the demo (requires root)
sudo ./examples/compose-web-stack/run.sh
```

The script:
1. Pulls alpine and builds the 3 images (reuses `web-stack/` Remfiles)
2. Runs `remora compose up` in foreground
3. Waits for the stack to accept connections on port 8080
4. Runs 5 verification tests (static page, health, CRUD, persistence)
5. Tears down with `remora compose down -v`

## Comparison

| | `web-stack/run.sh` | `compose-web-stack/compose.rem` |
|---|---|---|
| Network setup | 6 imperative commands | 2 declarations |
| Volume setup | 1 command | 1 declaration |
| Container start | 3 commands + sleep + liveness checks | 3 service blocks |
| Dependency order | Implicit (script order + sleep) | Explicit (depends-on + readiness) |
| Service discovery | `--link name:alias` flags | Automatic DNS |
| Teardown | 12 cleanup commands | `compose down -v` |
| Lines of orchestration | ~120 (bash) | ~45 (S-expressions) |
