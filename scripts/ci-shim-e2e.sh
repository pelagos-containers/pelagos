#!/usr/bin/env bash
# Real-shim end-to-end for CI (#635): a one-node k3s whose runtime is
# pelagos-cri with REAL Kata + gVisor shims, then the kubelet-driven e2e
# scripts. Run as root.
#
#   sudo -E bash scripts/ci-shim-e2e.sh [PELAGOS_BIN_DIR]
#
# PELAGOS_BIN_DIR holds `pelagos` and `pelagos-cri` (default target/debug).
# x86_64 and aarch64. RUNTIMES (default: "kata gvisor" if /dev/kvm is writable,
# else "gvisor") picks what is installed and tested: hosted arm runners have no
# /dev/kvm, so they run gVisor only; Kata arm64 is run by hand on a KVM host.
# Everything it installs is pinned and checksummed. It is NOT meant for a
# developer machine or a cluster node: it starts k3s, writes /opt/kata,
# /usr/local/bin/runsc and CNI state. Logs are dumped on failure.
set -euo pipefail

BIN="${1:-target/debug}"
KATA_VER=4.4.0
GVISOR_REL=20261116.0
K3S_VER='v1.35.5+k3s1'
case "$(uname -m)" in
  x86_64)
    KATA_ARCH=amd64; GVISOR_ARCH=x86_64; K3S_ASSET=k3s
    KATA_SHA256=ccbc0cbc591089d0c57012b78da64b23ef43e0bac0f869dc7bfebe8588c33986
    GVISOR_SHA512=a34261504066142b6c1cc4d1813bb2d092761675416e350edc98c4cc66469b63b6f41acdeed09681b6c717c00e80e437f5f2f9c8d37705b8a9260632867c2b7d
    K3S_SHA256=267d18da7b3c837d82283f0588fb9031a8a6ff3c0dac772c260c40852ce515f6 ;;
  aarch64)
    KATA_ARCH=arm64; GVISOR_ARCH=aarch64; K3S_ASSET=k3s-arm64
    KATA_SHA256=9dd4407d4b756858fe9ce33322a605f5a932743839a8ec47108e8513dcc3aaa3
    GVISOR_SHA512=4f95f25e09e856dd868e05a1ea5ee394bbe0ad0843ece141d161ac592385ee61eb654575c3b71fc6eab974eaa51028ed1a736f4821b4d462c3470b1e6616d5f3
    K3S_SHA256=aced90bea6a4e025abef4cf6db7d8cc316a7d20a2b963f809852a37f2b9dbc03 ;;
  *) echo "unsupported architecture $(uname -m)" >&2; exit 2 ;;
esac
if [ -z "${RUNTIMES:-}" ]; then
  if [ -w /dev/kvm ]; then RUNTIMES="kata gvisor"; else RUNTIMES="gvisor"; fi
