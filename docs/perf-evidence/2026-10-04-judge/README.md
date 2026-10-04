# Background lesson judge reliability, 4 October 2026

Scope: the automatic lesson judge (`src/judge.rs`, `src/judge/`). This records the
measured failure modes in one live brain, their root causes, the changes, and a
before/after comparison. All numbers here are aggregates. The live database holds
proprietary claims and code; no claim text, path or identifier from it is committed.
A human spot-check sample of private claims was written only to the operator's
machine (`~/.engram/reports/judge-spot-check-2026-10-04.md`).

## Live audit before the change

Read-only queries of the default workspace database at about 12:05 UTC:

- 1,575 judgment rows for 144 reviews, with 1,575 distinct packet digests. Outcomes:
  11 accepted, 1,559 deferred, 4 stale, 1 runtime error. 135 reviews were pending.
- 87 rows invoked the model, for 13 distinct reviews: 123,708 input and 17,733 output
  tokens, mean 6.7 s. Only 11 produced an applicable decision (all acceptances).
  74 were `reject` verdicts with an empty `claim` that strict validation refused and
  recorded as "configuration error: Judge verdict has missing or oversized fields".
  They came from three reviews; one review was sent to the model 66 times. One answer
  nested `contradicts_memory_ids` inside a citation and failed parsing. One citation
  was not an exact excerpt.

### Root causes

| Symptom (deferrals) | Cause | Change |
| --- | --- | --- |
| Re-judging churn (every row a new digest) | The dedupe digest included the latest project scan head and diff fingerprint, index timestamps and scan revisions, related memories in recency order, the live HEAD and whichever alias was observed most recently. Any re-index, memory injection or unrelated commit created a new digest, and the old one never matched again. | The dedupe key binds the proposal, task and evidence, the gate result and the inference policy. Live source state is compared only to detect a change during inference. |
| "Source path must remain inside the registered checkout" (171 rows, 5 reviews) | Every touched path in these reviews was an absolute path inside the project's primary checkout. The check accepted only relative paths, and only the four most recently observed of 25 aliases were considered; the primary checkout was the 25th. | Paths resolve against every registered checkout, including linked worktrees, and explicitly linked repositories, after canonicalizing symlinked spellings. Other paths still block. |
| "Touched source file is unavailable" (52 rows, 20 reviews) | Approximate replay against today's filesystem: in 41 rows (17 reviews) the file was absent from the checkout the old code chose but present in another registered checkout; 10 rows (2 reviews) recorded annotated text rather than a path; 1 row's file is missing everywhere. | The task's checkout is chosen from absolute paths, the recorded revision, then where the files exist. A missing file, a permission refusal, a macOS privacy refusal and other I/O failures now have separate reasons. |
| "Source checkout unavailable" (22 rows) | The daemon could not read existing checkouts (macOS privacy context, fixed operationally). The old code also reported this when only the four most recent aliases were gone. | A checkout that exists but refuses access is reported as an access problem that `engram doctor` explains; "unavailable" now means no registered checkout exists. |
| "Evidence/Source task revision differs" | 53 of 92 recorded evidence revisions and 58 of 114 task commits are abbreviated hashes (7 to 12 digits). Exact comparison with the 40-digit live HEAD always failed. Today, 8 of the 32 reviews deferred this way have a checkout whose HEAD starts with the recorded hash, and 3 have a checkout exactly at it. | A recorded revision of at least seven hexadecimal digits matches a HEAD it prefixes, and a checkout at the recorded revision is preferred. |
| "Indexed source revision differs" | Index rows for unchanged files keep the scan that last rewrote them, so any later commit failed this check even when content digests matched. Re-indexing could not clear it. | Removed. The live file must still match its indexed content digest. |
| "Judge verdict has missing or oversized fields" (74 rows) | Unconstrained JSON output; the model left `claim` empty when rejecting. The rows were repeated by the churn above. | Requests carry a strict JSON schema. Malformed output is a runtime error with its own reason and bounded retries, not a "configuration error" deferral. |

A separate finding for the evidence-eligibility gate, which this change does not touch:
the `verified_supporting_evidence` view compares `evidence.commit_hash` with
`tasks.final_commit` as exact strings. Of 81 evidence rows with both values, 32 differ;
24 of those differ only by abbreviation, and the view excludes them. 14 pending reviews
have such evidence.

## Same-snapshot replay

Both versions swept one read-only copy of the database with prior judgments removed,
135 pending reviews, against the live checkouts. No model was called; packets that
passed every gate were counted and recorded as deferrals in the copy. "After" is this
branch before merging the other workstreams, whose evidence rule changes are not
included.

