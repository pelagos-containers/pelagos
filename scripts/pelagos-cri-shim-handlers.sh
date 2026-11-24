#!/usr/bin/env bash
# Manage pelagos-cri shim handlers (Kata, gVisor, ...) — epic #574.
#
#   pelagos-cri-shim-handlers list
#   pelagos-cri-shim-handlers add NAME=BINARY [NAME=BINARY ...]   [--no-restart]
#   pelagos-cri-shim-handlers remove NAME [NAME ...]              [--no-restart]
#   pelagos-cri-shim-handlers runtimeclass NAME [NAME ...]        # kubectl apply
#
# Handlers live in PELAGOS_CRI_ARGS inside ENV_FILE (default
# /etc/pelagos/cri.env), which pelagos-cri.service reads. The script is
# idempotent; adding an existing NAME replaces its binary. Removing every
# handler leaves a native-only configuration. Restarting pelagos-cri re-adopts
# running shim pods (their shims live in their own systemd scopes), so it does
# not disturb running pods.
set -euo pipefail

ENV_FILE="${ENV_FILE:-/etc/pelagos/cri.env}"
UNIT="${UNIT:-pelagos-cri.service}"
KUBECTL="${KUBECTL:-kubectl}"
RESTART=1

die() { echo "pelagos-cri-shim-handlers: $*" >&2; exit 1; }

# Print the current "NAME=BINARY" pairs, one per line.
current() {
    [ -f "$ENV_FILE" ] || return 0
    local line
    line=$(grep -E '^PELAGOS_CRI_ARGS=' "$ENV_FILE" | tail -n1 || true)
    line=${line#PELAGOS_CRI_ARGS=}
    line=${line#\"}; line=${line%\"}
    # shellcheck disable=SC2086
    set -- $line
    while [ $# -gt 0 ]; do
        if [ "$1" = "--shim-handler" ] && [ $# -ge 2 ]; then echo "$2"; shift; fi
        shift
    done
}

# Non-handler flags in PELAGOS_CRI_ARGS are preserved.
other_flags() {
    [ -f "$ENV_FILE" ] || return 0
    local line out=""
    line=$(grep -E '^PELAGOS_CRI_ARGS=' "$ENV_FILE" | tail -n1 || true)
    line=${line#PELAGOS_CRI_ARGS=}
    line=${line#\"}; line=${line%\"}
    # shellcheck disable=SC2086
    set -- $line
    while [ $# -gt 0 ]; do
        if [ "$1" = "--shim-handler" ]; then shift; else out="$out $1"; fi
        shift
    done
    echo "${out# }"
}

write() { # args: handler pairs on stdin
    local pairs flags args tmp
    pairs=$(cat); flags=$(other_flags)
    args="$flags"
    while IFS= read -r p; do
        [ -n "$p" ] && args="$args --shim-handler $p"
    done <<<"$pairs"
    args="${args# }"
    mkdir -p "$(dirname "$ENV_FILE")"
    tmp=$(mktemp "$ENV_FILE.XXXXXX")
    { [ -f "$ENV_FILE" ] && grep -v -E '^PELAGOS_CRI_ARGS=' "$ENV_FILE" || true
      [ -n "$args" ] && echo "PELAGOS_CRI_ARGS=\"$args\"" || true; } >"$tmp"
    chmod 0644 "$tmp"; mv "$tmp" "$ENV_FILE"
}

restart() {
    if [ "$RESTART" = 1 ] && command -v systemctl >/dev/null 2>&1 \
        && systemctl cat "$UNIT" >/dev/null 2>&1; then
        systemctl daemon-reload
        systemctl restart "$UNIT"
        echo "restarted $UNIT"
    else
        echo "not restarting $UNIT (restart it to apply)"
    fi
}

cmd=${1:-}; [ -n "$cmd" ] || die "usage: $0 list|add|remove|runtimeclass ..."; shift
args=()
for a in "$@"; do
    case "$a" in --no-restart) RESTART=0 ;; *) args+=("$a") ;; esac
done

case "$cmd" in
list) current ;;
add)
    [ ${#args[@]} -gt 0 ] || die "add needs NAME=BINARY"
    cur=$(current)
    for spec in "${args[@]}"; do
        name=${spec%%=*}; bin=${spec#*=}
        [[ "$spec" == *=* && -n "$name" && -n "$bin" ]] || die "bad spec '$spec' (want NAME=BINARY)"
        [[ "$name" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] || die "NAME '$name' must be a DNS label (RuntimeClass handler)"
        [[ "$name" != pelagos ]] || die "'pelagos' is the built-in native handler"
        [ -x "$bin" ] || die "$bin is not an executable file"
        cur=$(printf '%s\n' "$cur" | grep -v -E "^${name}=" || true)
        cur=$(printf '%s\n%s=%s\n' "$cur" "$name" "$bin")
    done
    printf '%s\n' "$cur" | sed '/^$/d' | write
    restart ;;
remove)
    [ ${#args[@]} -gt 0 ] || die "remove needs NAME"
    cur=$(current)
    for name in "${args[@]}"; do
        cur=$(printf '%s\n' "$cur" | grep -v -E "^${name}=" || true)
    done
    printf '%s\n' "$cur" | sed '/^$/d' | write
    restart ;;
runtimeclass)
    [ ${#args[@]} -gt 0 ] || die "runtimeclass needs NAME"
    for name in "${args[@]}"; do
        printf 'apiVersion: node.k8s.io/v1\nkind: RuntimeClass\nmetadata:\n  name: %s\nhandler: %s\n' \
            "$name" "$name" | "$KUBECTL" apply -f -
    done ;;
*) die "unknown command '$cmd'" ;;
esac
