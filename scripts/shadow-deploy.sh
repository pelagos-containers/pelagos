#!/usr/bin/env bash
# shadow-deploy.sh — build pelagos on this machine and install it directly on a
# cluster node without going through the release pipeline.
#
# Usage:
#   scripts/shadow-deploy.sh [NODE]              # install, don't restart CRI
#   scripts/shadow-deploy.sh [NODE] --restart    # install + restart pelagos-cri
#
# NODE defaults to ipc4.  --restart is required to make the CRI pick up the new
# binary; omit it when testing CLI commands only (e.g. `pelagos image pull`).
#
# After install, test on the node:
#   ssh NODE pelagos --version
#   ssh NODE sudo pelagos image pull docker.io/library/alpine:latest
#   ssh NODE sudo crictl pods   (if you restarted the CRI)

set -euo pipefail

NODE="${1:-ipc4}"
RESTART=false
for arg in "$@"; do
    [[ "$arg" == "--restart" ]] && RESTART=true
done

BINARIES=(
    target/release/pelagos
    target/release/pelagos-cri
    target/release/pelagos-dns
    target/release/pelagos-shim-wasm
)

echo "==> Building release binaries..."
cargo build --release

echo "==> Uploading to ${NODE}:/tmp/..."
# shellcheck disable=SC2029
scp "${BINARIES[@]}" "${NODE}:/tmp/"

echo "==> Installing on ${NODE}..."
ssh "${NODE}" bash -s << 'REMOTE'
set -euo pipefail
INSTALL_DIR=/usr/local/bin
install -m 755 /tmp/pelagos        "${INSTALL_DIR}/pelagos"
install -m 755 /tmp/pelagos-cri    "${INSTALL_DIR}/pelagos-cri"
install -m 755 /tmp/pelagos-dns    "${INSTALL_DIR}/pelagos-dns"
install -m 755 /tmp/pelagos-shim-wasm "${INSTALL_DIR}/pelagos-shim-wasm"
install -m 755 /tmp/pelagos-shim-wasm "${INSTALL_DIR}/containerd-shim-pelagos-wasm-v1"
echo "Installed: $(pelagos --version)"
REMOTE

if [[ "$RESTART" == "true" ]]; then
    echo "==> Restarting pelagos-cri on ${NODE} (pods will be orphaned and rescheduled)..."
    ssh "${NODE}" sudo systemctl restart pelagos-cri.service
    echo "==> pelagos-cri restarted."
else
    echo ""
    echo "Binary installed. CRI not restarted (add --restart to do so)."
    echo "To test CLI commands: ssh ${NODE} sudo pelagos image pull <ref>"
fi
