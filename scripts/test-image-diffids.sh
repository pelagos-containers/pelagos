#!/usr/bin/env bash
# #632 regression: every layer's gunzip sha256 must equal the image config's
# rootfs.diff_ids[i] in what `pelagos image save` / `image push` export, for
# (a) a pulled-and-retagged image and (b) an image built FROM a pulled base.
# Stock containerd rejects an image where they differ ("wrong diff id").
#
# Usage: PELAGOS=/path/to/pelagos [BASE=alpine:3.20] [REGISTRY=host:port] \
#        scripts/test-image-diffids.sh
# REGISTRY (optional, plain-HTTP): also push both images and verify the
# blobs the registry holds.
set -uo pipefail
PELAGOS="${PELAGOS:-pelagos}"; BASE="${BASE:-alpine:3.20}"
CTX="$(cd "$(dirname "$0")" && pwd)/diffid-test-context"
W="$(mktemp -d)"; trap 'rm -rf "$W"; $PELAGOS image rm difftest-retag:1 difftest-built:1 >/dev/null 2>&1 || true' EXIT
PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL: $1"; }

# check_tar FILE: verify an OCI layout tar. Prints "ok N" or the mismatches.
check_tar() { python3 -I - "$1" <<'PY'
import sys, tarfile, json, gzip, hashlib
t = tarfile.open(sys.argv[1])
def blob(d): return t.extractfile("blobs/sha256/" + d.split(":")[1]).read()
idx = json.loads(t.extractfile("index.json").read())
man = json.loads(blob(idx["manifests"][0]["digest"]))
cfg = json.loads(blob(man["config"]["digest"]))
ids = cfg["rootfs"]["diff_ids"]
bad = []
if len(ids) != len(man["layers"]): bad.append("layer count %d != diff_ids %d" % (len(man["layers"]), len(ids)))
for i, l in enumerate(man["layers"]):
    raw = gzip.decompress(blob(l["digest"]))
    got = "sha256:" + hashlib.sha256(raw).hexdigest()
    if i < len(ids) and got != ids[i]: bad.append("layer %d: content %s != diff_id %s" % (i, got[:19], ids[i][:19]))
print("ok %d" % len(ids) if not bad else "; ".join(bad))
PY
}

echo "=== image diff_id consistency ($BASE) ==="
$PELAGOS image pull "$BASE" >/dev/null 2>&1 || { fail "cannot pull $BASE"; exit 1; }
$PELAGOS image tag "$BASE" difftest-retag:1 >/dev/null 2>&1 || fail "tag"
cp -r "$CTX" "$W/ctx" && sed -i "s|^FROM .*|FROM $BASE|" "$W/ctx/Remfile" \
  && $PELAGOS build -t difftest-built:1 "$W/ctx" >/dev/null 2>&1 || { fail "build failed"; exit 1; }

for img in difftest-retag:1 difftest-built:1; do
  $PELAGOS image save "$img" -o "$W/$img.tar" >/dev/null 2>&1 || { fail "$img: save failed"; continue; }
  r="$(check_tar "$W/$img.tar")"
  case "$r" in ok*) pass "$img: all layers match their diff_ids (${r#ok } layers)";; *) fail "$img: $r";; esac
done

if [ -n "${REGISTRY:-}" ]; then
  for img in difftest-retag:1 difftest-built:1; do
    $PELAGOS image push --insecure "$img" --dest "$REGISTRY/difftest/${img%%:*}:1" >/dev/null 2>&1 || { fail "$img: push failed"; continue; }
    python3 -I - "$REGISTRY" "difftest/${img%%:*}" <<'PY' && pass "$img: registry blobs match diff_ids" || fail "$img: registry blobs inconsistent"
import sys, json, gzip, hashlib, urllib.request as u
reg, repo = sys.argv[1:3]
def get(p, accept=None):
    r = u.Request("http://%s/v2/%s/%s" % (reg, repo, p))
    if accept: r.add_header("Accept", accept)
    return u.urlopen(r).read()
man = json.loads(get("manifests/1", "application/vnd.oci.image.manifest.v1+json"))
cfg = json.loads(get("blobs/" + man["config"]["digest"]))
for i, l in enumerate(man["layers"]):
    raw = gzip.decompress(get("blobs/" + l["digest"]))
    assert "sha256:" + hashlib.sha256(raw).hexdigest() == cfg["rootfs"]["diff_ids"][i], i
PY
  done
fi
echo ""; echo "Results: $PASS passed, $FAIL failed"; [ "$FAIL" -eq 0 ]
