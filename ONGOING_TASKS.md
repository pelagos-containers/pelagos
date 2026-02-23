# Ongoing Tasks

## Current State (Feb 23, 2026)

Both repos clean and pushed. Stack fully operational (6/6 Prometheus targets up).

**remora** (`~/Projects/remora`): branch `master`, last commit `01fab2a`
**home-monitoring** (`~/Projects/home-monitoring`): branch `main`, last commit `b937b16`

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

## Next tasks

### A. Alert rules

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

### B. Pushover alerts

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

### C. MikroTik credentials for mktxp

`config/mktxp/mktxp.conf` needs a real RouterOS API username and password.
Without them mktxp scrapes zero metrics (process is up, but all gauges are empty).

### D. CRI compliance

See `docs/CRI_COMPLIANCE.md` for the full roadmap (phases C1–C7).
Short version: daemon → gRPC skeleton → ImageService → pod sandbox → CNI →
container lifecycle → exec/logs/stats. The pod sandbox (C4) is the critical
path item requiring the most new design work.

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
  The script substitutes it at runtime; the token never touches the compose file.
