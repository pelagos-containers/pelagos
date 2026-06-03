# Registry Mirrors

Pelagos supports pull-through cache / registry mirror configuration so that image
pulls can be routed to a local cache before hitting the origin registry.  This is
useful for avoiding Docker Hub anonymous rate limits (100 pulls / 6 h per IP) in
CI or multi-node dev clusters.

## Configuration file

Create `/etc/pelagos/registries.toml`:

```toml
[mirrors]
"docker.io"      = ["http://nazgul:5000"]
"registry.k8s.io" = ["http://nazgul:5001"]
"ghcr.io"        = ["http://nazgul:5002", "https://fallback.example.com"]
```

Each key is an origin registry hostname.  Values are an ordered list of mirror
endpoints to try before falling back to the origin.

**HTTP mirrors** (plain `http://`) are automatically treated as insecure — no
extra flag is needed.

**Multiple mirrors** are tried left-to-right.  If a mirror returns an error or is
unreachable, the next entry is tried.  After exhausting all mirrors, the pull
falls back to the origin registry.

## Overriding the config path

Set `PELAGOS_REGISTRIES` to an absolute path to use a different file:

```sh
PELAGOS_REGISTRIES=/home/user/.config/pelagos/registries.toml pelagos image pull alpine
```

## Setting up a pull-through cache

### Using `registry:2` (Docker Distribution)

```sh
docker run -d \
  --name registry-mirror \
  -p 5000:5000 \
  -e REGISTRY_PROXY_REMOTEURL=https://registry-1.docker.io \
  registry:2
```

Then in `/etc/pelagos/registries.toml`:

```toml
[mirrors]
"docker.io" = ["http://localhost:5000"]
```

### Using Zot

```yaml
# /etc/zot/config.yaml
http:
  address: "0.0.0.0"
  port: 5000
storage:
  rootDirectory: /var/lib/zot
extensions:
  sync:
    registries:
      - urls: ["https://registry-1.docker.io"]
        onDemand: true
        tlsVerify: true
```

## How it works

When `pelagos image pull <ref>` is invoked:

1. The origin registry is extracted from the normalised reference
   (e.g. `docker.io` for `alpine:latest`).
2. Configured mirrors for that registry are looked up in `registries.toml`.
3. Each mirror is tried in order: the reference is rewritten to point at the
   mirror host, and a pull is attempted.
4. On the first success the cached image is stored under its **original**
   reference — so `pelagos image ls` and subsequent runs see `docker.io/…`,
   not the mirror hostname.
5. If all mirrors fail, the origin registry is tried normally.

The `pelagos-cri` daemon inherits this behaviour because it delegates pulls to
`pelagos image pull` — no separate CRI-level mirror configuration is needed.
