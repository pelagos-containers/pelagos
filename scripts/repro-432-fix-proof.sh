#!/usr/bin/env bash
# #432 fix proof: does identity-mapping the FULL id range in the nested user
# namespace (instead of only 0->0) let overlay copy-up of an image file owned
# by an otherwise-unmapped gid succeed? Run as root on ext4/xfs.
set -u

trial() {  # $1 = unshare id-map args; $2 = label
  local mapargs="$1" label="$2"
  local B; B=$(mktemp -d /var/tmp/f432.XXXX)
  mkdir -p "$B/l/usr/local/cargo" "$B/u" "$B/w" "$B/m"
  echo seed > "$B/l/usr/local/cargo/env"
  chown -R 0:983 "$B/l/usr/local"   # image dir owned by a non-root gid (like rust's cargo)
  printf '%-46s -> ' "$label"
  capsh --drop=cap_sys_admin -- -c "unshare -m --user $mapargs bash -c '
    mount -t overlay overlay -o lowerdir=$B/l,upperdir=$B/u,workdir=$B/w,metacopy=off $B/m 2>/dev/null || { echo MOUNT-FAIL; exit; }
    if touch $B/m/usr/local/cargo/.package-cache 2>/dev/null; then echo CREATE-OK; else echo CREATE-FAIL-EOVERFLOW; fi
  '"
  rm -rf "$B"
}

trial "--map-root-user"                              "0->0 only            (current pelagos = broken)"
trial "--map-users 0:0:65536 --map-groups 0:0:65536" "identity 0..65535    (proposed fix, 64k range)"
trial "--map-users 0:0:4294967295 --map-groups 0:0:4294967295" "identity full range  (proposed fix, full)"