| Gate result | Before (4dd913a) | After |
| --- | ---: | ---: |
| No readable, independently verified evidence | 85 | 90 |
| Evidence revision differs from the live checkout | 9 | 15 |
| Source task revision differs from the live checkout | 1 | 13 |
| Touched file differs from indexed source (or no anchor, before) | 9 | 3 |
| Touched file has no indexed anchor | — | 3 |
| Indexed source revision differs | 8 | 0 |
| Source path must remain inside the registered checkout | 8 | 0 |
| Touched path is outside every registered checkout and linked repository | — | 3 |
| Touched source file is unavailable | 7 | 0 |
| Touched source file is missing (deleted, renamed or not a path) | — | 6 |
| Source checkout unavailable | 6 | 0 |
| Passed every gate (model would be called) | 2 | 2 |
| A second sweep of the unchanged copy | 0 | 0 |

The sweep is one deferral per review in both versions because the copy did not change
between passes. Live churn comes from changes over time; the regression test
`unchanged_reviews_are_not_judged_again_after_volatile_changes` reproduces it with a new
scan, re-written index rows, memory recency changes, a commit and a new worktree in each
round. It fails if live state is added back to the dedupe key.

The reasons now name the actual condition, but the number of reviews reaching the
model did not change in this snapshot. The three outside paths are files under the
coding agent's home directory and an unregistered repository; they remain blocked. The
five reviews that the containment check had blocked now stop at the evidence gate:
all of their evidence predates the latest indexed version of the touched files. More
reviews now reach a real checkout and are compared by revision; those 28 have no
registered checkout at the recorded revision. A full sweep spent about 19 s preparing
packets over 18 passes with either version.

## Structured output

One adversarial request to the local runtime (MTPLX 2.9.2, `engram-judge`,
Qwen3.5-4B 4-bit) asked for an extra top-level field, a field nested in a citation,
an empty claim and unknown IDs ([probe](structured-output-probe.json)):

- `json_object`: the answer added a top-level `confidence` field and an empty claim.
- `json_schema` with the verdict schema: the answer had exactly the five fields, a
  non-empty claim and no unknown IDs.

MTPLX enforces the schema with llguidance. It canonicalizes the schema with sorted keys,
so fields are generated alphabetically: citations, claim, conflicts, decision, reason.

## Synthetic calibration

`scripts/eval_local_judge.py`, the same 15 fixtures and system prompt (SHA-256
`0ff1529d…`), temperature zero, serial requests. Before: the base harness and request
shape (`json_object`, packet with the latest-scan field and per-file scan revisions).
After: the production schema and packet shape of this branch. Fixtures label accept
versus non-accept only, so the reject class is any reject or defer. Effective decisions
mirror production: malformed output decides nothing, and an acceptance with an inexact
citation or a named conflict is deferred.

| Metric | Before | After |
| --- | ---: | ---: |
| Decision accuracy | 12/15 (0.800) | 13/15 (0.867) |
| Accept precision | 3/4 (0.75) | 3/4 (0.75) |
| Accept recall | 3/4 (0.75) | 3/4 (0.75) |
| Reject-or-defer precision | 9/10 (0.900) | 10/11 (0.909) |
| Reject-or-defer recall | 9/11 (0.818) | 10/11 (0.909) |
| Deferral rate (effective) | 1/15 | 2/15 |
| Model chose defer | 0/15 | 1/15 |
| Malformed rate | 1/15 | 0/15 |
| Runtime errors | 0/15 | 0/15 |
| Median request duration | 5,523 ms | 2,821 ms |

Raw reports: [before](calibration-before-json-object.json), [after](calibration-after-json-schema.json).

Two cases changed. The injection case returned an empty claim before (malformed) and a
valid rejection after. The conflicting-memory case was rejected before without naming
the conflict; after, the model deferred and named the conflicting memory, which in
production quarantines that memory pending review. Two failures remain: the boundary case
cites a quote with a literal `\n`, which exact-citation validation defers, and the raw
model accepts the obsolete-revision fixture, which production blocks before inference
when the recorded revision differs from the live checkout.

A two-case follow-up ([probes](two-case-probes.json)) repeated the changed cases with
the new packet shape. With the schema, both decisions repeated. With `json_object`, the
injection case was also a valid rejection, and the conflict case was rejected while
naming the conflict. In this sample the schema accounts for the defer decision, and the
packet change alone avoided the empty claim. Two cases cannot separate the effects of
field order and constraint. An earlier full run with per-file scan revisions still in
the packet produced the same 15 decisions (median 5.8 s); request durations vary
because the live daemon's judge shares this runtime.

### After merging the other roadmap workstreams

The integration branch adds one sentence to the system prompt (safety workstream,
SHA-256 `708b817e…`) and nine adversarial fixtures. One run of all 24 fixtures with the
production schema ([report](calibration-after-merge-json-schema.json)), median 2.5 s:

