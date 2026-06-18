#!/usr/bin/env bash
# Run by pelagos-install.service on the guinea-pig node (ipc6) when the build
# delivery drops new binaries in /srv/pelagos-incoming. Installs them atomically
# and restarts pelagos-cri. Verifies a non-empty file first so a truncated/
# partial delivery can never crash-loop pelagos-cri (a truncated scp once did).
set -euo pipefail
IN=/srv/pelagos-incoming
DEST=/usr/local/bin

install_one() {
  local name="$1"
  local src="$IN/$name"
  [ -s "$src" ] || { echo "skip $name: missing/empty"; return 0; }
  install -m0755 "$src" "$DEST/$name"
  echo "installed $name"
}

install_one pelagos
install_one pelagos-cri

logger -t pelagos-install "installed $(cat "$IN/.commit" 2>/dev/null || echo unknown); restarting pelagos-cri"
systemctl restart pelagos-cri

# Consume the marker so the next delivery re-triggers cleanly.
rm -f "$IN/.ready"
echo "pelagos-cri restarted"
