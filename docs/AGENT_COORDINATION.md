# Agent Coordination: Pelagos ↔ k3s-experiments

This doc is kept **identical in both repos**
(`pelagos/docs/AGENT_COORDINATION.md` and
`k3s-experiments/docs/AGENT_COORDINATION.md`) so either side can be debugged
without needing to open the other repo. If you edit one copy, copy the change
to the other.

Two agents coordinate through a shared blackboard so that bugs found in the
cluster get fixed and released without a human relaying state between them:

- **pelagos-agent** — this repo. Builds and releases the Pelagos container
  runtime.
- **k3s-agent** — `~/Projects/k3s-experiments/`. Runs Pelagos as the cluster's
  CRI, files bugs when it finds them, validates releases.

## The blackboard

Location: `~/Projects/agent-coordinator/state/` (separate git repo, both
agents commit/push to it directly).

### `cluster.json` — k3s-agent writes, pelagos-agent reads

```jsonc
{
  "pelagos_installed": "0.65.74",       // version currently running cluster-wide
  "nodes": ["ipc4", ...],
  "last_test_run": "2026-08-05T14:28:30Z",
  "last_test_result": {
    "version_tested": "0.65.74",
    "pass": true,
    "issues_found": [],
    "notes": "...",
    "affected_node": null
  },
  "signals_out": {
    "to_pelagos": "new-cluster-bugs",    // or "cluster-bug-fix-confirmed" or null
    "new_issue_numbers": [501, 502]      // only meaningful when to_pelagos == "new-cluster-bugs"
  },
  "updated_by": "k3s-agent",
  "updated_at": "2026-08-05T14:28:30Z"
}
```

`signals_out.to_pelagos` values:
- `"new-cluster-bugs"` — new GitHub issues filed against pelagos, listed in
  `new_issue_numbers`. Only this value triggers a pelagos-side work cycle.
- `"cluster-bug-fix-confirmed"` — a prior fix was validated on the cluster.
  Currently a no-op on the pelagos side (see Non-goals below).
- `null` — nothing pending.

### `pelagos.json` — pelagos-agent writes, k3s-agent reads

```jsonc
{
  "latest_release": "0.65.74",
  "release_status": "released",
  "release_timestamp": "2026-08-05T04:54:24Z",
  "in_progress": { "branch": null, "issues": [] },
  "signals_out": {
    "to_k3s": "upgrade-and-test",        // or null
    "target_version": "0.65.74",
    "issues_to_validate": [494]
  },
  "updated_by": "pelagos-agent",
  "updated_at": "2026-08-05T13:56:58Z"
}
```

`signals_out.to_k3s == "upgrade-and-test"` tells the k3s-agent a new release
is ready at `target_version`, fixing `issues_to_validate`.

## How each side watches the blackboard

The two sides use **different mechanisms** — this is intentional, not
inconsistent:

| Side | Mechanism | Lifetime |
|------|-----------|----------|
| pelagos-agent | `systemd --user` service, `inotifywait` daemon | Persistent — survives reboot/logout, always running |
| k3s-agent | `/watch-pelagos-release` skill: session-bound `inotifywait` (background) + 30-min `ScheduleWakeup` fallback | Only while a Claude Code session on k3s-experiments is open and has run the skill |

