# Remora Monitoring Stack

A three-service observability stack built with [Remora](../../../README.md):

| Service | Port | Role |
|---------|------|------|
| **Prometheus** | 9090 | Time-series metrics — scrapes itself and Loki |
| **Loki** | 3100 | Log aggregation backend |
| **Grafana** | 3000 | Dashboards — queries Prometheus and Loki |

## Lisp features demonstrated

| Feature | Where |
|---------|-------|
| `define` | All ports and memory limits named at top |
| `env` with fallback | `GRAFANA_PASSWORD` defaults to `"admin"` |
| `on-ready` | Hooks for prometheus and loki readiness |
| Multiple `depends-on` | Grafana waits for prometheus:9090 **and** loki:3100 |
| Dotted-pair `:env` | `("GF_SECURITY_ADMIN_PASSWORD" . grafana-password)` |
| `define-service` | Flat keyword-style service definitions |

## Quick start

```bash
# Build and run (requires root for namespaces and networking)
sudo ./examples/compose/monitoring/run.sh

# Override Grafana password
GRAFANA_PASSWORD=secret sudo ./examples/compose/monitoring/run.sh

# Skip rebuild if images already exist
sudo ./examples/compose/monitoring/run.sh --no-stack
```

## Manual steps

```bash
# Pull base image
sudo remora image pull alpine:latest

# Build images
sudo remora build -t monitoring-prometheus --network bridge examples/compose/monitoring/prometheus
sudo remora build -t monitoring-loki       --network bridge examples/compose/monitoring/loki
sudo remora build -t monitoring-grafana    --network bridge examples/compose/monitoring/grafana

# Start stack
sudo remora compose up -f examples/compose/monitoring/compose.reml -p monitoring

# In another terminal: check status
sudo remora compose ps -f examples/compose/monitoring/compose.reml -p monitoring

# Tear down
sudo remora compose down -f examples/compose/monitoring/compose.reml -p monitoring -v
```

## Architecture

```
                    monitoring-net (10.89.1.0/24)
                    ┌─────────────────────────────┐
                    │                             │
  host:9090 ──────► │  prometheus                 │
                    │    scrapes: self, loki       │
                    │                             │
  host:3100 ──────► │  loki                       │
                    │    storage: tmpfs            │
                    │                             │
  host:3000 ──────► │  grafana                    │
                    │    depends-on: prometheus    │
                    │    depends-on: loki          │
                    │    datasources: provisioned  │
                    │                             │
                    └─────────────────────────────┘
```

## Dotted-pair env syntax

This example uses the dotted-pair `("KEY" . value)` syntax for env entries,
where `value` is a Lisp expression evaluated at call-site:

```lisp
:env ("GF_SECURITY_ADMIN_PASSWORD" . grafana-password)
```

`grafana-password` was defined earlier as:

```lisp
(define grafana-password
  (let ((p (env "GRAFANA_PASSWORD")))
    (if (null? p) "admin" p)))
```

So the password is resolved from the host environment at stack startup,
not hard-coded in the service definition.
