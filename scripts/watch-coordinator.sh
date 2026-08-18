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
PELAGOS_REMOTE="pelagos-containers/pelagos"
LOG_DIR="$HOME/.local/state/pelagos-watch"
LOG_FILE="$LOG_DIR/watch.log"
LOCK_FILE="$LOG_DIR/watch.lock"
WORKTREE_BASE="$LOG_DIR/worktrees"

mkdir -p "$LOG_DIR" "$WORKTREE_BASE"

log() {
  printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >>"$LOG_FILE"
}

process_signal() {
  flock -n "$LOCK_FILE" "$0" run_once || log "skip: another cycle already running"
}

run_once() {
  local signal
  signal=$(jq -r '.signals_out.to_pelagos // empty' "$CLUSTER_JSON")

  # Closed signal vocabulary — see docs/AGENT_COORDINATION.md. Do not add a
  # case here without adding the value to that doc's table in the same
  # change: `retest-failed` and `priority-request` were both used informally
  # for weeks before being formalized, and were silently dropped by this
  # exact-string check the entire time (indistinguishable in the log from
  # genuine idle state) — see #535.
  case "$signal" in
    new-cluster-bugs | retest-failed | priority-request)
      ;; # actionable — falls through to the cycle below
    cluster-bug-fix-confirmed)
      local note
      note=$(jq -r '.signals_out.note // "(no note)"' "$CLUSTER_JSON")
      log "signal='cluster-bug-fix-confirmed' (FYI, no work cycle) note: $note"
      "$COORD_DIR/bin/write-state.sh" cluster.json '.signals_out.to_pelagos = null' \
        "chore: pelagos-agent acknowledges cluster-bug-fix-confirmed" >>"$LOG_FILE" 2>&1 \
        || log "warning: failed to clear cluster-bug-fix-confirmed signal (will retry on next event)"
      return 0
      ;;
    "")
      log "signal='' (nothing to do)"
      return 0
      ;;
    *)
      log "WARNING: signal='$signal' is not in the closed set (null, new-cluster-bugs, retest-failed, priority-request, cluster-bug-fix-confirmed — see docs/AGENT_COORDINATION.md). Ignoring but NOT clearing it, so this is visible until fixed at the source rather than silently indistinguishable from idle."
      return 0
      ;;
  esac

  local issues
  issues=$(jq -c '.signals_out.new_issue_numbers // []' "$CLUSTER_JSON")
  local issue_count
  issue_count=$(jq 'length' <<<"$issues")
  if [ "$issue_count" -eq 0 ]; then
    log "signal=$signal but new_issue_numbers is empty, skipping"
    return 0
  fi
  local note
  note=$(jq -r '.signals_out.note // empty' "$CLUSTER_JSON")

  log "picked up $signal: issues=$issues"

  # Clear the inbound signal and record watcher_status (both writes go
  # through write-state.sh, which flocks agent-coordinator/.blackboard.lock
  # — the same lock the other agent's writes go through — so this cross-file
  # claim can't race a concurrent write the way a raw jq+git-commit could.
  # See docs/AGENT_COORDINATION.md's "How writes happen".
  #
  # agent-coordinator has NO git remote (confirmed: `git remote -v` is empty)
  # — it's a local-only repo both agents share by filesystem path on this
  # host, not something to push. Commit only — write-state.sh already knows
  # this and doesn't push.
  local now_claim
  now_claim=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  # This write is load-bearing: if it fails, `set -e` must abort here, before
  # any work happens, so the signal stays set and gets retried — matching
  # the #496 postmortem (never clear a signal and then risk aborting before
  # the work it represents actually happens).
  "$COORD_DIR/bin/write-state.sh" cluster.json \
    '.signals_out.to_pelagos = null | .signals_out.new_issue_numbers = []' \
    "chore: pelagos-agent claiming issues $issues for autonomous fix cycle" >>"$LOG_FILE" 2>&1

  # This one is visibility-only (watcher_status), not load-bearing — the
  # signal is already claimed above, so failing here must NOT abort the
  # cycle via set -e (that would be the #496 pattern: claimed but never
  # done). Best-effort: log and continue.
  "$COORD_DIR/bin/write-state.sh" pelagos.json \
    ".watcher_status = {active: true, issues: $issues, signal: \"$signal\", started_at: \"$now_claim\"}" \
    "chore: pelagos-agent watcher status: active on issues $issues (signal=$signal)" >>"$LOG_FILE" 2>&1 \
    || log "warning: failed to write watcher_status active=true (non-fatal, continuing cycle for issues=$issues)"

  # Record the current latest release BEFORE the cycle runs, so we can
  # deterministically detect whether a new one landed afterward — see the
  # "Deterministic coordinator-board write" comment below for why this
  # doesn't rely on the headless agent remembering to do it itself.
  local release_before
  release_before=$(gh release list --repo "$PELAGOS_REMOTE" --limit 1 --json tagName --jq '.[0].tagName // ""' 2>>"$LOG_FILE" || echo "")

  # Isolated worktree, not the shared checkout this same machine may have an
  # interactive Claude Code session working in at the same time. Sharing a
  # checkout is a real hazard, not a hypothetical one: the first headless
  # cycle run under this script (issue #507) happened to run concurrently
  # with an interactive session in the same ~/Projects/pelagos checkout, and
  # `git status` in that session showed the headless cycle's uncommitted
  # changes. A worktree gives every cycle its own working directory and
  # index, checked out from a fresh `origin/main`, while still sharing the
  # same .git object store (cheap — no full clone).
  git -C "$PELAGOS_REPO" fetch origin main >>"$LOG_FILE" 2>&1
  local worktree_dir
  worktree_dir="$WORKTREE_BASE/wt-$(date -u +%Y%m%dT%H%M%SZ)-$$"
  git -C "$PELAGOS_REPO" worktree add "$worktree_dir" origin/main --detach >>"$LOG_FILE" 2>&1
  log "created worktree $worktree_dir for issues=$issues"

  local signal_context=""
  case "$signal" in
    retest-failed)
      signal_context="This is a RETEST-FAILED signal: a previously-released fix for these
