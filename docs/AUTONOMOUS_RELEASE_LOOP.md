# Autonomous Release Loop

Closes the loop with the k3s cluster agent: it files GitHub issues via the
agent-coordinator blackboard, this loop implements + tests + releases the
fix, and writes the result back so the cluster agent can install and verify.

## Trigger

`scripts/watch-coordinator.sh` runs `inotifywait -e moved_to,close_write` on
`~/Projects/agent-coordinator/state/`. `moved_to` (not `close_write` alone) is
required because `git commit` writes a temp file and renames it into place —
`close_write` fires on the temp path, not the target (see memory:
`feedback_inotify_moved_to`).

On every filesystem event it checks `cluster.json`'s
`signals_out.to_pelagos`. Only `"new-cluster-bugs"` triggers a cycle; anything
else (including `null` or `"cluster-bug-fix-confirmed"`) is a no-op for this
loop.

## Cycle

1. Read `signals_out.new_issue_numbers`, clear the signal, commit + push
   agent-coordinator immediately (claims the work, so concurrent filings
   accumulate into the *next* signal instead of racing).
2. Invoke `claude -p` headlessly in the pelagos repo. The prompt tells it to
   follow CLAUDE.md's **"Once more into the breach!"** macro for each issue,
   with two standing adjustments (confirmed by user, 2026-08-05):
   - Skip interactive plan approval, but still post the plan as an issue
     comment before implementing, so the reasoning isn't lost.
   - Auto-merge on green CI via the existing `ci-merge-release` workflow —
     no human review gate.
3. On completion, the headless run itself updates
   `agent-coordinator/state/pelagos.json` (`signals_out.to_k3s:
   "upgrade-and-test"`, `target_version`, `issues_to_validate`) and pushes —
   this is the same "after a release" step CLAUDE.md already requires, just
   done unattended instead of relying on the interactive session to remember.

## Mechanism choice

Runs as a `systemd --user` service (`pelagos-watch-coordinator.service`), not
a cron poll or session-bound `/loop`: it needs to react immediately to
blackboard writes and survive independently of any interactive Claude Code
session. See `scripts/watch-coordinator.sh` for the implementation.

## Concurrency and logging

`flock` on `~/.local/state/pelagos-watch/watch.lock` prevents overlapping
cycles (git's rename dance can fire `moved_to` more than once per commit).
Logs go to `~/.local/state/pelagos-watch/watch.log`.

## Explicit non-goals

- Does not act on cluster state (no `kubectl`, no SSH to cluster nodes) — per
  memory `feedback_cluster_boundary`, that stays the k3s agent's job.
- Does not currently act on `"cluster-bug-fix-confirmed"` (e.g. auto-closing
  the validated issue) — left as a manual step for now.

## Operating it

```bash
systemctl --user status pelagos-watch-coordinator.service
systemctl --user stop pelagos-watch-coordinator.service     # pause the loop
tail -f ~/.local/state/pelagos-watch/watch.log
```
