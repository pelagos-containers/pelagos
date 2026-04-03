#!/usr/bin/env bash
# update-aur.sh — update AUR packages after a GitHub release
#
# Usage: scripts/update-aur.sh <version>
# Example: scripts/update-aur.sh 0.60.2
#
# Requires: git, curl, makepkg (Arch Linux)
# The pkg/aur/pelagos and pkg/aur/pelagos-bin dirs must have their
# AUR git remotes configured (see docs/AUR_PUBLISHING.md).

set -euo pipefail

VERSION="${1:?Usage: $0 <version>}"
REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"

echo "==> Fetching sha256sums for v${VERSION}..."

X86=$(curl -sL "https://github.com/pelagos-containers/pelagos/releases/download/v${VERSION}/pelagos-x86_64-linux.sha256" | awk '{print $1}')
ARM=$(curl -sL "https://github.com/pelagos-containers/pelagos/releases/download/v${VERSION}/pelagos-aarch64-linux.sha256" | awk '{print $1}')
SRC=$(curl -sL "https://github.com/pelagos-containers/pelagos/archive/refs/tags/v${VERSION}.tar.gz" | sha256sum | awk '{print $1}')

echo "    x86_64: $X86"
echo "    aarch64: $ARM"
echo "    source:  $SRC"

update_pkgbuild() {
    local file="$1"
    sed -i "s/^pkgver=.*/pkgver=${VERSION}/" "$file"
}

echo ""
echo "==> Updating pkg/aur/pelagos ..."
PKG="$REPO_ROOT/pkg/aur/pelagos"
update_pkgbuild "$PKG/PKGBUILD"
sed -i "s/^sha256sums=(.*/sha256sums=('${SRC}')/" "$PKG/PKGBUILD"
(cd "$PKG" && makepkg --printsrcinfo > .SRCINFO)
(cd "$PKG" && git add PKGBUILD .SRCINFO && git commit -m "upgpkg: pelagos ${VERSION}" && git push aur master)

echo ""
echo "==> Updating pkg/aur/pelagos-bin ..."
BIN="$REPO_ROOT/pkg/aur/pelagos-bin"
update_pkgbuild "$BIN/PKGBUILD"
sed -i "s/^sha256sums_x86_64=.*/sha256sums_x86_64=('${X86}' 'SKIP' '${SRC}')/" "$BIN/PKGBUILD"
sed -i "s/^sha256sums_aarch64=.*/sha256sums_aarch64=('${ARM}' 'SKIP' '${SRC}')/" "$BIN/PKGBUILD"
(cd "$BIN" && makepkg --printsrcinfo > .SRCINFO)
(cd "$BIN" && git add PKGBUILD .SRCINFO && git commit -m "upgpkg: pelagos-bin ${VERSION}" && git push aur master)

echo ""
echo "==> Committing updated PKGBUILDs to main repo ..."
(cd "$REPO_ROOT" && git add pkg/aur/ && git commit -m "chore(aur): update sha256sums for v${VERSION}" && git push)

echo ""
echo "==> Done. AUR packages updated to v${VERSION}."
echo "    https://aur.archlinux.org/packages/pelagos"
echo "    https://aur.archlinux.org/packages/pelagos-bin"
