#!/usr/bin/env bash
set -euo pipefail

LOG="teardown.log"

sudo scripts/reset-test-env.sh
sudo -E RUST_LOG=pelagos::teardown=info cargo test --test integration_tests 2>&1 | tee "$LOG"
scripts/analyze-teardown-log.sh "$LOG"
