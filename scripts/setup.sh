#!/usr/bin/env bash
#
# pelagos system setup — creates the pelagos group, initialises /var/lib/pelagos/,
# and optionally adds a user to the pelagos group.
#
# Run this once after installation (or from a package manager postinst hook).
# The script is idempotent: safe to run multiple times.
#
# Usage:
#   sudo ./scripts/setup.sh               # auto-adds SUDO_USER to pelagos group
#   sudo ./scripts/setup.sh --add-user cb # adds a specific user
#   sudo ./scripts/setup.sh --no-user     # skip user addition entirely
#
# After running, users in the 'pelagos' group can pull images without sudo:
#   pelagos image pull alpine
#
# Container operations (run, compose) still require root because they use
# Linux namespaces, mounts, and network configuration.

set -euo pipefail

# ── Argument parsing ────────────────────────────────────────────────────────

ADD_USER=""
NO_USER=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --add-user)
            ADD_USER="${2:?'--add-user requires a username'}"
            shift 2
            ;;
        --no-user)
            NO_USER=true
            shift
            ;;
        *)
            echo "usage: $0 [--add-user USERNAME] [--no-user]" >&2
            exit 1
            ;;
    esac
done

# ── Root check ──────────────────────────────────────────────────────────────

if [[ "$(id -u)" -ne 0 ]]; then
    echo "error: this script must be run as root (use sudo)" >&2
    exit 1
fi

# ── Helpers ─────────────────────────────────────────────────────────────────

ok()   { echo "  [ok]  $*"; }
done_() { echo "  [--]  $* (already done)"; }
info() { echo "==> $*"; }

# ── Create pelagos system group ───────────────────────────────────────────────

info "Checking pelagos group..."
if getent group pelagos > /dev/null 2>&1; then
    done_ "group 'pelagos' already exists"
else
    groupadd --system pelagos
    ok "created system group 'pelagos'"
fi

# ── Create /var/lib/pelagos/ ──────────────────────────────────────────────────

info "Setting up /var/lib/pelagos/..."

# Root directory: root:pelagos 0755 (root owns, group can enter)
mkdir -p /var/lib/pelagos
chown root:pelagos /var/lib/pelagos
chmod 0755 /var/lib/pelagos
ok "/var/lib/pelagos (root:pelagos 0755)"

# Image store subdirs: root:pelagos 2775 (setgid + group-writable)
# These are written by image pull and build — group members can write.
# Content-addressed (sha256 digest as directory name) so group-write is safe.
# The setgid bit (2xxx) ensures that subdirectories created by root also
# inherit the 'pelagos' group, so group members can write into them.
# We also recursively chown any existing subdirs that were created by a
# previous root pull before the setgid bit was in place.
for subdir in images layers blobs build-cache; do
    mkdir -p "/var/lib/pelagos/$subdir"
    chown -R root:pelagos "/var/lib/pelagos/$subdir"
    chmod -R g+rwX "/var/lib/pelagos/$subdir"
    chmod g+s "/var/lib/pelagos/$subdir"
    ok "/var/lib/pelagos/$subdir (root:pelagos 2775, setgid, existing subdirs repaired)"
done

# Runtime subdirs: root:root 0755
# These require root (mounts, network config, container state).
for subdir in volumes networks rootfs; do
    mkdir -p "/var/lib/pelagos/$subdir"
    chown root:root "/var/lib/pelagos/$subdir"
    chmod 0755 "/var/lib/pelagos/$subdir"
    ok "/var/lib/pelagos/$subdir (root:root 0755)"
done

# ── Add user to pelagos group ─────────────────────────────────────────────────

if $NO_USER; then
    info "Skipping user addition (--no-user)."
else
    # Determine which user to add.
    if [[ -z "$ADD_USER" ]]; then
        # Default: the user who invoked sudo, if any.
        ADD_USER="${SUDO_USER:-}"
    fi

    if [[ -z "$ADD_USER" ]]; then
        info "No user to add (run as root directly, not via sudo)."
        echo "      To add a user later: sudo usermod -aG pelagos <username>"
    else
        info "Adding '$ADD_USER' to the pelagos group..."
        if id -nG "$ADD_USER" | tr ' ' '\n' | grep -q '^pelagos$'; then
            done_ "'$ADD_USER' is already in the pelagos group"
        else
            usermod -aG pelagos "$ADD_USER"
            ok "added '$ADD_USER' to group 'pelagos'"
            echo ""
            echo "  NOTE: '$ADD_USER' must log out and back in (or run 'newgrp pelagos')"
            echo "        for group membership to take effect."
        fi
    fi
fi

# ── Enable user_allow_other in /etc/fuse.conf ────────────────────────────────
#
# fuse-overlayfs mounts with allow_other so that `pelagos exec --user UID`
# works for non-root UIDs inside rootless containers.  The allow_other FUSE
# mount option requires user_allow_other in /etc/fuse.conf when mounted by a
# non-root user.

info "Configuring /etc/fuse.conf..."
if [[ -f /etc/fuse.conf ]]; then
    if grep -q '^user_allow_other' /etc/fuse.conf; then
        done_ "user_allow_other already enabled in /etc/fuse.conf"
    else
        # Uncomment if present as a comment, otherwise append.
        if grep -q '#user_allow_other' /etc/fuse.conf; then
            sed -i 's/#user_allow_other/user_allow_other/' /etc/fuse.conf
            ok "enabled user_allow_other in /etc/fuse.conf"
        else
            echo "user_allow_other" >> /etc/fuse.conf
            ok "appended user_allow_other to /etc/fuse.conf"
        fi
    fi
else
    echo "user_allow_other" > /etc/fuse.conf
    ok "created /etc/fuse.conf with user_allow_other"
fi

# ── Done ─────────────────────────────────────────────────────────────────────

echo ""
echo "Setup complete. Users in the 'pelagos' group can pull images without sudo:"
echo "  pelagos image pull alpine"
echo ""
echo "Container operations still require root:"
echo "  sudo pelagos run alpine /bin/sh"
