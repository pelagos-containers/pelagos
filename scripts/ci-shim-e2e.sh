#!/usr/bin/env bash
# Real-shim end-to-end for CI (#635): a one-node k3s whose runtime is
# pelagos-cri with REAL Kata + gVisor shims, then the kubelet-driven e2e
# scripts. Run as root on a machine with /dev/kvm (GitHub ubuntu-latest).
#
#   sudo -E bash scripts/ci-shim-e2e.sh [PELAGOS_BIN_DIR]
#
# PELAGOS_BIN_DIR holds `pelagos` and `pelagos-cri` (default target/debug).
# Everything it installs is pinned and checksummed. It is NOT meant for a
# developer machine or a cluster node: it starts k3s, writes /opt/kata,
# /usr/local/bin/runsc and CNI state. Logs are dumped on failure.
set -euo pipefail

BIN="${1:-target/debug}"
KATA_VER=4.4.0
KATA_SHA256=ccbc0cbc591089d0c57012b78da64b23ef43e0bac0f869dc7bfebe8588c33986
GVISOR_REL=20261116
RUNSC_SHA512=bd4281b101a1c406790b1196401f8b76b9b49146068ec0cfefd416c2a57b9c472478cc646d28d0894f0d515c7464c4072b2007227e65dbb1a69de16ea2325c2a
RUNSC_SHIM_SHA512=832440dc5f3066ce4af7d9372c35af922490e39672d68cd3d232be8271876e50cdd53d06cae2748d271e4604c3cc31781ffdae8af97366b2569a4dc03c8953bc
K3S_VER='v1.35.5+k3s1'
K3S_SHA256=267d18da7b3c837d82283f0588fb9031a8a6ff3c0dac772c260c40852ce515f6
PAUSE=registry.k8s.io/pause:3.10
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

log "KVM"
ls -l /dev/kvm; test -w /dev/kvm

log "Kata $KATA_VER -> /opt/kata"
curl -sfL -o "$W/kata.tar.zst" "https://github.com/kata-containers/kata-containers/releases/download/$KATA_VER/kata-static-$KATA_VER-amd64.tar.zst"
echo "$KATA_SHA256  $W/kata.tar.zst" | sha256sum -c -
tar --zstd -xf "$W/kata.tar.zst" -C / ; ls /opt/kata/runtime-rs/bin/containerd-shim-kata-v2
rm -f "$W/kata.tar.zst"

log "gVisor $GVISOR_REL"
for f in runsc containerd-shim-runsc-v1; do
  curl -sfL -o "/usr/local/bin/$f" "https://storage.googleapis.com/gvisor/releases/release/$GVISOR_REL/x86_64/$f"; chmod +x "/usr/local/bin/$f"
done
echo "$RUNSC_SHA512  /usr/local/bin/runsc" | sha512sum -c -
echo "$RUNSC_SHIM_SHA512  /usr/local/bin/containerd-shim-runsc-v1" | sha512sum -c -

log "k3s $K3S_VER"
curl -sfL -o /usr/local/bin/k3s "https://github.com/k3s-io/k3s/releases/download/${K3S_VER//+/%2B}/k3s"
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
bash scripts/pelagos-cri-shim-handlers.sh add \
  kata=/opt/kata/runtime-rs/bin/containerd-shim-kata-v2 \
  gvisor=/usr/local/bin/containerd-shim-runsc-v1 --no-restart
systemctl start pelagos-cri
for _ in $(seq 1 30); do test -S /run/pelagos/cri.sock && break; sleep 1; done
test -S /run/pelagos/cri.sock

log "k3s server on pelagos-cri (stock flannel CNI — exercises #611)"
systemd-run --unit=k3s --property=Restart=no /usr/local/bin/k3s server \
  --container-runtime-endpoint unix:///run/pelagos/cri.sock \
  --node-name "$NODE" --write-kubeconfig-mode 644 \
  --disable traefik --disable servicelb --disable metrics-server --disable coredns --disable local-storage \
  --disable-network-policy --disable-cloud-controller \
  --kubelet-arg=pod-infra-container-image=$PAUSE >/dev/null
for _ in $(seq 1 120); do
  /usr/local/bin/k3s kubectl get node "$NODE" 2>/dev/null | grep -q " Ready" && break; sleep 2
done
/usr/local/bin/k3s kubectl get node "$NODE" -o wide
ls /var/lib/rancher/k3s/agent/etc/cni/net.d/ || true

log "kubelet e2e: Kata + gVisor"
NODE=$NODE KUBECTL="/usr/local/bin/k3s kubectl" RUNTIMES="kata gvisor" TIMEOUT=300 \
  bash scripts/test-shim-runtimeclass-e2e.sh

log "kubectl port-forward serves many connections (#617)"
NODE=$NODE KUBECTL="/usr/local/bin/k3s kubectl" bash scripts/test-portforward-multi.sh
log "ALL SHIM E2E PASSED"