issue(s) did not hold when the k3s-agent retested it. Re-investigate from
scratch — don't assume the earlier fix's diagnosis was correct.

"
      ;;
    priority-request)
      signal_context="This is a PRIORITY-REQUEST signal — the k3s-agent flagged this as
urgent, out-of-band attention needed (e.g. blocking a time-sensitive
cluster operation). Treat it as higher priority than a routine cycle.

"
      ;;
  esac
  if [ -n "$note" ]; then
    signal_context="${signal_context}k3s-agent's note: $note

"
  fi

  local prompt
  prompt="Autonomous release cycle triggered by the agent-coordinator blackboard.

The cluster agent (k3s-experiments) filed these GitHub issues against this repo: $issues

${signal_context}You are working in an isolated git worktree at $worktree_dir, checked out from
origin/main — NOT the primary ~/Projects/pelagos checkout, which may have an
interactive session using it concurrently. Do all git operations (branch,
commit, push) from within $worktree_dir. Pushing branches/tags to origin and
opening PRs via gh is fine and expected — those are shared remote operations,
not local-checkout state.

Follow CLAUDE.md's \"Once more into the breach!\" macro for each issue, fully
autonomously, with these adjustments to that macro (per explicit user direction,
2026-08-05 and 2026-08-06):
- Step 2 (create a worktree) is ALREADY DONE — this script created
  $worktree_dir for you. Do not create another worktree; just \`cd\` there
  (already done, you're running from it) and \`git checkout -b\` your feature
  branch directly. Step 9 (remove the worktree) is also this script's job,
  not yours — do not run 'git worktree remove' yourself.
- Skip interactive plan approval, but still WRITE the plan as a comment on the
  issue before implementing, so the reasoning is preserved.
- After CI goes green and ci-merge-release completes the merge/tag/release,
  auto-merge without waiting for further human review.
- If multiple issues are filed, batch them into one release cycle where it
  makes sense (one version bump covering all fixes), same as recent releases.
- CLAUDE.md's macro step 5.5 (build+smoke-test directly on target hardware
  before opening the PR, for spark-0d93/aarch64 or the ipc x86_64 cluster
  issues) applies to you exactly as it does to an interactive session — you
  are not exempt from it just because this cycle is unattended. SSH access
  to spark-0d93 and the in-cluster build infra are both already available;
  use them. Do not treat a green GitHub Actions CI run as sufficient proof
  a hardware-specific fix actually works — CI runs on generic GitHub-hosted
  runners and cannot see host-specific kernel/driver/overlay-fs behavior.

Do NOT touch agent-coordinator or pelagos.json yourself — the watcher script
handles the post-release coordinator-board write deterministically after this
cycle finishes (a prior cycle's own attempt at this final step didn't
reliably happen, so it's no longer this agent's responsibility).

Do not touch cluster.json beyond what was already cleared. Do not act on
cluster state beyond filing/releasing (no kubectl, no cluster SSH) — that
remains the k3s agent's job."

  log "invoking headless claude for issues=$issues"
  (
    cd "$worktree_dir"
    # Without this, the harness kills any background task (including this
    # cycle's own ci-merge-release Workflow poll, which can take 15-20min
    # for CI) after a 600s ceiling and the headless invocation exits early —
    # hit directly on issue #509/#510's cycle: its own release-workflow
    # monitor got killed, it exited having merged+tagged but before the
    # release actually finished publishing, and this script's (then
    # single-shot) post-check ran in the ~2min gap before the release
    # workflow completed, finding nothing and skipping the coordinator
    # write. The retry loop below is defense in depth on top of this fix,
    # not a replacement for it.
    CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS=0 claude -p "$prompt" --output-format text
  ) >>"$LOG_FILE" 2>&1
  log "headless cycle finished for issues=$issues"

  git -C "$PELAGOS_REPO" worktree remove "$worktree_dir" --force >>"$LOG_FILE" 2>&1 \
    || log "warning: failed to remove worktree $worktree_dir (may need manual cleanup)"

  # Deterministic coordinator-board write. Not delegated to the headless
  # agent's own judgment of "when am I done" — issue #507's cycle merged,
  # tagged, and released v0.65.80 successfully, but its last logged output
  # was just "Continuing to wait for the release workflow to finish; no
  # action needed right now" — it never reached its final instruction to
  # write pelagos.json. A single-shot `claude -p` invocation has no durable
  # path to resume after a backgrounded Workflow's completion notification
  # the way an interactive session does, so treat the coordinator write as
  # this script's job, verified against actual GitHub state, not the
  # headless agent's self-report.
  #
  # Retry, not a single check: issue #509/#510's cycle showed the release
  # workflow can still be mid-flight (lint+unit+integration tests, ~15min)
  # even after the headless invocation itself has exited. Poll for up to
  # 20 minutes rather than giving up on the first miss.
  local release_after=""
  local attempt
  for attempt in $(seq 1 20); do
    release_after=$(gh release list --repo "$PELAGOS_REMOTE" --limit 1 --json tagName --jq '.[0].tagName // ""' 2>>"$LOG_FILE" || echo "")
    if [ -n "$release_after" ] && [ "$release_after" != "$release_before" ]; then
      break
    fi
    release_after=""
    if [ "$attempt" -lt 20 ]; then
      sleep 60
    fi
  done

  # Both branches below clear watcher_status.active regardless of whether a
  # release resulted — "active" means "a cycle is in flight", not "a cycle
  # succeeded"; success/failure lives in release_status and the issue
  # tracker, not here. Values interpolated into the jq filter (version,
  # published_at, now, issues) are all machine-generated (git tag, GitHub
  # API ISO8601 timestamp, jq -c array) rather than free-form text, so this
  # follows the same low-risk interpolation pattern already used for the
  # claim-step write above rather than raw jq --arg (write-state.sh's
  # interface is a single filter string, no --arg passthrough).
  if [ -n "$release_after" ]; then
    local version="${release_after#v}"
    local published_at
    published_at=$(gh release view "$release_after" --repo "$PELAGOS_REMOTE" --json publishedAt --jq '.publishedAt' 2>>"$LOG_FILE")
    if "$COORD_DIR/bin/write-state.sh" pelagos.json \
      ".latest_release = \"$version\"
       | .release_status = \"released\"
       | .release_timestamp = \"$published_at\"
       | .signals_out.to_k3s = \"upgrade-and-test\"
       | .signals_out.target_version = \"$version\"
       | .signals_out.issues_to_validate = $issues
       | .watcher_status.active = false" \
      "chore: pelagos-agent post-release state update $release_after (issues $issues)" >>"$LOG_FILE" 2>&1
    then
      log "deterministically wrote coordinator board for $release_after (issues=$issues)"
    else
      log "warning: failed to write post-release coordinator board for $release_after (issues=$issues) — the release itself already succeeded, only this board write failed; recover manually per docs/AGENT_COORDINATION.md"
    fi
  else
    "$COORD_DIR/bin/write-state.sh" pelagos.json '.watcher_status.active = false' \
      "chore: pelagos-agent watcher status: cycle finished, no release detected (issues $issues)" >>"$LOG_FILE" 2>&1 \
      || log "warning: failed to clear watcher_status.active for issues=$issues (non-fatal)"
    log "no new release detected after 20min of polling for issues=$issues — NOT writing coordinator board (check watch.log for the cycle's own output; CI may have failed or nothing shippable was found)"
  fi
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
