export const meta = {
  name: 'ci-merge-release',
  description: 'Poll PR CI until green, merge, bump Cargo.toml patch version, tag, release',
  phases: [
    { title: 'Poll CI' },
    { title: 'Merge' },
    { title: 'Release' },
  ],
}

// args: { pr: <number> }
// Polls CI every 90s until all checks pass, then merges, bumps Cargo.toml patch,
// commits, tags, and pushes — triggering the release workflow.

const STATUS_SCHEMA = {
  type: 'object',
  properties: {
    status: { type: 'string', enum: ['pass', 'fail', 'pending'] },
    failed: { type: 'array', items: { type: 'string' } },
  },
  required: ['status', 'failed'],
}

const MERGE_SCHEMA = {
  type: 'object',
  properties: {
    merged: { type: 'boolean' },
    sha: { type: 'string' },
    error: { type: 'string' },
  },
  required: ['merged'],
}

const RELEASE_SCHEMA = {
  type: 'object',
  properties: {
    version: { type: 'string' },
    tag: { type: 'string' },
    pushed: { type: 'boolean' },
    error: { type: 'string' },
  },
  required: ['version', 'tag', 'pushed'],
}

const pr = args && args.pr
if (!pr) {
  return { error: 'args.pr is required — call with Workflow({ name: "ci-merge-release", args: { pr: 123 } })' }
}

const REPO = '/home/cb/Projects/pelagos'
const POLL_INTERVAL_S = 120
const MAX_POLLS = 150  // 150 × 120s = 5 hours

// ── Poll CI ────────────────────────────────────────────────────────────────────
// One agent call per iteration, not two: the check and the wait-before-next-check
// both happen inside the SAME call (the agent runs `gh pr checks`, and if still
// pending, sleeps via its own Bash tool before returning "pending"). An earlier
// version spawned a second, separate agent() call whose entire job was `sleep 90`
// — using a full LLM agent invocation as a sleep timer is pure waste, and on a
// CI run that hangs (a wedged GitHub Actions runner, not a real failure) it burns
// hundreds of agent calls and millions of tokens for what should cost a handful
// of cheap status checks. See pelagos#(ci-merge-release token waste, 2026-08-19):
// a hung e2e-tests runner drove this loop through all 300 iterations under the
// old design, costing 600 agent calls / ~23.7M tokens before timing out.
phase('Poll CI')
log(`Polling CI for PR #${pr} (max ${MAX_POLLS} attempts, ${POLL_INTERVAL_S}s apart, 1 agent call/attempt)`)

let passed = false
for (let i = 1; i <= MAX_POLLS; i++) {
  const result = await agent(
    `Check GitHub PR #${pr} CI status. Working directory: ${REPO}

Run: gh pr checks ${pr}

Classify the output:
- ALL checks are "pass" or "skipped" → status="pass", failed=[]
- ANY check is "fail"                → status="fail", failed=[list of failing check names]
- ANY check is "pending"/"in_progress" (and none failed) → status="pending", failed=[]

If (and only if) the classification is "pending", run \`sleep ${POLL_INTERVAL_S}\` yourself
(via your Bash tool) BEFORE returning, so the next poll is naturally spaced out — do not
skip this, and do not spawn any other agent/task to do it. If "pass" or "fail", return
immediately without sleeping.

Return JSON.`,
    { schema: STATUS_SCHEMA, label: `poll #${i}` }
  )

  if (result.status === 'pass') {
    passed = true
    log(`All checks passed on attempt #${i}`)
    break
  } else if (result.status === 'fail') {
    log(`CI failed on attempt #${i}: ${result.failed.join(', ')}`)
    return { error: 'CI checks failed', failed: result.failed, pr }
  } else {
    log(`Attempt #${i}: pending (agent slept ${POLL_INTERVAL_S}s before returning)`)
  }
}

if (!passed) {
  return { error: `Timed out after ${MAX_POLLS} polls (≈${(MAX_POLLS * POLL_INTERVAL_S / 3600).toFixed(1)}h) — check whether CI is genuinely still running or a runner is hung`, pr }
}

// ── Merge ──────────────────────────────────────────────────────────────────────
phase('Merge')
const mergeResult = await agent(
  `Merge GitHub PR #${pr} with a merge commit (NOT squash, NOT rebase).
Working directory: ${REPO}

Run: gh pr merge ${pr} --merge

Then confirm: gh pr view ${pr} --json state,mergeCommit --jq '{state:.state, sha:(.mergeCommit.oid // "")}'

Return merged=true and the sha if state is "MERGED", else merged=false and an error string.`,
  { schema: MERGE_SCHEMA }
)

if (!mergeResult.merged) {
  return { error: mergeResult.error || 'Merge failed', pr }
}
log(`Merged PR #${pr} at ${mergeResult.sha}`)

// ── Release ────────────────────────────────────────────────────────────────────
phase('Release')
const releaseResult = await agent(
  `Bump Cargo.toml patch version and push a release tag.
Working directory: ${REPO}

Steps (run exactly in this order):
1. git checkout main
2. git pull origin main
3. Read the current version: grep '^version' Cargo.toml | head -1
4. Parse it as MAJOR.MINOR.PATCH and compute NEW_PATCH = PATCH + 1
5. NEW_VERSION = MAJOR.MINOR.NEW_PATCH
6. Apply: sed -i "s/^version = \\".*\\"/version = \\"$NEW_VERSION\\"/" Cargo.toml
7. Verify: grep '^version' Cargo.toml
8. git add Cargo.toml
9. git commit -m "chore(release): v$NEW_VERSION"
10. git tag v$NEW_VERSION
11. git push origin main
12. git push origin v$NEW_VERSION

Return JSON: { version: "MAJOR.MINOR.NEW_PATCH", tag: "vMAJOR.MINOR.NEW_PATCH", pushed: true/false, error: "..." }`,
  { schema: RELEASE_SCHEMA }
)

if (!releaseResult.pushed) {
  return { error: releaseResult.error || 'Release push failed', pr, ...releaseResult }
}

log(`Tagged and pushed ${releaseResult.tag} — release workflow triggered`)
return {
  merged: true,
  pr,
  mergedAt: mergeResult.sha,
  version: releaseResult.version,
  tag: releaseResult.tag,
}
