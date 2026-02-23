# Ongoing Tasks

## Current State (Feb 23, 2026)

Both repos are clean and pushed.

**remora** (`~/Projects/remora`): branch `master`, last commit `75d4949`
**home-monitoring** (`~/Projects/home-monitoring`): branch `main`, last commit `80c0610`

---

## ⚠️ IMMEDIATE: Two manual steps to finish truenas-api-exporter

### Step 1 — Build the image (one-time, needs root + internet)

```bash
sudo -E remora build \
  --network bridge \
  -t truenas-api-exporter:latest \
  --file ~/Projects/home-monitoring/monitoring-setup/truenas-graphite-exporter/Remfile \
  ~/Projects/home-monitoring/monitoring-setup/truenas-graphite-exporter/
```

### Step 2 — Add TrueNAS API key

Log into http://192.168.88.30 → Credentials → API Keys → create/copy key.

Add to `~/Projects/home-monitoring/monitoring-setup/.env`:
```
TRUENAS_API_KEY=your-key-here
```

Or pass inline at runtime:
```bash
sudo -E TRUENAS_API_KEY=yourkey ~/Projects/remora/scripts/start-monitoring.sh
```

**Until both steps are done:** the stack won't fully start — prometheus
depends-on `truenas-api-exporter :ready-port 9109` and will block indefinitely
if the service isn't up.

---

## To start the stack

```bash
sudo -E ~/Projects/remora/scripts/start-monitoring.sh
```

The script builds remora from source (no-op if unchanged), pulls all images
(skips locally-built ones), substitutes `PLEX_TOKEN` and `TRUENAS_API_KEY`
from env or `.env`, cleans up previous state, and brings up all 8 services.

### Stack: 8 services

```
snmp-exporter        :9116   MikroTik SNMP
plex-exporter        :9594   Plex REST API
mktxp                :49090  MikroTik RouterOS API (bandwidth, queues, DHCP)
graphite-exporter    :9108   TrueNAS graphite push receiver (listens :2003)
truenas-api-exporter :9109   TrueNAS SCALE REST API — ZFS pools, scrub, SMART
alertmanager         :9093   Alert routing (null receiver; Pushover-ready)
prometheus           :9090   scrapes all of the above
grafana              :3000   dashboards  (admin / prom-operator)
```

### Dependency order (compose waits for each :ready-port before prometheus starts)

```
alertmanager, snmp-exporter, plex-exporter, mktxp,
graphite-exporter, truenas-api-exporter  →  prometheus  →  grafana
```

---

## Next tasks

### A. Alert rules (when ready)

Translate Helm chart PrometheusRule CRDs to standalone `rule_files:` YAML.

Source files:
- `monitoring-setup/prometheus/disk-temp-alerts.yaml`
- `monitoring-setup/prometheus/truenas-alerts.yaml`

Target: `remora/config/prometheus/rules/*.yml`

Add to `prometheus.yml`:
```yaml
rule_files:
  - /etc/prometheus/rules/*.yml
```

Bind-mount `./config/prometheus/rules` into the prometheus service in `compose.rem`.

### B. Pushover alerts (when credentials are available)

Edit `remora/config/alertmanager/alertmanager.yml`:
1. Replace the null receiver with:
```yaml
- name: 'pushover'
  pushover_configs:
  - user_key: 'YOUR_USER_KEY'
    token: 'YOUR_API_TOKEN'
    priority: '{{ if eq .Status "firing" }}1{{ else }}0{{ end }}'
```
2. Update `route.receiver: 'pushover'`

Get credentials at https://pushover.net.

---

## Known limitations / watch list

- **compose `(command ...)` replaces entire entrypoint+cmd** — to pass flags to
  an image's existing entrypoint, repeat the entrypoint binary as the first
  element. See prometheus, graphite-exporter, alertmanager for the pattern.

- **TrueNAS graphite push** — port 2003 is host-mapped. TrueNAS must push to
  this machine's LAN IP (e.g. 192.168.88.X), not localhost. Configure at:
  TrueNAS SCALE → System → Advanced → Reporting → Graphite.

- **Plex token** — resolved from `$PLEX_TOKEN` env var or `monitoring-setup/.env`.
  Placeholder `YOUR_PLEX_TOKEN_HERE` is substituted at runtime by start-monitoring.sh.

- **truenas-api-exporter is locally built** — `remora image pull` is skipped
  for it. Must be built once with `remora build` (see step 1 above). Rebuild
  after any changes to `truenas_api_exporter.py`.