If nobody has an interactive k3s-experiments session open with the watcher
skill running, `to_k3s` signals sit unread until the next session starts and
manually checks the blackboard (per `k3s-experiments/CLAUDE.md`'s "On
startup — read the blackboard" step). This is a known asymmetry — see
Follow-ups below.

### pelagos side: `pelagos-watch-coordinator.service`

- Script: `pelagos/scripts/watch-coordinator.sh`
- Unit: `pelagos/scripts/pelagos-watch-coordinator.service`, installed at
  `~/.config/systemd/user/pelagos-watch-coordinator.service`
- Watches: `~/Projects/agent-coordinator/state/` via
  `inotifywait -e moved_to,close_write`. Uses `moved_to` because `git commit`
  writes a temp file and renames it into place — `close_write` fires on the
  temp path, not the target, so a watch using only `close_write` never
  triggers on git-committed files.
- On `cluster.json`'s `signals_out.to_pelagos == "new-cluster-bugs"`:
  1. Claims the issues — clears the signal, commits agent-coordinator
     immediately (so concurrent filings land in the *next* signal instead of
     racing). No push — see "no remote" note above.
  2. Records the current latest GitHub release tag (`gh release list`), then
     creates an **isolated git worktree** under
     `~/.local/state/pelagos-watch/worktrees/` from a fresh `origin/main`,
     and runs `claude -p "<prompt>"` headlessly **inside that worktree**,
     instructing it to follow `CLAUDE.md`'s **"Once more into the breach!"**
     macro per issue — plan posted as an issue comment (no interactive
     approval wait), implement, test, PR, auto-merge on green CI via the
     existing `ci-merge-release` workflow. The worktree is removed after the
     cycle finishes either way.
  3. **Deterministically**, not left to the headless agent: re-checks the
     latest release tag. If it changed, writes `pelagos.json` back
     (`to_k3s: upgrade-and-test`, `target_version`, `issues_to_validate`)
     itself, verified against `gh release view`, and commits
     agent-coordinator. If it didn't change, logs that no release was
     detected rather than writing a stale/fake entry.
- `flock` on `~/.local/state/pelagos-watch/watch.lock` prevents overlapping
  cycles (a single `git commit` can fire `moved_to` more than once).
- Logs: `~/.local/state/pelagos-watch/watch.log`.

**Both of the above were real bugs, not hypothetical, found running this
system live:**
- **No git remote on agent-coordinator.** The script originally ran `git
  push` after claiming issue #496 — it failed (`fatal: No configured push
  destination`), and because the script used `set -e`, that failure aborted
  the whole cycle *after* the signal was already cleared and committed,
  silently swallowing the issue. `agent-coordinator` is local-only, shared
  by filesystem path between both agents on this host — commit is
  sufficient, push was never valid here.
- **Shared checkout + non-deterministic final step.** Issue #507's headless
  cycle ran in the same `~/Projects/pelagos` checkout an interactive session
  was using at the same time (a real collision, not just a risk), and its
  release (v0.65.80) shipped successfully but the cycle never reached its
  final "write the coordinator board" instruction — its last logged output
  was just `"Continuing to wait for the release workflow to finish; no
  action needed right now"`. A single-shot `claude -p` invocation has no
  durable path to resume after a backgrounded Workflow's completion
  notification the way an interactive session does. Fixed by moving the
  coordinator write out of the LLM's free-form last step and into the
  script itself (deterministic, verified against actual GitHub state), and
  by giving every cycle its own worktree.

### k3s side: `/watch-pelagos-release` skill

- Skill: `~/.claude/skills/watch-pelagos-release/SKILL.md`
- Helper: `k3s-experiments/scripts/watch-pelagos-release.sh` — checks GitHub
  releases directly for issue mentions (independent of the blackboard;
  useful for manually confirming a release shipped a specific fix).
- Invoked manually with `/watch-pelagos-release`, or self-schedules: if no
  signal yet, it starts a background `inotifywait` on `pelagos.json` plus a
  30-minute `ScheduleWakeup` fallback, then re-invokes itself when either
  fires.
- On `to_k3s == "upgrade-and-test"` with a version newer than
  `cluster_json.pelagos_installed`: clears the signal, upgrades all nodes,
  restarts Cilium, runs full connectivity + NetworkPolicy validation, writes
  `cluster.json` results back (including filing new issues + setting
  `to_pelagos: "new-cluster-bugs"` if anything failed).

## End-to-end sequence

```
k3s-agent finds a bug
  → files GitHub issue(s), labels cluster-origin
  → writes cluster.json: signals_out.to_pelagos = "new-cluster-bugs", new_issue_numbers = [...]
  → commit + push agent-coordinator

pelagos-watch-coordinator.service (inotify fires on moved_to)
  → claims issues, clears to_pelagos, commit + push
  → headless claude -p: implement fix(es), test, PR, auto-merge, release
  → writes pelagos.json: signals_out.to_k3s = "upgrade-and-test", target_version, issues_to_validate
  → commit + push agent-coordinator

k3s-agent (if a session is watching, or on next startup check)
  → clears to_k3s
  → upgrades cluster to target_version
  → validates (connectivity, NetworkPolicy enforcement, etc.)
  → writes cluster.json: pass/fail, issues_found, signals_out.to_pelagos = "cluster-bug-fix-confirmed" or "new-cluster-bugs"
  → commit + push agent-coordinator
```

## Operating the pelagos-side service

```bash
systemctl --user status pelagos-watch-coordinator.service
systemctl --user stop pelagos-watch-coordinator.service     # pause the loop
systemctl --user start pelagos-watch-coordinator.service    # resume
systemctl --user restart pelagos-watch-coordinator.service  # e.g. after editing the script
tail -f ~/.local/state/pelagos-watch/watch.log
```

To manually force a cycle without waiting for a filesystem event (e.g. to
test after editing the script):

```bash
~/Projects/pelagos/scripts/watch-coordinator.sh run_once
```

## Debugging

| Symptom | Check |
|---------|-------|
| Service not running | `systemctl --user status pelagos-watch-coordinator.service`; if `enabled` but not `active`, check `journalctl --user -u pelagos-watch-coordinator.service` |
| Signal set but nothing happened | `tail ~/.local/state/pelagos-watch/watch.log` — look for `skip: another cycle already running` (stale lock) or the signal being logged as ignored |
| Watcher missed the write entirely | Confirm the write actually used `moved_to` (a `git commit`/rename) not an in-place edit inside the watched dir — `close_write,moved_to` is set as a belt-and-suspenders, but a plain unlink+recreate outside git may not fire either |
| Stuck lock | `rm ~/.local/state/pelagos-watch/watch.lock` if a prior headless run crashed mid-cycle without releasing it (check the log first to see why it crashed) |
| Headless `claude -p` cycle failing | Check `~/.local/state/pelagos-watch/watch.log` for its full transcript output; confirm `pelagos/.claude/settings.local.json` still grants the tool access the cycle needs (Bash, Edit, Write, Workflow) |
| k3s side never picks up `to_k3s` | No persistent watcher exists on that side yet (see Follow-ups) — manually run `/watch-pelagos-release` in a k3s-experiments session, or start a new session (it checks on startup per that repo's CLAUDE.md) |
| Coordinator board not updated despite a release shipping | Check `watch.log` for `"no new release detected"` (means the script's own pre/post `gh release list` comparison found nothing new — investigate why separately) vs a crash before reaching that step. The write is deterministic and script-owned now (not delegated to the headless agent's self-report), so if a release genuinely shipped and the board is still stale, that's a bug in the script's release-tag comparison, not a headless-agent-forgot-a-step issue anymore. |
| Stale worktree left behind | `git -C ~/Projects/pelagos worktree list` — a crashed cycle can leave one under `~/.local/state/pelagos-watch/worktrees/`; remove with `git -C ~/Projects/pelagos worktree remove <path> --force` |

## Explicit non-goals / scope boundaries

- The pelagos-side watcher never touches cluster state — no `kubectl`, no
  SSH to cluster nodes. That stays the k3s-agent's job.
- The pelagos-side watcher does not currently act on
  `"cluster-bug-fix-confirmed"` (e.g. auto-closing the validated issue) —
  left as a manual step for now.
- After a release, the pelagos-agent does not proceed to touch the cluster
  itself, even indirectly — it stops at writing `pelagos.json`.

## Follow-ups (not yet built)

- k3s side has no persistent daemon equivalent to
  `pelagos-watch-coordinator.service` — it only watches while an interactive
  session has invoked `/watch-pelagos-release`. A `systemd --user` service
  mirroring the pelagos side would close this gap, but hasn't been requested
  yet. Until then, a release can sit unacknowledged if no k3s-experiments
  session is open.
