# Ongoing Tasks

## Current Task: Home Monitoring Stack via Remora Compose

### Context

The user has a Kubernetes/Helm monitoring stack at `~/Projects/home-monitoring` that they want
to model using `remora compose`. The stack monitors a MikroTik router and TrueNAS NAS via
Prometheus + Grafana + several exporters.

This work has two phases:
1. **Add bind-mount support to compose** — the critical missing feature
2. **Write the core monitoring compose file** — validate with prometheus + grafana + exporters

---

## Phase 1: Bind-Mount Support in Compose

### Why It's Needed

Six of the eight monitoring services require config file injection:
- `prometheus.yml` — scrape targets, alerting rules
- `alertmanager.yml` — routing, receivers
- `snmp.yml` — SNMP walk config for MikroTik
- `mapping.yaml` — graphite→prometheus metric renaming
- `mktxp.conf` — MikroTik credentials + connection
- Grafana provisioning configs (datasources, dashboards)

Without bind-mounts, these can only be baked into custom images — defeating the purpose.

### API Design

New field in compose S-expression syntax:

```lisp
; read-write bind mount
(bind-mount "/host/path" "/container/path")

; read-only bind mount (preferred for config files)
(bind-mount "/host/path" "/container/path" :ro)
```

This is consistent with the existing keyword-argument style used in `(depends-on ...)`.

### Changes

#### `src/compose.rs`

1. Add `BindMount` struct:
   ```rust
   pub struct BindMount {
       pub host_path: String,
       pub container_path: String,
       pub read_only: bool,
   }
   ```

2. Add `bind_mounts: Vec<BindMount>` field to `ServiceSpec`.

3. Parse `"bind-mount"` case in `parse_service_spec()`:
   - `list[1]` → `host_path` (required atom)
   - `list[2]` → `container_path` (required atom)
   - scan remaining for `:ro` keyword → sets `read_only = true`

4. Unit tests in `compose.rs`:
   - `test_bind_mount_rw` — parse `(bind-mount "/h" "/c")`
   - `test_bind_mount_ro` — parse `(bind-mount "/h" "/c" :ro)`
   - `test_bind_mount_missing_args` — error on missing host or container path

#### `src/cli/compose.rs`

In `spawn_service()`, after the volumes loop, add:

```rust
for bm in &svc.bind_mounts {
    if bm.read_only {
        cmd = cmd.with_bind_mount_ro(&bm.host_path, &bm.container_path);
    } else {
        cmd = cmd.with_bind_mount(&bm.host_path, &bm.container_path);
    }
}
```

#### `tests/integration_tests.rs`

New test `compose_bind_mount`:
- Write a compose.rem with a service that bind-mounts a tmpdir containing a file
- `compose up --foreground`, let it run and exit
- Assert service output contains file content

#### `docs/INTEGRATION_TESTS.md`

Add entry for `compose_bind_mount` test.

---

## Phase 2: Core Monitoring Compose File

### Scope (core stack, validated first)

Start with these four services, all reachable and config-light:

| Service | Image | Depends on |
|---------|-------|------------|
| snmp-exporter | `prom/snmp-exporter:v0.21.0` | (none) |
| plex-exporter | `ghcr.io/axsuul/plex-media-server-exporter:latest` | (none) |
| prometheus | `prom/prometheus:latest` | snmp-exporter, plex-exporter |
| grafana | `grafana/grafana:latest` | prometheus |

Add remaining exporters (mktxp, graphite-exporter, truenas-api-exporter, alertmanager)
after core is validated.

### Files to Create

All files live in `~/Projects/home-monitoring/remora/`:

```
remora/
  compose.rem                  # Main compose file
  config/
    prometheus/
      prometheus.yml           # Scrape configs for all exporters
    grafana/
      provisioning/
        datasources/
          prometheus.yaml      # Auto-configure Prometheus datasource
    snmp/
      snmp.yml                 # MikroTik walk config (minimal)
```

### compose.rem Design

