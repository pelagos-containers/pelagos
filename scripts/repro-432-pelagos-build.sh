#!/usr/bin/env bash
# #432 definitive repro: run a REAL `pelagos build` with a cargo RUN step under
# the exact nested/in-pod condition — uid 0 with CAP_SYS_ADMIN dropped (#426) —
# which forces the user-namespace + native-overlay+userxattr path, straced, to
# capture the syscall that returns EOVERFLOW (cargo "package cache lock" error).
#
# Needs a pelagos >= v0.65.44 (for --network host) and an ext4/xfs store (NOT
# btrfs). Invoke on the node as:
#   sudo capsh --drop=cap_sys_admin -- -c 'PELAGOS=/path/to/pelagos bash repro-432-pelagos-build.sh'
set -u
PELAGOS="${PELAGOS:-pelagos}"
D=$(mktemp -d /var/tmp/repro432.XXXX)   # /var/tmp = ext4 (NOT /tmp tmpfs)
cd "$D"

echo "=== identity (want uid 0, CapEff missing cap_sys_admin) ==="
id; grep CapEff /proc/self/status
echo "=== store backing fs ==="; stat -f -c '%T' /var/lib/pelagos
echo "=== pelagos ==="; "$PELAGOS" --version

# Valid dependency so cargo proceeds to resolve -> acquires the package-cache
# lock (flock on $CARGO_HOME/.package-cache) -> then fetches from the registry.
cat > Remfile <<'REMFILE'
FROM docker.io/library/rust:1-bookworm
RUN set -x; \
    echo "--- does .package-cache preexist in lower? ---"; \
    ls -la /usr/local/cargo/.package-cache 2>&1 || echo "(absent)"; \
    echo "--- touch (glibc open, adds O_LARGEFILE) then stat size ---"; \
    touch /usr/local/cargo/.package-cache 2>&1 && \
      stat -c 'PROBE size=%s ino=%i blocks=%b' /usr/local/cargo/.package-cache; \
    echo "--- now cargo (Rust std open, NO O_LARGEFILE) ---"; \
    cargo new /work/proj && cd /work/proj && \
    printf '%s\n' 'anyhow = "1"' >> Cargo.toml && \
    cargo build
REMFILE
echo "=== Remfile ==="; cat Remfile

echo "=== strace'd pelagos build --network host ==="
strace -f -qq -y -e trace=%file,%desc -o "$D/s.out" \
  "$PELAGOS" build --network host -t repro432:test "$D" >"$D/b.out" 2>"$D/b.err"
echo "pelagos build exit=$?"
echo "=== build stderr (last 30) ==="; tail -30 "$D/b.err"
echo "=== build stdout (last 10) ==="; tail -10 "$D/b.out"
echo "=== EOVERFLOW syscalls in strace ==="
grep -n "EOVERFLOW" "$D/s.out" | head -30 || echo "  (none)"
echo "=== package-cache / os-error-75 in build output ==="
grep -niE "package cache|error 75|value too large|EOVERFLOW" "$D/b.err" "$D/b.out" || echo "  (none)"
echo "=== flock/fcntl calls on .package-cache in strace ==="
grep -nE "package-cache|\.package_cache" "$D/s.out" | head -20 || echo "  (none)"
echo "DIR=$D"
