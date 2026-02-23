# Ongoing Tasks

## Current State (Feb 23, 2026)

### Stack: 6 services, committed, ready to run

```
snmp-exporter      :9116   MikroTik SNMP
plex-exporter      :9594   Plex REST API
mktxp              :49090  MikroTik RouterOS API (bandwidth, queues, DHCP, etc.)
graphite-exporter  :9108   TrueNAS graphite push receiver (listens :2003)
prometheus         :9090   scrapes all of the above
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

## Next Task 1: alertmanager

All details are known. Straightforward — just create config and add service.

### Step 1 — create `config/alertmanager/alertmanager.yml`

Start with null receiver (no Pushover credentials on hand):

```yaml
global:
  resolve_timeout: 5m
route:
  group_by: ['alertname']
  group_wait: 10s
  group_interval: 10s
  repeat_interval: 12h
  receiver: 'null'
receivers:
- name: 'null'
```

To add Pushover later: grab `user_key` and `token` from https://pushover.net,
then replace the receiver with:
```yaml
- name: 'pushover'
  pushover_configs:
  - user_key: 'YOUR_USER_KEY'
    token: 'YOUR_API_TOKEN'
    priority: '{{ if eq .Status "firing" }}1{{ else }}0{{ end }}'
```

### Step 2 — add to `config/prometheus/prometheus.yml`

Add `alerting:` block above `scrape_configs:`:
```yaml
alerting:
  alertmanagers:
    - static_configs:
        - targets: ['alertmanager:9093']
```

### Step 3 — add service to `compose.rem`

Insert before prometheus (alertmanager has no deps; prometheus depends-on it):
```lisp
(service alertmanager
  (image "prom/alertmanager:latest")
  (network monitoring)
  (port 9093 9093)
  (bind-mount "./config/alertmanager/alertmanager.yml" "/etc/alertmanager/alertmanager.yml" :ro)
  (command
    "/bin/alertmanager"
    "--config.file=/etc/alertmanager/alertmanager.yml"
    "--storage.path=/alertmanager"))
```

Add to prometheus `depends-on`: `(alertmanager :ready-port 9093)`

### Step 4 — add to `start-monitoring.sh` image pull list

```bash
"prom/alertmanager:latest"
```

### Step 5 — add alert rules (optional, after alertmanager is running)

Existing alert rules in the Helm chart:
- `monitoring-setup/prometheus/disk-temp-alerts.yaml` — drive temperature
- `monitoring-setup/prometheus/truenas-alerts.yaml` — pool health
These are Kubernetes PrometheusRule CRDs; translate to standalone prometheus
`rule_files:` format. Place in `config/prometheus/rules/` and add to
prometheus.yml:
```yaml
rule_files:
  - /etc/prometheus/rules/*.yml
```
Bind-mount the rules dir into prometheus.

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

- **Symbolic user resolution uses host `/etc/passwd`** — works for `nobody`,
  `root`, and numeric IDs. A user defined only inside the container image's
  `/etc/passwd` won't resolve. Not a problem in practice for published images.

- **TrueNAS graphite push** — port 2003 is host-mapped. TrueNAS must push
  to this machine's LAN IP (e.g. 192.168.88.X), not localhost.

- **Pushover credentials** — alertmanager starts with null receiver; add
  Pushover user_key + API token from https://pushover.net when available.

- **No alert rules yet** — prometheus.yml has no `rule_files:` entry.
  Alert rules from the Helm chart need translation from PrometheusRule CRD
  format to plain YAML before they'll fire.
