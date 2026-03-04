#!/usr/bin/env bash
# Test the `pelagos build` feature end-to-end.
#
# Must run as root (use -E to preserve rustup/cargo environment):
#   sudo -E scripts/test-build.sh
set -uo pipefail

PASS=0
FAIL=0
SKIP=0
BINARY="./target/debug/pelagos"
TMPDIR=""

pass() { PASS=$((PASS+1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL: $1"; }
skip() { SKIP=$((SKIP+1)); echo "  SKIP: $1"; }

cleanup() {
    if [ -n "$TMPDIR" ] && [ -d "$TMPDIR" ]; then
        rm -rf "$TMPDIR"
    fi
    # Remove test images created during the run.
    $BINARY image rm "test-build:latest" 2>/dev/null || true
    $BINARY image rm "test-copy:latest" 2>/dev/null || true
    $BINARY image rm "test-env:latest" 2>/dev/null || true
    $BINARY image rm "test-workdir:latest" 2>/dev/null || true
    $BINARY image rm "test-multi:latest" 2>/dev/null || true
    $BINARY image rm "test-runfail:latest" 2>/dev/null || true
    $BINARY image rm "test-chmod:latest" 2>/dev/null || true
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Prerequisites
# ---------------------------------------------------------------------------

echo "=== Prerequisites ==="

if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: must run as root (sudo -E scripts/test-build.sh)"
    exit 1
fi

if [ ! -x "$BINARY" ]; then
    echo "Binary not found at $BINARY — building..."
    cargo build 2>&1
    if [ ! -x "$BINARY" ]; then
        echo "ERROR: cargo build failed"
        exit 1
    fi
fi

# Ensure alpine is pulled.
if ! $BINARY image ls 2>/dev/null | grep -q alpine; then
    echo "Pulling alpine:latest..."
    $BINARY image pull alpine
fi

TMPDIR=$(mktemp -d /tmp/pelagos-test-build.XXXXXX)
echo "Using temp dir: $TMPDIR"
echo

# ---------------------------------------------------------------------------
# Test 1: Simple build with RUN
# ---------------------------------------------------------------------------

echo "=== Test 1: Build with RUN instruction ==="

cat > "$TMPDIR/Remfile" <<'EOF'
FROM alpine:latest
RUN echo build-ok > /tmp/build-marker
CMD ["/bin/sh", "-c", "cat /tmp/build-marker"]
EOF

if $BINARY build -t test-build:latest -f "$TMPDIR/Remfile" "$TMPDIR" 2>&1; then
    pass "build completed"
else
    fail "build failed"
fi

# Verify the image was saved.
if $BINARY image ls 2>/dev/null | grep -q "test-build"; then
    pass "image listed in image ls"
else
    fail "image not found in image ls"
fi

# Run the built image and check output.
OUTPUT=$($BINARY run test-build:latest 2>/dev/null)
if echo "$OUTPUT" | grep -q "build-ok"; then
    pass "run output contains build-ok"
else
    fail "run output missing build-ok (got: $OUTPUT)"
fi

echo

# ---------------------------------------------------------------------------
# Test 2: Build with COPY
# ---------------------------------------------------------------------------

echo "=== Test 2: Build with COPY instruction ==="

mkdir -p "$TMPDIR/copy-ctx"
echo "hello-from-copy" > "$TMPDIR/copy-ctx/data.txt"

cat > "$TMPDIR/copy-ctx/Remfile" <<'EOF'
FROM alpine:latest
COPY data.txt /srv/data.txt
CMD ["/bin/sh", "-c", "cat /srv/data.txt"]
EOF

if $BINARY build -t test-copy:latest "$TMPDIR/copy-ctx" 2>&1; then
    pass "copy build completed"
else
    fail "copy build failed"
fi

OUTPUT=$($BINARY run test-copy:latest 2>/dev/null)
if echo "$OUTPUT" | grep -q "hello-from-copy"; then
    pass "copied file accessible in image"
else
    fail "copied file not found (got: $OUTPUT)"
fi

echo

# ---------------------------------------------------------------------------
# Test 3: Build with ENV
# ---------------------------------------------------------------------------

echo "=== Test 3: Build with ENV instruction ==="

cat > "$TMPDIR/Remfile-env" <<'EOF'
FROM alpine:latest
ENV GREETING=hello-env
CMD ["/bin/sh", "-c", "echo $GREETING"]
EOF

if $BINARY build -t test-env:latest -f "$TMPDIR/Remfile-env" "$TMPDIR" 2>&1; then
    pass "env build completed"
else
    fail "env build failed"
fi

OUTPUT=$($BINARY run test-env:latest 2>/dev/null)
if echo "$OUTPUT" | grep -q "hello-env"; then
    pass "ENV variable visible in container"
else
    fail "ENV variable not found (got: $OUTPUT)"
fi

echo

# ---------------------------------------------------------------------------
# Test 4: Build with WORKDIR
# ---------------------------------------------------------------------------

echo "=== Test 4: Build with WORKDIR instruction ==="

cat > "$TMPDIR/Remfile-workdir" <<'EOF'
FROM alpine:latest
RUN mkdir -p /myapp
WORKDIR /myapp
CMD ["/bin/sh", "-c", "pwd"]
EOF

if $BINARY build -t test-workdir:latest -f "$TMPDIR/Remfile-workdir" "$TMPDIR" 2>&1; then
    pass "workdir build completed"
else
    fail "workdir build failed"
fi

OUTPUT=$($BINARY run test-workdir:latest 2>/dev/null)
if echo "$OUTPUT" | grep -q "/myapp"; then
    pass "WORKDIR applied correctly"
else
    fail "WORKDIR not applied (got: $OUTPUT)"
fi

echo

# ---------------------------------------------------------------------------
# Test 5: Multi-step build (RUN + COPY + ENV + CMD)
# ---------------------------------------------------------------------------

echo "=== Test 5: Multi-step build ==="

mkdir -p "$TMPDIR/multi-ctx"
echo "multi-data" > "$TMPDIR/multi-ctx/payload.txt"

cat > "$TMPDIR/multi-ctx/Remfile" <<'EOF'
FROM alpine:latest
RUN mkdir -p /app
COPY payload.txt /app/payload.txt
ENV APP_MODE=production
WORKDIR /app
CMD ["/bin/sh", "-c", "cat payload.txt && echo mode=$APP_MODE"]
EOF

if $BINARY build -t test-multi:latest "$TMPDIR/multi-ctx" 2>&1; then
    pass "multi-step build completed"
else
    fail "multi-step build failed"
fi

OUTPUT=$($BINARY run test-multi:latest 2>/dev/null)
if echo "$OUTPUT" | grep -q "multi-data"; then
    pass "multi-step: COPY payload present"
else
    fail "multi-step: COPY payload missing (got: $OUTPUT)"
fi
if echo "$OUTPUT" | grep -q "mode=production"; then
    pass "multi-step: ENV applied"
else
    fail "multi-step: ENV missing (got: $OUTPUT)"
fi

echo

# ---------------------------------------------------------------------------
# Test 6: RUN failure aborts the build
# ---------------------------------------------------------------------------

echo "=== Test 6: RUN failure aborts build ==="

cat > "$TMPDIR/Remfile-runfail" <<'EOF'
FROM alpine:latest
RUN exit 1
CMD ["/bin/sh", "-c", "echo should-not-exist"]
EOF

OUTPUT=$($BINARY build -t test-runfail:latest -f "$TMPDIR/Remfile-runfail" "$TMPDIR" 2>&1)
RC=$?
if [ "$RC" -ne 0 ]; then
    pass "build exits non-zero on RUN failure"
else
    fail "build should have failed but exited 0"
fi

if echo "$OUTPUT" | grep -qi "failed\|error"; then
    pass "error message present on RUN failure"
else
    fail "no error message on RUN failure (got: $OUTPUT)"
fi

# Image should NOT have been saved.
if $BINARY image ls 2>/dev/null | grep -q "test-runfail"; then
    fail "failed build should not save image"
else
    pass "no image saved after failed build"
fi

echo

# ---------------------------------------------------------------------------
# Test 7: Missing base image gives helpful error
# ---------------------------------------------------------------------------

echo "=== Test 7: Missing base image ==="

cat > "$TMPDIR/Remfile-noimage" <<'EOF'
FROM nonexistent-image-xyz:v99
CMD ["/bin/sh"]
EOF

OUTPUT=$($BINARY build -t test-noimage:latest -f "$TMPDIR/Remfile-noimage" "$TMPDIR" 2>&1)
RC=$?
if [ "$RC" -ne 0 ]; then
    pass "build fails for missing base image"
else
    fail "build should have failed for missing image"
fi

if echo "$OUTPUT" | grep -qi "not found\|pull"; then
    pass "error suggests pulling the image"
else
    fail "error should mention image not found (got: $OUTPUT)"
fi

echo

# ---------------------------------------------------------------------------
# Test 8: COPY path traversal outside context is rejected
# ---------------------------------------------------------------------------

echo "=== Test 8: COPY path traversal rejected ==="

# Create a file outside the context dir.
echo "secret" > "$TMPDIR/outside-secret.txt"
mkdir -p "$TMPDIR/traversal-ctx"

cat > "$TMPDIR/traversal-ctx/Remfile" <<'EOF'
FROM alpine:latest
COPY ../outside-secret.txt /stolen.txt
CMD ["/bin/sh"]
EOF

OUTPUT=$($BINARY build -t test-traversal:latest "$TMPDIR/traversal-ctx" 2>&1)
RC=$?
if [ "$RC" -ne 0 ]; then
    pass "build rejects path traversal COPY"
else
    fail "build should have rejected path traversal"
fi

echo

# ---------------------------------------------------------------------------
# Test 9: Invalid Remfile syntax is rejected
# ---------------------------------------------------------------------------

echo "=== Test 9: Invalid Remfile syntax ==="

cat > "$TMPDIR/Remfile-bad" <<'EOF'
FROM alpine:latest
FOOBAR not a real instruction
EOF

OUTPUT=$($BINARY build -t test-bad:latest -f "$TMPDIR/Remfile-bad" "$TMPDIR" 2>&1)
RC=$?
if [ "$RC" -ne 0 ]; then
    pass "build rejects unknown instruction"
else
    fail "build should have rejected unknown instruction"
fi

if echo "$OUTPUT" | grep -qi "unknown instruction\|parse error"; then
    pass "error identifies the bad instruction"
else
    fail "error should mention unknown instruction (got: $OUTPUT)"
fi

echo

# ---------------------------------------------------------------------------
# Test 10: Missing Remfile gives helpful error
# ---------------------------------------------------------------------------

echo "=== Test 10: Missing Remfile ==="

mkdir -p "$TMPDIR/empty-ctx"

OUTPUT=$($BINARY build -t test-missing:latest "$TMPDIR/empty-ctx" 2>&1)
RC=$?
if [ "$RC" -ne 0 ]; then
    pass "build fails when Remfile is missing"
else
    fail "build should fail when Remfile is missing"
fi

if echo "$OUTPUT" | grep -qi "not found\|Remfile"; then
    pass "error mentions missing Remfile"
else
    fail "error should mention Remfile not found (got: $OUTPUT)"
fi

echo

# ---------------------------------------------------------------------------
# Test 11: COPY + RUN chmod produces correct output
#
# Regression test for the overlayfs metacopy bug (Linux 6.x+).
# When metacopy=on (default on Linux 6+), a chmod in a RUN step writes only a
# metadata inode to the overlay upper directory; file data stays in the lower
# layer. Reading upper/ directly after container exit then returns zero bytes.
# Fix: metacopy=off in the overlay mount options in container.rs.
# ---------------------------------------------------------------------------

echo "=== Test 11: COPY script + RUN chmod produces correct output (metacopy regression) ==="

mkdir -p "$TMPDIR/chmod-ctx"
cat > "$TMPDIR/chmod-ctx/server.sh" <<'EOF'
#!/bin/sh
echo "hello-from-chmod-script"
EOF

cat > "$TMPDIR/chmod-ctx/Remfile" <<'EOF'
FROM alpine
COPY server.sh /usr/local/bin/server.sh
RUN chmod +x /usr/local/bin/server.sh
CMD ["/usr/local/bin/server.sh"]
EOF

if $BINARY build -t test-chmod:latest "$TMPDIR/chmod-ctx" 2>&1; then
    pass "chmod build completed"
else
    fail "chmod build failed"
fi

OUTPUT=$($BINARY run test-chmod:latest 2>/dev/null)
if echo "$OUTPUT" | grep -q "hello-from-chmod-script"; then
    pass "COPY+chmod: script output correct"
else
    fail "COPY+chmod: output missing or empty (got: '$OUTPUT'). \
Likely cause: overlayfs metacopy wrote a zero-byte upper inode for the chmod step. \
Fix: ensure metacopy=off is in container.rs overlay mount options."
fi

# Also verify the file is readable and not empty inside the container.
FILE_SIZE=$($BINARY run test-chmod:latest /bin/sh -c "wc -c < /usr/local/bin/server.sh" 2>/dev/null | tr -d ' ')
if [ "$FILE_SIZE" -gt 0 ] 2>/dev/null; then
    pass "COPY+chmod: file has non-zero size inside container ($FILE_SIZE bytes)"
else
    fail "COPY+chmod: file is empty or missing inside container (size: '$FILE_SIZE')"
fi

echo

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

echo "========================================"
echo "  PASS: $PASS   FAIL: $FAIL   SKIP: $SKIP"
echo "========================================"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
