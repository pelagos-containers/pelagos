#!/usr/bin/env bash
# Build pelagos ON the cluster test node (ipc6) and install it locally, so we
# never copy multi-MB binaries from omen over a slow/remote uplink. Only git
# source deltas cross the network; the artifacts are built and stay on ipc6.
#
# Intended to run ON ipc6 (the 12-core build/test node), e.g.:
#   ssh -J ipc1 cb@192.168.88.57 'bash -s' < scripts/ipc6-deploy.sh -- <branch> [critest-focus]
# or, once the repo is cloned at ~/src/pelagos, from there:
#   bash scripts/ipc6-deploy.sh <branch> [critest-focus]
#
# Workflow it supports (the "cluster build" loop):
#   omen:  edit -> commit -> git push          (KB of source delta)
#   ipc6:  this script: git pull -> cargo build -> install -> restart -> critest
#
# Args:
#   $1  git branch (or ref) to build         (default: current checkout)
#   $2  optional critest --ginkgo.focus regex to run after install
#
# Requires (one-time): rustup toolchain + build-essential + a clone at
# ~/src/pelagos. See docs — this script assumes they exist.
set -euo pipefail

REPO="${PELAGOS_SRC:-$HOME/src/pelagos}"
SOCK="unix:///run/pelagos/cri.sock"
BRANCH="${1:-}"
FOCUS="${2:-}"

[ -d "$REPO/.git" ] || { echo "no pelagos clone at $REPO" >&2; exit 1; }
# shellcheck disable=SC1090
source "$HOME/.cargo/env"
cd "$REPO"

if [ -n "$BRANCH" ]; then
  echo "== fetch + checkout $BRANCH =="
  git fetch --all -q
  git checkout -q "$BRANCH"
  # Hard-match the remote so a force-push or rebase is honored cleanly.
  if git rev-parse --verify -q "origin/$BRANCH" >/dev/null; then
    git reset --hard -q "origin/$BRANCH"
  fi
fi
echo "HEAD=$(git rev-parse --short HEAD) ($(git rev-parse --abbrev-ref HEAD))"

echo "== build (release) =="
cargo build --release --bin pelagos -p pelagos-cri

echo "== install =="
sudo install -m0755 target/release/pelagos     /usr/local/bin/pelagos
sudo install -m0755 target/release/pelagos-cri /usr/local/bin/pelagos-cri
pelagos --version || true

echo "== restart pelagos-cri =="
sudo systemctl restart pelagos-cri
sleep 2
echo "pelagos-cri: $(systemctl is-active pelagos-cri)"

if [ -n "$FOCUS" ]; then
  echo "== critest --ginkgo.focus='$FOCUS' =="
  sudo timeout 180 critest --runtime-endpoint "$SOCK" --image-endpoint "$SOCK" \
    --ginkgo.focus="$FOCUS" 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' \
    | grep -E 'Ran [0-9]|Passed|Failed|SUCCESS|FAIL!|\[FAIL\]' | tail -8
fi
