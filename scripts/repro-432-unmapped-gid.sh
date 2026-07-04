#!/usr/bin/env bash
# #432 root-cause isolation: is the overlay copy-up EOVERFLOW caused by a lower
# file owned by a uid/gid that is UNMAPPED in the nested user namespace
# (pelagos maps only 0->0, but image layers own files as arbitrary ids)?
# Run as root on an ext4/xfs node.
set -u

test_gid() {  # $1 = gid that owns the lower cargo tree; $2 = label
  local gid="$1" label="$2"
  local B; B=$(mktemp -d /var/tmp/g432.XXXX)
  mkdir -p "$B/l/usr/local/cargo" "$B/u" "$B/w" "$B/m"
  echo seed > "$B/l/usr/local/cargo/env"
  chown -R "0:$gid" "$B/l/usr/local"
  printf '%-42s -> ' "$label"
  # nested condition: uid 0, no CAP_SYS_ADMIN, userns mapping ONLY 0->0
  capsh --drop=cap_sys_admin -- -c "unshare -Urm bash -c '
    mount -t overlay overlay -o lowerdir=$B/l,upperdir=$B/u,workdir=$B/w,metacopy=off $B/m 2>/dev/null || { echo MOUNT-FAIL; exit; }
    if touch $B/m/usr/local/cargo/.package-cache 2>/dev/null; then echo CREATE-OK; else echo CREATE-FAIL-EOVERFLOW; fi
  '"
  rm -rf "$B"
}

test_gid 0    "lower owned by gid 0    (mapped in userns)"
test_gid 983  "lower owned by gid 983  (UNMAPPED in userns)"
test_gid 1000 "lower owned by gid 1000 (UNMAPPED in userns)"