```lisp
(compose
  (network monitoring (subnet "172.20.0.0/24"))

  (volume grafana-data)

  ; SNMP Exporter — scrapes MikroTik at 192.168.88.1
  (service snmp-exporter
    (image "prom/snmp-exporter:v0.21.0")
    (network monitoring)
    (port 9116 9116)
    (bind-mount "./config/snmp/snmp.yml" "/etc/snmp_exporter/snmp.yml" :ro))

  ; Plex Exporter — polls Plex REST API (env-var configured, no config file)
  (service plex-exporter
    (image "ghcr.io/axsuul/plex-media-server-exporter:latest")
    (network monitoring)
    (port 9594 9594)
    (env PLEX_ADDR "http://192.168.88.30:32400")
    (env PLEX_TOKEN "<from .env>")
    (env PLEX_SSL_VERIFY "false"))

  ; Prometheus — scrapes exporters, exposes query API for Grafana
  (service prometheus
    (image "prom/prometheus:latest")
    (network monitoring)
    (port 9090 9090)
    (bind-mount "./config/prometheus/prometheus.yml" "/etc/prometheus/prometheus.yml" :ro)
    (depends-on
      (snmp-exporter :ready-port 9116)
      (plex-exporter :ready-port 9594)))

  ; Grafana — visualizes Prometheus data
  (service grafana
    (image "grafana/grafana:latest")
    (network monitoring)
    (port 3000 3000)
    (volume grafana-data "/var/lib/grafana")
    (bind-mount "./config/grafana/provisioning" "/etc/grafana/provisioning" :ro)
    (env GF_SECURITY_ADMIN_PASSWORD "prom-operator")
    (env GF_SECURITY_ADMIN_USER "admin")
    (depends-on (prometheus :ready-port 9090))))
```

### prometheus.yml Content

```yaml
global:
  scrape_interval: 30s
  evaluation_interval: 30s

scrape_configs:
  - job_name: snmp_mikrotik
    static_configs:
      - targets: ['192.168.88.1']
    metrics_path: /snmp
    params:
      module: [mikrotik]
    relabel_configs:
      - source_labels: [__address__]
        target_label: __param_target
      - source_labels: [__param_target]
        target_label: instance
      - target_label: __address__
        replacement: snmp-exporter:9116

  - job_name: plex
    static_configs:
      - targets: ['plex-exporter:9594']
```

### Grafana Datasource Provisioning

```yaml
# config/grafana/provisioning/datasources/prometheus.yaml
apiVersion: 1
datasources:
  - name: Prometheus
    type: prometheus
    url: http://prometheus:9090
    isDefault: true
    access: proxy
```

### snmp.yml

Pulled directly from the existing `prometheus/snmp-values.yaml` MikroTik module definition —
stripped to the minimal subset needed for the MikroTik walk.

---

## Execution Order

1. ✅ Write this plan to ONGOING_TASKS.md
2. ✅ Implement bind-mount support (`compose.rs` + `cli/compose.rs`)
3. ✅ Write unit tests for bind-mount parsing (4 new tests, 139 total pass)
4. ✅ Write integration test `test_compose_bind_mount_parse_and_validate`
5. ✅ Update `docs/INTEGRATION_TESTS.md`
6. ✅ `cargo fmt`, `cargo clippy`, `cargo test --lib` — all pass
7. ✅ Create `~/Projects/home-monitoring/remora/` directory and config files
8. ⏳ Pull images: user must run `sudo -E remora image pull ...` (see README)
9. ⏳ Run `sudo -E remora compose up -f remora/compose.rem --foreground`
10. ⏳ Verify Grafana at `:3000`, Prometheus at `:9090`
11. ⏳ Report back — then decide on remaining exporters

---

## Notes & Risks

- **Relative paths in bind-mount**: The compose file uses `./config/...` paths. The runtime
  must resolve these relative to the compose file's directory, not the CWD. Need to handle
  this in `spawn_service()` using the compose file path stored in project state.

- **snmp.yml format**: The SNMP exporter's config file format changed in v0.21+. The existing
  `snmp-values.yaml` embeds the module config directly; we need to translate it to the
  standalone `snmp.yml` format.

- **Plex token**: Must be sourced from the `.env` file. The compose file will have a placeholder;
  user must substitute or we add env-file support (out of scope for now).

- **No alertmanager yet**: Alerting is out of scope for the core validation. Add after core works.

- **Custom truenas-api-exporter image**: Python image needs a Remfile + `remora build`. Deferred
  until after core stack is validated.

- **Network name length limit**: The scoped network name `home-monitoring-monitoring` is 26 chars,
  over the 12-char limit triggering hash truncation. This is handled automatically in
  `scoped_network_name()` — just worth noting for debugging.