fi
has_rt() { case " $RUNTIMES " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
W="${RUNNER_TEMP:-/var/tmp}/shim-e2e"; mkdir -p "$W"
NODE=ci-shim
export KUBECONFIG=/etc/rancher/k3s/k3s.yaml

log() { echo "=== $*"; }
dump() {
  echo "##### FAILURE DIAGNOSTICS"
  journalctl -u pelagos-cri --no-pager 2>/dev/null | tail -150 || true
  journalctl -u k3s --no-pager 2>/dev/null | tail -80 || true
  /usr/local/bin/k3s kubectl get pods -A -o wide 2>&1 | tail -30 || true
  /usr/local/bin/k3s kubectl -n shim-e2e describe pods 2>&1 | tail -60 || true
  systemctl list-units 'pelagos-shim-*' --no-legend 2>&1 | head || true
}
trap 'rc=$?; [ $rc -ne 0 ] && dump; exit $rc' EXIT

log "runtimes: $RUNTIMES ($(uname -m))"
if has_rt kata; then
  log "KVM"
  ls -l /dev/kvm; test -w /dev/kvm

  log "Kata $KATA_VER -> /opt/kata"
  curl -sfL -o "$W/kata.tar.zst" "https://github.com/kata-containers/kata-containers/releases/download/$KATA_VER/kata-static-$KATA_VER-$KATA_ARCH.tar.zst"
  echo "$KATA_SHA256  $W/kata.tar.zst" | sha256sum -c -
  tar --zstd -xf "$W/kata.tar.zst" -C / ; ls /opt/kata/runtime-rs/bin/containerd-shim-kata-v2
  rm -f "$W/kata.tar.zst"
fi

log "gVisor $GVISOR_REL (runsc + containerd-shim-runsc-v1 + gvisor-bin/) -> /usr/local/bin"
curl -sfL -o "$W/gvisor.tar.zstd" "https://storage.googleapis.com/gvisor/releases/release/$GVISOR_REL/$GVISOR_ARCH/gvisor.tar.zstd"
echo "$GVISOR_SHA512  $W/gvisor.tar.zstd" | sha512sum -c -
tar --zstd -xf "$W/gvisor.tar.zstd" -C /usr/local/bin
rm -f "$W/gvisor.tar.zstd"; runsc --version | head -1

log "k3s $K3S_VER"
curl -sfL -o /usr/local/bin/k3s "https://github.com/k3s-io/k3s/releases/download/${K3S_VER//+/%2B}/$K3S_ASSET"
echo "$K3S_SHA256  /usr/local/bin/k3s" | sha256sum -c -; chmod +x /usr/local/bin/k3s

log "pelagos binaries"
install -m0755 "$BIN/pelagos" /usr/local/bin/pelagos
install -m0755 "$BIN/pelagos-cri" /usr/local/bin/pelagos-cri
/usr/local/bin/pelagos --version

log "pelagos-cri with shim handlers (via the shipped helper + cri.env)"
install -d /etc/pelagos /run/pelagos
cat > /etc/systemd/system/pelagos-cri.service <<UNIT
[Unit]
Description=Pelagos CRI (CI)
[Service]
ExecStart=/usr/local/bin/pelagos-cri --pelagos-bin /usr/local/bin/pelagos --socket /run/pelagos/cri.sock \$PELAGOS_CRI_ARGS
EnvironmentFile=-/etc/pelagos/cri.env
Environment=RUST_LOG=info
LimitNOFILE=1048576
Restart=no
UNIT
systemctl daemon-reload
HANDLERS=""
has_rt kata && HANDLERS="$HANDLERS kata=/opt/kata/runtime-rs/bin/containerd-shim-kata-v2"
has_rt gvisor && HANDLERS="$HANDLERS gvisor=/usr/local/bin/containerd-shim-runsc-v1"
bash scripts/pelagos-cri-shim-handlers.sh add $HANDLERS --no-restart
systemctl start pelagos-cri
for _ in $(seq 1 30); do test -S /run/pelagos/cri.sock && break; sleep 1; done
test -S /run/pelagos/cri.sock

log "k3s server on pelagos-cri (stock flannel CNI — exercises #611)"
systemd-run --unit=k3s --property=Restart=no /usr/local/bin/k3s server \
  --container-runtime-endpoint unix:///run/pelagos/cri.sock \
  --node-name "$NODE" --write-kubeconfig-mode 644 \
  --disable traefik --disable servicelb --disable metrics-server --disable coredns --disable local-storage \
  --disable-network-policy --disable-cloud-controller >/dev/null
for _ in $(seq 1 120); do
  /usr/local/bin/k3s kubectl get node "$NODE" 2>/dev/null | grep -q " Ready" && break; sleep 2
done
/usr/local/bin/k3s kubectl get node "$NODE" -o wide
ls /var/lib/rancher/k3s/agent/etc/cni/net.d/ || true

log "kubelet e2e: $RUNTIMES"
NODE=$NODE KUBECTL="/usr/local/bin/k3s kubectl" RUNTIMES="$RUNTIMES" TIMEOUT=300 \
  bash scripts/test-shim-runtimeclass-e2e.sh

log "kubectl port-forward serves many connections (#617)"
NODE=$NODE KUBECTL="/usr/local/bin/k3s kubectl" bash scripts/test-portforward-multi.sh
if has_rt kata; then
  log "Kata pod recovers when its shim or VMM is killed (#641, #642)"
  NODE=$NODE KUBECTL="/usr/local/bin/k3s kubectl" \
    CRICTL="/usr/local/bin/k3s crictl -r unix:///run/pelagos/cri.sock" \
    bash scripts/test-shim-failure-recovery.sh
fi
log "ALL SHIM E2E PASSED"
