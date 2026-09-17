# Repository refresh and historical replay diagnosis — 2026-09-17

Implementation `3b65868` was built, installed and restarted before this check.
The local checkouts and fetched origin views for `agi-backend-rust`, `mobile`
and `agents` were refreshed. All three local indexes match their checkout
revision and working-tree fingerprint. Origin views use separate derived
projects. No checkout, branch or working file was reset or updated.

The workspace architecture guide was regenerated into canonical SQLite
version 3. The browser reports **Current with the workspace**. The phone-task
flow reports **Sources unchanged** and exposes source anchors across the three
repositories. Generated onboarding sections still require review; regeneration
does not turn generated prose into validated evidence.

## A refresh isolation bug, and its repair

The CLI's old `index --origin` implementation indexed a fetched worktree under
the local project's identity. The browser/watcher path already used a separate
origin project. Running the old CLI during this refresh overwrote the local
graph and falsely quarantined six source-bound memories: five candidates and
one validated lesson.

The CLI now uses the daemon's existing origin-refresh path, resolving a relative
path at the client before dispatch. JSON output is machine-readable. Combining
origin refresh with changed-path or Graphify inputs is rejected explicitly.

After restoring the correct local indexes, all original source-anchor digests
for those six memories matched again. A WAL-safe backup was taken before repair.
The new `memory restore-drift` operation requires the exact latest quarantine
event, the specific symbol-drift cause, and every original source digest. It
restores active status without promoting epistemic state, raising confidence,
or extending validity. It rejects stale events, unrelated quarantine causes,
missing/changed anchors, archived records and replayed requests.

All six records were repaired through the daemon. A subsequent origin refresh
preserved their states and confidence. Older, unrelated suspect memories were
left unchanged. The regression tests cover isolation, client-relative paths,
pure JSON and guarded recovery for both candidate and validated records.

## Why the historical replay remains 1/6

The unchanged replay asks whether each historical lesson is actually delivered
as memory by the production context compiler. It does not measure general code
search, repository indexing coverage or the architecture guide.

| Probe | Current reason |
|---|---|
| Guide flags | Saved candidate with a pending review; not yet validated |
| Agents Cloud Run pipeline | No matching saved lesson |
| Production database pooler | No matching saved lesson |
| Production traffic pinned to a revision | Validated, active and delivered |
| Request logs completed after their start timestamp | No matching saved lesson |
| Contract suite thread limit | Older candidate quarantined for symbol drift; needs re-verification |

None of the five misses is explained by an eligible matching lesson being
retrieved and then dropped for budget. The historical capture and validation
gap remains. Refreshing source indexes cannot reconstruct production logs or
independent evidence that was never recorded. No missing incident was promoted
from its mention in a prompt, and no review was accepted merely to raise a score.

The database has 50 memories: 43 active candidates, one archived candidate,
three suspect candidates, two active validated memories and one archived
validated memory. The aggregate conversion metric counts the archived validated
record; three validated records does not mean three injectable memories.

## Retrieval and verification limits

A 4,000-token local flow bundle compiled to 3,991 estimated tokens, disclosing
nine omitted records and eight coverage/freshness caveats. It compiles existing
local records; it does not call a model. Source inspection in the browser
confirmed the conditional guide-retrieval branch.

Evidence-pack smoke checks returned relevant guide sources for the backend,
guide routes for agents, and phone-execution documentation for mobile. However,
the natural-language backend question comparing two guide flags returned
unrelated symbol matches; the shorter `guide` query found the relevant module.
These are smoke checks, not a question-answering accuracy benchmark. The product
flow and its source anchors provide a more direct starting point for that topic.

All 266 Rust tests, formatting and Clippy with warnings denied passed.
Independent code review found no remaining actionable issue after the relative
path correction. Live `engram doctor --json` passed, including database integrity.
The paired injection regression is reported separately; this refresh does not
establish prospective learning or onboarding gains.

Aggregate verification and local artifact hashes are in
[verification.json](2026-09-17-repository-refresh/verification.json).
The unchanged six-case replay and capture measurements are in
[capture-replay.json](2026-09-17-repository-refresh/capture-replay.json).
Private source excerpts, guide prose and raw live-state snapshots remain local.
