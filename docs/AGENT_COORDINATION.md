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

## How writes happen — `bin/write-state.sh`, mandatory for both sides

**Never edit `state/*.json` directly and commit separately.** That leaves a
window between writing the file and running `git commit` where the other
agent can read your uncommitted change, act on it, and commit first — your
later commit then silently overwrites theirs. No error, no conflict marker,
just a lost update. This happened for real in a k3s-agent session on
2026-08-13: an edit sat uncommitted long enough for pelagos-agent's own live
session to read it, correctly claim the issue it named, and commit — and the
k3s-agent's later commit landed on top and re-raised a signal that had
already been legitimately claimed, misreading pelagos-agent's action as data
corruption.

Both sides MUST go through `agent-coordinator/bin/write-state.sh` for every
blackboard write:

```
~/Projects/agent-coordinator/bin/write-state.sh <pelagos.json|cluster.json> '<jq filter>' "<commit message>"
```

It `flock`s `agent-coordinator/.blackboard.lock` (30s wait, then fails
loudly rather than silently proceeding unlocked), re-reads the file **fresh
after acquiring the lock** (not whatever you read earlier — that copy may
already be stale), applies the jq filter, validates the result is real JSON
before touching the target file, and commits. A bad filter or a no-op change
exits cleanly without writing or committing anything.

The lock only protects you if **both** sides use it — a bare `Edit` on one
side still bypasses it entirely, so this only works if it's the sole write
path for both agents, no exceptions.

### Single-writer boundary, and the exception to it

The rule per file is `cluster.json` — k3s-agent writes, pelagos-agent reads;
`pelagos.json` — pelagos-agent writes, k3s-agent reads. The one deliberate
exception: pelagos-agent's claim step (below) clears
`cluster.json.signals_out.to_pelagos` — a cross-write into the other side's
file. That's intentional (it's how a claim is visible without k3s-agent
having to poll), but it means `write-state.sh`'s lock is genuinely load
-bearing for that one field, not just defense-in-depth. Don't add further
cross-file writes without the same lock discipline.

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
     latest release tag, retrying every 60s for up to 20 minutes (the
     release workflow's lint+unit+integration gate takes ~15min and can
     still be running after the headless invocation itself has exited — see
     incident 3 below). If a new tag appears, writes `pelagos.json` back
     (`to_k3s: upgrade-and-test`, `target_version`, `issues_to_validate`)
     itself, verified against `gh release view`'s `publishedAt` (not
     `createdAt` — the draft is created before the gate finishes), and
     commits agent-coordinator. If nothing appears after 20 minutes, logs
     that no release was detected rather than writing a stale/fake entry.
  4. The headless invocation runs with `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS=0`
     — without it, the harness kills any background task the headless agent
     starts (including its own `ci-merge-release` Workflow poll) after a
     hard 600s ceiling, causing the invocation to exit before the release
     it triggered has actually finished (see incident 3).
- `flock` on `~/.local/state/pelagos-watch/watch.lock` prevents overlapping
  cycles (a single `git commit` can fire `moved_to` more than once).
- Logs: `~/.local/state/pelagos-watch/watch.log`.

**All three of the following were real bugs, not hypothetical, found running
this system live:**
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
- **600s background-task kill ceiling racing the release workflow.** Issue
  #509/#510's cycle merged its PR and launched `ci-merge-release`'s poll for
  the release workflow — but the harness kills a headless `claude -p`
  invocation's background tasks after 600s by default, so that poll got
  killed and the invocation exited having merged+tagged (v0.65.81) but
  *before* the release workflow (lint+unit+integration tests, ~15min) had
  actually finished. The script's coordinator-write check (at the time, a
  single shot immediately after the headless process exited) ran in the
  ~2-minute gap before the release actually published, found nothing, and
  skipped the write — even though the release completed successfully
  shortly after. Caught by an interactive session's own `inotifywait` watch
  on the blackboard (set up for a different reason — the user asked to "be
  on the lookout for new coordinator information") noticing the board had
  gone stale relative to `gh release list`. Fixed two ways: the headless
  invocation now sets `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS=0` so its own
  Workflow poll isn't killed early, and the script's post-check is now a
  20-minute retry loop instead of a single shot, as defense in depth on top
  of that fix rather than a replacement for it.

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
| Coordinator board not updated despite a release shipping | Check `watch.log` for `"no new release detected after 20min of polling"` — the write is a deterministic, script-owned 20-minute retry loop against `gh release list`/`gh release view`, not delegated to the headless agent's self-report. If this fires despite a real release existing, either the release took longer than 20 minutes (rare — the gate is normally ~15min) or something's wrong with the retry loop itself; recover manually the same way as any other coordinator-write gap (write `pelagos.json` by hand using `gh release view <tag> --json publishedAt`). |
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