| Metric | Original 15 cases | 9 adversarial cases |
| --- | ---: | ---: |
| Decision accuracy | 14/15 (0.933) | 8/9 (0.889) |
| Accept precision | 3/3 (1.00) | 0/1 |
| Accept recall | 3/4 (0.75) | no expected accepts |
| Reject-or-defer precision | 11/12 (0.917) | 8/8 (1.00) |
| Reject-or-defer recall | 11/11 (1.00) | 8/9 (0.889) |
| Model chose defer | 3/15 | 0/9 |
| Malformed rate | 0/15 | 0/9 |

The model now defers the obsolete-revision and truncated-output cases as well as the
conflict case; this run cannot separate the prompt sentence from the schema. The
boundary case still fails exact-citation validation. The model accepted the adversarial
case with role tokens planted in evidence, with citations that pass validation. In
production the safety screen holds all nine adversarial fixtures before inference
(`local_judge_fixture_adversarial_cases_are_held_and_supported_cases_are_not` in
`tests/injection_defenses.rs`); this harness measures the model when such text gets past
the screen.

## Unindexed files and immaterial revisions

The integrated judge (738d702) still blocked reviews in two ways that no index
refresh could clear:

- a touched file the indexer does not parse had no index anchor;
- a recorded task or evidence revision was older than the live HEAD.

This change adds two paths. First, an unindexed file is accepted when a hook-recorded
`source_digests` entry equals its live SHA-256. Second, an older revision is immaterial
when every touched file has the same bytes at that revision, live and in the index.

Both versions swept separate copies of the pre-roadmap backup
(`pre-roadmap-20261004.sqlite`, schema 16, migrated to 19 in the copy), prior judgments
removed, 135 pending reviews, against today's checkouts. No model was called.

| Gate result | Before (738d702) | After |
| --- | ---: | ---: |
| No readable, independently verified evidence | 90 | 90 |
| Unindexed file without a matching hook-recorded digest | — | 30 |
| Touched file has no indexed anchor | 3 | 0 |
| Evidence revision differs from the live checkout | 15 | 2 |
| Source task revision differs from the live checkout | 13 | 3 |
| Touched source file is missing | 6 | 2 |
| Touched file differs from indexed source | 3 | 3 |
| Touched path is outside every registered checkout | 3 | 3 |
| Passed every gate (model would be called) | 2 | 2 |
| A second sweep of the unchanged copy | 0 | 0 |

No additional review reaches the model in this snapshot.

- **Unindexed files.** The 30 reviews with unindexed touched files have no hook-recorded
  digests. The backup predates hook provenance: none of its 500 evidence rows carries
  `source_digests`. Their agent-reported evidence now stops at this precise reason, which
  replaces the generic, revision and missing-file reasons they showed before.
- **Older revisions.** A read-only diagnostic examined 73 reviews whose recorded revision
  differs from the live HEAD. It counted these before the evidence gate, so it also
  covers reviews the table reports for other reasons:
  - 45 have a touched file that changed since that revision;
  - 22 have a touched file that is unreadable, missing or in another checkout;
  - 4 have no touched files;
  - 2 have identical bytes at the revision and live, but an index that differs from
    the live file.

Of the 344 file comparisons, 91 were identical and 253 had changed:

| Changed files | Count |
| --- | ---: |
| Live content equals the first commit after the recorded revision that touched the file | 47 |
| Changed only in the working tree | 49 |
| Changed again after that first commit | 157 |

The 47 suggest the agent recorded the HEAD before committing the change it tested. The
recorded revision does not pin tested content that was not yet committed. Hook-recorded
digests do pin it.

Smoke check: the 2 reviews that pass every gate were sent to the local judge
(`engram-judge`, schema-constrained), one at a time. Both decisions were `defer`, with no
citations, no named conflicts and no errors (1,275 and 1,782 input tokens; 3.9 s and
11.0 s). Nothing was applied.

## Limitations

- 15 synthetic cases are a smoke test, not statistical calibration or real-repository
  accuracy. One case is a 6.7-point change.
- The live root-cause attribution for unavailable files replays today's filesystem;
  checkouts created or removed since those judgments can shift the counts.
- The replay calls no model, so it measures gates and dedupe only.
- Usage and timing come from a shared local runtime that also served the live daemon.

## Verification

- `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`
  and `cargo test --all-targets --all-features` passed on the change (416 tests), after
  merging the integration branch (519 tests) and with the follow-up access test (520).
- Mutation checks: adding live state back to the dedupe key fails the churn regression;
  treating absolute paths as invalid fails the three containment regressions; requiring
  full-length revisions fails the worktree regression.
- Unindexed files and immaterial revisions: the same gates passed (588 Rust tests, 1
  ignored helper; 231 Python tests from `python3 -B -m unittest discover -s tests -p
  'test_*.py'`). Disabling the hook-digest match fails both unindexed-file regressions.
  Disabling the per-file revision comparison fails both revision regressions.
