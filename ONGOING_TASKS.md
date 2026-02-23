# Ongoing Tasks

## Current State (Feb 23, 2026)

### Stack: 7 services, committed, ready to run

```
snmp-exporter      :9116   MikroTik SNMP
plex-exporter      :9594   Plex REST API
mktxp              :49090  MikroTik RouterOS API (bandwidth, queues, DHCP, etc.)
graphite-exporter  :9108   TrueNAS graphite push receiver (listens :2003)
alertmanager       :9093   Alert routing (null receiver; Pushover-ready)
prometheus         :9090   scrapes all of the above + alerts to alertmanager
grafana            :3000   dashboards
```

### Git state (clean, both repos pushed)

**remora repo** (`~/Projects/remora`):
- branch: master, up to date with origin
- Last commit: `93e0bdb` Update ONGOING_TASKS.md

**home-monitoring repo** (`~/Projects/home-monitoring`):
- branch: main, up to date with origin
- Last commit: `3ca8efd` Add mktxp and graphite-exporter to monitoring stack

### Files changed this session (home-monitoring repo)

| File | What |
|------|------|
| `remora/compose.rem` | Added mktxp + graphite-exporter services; prometheus depends-on both |
| `remora/config/prometheus/prometheus.yml` | Added mktxp and truenas_graphite scrape jobs |
| `remora/config/mktxp/mktxp.conf` | MikroTik RouterOS API credentials (new) |
| `remora/config/graphite/graphite_mapping.yaml` | TrueNAS metric name mapping (new) |

### Files changed this session (remora repo)

| File | What |
|------|------|
| `scripts/start-monitoring.sh` | Added mktxp + graphite-exporter to image pull list |
| `docs/HOME_MONITORING_STACK.md` | Updated with all 6 services, TrueNAS config note |

---

## To start the stack

```bash
sudo -E ~/Projects/remora/scripts/start-monitoring.sh
```

The script builds remora from source (no-op if unchanged), pulls all 6 images,
and brings up all services. Prometheus waits for all 4 exporters to be ready
before starting.

**mktxp caveat**: if it exits immediately with a write error, it's trying to
write state to `/config/` which is read-only. Fix: add tmpfs to the service
and use a small shell wrapper to copy the bind-mounted conf into it first.
(Not observed yet — might just work.)

**TrueNAS graphite**: configure TrueNAS SCALE at System → Advanced → Reporting →
Graphite, set hostname to this machine's LAN IP (not localhost), port 2003.

---

## ✅ tmpfs support in compose — DONE

### Why
mktxp writes state files to `/config/` alongside its config. We can't use a
read-only bind-mount for the conf dir. Rather than accepting a messy RW
bind-mount, add proper `(tmpfs "/path")` support to compose.

### Plan

**`src/compose.rs`**
1. Add `tmpfs_mounts: Vec<String>` to `ServiceSpec` (after `bind_mounts`)
2. Initialize `tmpfs_mounts: Vec::new()` in `parse_service_spec` struct literal
3. Add `"tmpfs"` match arm: `require_atom(list, 1, ...)` → `spec.tmpfs_mounts.push(path)`
4. Add 3 unit tests: single path, multiple paths, missing path → MissingField error

**`src/cli/compose.rs`** — `spawn_service`
5. After the bind-mount loop, add: `for path in &svc.tmpfs_mounts { cmd = cmd.with_tmpfs(path, ""); }`

**`tests/integration_tests.rs`**
6. Add `test_compose_tmpfs_parse_and_validate` (no root/rootfs needed)
   - service with 1 tmpfs, service with 2 tmpfs, topo sort still correct

**`docs/INTEGRATION_TESTS.md`**
7. Add entry for the new test

**`home-monitoring/remora/compose.rem`** — after remora is built
8. Update mktxp service: bind-mount conf dir back to `:ro`, add `(tmpfs "/config")`,
   use shell wrapper to copy conf into tmpfs before starting

### mktxp final service spec
```lisp
(service mktxp
  (image "ghcr.io/akpw/mktxp:latest")
  (network monitoring)
  (port 49090 49090)
  (bind-mount "./config/mktxp/mktxp.conf" "/conf/mktxp.conf" :ro)
  (tmpfs "/config")
  (command "sh" "-c" "cp /conf/mktxp.conf /config/mktxp.conf && mktxp --cfg-dir /config export"))
```

---

## ✅ Task 1: alertmanager — DONE

