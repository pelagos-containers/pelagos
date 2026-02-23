#!/bin/bash
# Quick health check for the home monitoring stack.
# Hits each exporter and UI endpoint; prints OK or FAIL with a one-line reason.
#
# Usage: ./scripts/check-monitoring.sh

set -uo pipefail

ok()   { printf "  %-30s \033[32mOK\033[0m\n"   "$1"; }
fail() { printf "  %-30s \033[31mFAIL\033[0m  %s\n" "$1" "$2"; }

check() {
    local label="$1"
    local url="$2"
    local grep_pat="${3:-}"

    local body
    body=$(curl -sf --max-time 5 "$url" 2>&1) || { fail "$label" "no response from $url"; return; }

    if [ -n "$grep_pat" ] && ! echo "$body" | grep -q "$grep_pat"; then
        fail "$label" "response missing '$grep_pat'"
    else
        ok "$label"
    fi
}

echo "Home monitoring stack — endpoint check"
echo "======================================="

check "snmp-exporter    :9116" "http://localhost:9116/metrics"     "# HELP"
check "mktxp            :49090" "http://localhost:49090/metrics"   "# HELP"
check "graphite-exporter :9108" "http://localhost:9108/metrics"    "# HELP"
check "truenas-exporter  :9109" "http://localhost:9109/metrics"    "# HELP"
check "plex-exporter     :9594" "http://localhost:9594/metrics"    "# HELP"
check "alertmanager      :9093" "http://localhost:9093/-/healthy"  "OK"
check "prometheus        :9090" "http://localhost:9090/-/healthy"  "Prometheus"
check "grafana           :3000" "http://localhost:3000/api/health" "\"ok\""

echo ""
echo "Prometheus scrape targets:"
targets=$(curl -sf --max-time 5 "http://localhost:9090/api/v1/targets") \
    || { echo "  (could not reach Prometheus API)"; exit 0; }

# Parse with jq if available, otherwise fall back to line-oriented grep.
if command -v jq &>/dev/null; then
    echo "$targets" | jq -r '
      .data.activeTargets[] |
      [ .health, (.labels.job // "?"), .scrapeUrl, (.lastError // "") ] |
      @tsv' | while IFS=$'\t' read -r health job url err; do
        if [ "$health" = "up" ]; then
            printf "  \033[32m%-6s\033[0m %-30s  %s\n" "$health" "$job" "$url"
        else
            printf "  \033[31m%-6s\033[0m %-30s  %s\n" "$health" "$job" "$url"
            [ -n "$err" ] && printf "         error: %s\n" "$err"
        fi
    done
else
    # Minimal fallback: print raw health + job pairs.
    echo "$targets" | grep -o '"health":"[^"]*"' | sort | uniq -c | \
        awk '{printf "  %s  (x%s)\n", $2, $1}'
    echo "  (install jq for detailed output)"
fi
