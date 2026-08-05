#!/usr/bin/env bash
# Watches the agent-coordinator blackboard for cluster-filed bug signals and
# runs a fully autonomous fix -> test -> release cycle for each, writing the
# result back to the blackboard for the k3s agent to install and verify.
#
# Runs as a systemd --user service (see pelagos-watch-coordinator.service).
# Uses `-e moved_to` because git commits are atomic renames; close_write
# never fires on the target path (see memory: feedback_inotify_moved_to).
set -euo pipefail

COORD_DIR="$HOME/Projects/agent-coordinator"
STATE_DIR="$COORD_DIR/state"
CLUSTER_JSON="$STATE_DIR/cluster.json"
PELAGOS_JSON="$STATE_DIR/pelagos.json"
PELAGOS_REPO="$HOME/Projects/pelagos"
LOG_DIR="$HOME/.local/state/pelagos-watch"
LOG_FILE="$LOG_DIR/watch.log"
LOCK_FILE="$LOG_DIR/watch.lock"

mkdir -p "$LOG_DIR"

log() {
  printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >>"$LOG_FILE"
}

process_signal() {
  flock -n "$LOCK_FILE" "$0" run_once || log "skip: another cycle already running"
}

run_once() {
  local signal
  signal=$(jq -r '.signals_out.to_pelagos // empty' "$CLUSTER_JSON")

  if [ "$signal" != "new-cluster-bugs" ]; then
    log "signal='$signal' (nothing to do)"
    return 0
  fi

  local issues
  issues=$(jq -c '.signals_out.new_issue_numbers // []' "$CLUSTER_JSON")
  local issue_count
  issue_count=$(jq 'length' <<<"$issues")
  if [ "$issue_count" -eq 0 ]; then
    log "signal=new-cluster-bugs but new_issue_numbers is empty, skipping"
    return 0
  fi

  log "picked up new-cluster-bugs: issues=$issues"

  # Clear the inbound signal immediately so we don't reprocess it, and so
  # concurrent filings accumulate into the *next* signal instead of being lost.
  (
    cd "$COORD_DIR"
    jq '.signals_out.to_pelagos = null | .signals_out.new_issue_numbers = []' "$CLUSTER_JSON" >"$CLUSTER_JSON.tmp"
    mv "$CLUSTER_JSON.tmp" "$CLUSTER_JSON"
    git add state/cluster.json
    git commit -m "chore: pelagos-agent claiming issues $issues for autonomous fix cycle"
    git push
  ) >>"$LOG_FILE" 2>&1

  local prompt
  prompt="Autonomous release cycle triggered by the agent-coordinator blackboard.

The cluster agent (k3s-experiments) filed these GitHub issues against this repo: $issues

Follow CLAUDE.md's \"Once more into the breach!\" macro for each issue, fully
autonomously, with these adjustments to that macro (per explicit user direction,
2026-08-05):
- Skip interactive plan approval, but still WRITE the plan as a comment on the
  issue before implementing, so the reasoning is preserved.
- After CI goes green and ci-merge-release completes the merge/tag/release,
  auto-merge without waiting for further human review.
- If multiple issues are filed, batch them into one release cycle where it
  makes sense (one version bump covering all fixes), same as recent releases.

When the release is fully published (GitHub release + Cargo.toml bump +
crates.io/AUR best-effort per existing known issues), update
$PELAGOS_JSON per CLAUDE.md's 'After a release' section:
- latest_release = the new version
- release_status = released
- release_timestamp = actual release time
- signals_out.to_k3s = upgrade-and-test
- signals_out.target_version = the new version
- signals_out.issues_to_validate = $issues
- updated_by = pelagos-agent
- updated_at = now
Commit and push agent-coordinator with that update as the final step.

Do not touch cluster.json beyond what was already cleared. Do not act on
cluster state beyond filing/releasing (no kubectl, no cluster SSH) — that
remains the k3s agent's job."

  log "invoking headless claude for issues=$issues"
  (
    cd "$PELAGOS_REPO"
    claude -p "$prompt" --output-format text
  ) >>"$LOG_FILE" 2>&1
  log "headless cycle finished for issues=$issues"
}

case "${1:-watch}" in
  run_once)
    run_once
    ;;
  watch)
    log "watcher starting, watching $STATE_DIR"
    # Fire once on startup in case a signal landed while we were down.
    process_signal
    while inotifywait -q -e moved_to,close_write "$STATE_DIR" >/dev/null; do
      process_signal
    done
    ;;
  *)
    echo "usage: $0 [watch|run_once]" >&2
    exit 1
    ;;
esac