Files changed (home-monitoring repo):
- `remora/config/alertmanager/alertmanager.yml` — null receiver, Pushover-ready comments
- `remora/config/prometheus/prometheus.yml` — added `alerting:` block
- `remora/compose.rem` — added alertmanager service; prometheus depends-on alertmanager
Files changed (remora repo):
- `scripts/start-monitoring.sh` — added `prom/alertmanager:latest` to image pull list

To enable Pushover when credentials are available: edit `config/alertmanager/alertmanager.yml`,
replace null receiver, update `route.receiver: 'pushover'`.
Get credentials at https://pushover.net.

---

## Next Task 2: truenas-api-exporter (Custom Python Remfile build)

### Source files

All in `~/Projects/home-monitoring/monitoring-setup/truenas-graphite-exporter/`:
- `truenas_api_exporter.py` — the exporter (polls TrueNAS REST API)
- `Dockerfile.api-exporter` — reference Dockerfile to base the Remfile on

### What the exporter does

- Polls TrueNAS SCALE REST API at `http://192.168.88.30` (or HTTPS :443)
- Queries SMART data, ZFS pool health, disk temperatures
- Exposes Prometheus metrics (check script for actual port — likely :9100)
- Needs env vars: `TRUENAS_HOST`, `TRUENAS_API_KEY`, `VERIFY_SSL=false`

### TrueNAS API key

Was stored as a K8s secret. To get/create one:
1. Log into TrueNAS SCALE web UI at http://192.168.88.30
2. Go to Credentials → API Keys
3. Create a new key (or retrieve existing)

### Remfile to write

Location: `~/Projects/home-monitoring/monitoring-setup/truenas-graphite-exporter/Remfile`

```dockerfile
FROM python:3.11-slim
WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY truenas_api_exporter.py .
CMD ["python", "truenas_api_exporter.py"]
```

If no `requirements.txt` exists, check imports in `truenas_api_exporter.py`
and create one. Expected deps: `requests`, `prometheus_client`.

### Build command

```bash
sudo -E remora build \
  --network bridge \
  -t truenas-api-exporter:latest \
  --file ~/Projects/home-monitoring/monitoring-setup/truenas-graphite-exporter/Remfile \
  ~/Projects/home-monitoring/monitoring-setup/truenas-graphite-exporter/
```

`--network bridge` is needed for `pip install` to reach PyPI.

### compose.rem service (after build succeeds)

```lisp
(service truenas-api-exporter
  (image "truenas-api-exporter:latest")
  (network monitoring)
  (port 9100 9100)
  (env TRUENAS_HOST "http://192.168.88.30")
  (env TRUENAS_API_KEY "YOUR_API_KEY_HERE")
  (env VERIFY_SSL "false"))
```

Add prometheus scrape job:
```yaml
- job_name: truenas_api
  static_configs:
    - targets: ['truenas-api-exporter:9100']
  scrape_interval: 60s
```

---

## Known Limitations / Watch List

- **mktxp writable config dir** — mktxp might try to write state alongside
  the config. If it exits with a write error, add a tmpfs at `/config` and
  use a startup script (`sh -c 'cp /conf/mktxp.conf /config/ && mktxp ...'`)
  to copy the bind-mounted conf into it first.

- **compose `(command ...)` replaces entire entrypoint+cmd** — if you want to
  pass extra args to the image's existing entrypoint, you must repeat the
  entrypoint in the `(command ...)` list. See prometheus, graphite-exporter,
  alertmanager for the pattern (`/bin/X` first, then flags).

- **Plex token** — script reads from `$PLEX_TOKEN` env var or
  `monitoring-setup/.env`. Placeholder `YOUR_PLEX_TOKEN_HERE` is substituted
  at runtime.

- **Symbolic user resolution** — resolved against the container's own layer
  stack (`/etc/passwd` inside the image) first, then falls back to the host.
  This means image-internal users (e.g. `mktxp`, `nobody` from Alpine) work
  correctly even when they don't exist on the host.

- **TrueNAS graphite push** — port 2003 is host-mapped. TrueNAS must push
  to this machine's LAN IP (e.g. 192.168.88.X), not localhost.

- **Pushover credentials** — alertmanager starts with null receiver; add
  Pushover user_key + API token from https://pushover.net when available.

- **No alert rules yet** — prometheus.yml has no `rule_files:` entry.
  Alert rules from the Helm chart need translation from PrometheusRule CRD
  format to plain YAML before they'll fire.
