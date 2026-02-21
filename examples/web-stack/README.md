# Web Stack Example

A 3-container blog application demonstrating Remora's multi-container capabilities.

## Architecture

```
Host :8080 → [nginx proxy :80] → [bottle app :5000] → [redis :6379]
```

| Container | Image | Role |
|-----------|-------|------|
| **proxy** | `web-stack-proxy` | nginx reverse proxy, serves static HTML, forwards `/api/*` |
| **app** | `web-stack-app` | Python/Bottle REST API for notes CRUD |
| **redis** | `web-stack-redis` | Redis data store |

All containers run on the same bridge network. Only the proxy exposes a port to the host.

## Features Demonstrated

- **Image build** — `remora build` with Remfiles (FROM, RUN, COPY, CMD, ENV, WORKDIR)
- **Bridge networking** — containers communicate over `remora0` bridge
- **Container linking** — `--link name:alias` injects `/etc/hosts` entries for service discovery
- **NAT** — outbound internet for `apk add` during builds
- **Bridge IP access** — tests reach nginx via the proxy container's bridge IP
- **Named volumes** — `notes-data` volume created (for demonstration)

## Running

```bash
# Build remora first
cargo build --release
export PATH=$PWD/target/release:$PATH

# Run the demo (requires root)
sudo ./examples/web-stack/run.sh
```

The script will:
1. Pull `alpine:latest` if needed
2. Build all 3 images
3. Launch the stack
4. Run 5 verification tests
5. Clean up everything on exit

## API Endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | `/` | Static blog page |
| GET | `/health` | Health check |
| GET | `/api/notes` | List all notes |
| POST | `/api/notes` | Add a note (`{"text": "..."}`) |
| GET | `/api/notes/count` | Note count |
