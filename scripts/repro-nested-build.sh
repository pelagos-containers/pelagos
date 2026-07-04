#!/bin/bash
# Reproduce #426: `pelagos build` RUN step EINVALs when nested inside a pelagos
# container (the in-cluster / in-pod build case).
#
# Strategy: run an OUTER image-based container (image rootfs == overlayfs, just
# like a pod), bind-mounting the pelagos binary, the host layer/state store, and
# a trivial build context. Inside it, invoke `pelagos build`. The inner RUN step
# then performs a nested overlay mount — the operation we suspect fails.
set -x
PELAGOS=${PELAGOS:-$PWD/target/debug/pelagos}
OUTER_IMAGE=${OUTER_IMAGE:-alpine:latest}

CTX="$PWD/scripts/nested-build-context"
mkdir -p "$CTX"
cat > "$CTX/Remfile" <<'EOF'
FROM alpine:latest
RUN echo NESTED_RUN_STEP_OK > /marker && cat /marker
EOF

# Make sure the base image the INNER build needs is present in the shared store.
sudo "$PELAGOS" image pull "$OUTER_IMAGE" 2>/dev/null
sudo "$PELAGOS" image pull alpine:latest 2>/dev/null

# Run the inner build inside the outer container.
#  --bind pelagos binary            → inner runtime
#  --bind /var/lib/pelagos          → shared layer + state store
#  --bind context                   → Remfile
sudo "$PELAGOS" run --rm \
  --bind "$PELAGOS:/pelagos" \
  --bind /var/lib/pelagos:/var/lib/pelagos \
  --bind "$CTX:/ctx" \
  "$OUTER_IMAGE" \
  /pelagos build -t nested-repro:test --network none /ctx
echo "OUTER EXIT: $?"
