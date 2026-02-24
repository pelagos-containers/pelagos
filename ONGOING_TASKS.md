# Ongoing Tasks

## Current State (Feb 23, 2026)

Both repos clean and pushed. Stack fully operational (6/6 Prometheus targets up).
TrueNAS API key set in `monitoring-setup/.env` — pool metrics flowing.
Three Grafana dashboards provisioned: mktxp, Prometheus overview, TrueNAS (custom).

**remora** (`~/Projects/remora`): branch `master`, last commit `2ecd556`
**home-monitoring** (`~/Projects/home-monitoring`): branch `main`, last commit `1135612`

---

## To start/stop the stack

```bash
sudo -E ~/Projects/remora/scripts/start-monitoring.sh          # start
sudo -E ~/Projects/remora/scripts/start-monitoring.sh --down   # stop
./scripts/check-monitoring.sh                                   # verify
```

See `docs/HOME_MONITORING_STACK.md` for full operational reference.
See `docs/HOME_MONITORING_CONFIG_NOTES.md` for why configs are written the way they are.

---

## Grafana dashboards

All dashboards provisioned from files — no manual import needed.

| Dashboard | Source | Status |
|-----------|--------|--------|
| Prometheus 2.0 Overview | Grafana.com #3662 | ✅ Live |
| Mikrotik MKTXP Exporter | Grafana.com #13679 | ⏳ Empty until MikroTik credentials added |
| TrueNAS | Custom-built | ✅ Pool panels live; graphite panels pending TrueNAS push config |

Dashboard files: `home-monitoring/remora/config/grafana/provisioning/dashboards/`

---

## Next tasks

### A. TrueNAS graphite push

TrueNAS is currently pushing collectd metrics to another Prometheus/Grafana host.
TrueNAS SCALE only supports one graphite server target.

**Decision pending:** move the push to this host (for testing), then either:
- Decommission the other stack, OR
- Add a carbon relay (e.g. `carbon-relay-ng`) as a compose service to fan out to both hosts

To move: TrueNAS SCALE → System → Advanced → Reporting → Graphite server = this
machine's LAN IP, port 2003.

Once graphite data flows, verify the metric names against the TrueNAS dashboard
queries. The memory metric suffix may differ between TrueNAS versions
(`truenas_memory_used` vs `truenas_memory_memory_used`). Check with:
```bash
curl -s http://localhost:9108/metrics | grep "^truenas_memory" | head -10
```

### B. Alert rules

Translate Helm chart PrometheusRule CRDs to standalone `rule_files:` YAML.

Source files (in home-monitoring repo):
- `monitoring-setup/prometheus/disk-temp-alerts.yaml`
- `monitoring-setup/prometheus/truenas-alerts.yaml`

Target: `remora/config/prometheus/rules/*.yml`

Add to `prometheus.yml`:
```yaml
rule_files:
  - /etc/prometheus/rules/*.yml
```

Bind-mount `./config/prometheus/rules` into the prometheus service in `compose.rem`.

Hot-reload after: `curl -X POST http://localhost:9090/-/reload`

### C. Pushover alerts

Edit `remora/config/alertmanager/alertmanager.yml`:
1. Replace null receiver with:
```yaml
- name: 'pushover'
  pushover_configs:
  - user_key: 'YOUR_USER_KEY'
    token: 'YOUR_API_TOKEN'
    priority: '{{ if eq .Status "firing" }}1{{ else }}0{{ end }}'
```
2. Update `route.receiver: 'pushover'`

Credentials at https://pushover.net.

### D. MikroTik credentials for mktxp

`config/mktxp/mktxp.conf` needs a real RouterOS API username and password.
Without them mktxp scrapes zero metrics (process is up, but all gauges are empty).

### E. CRI compliance

See `docs/CRI_COMPLIANCE.md` for the full roadmap (phases C1–C7).
Short version: daemon → gRPC skeleton → ImageService → pod sandbox → CNI →
container lifecycle → exec/logs/stats. The pod sandbox (C4) is the critical
path item requiring the most new design work.

---

## Completed this session

- Fixed three build engine bugs (EINVAL dedup, WORKDIR mkdir, COPY dest resolution)
- Added `scripts/check-monitoring.sh` endpoint health checker
- Fixed Prometheus self-scrape (`localhost` → `prometheus` service name)
- Added `--web.enable-lifecycle` to prometheus flags for hot-reload
- Added Grafana dashboard provisioning (mktxp, Prometheus overview, TrueNAS)
- TrueNAS dashboard custom-built against our metric names (no community dashboard matches)
- Added docs: `ACCESS_PATTERNS.md`, `CRI_COMPLIANCE.md`, `HOME_MONITORING_CONFIG_NOTES.md`
- Added macro: "So Long and Thanks for all the Fish" to CLAUDE.md
- Committed previously untracked files: snmp.yml, datasources/prometheus.yaml, README.md

---

## Known limitations / watch list

- **compose `(command ...)` replaces entire entrypoint+cmd** — to pass flags to
  an image's existing entrypoint, repeat the entrypoint binary as the first
  element. See prometheus, graphite-exporter, alertmanager in `compose.rem`.

- **TrueNAS graphite push** — port 2003 is host-mapped. TrueNAS must push to
  this machine's LAN IP, not localhost. Configure at:
  TrueNAS SCALE → System → Advanced → Reporting → Graphite.

- **truenas-api-exporter is locally built** — built once by `start-monitoring.sh`
  if not already cached. Rebuild after changes to `truenas_api_exporter.py` with:
  `sudo remora image rm truenas-api-exporter:latest` then re-run the script.

- **Plex token** — set in `monitoring-setup/.env` as `PLEX_TOKEN=...`.

- **TrueNAS API key** — set in `monitoring-setup/.env` as `TRUENAS_API_KEY=...`.

- **TrueNAS graphite metric names** — derived from graphite_mapping.yaml regex
  rules; exact suffixes depend on TrueNAS/collectd version. Verify once push
  is flowing and adjust dashboard queries if needed.
