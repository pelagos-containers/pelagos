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
const MAX_POLLS = 300  // 300 × 90s ≈ 7.5 hours

// ── Poll CI ────────────────────────────────────────────────────────────────────
phase('Poll CI')
log(`Polling CI for PR #${pr} (max ${MAX_POLLS} attempts, 90s apart)`)

let passed = false
for (let i = 1; i <= MAX_POLLS; i++) {
  const result = await agent(
    `Check GitHub PR #${pr} CI status. Working directory: ${REPO}

Run: gh pr checks ${pr}

Classify the output:
- ALL checks are "pass" or "skipped" → status="pass", failed=[]
- ANY check is "fail"                → status="fail", failed=[list of failing check names]
- ANY check is "pending"/"in_progress" (and none failed) → status="pending", failed=[]

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
    log(`Attempt #${i}: pending — sleeping 90s`)
    await agent(`Run exactly: sleep 90 && echo done`, { label: 'sleep 90s' })
  }
}

if (!passed) {
  return { error: `Timed out after ${MAX_POLLS} polls (≈7.5h)`, pr }
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
