#!/usr/bin/env bash
#
# Build pelagos in release mode and install to /usr/local/bin.
#
# Usage:  ./scripts/install.sh [INSTALL_DIR]
#
# If run as a normal user, builds with your toolchain and uses sudo
# only to copy the binary. If run as root (e.g. in CI), skips sudo.
#
set -euo pipefail

INSTALL_DIR="${1:-/usr/local/bin}"

# If we're root via sudo (not a true root session like CI), the user's
# rustup/cargo may not be on root's PATH. Build as the invoking user.
if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ]; then
    echo "Building pelagos (release) as ${SUDO_USER}..."
    sudo -u "$SUDO_USER" cargo build --release
    echo "Installing to ${INSTALL_DIR}/pelagos..."
    install -m 755 target/release/pelagos "${INSTALL_DIR}/pelagos"
elif [ "$(id -u)" -eq 0 ]; then
    # True root (CI, container, etc.) — just build and install directly.
    echo "Building pelagos (release)..."
    cargo build --release
    echo "Installing to ${INSTALL_DIR}/pelagos..."
    install -m 755 target/release/pelagos "${INSTALL_DIR}/pelagos"
else
    # Normal user — build, then sudo for the install step.
    echo "Building pelagos (release)..."
    cargo build --release
    echo "Installing to ${INSTALL_DIR}/pelagos (may prompt for sudo)..."
    sudo install -m 755 target/release/pelagos "${INSTALL_DIR}/pelagos"
fi

echo "Done. $(pelagos --version 2>/dev/null || echo 'pelagos installed')"
