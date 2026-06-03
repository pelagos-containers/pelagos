# Zot Registry Mirror Setup

Pull-through cache for Docker Hub, registry.k8s.io, ghcr.io, and ECR public,
hosted on nazgul (192.168.89.2) and consumed by all k3s nodes via
`/etc/pelagos/registries.toml`.

## Architecture

```
k3s node (192.168.88.x)
  └── pelagos image pull alpine
        └── checks /etc/pelagos/registries.toml
              └── tries http://192.168.89.2:5000  (Zot on nazgul)
                    └── on miss: pulls from registry-1.docker.io, caches locally
                    └── on hit: serves from /mnt/primary_storage/zot/data
              └── on Zot failure: falls back to origin registry
```

## nazgul setup

### Paths

| Purpose | Path on nazgul |
|---|---|
| Zot image cache | `/mnt/primary_storage/zot/data` |
| Zot config | `/mnt/primary_storage/zot/config/config.json` |
| Compose file | `/mnt/primary_storage/zot/docker-compose.yml` |

### Zot config (`config.json`)

```json
{
  "distSpecVersion": "1.1.0",
  "storage": {
    "rootDirectory": "/var/lib/registry",
    "gc": true,
    "gcDelay": "1h",
    "gcInterval": "24h"
  },
  "http": {
    "address": "0.0.0.0",
    "port": "5000"
  },
  "log": {
    "level": "info"
  },
  "extensions": {
    "sync": {
      "registries": [
        {
          "urls": ["https://registry-1.docker.io"],
          "onDemand": true,
          "tlsVerify": true
        },
        {
          "urls": ["https://registry.k8s.io"],
          "onDemand": true,
          "tlsVerify": true
        },
        {
          "urls": ["https://ghcr.io"],
          "onDemand": true,
          "tlsVerify": true
        },
        {
          "urls": ["https://public.ecr.aws"],
          "onDemand": true,
          "tlsVerify": true
        }
      ]
    }
  }
}
```

To add Docker Hub credentials (raises rate limit from 100 to 5000 pulls/6h),
replace the docker.io entry with:

```json
        {
          "urls": ["https://registry-1.docker.io"],
          "credentials": {
            "username": "your-dockerhub-username",
            "password": "your-dockerhub-access-token"
          },
          "onDemand": true,
          "tlsVerify": true,
          "onDemandRetries": 3
        },
```

### Compose file (`docker-compose.yml`)

```yaml
name: zot-registry
services:
  zot:
    image: ghcr.io/project-zot/zot-linux-amd64:latest
    ports:
      - "5000:5000"
    volumes:
      - /mnt/primary_storage/zot/data:/var/lib/registry
      - /mnt/primary_storage/zot/config:/etc/zot
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "wget", "-qO-", "http://localhost:5000/v2/"]
      interval: 30s
      timeout: 5s
      retries: 3
```

### Deploy

```bash
ssh root@192.168.89.2
mkdir -p /mnt/primary_storage/zot/{data,config}
# write config.json and docker-compose.yml as above
cd /mnt/primary_storage/zot
docker compose up -d
```

## k3s node setup

Create `/etc/pelagos/registries.toml` on each node:

```toml
[mirrors]
"docker.io"       = ["http://192.168.89.2:5000"]
"registry.k8s.io" = ["http://192.168.89.2:5000"]
"ghcr.io"         = ["http://192.168.89.2:5000"]
"public.ecr.aws"  = ["http://192.168.89.2:5000"]
```

## Verification

```bash
# Zot API liveness (from any k3s node)
curl http://192.168.89.2:5000/v2/

# Test pull through mirror
RUST_LOG=info pelagos image pull alpine

# Inspect Zot cache
curl http://192.168.89.2:5000/v2/_catalog
```

## Maintenance

Zot runs online GC automatically every 24h (configurable via `gcInterval`).
No manual intervention needed. Logs via:

```bash
ssh root@192.168.89.2 docker logs zot-registry-zot-1 --tail 50
```
