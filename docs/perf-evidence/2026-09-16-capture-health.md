# Capture health and operational-lesson replay — 2026-09-16 UTC

This measures the capture funnel separately from injection fidelity. Runtime
build: `af5c488300e1ca0d5c5b0808d410d3780743364acdb5fd44ccf7f1ce4aab14e6`.
Source base: `0564ffe`; implementation is the capture-health change accompanying
this evidence. This is a historical repair and a replay, not a prospective
claim that routine session conversion has improved.

## Baseline and one recovered lesson

| Measure | Before | After explicit recovery |
|---|---:|---:|
| Validated / all memories | 2/49 (4.08%) | 3/50 (6.00%) |
| Successful tasks without acceptance | 26/48 | 26/49 |
| Closed tasks with acceptance recorded (yes or no) | 24/118 (20.34%) | 25/119 (21.01%) |
| Closed tasks explicitly accepted | 20/118 (16.95%) | 21/119 (17.65%) |
| Abandoned tasks | 70/120 (58.33%) | 70/121 (57.85%) |
| Expected lessons delivered in replay | 0/6 | 1/6 |
| Brief false-positive rate | unknown: no labels | unknown: no labels |

The task denominator changed because an unrelated live task opened and closed
with acceptance during measurement. Neither the acceptance change nor the lower
abandoned percentage is an improvement claim for this implementation.
Per-session measurements, hashed session identifiers, denominators, first
validation latency, and episode fallback groups are in
[capture-before](2026-09-16-capture-before.json) and
[capture-after](2026-09-16-capture-after.json). Creation attribution uses source
tasks in the first non-reused `LEARNING_DISTILLED` event. Later reuse receives no
creation credit, including overlapping sessions. Memories without this event
are reported as sourced knowledge with unknown creation provenance. A memory
initially distilled from several sessions contributes to each source session,
so per-session counts must not be summed as unique memories.
Tasks without session IDs are separate episodes; no conversation membership is
invented. Actual brief relevance needs labeled emitted items; the collector
accepts labels and deliberately leaves the unmeasured rate null.

Replay uses actual checkout roots and HEADs through the production compiler,
with a 2,000-token budget. It checks delivered text, including compact
truncation, separately from matching canonical memories. No cloud model or
private-repository upload is involved. The six probes cover guide-flag PRs,
the agents Cloud Run pipeline, the production pooler incident, the org-quota
traffic pin, request-log completion timestamps, and contract-suite parallelism.
Only the org-quota traffic-pin lesson is received after recovery. The other five
remain misses and require evidence-backed capture/review. In particular, an
existing candidate recommends `--test-threads=1`, conflicting with the requested
`--test-threads=4` lesson; this work does not silently validate that old claim.

The traffic observation was already stored on a successful task, but its kind
was `test_result` with strength `observational`. Re-recording its original
artifact as `runtime_observation` fixed the evidence classification. Metadata
records `reclassified_from` and the original observation timestamp; no new
production probe was claimed. The source record remains unchanged.

- Source evidence: `evd_5b6d06ac-05ee-4a3a-afdb-8e1346274d8d`.
- Source task: `tsk_5e3215ca-3b4c-4304-8d4b-1612cdd2eaae`.
- Correctly typed evidence: `evd_24a8afb4-7f49-4081-a73b-fc711eaf1fa8`.
- Learned memory: `mem_2923b433-83a1-485f-a329-890749c87168`.
- Candidate → observed → supported → validated, confidence 0.50,
  `human_override=false`; acceptance was not invented or backfilled.

An initial regression launch exposed a separate harness isolation problem:
globally configured MCP was still available to the hook-disabled control. That
run was discarded. Its three synthetic conveyor episodes, evidence records, and
candidate memories were removed from the live brain after a WAL-safe backup;
memory purge events and existing immutable audit records remain. No real-session
rows were restored or removed. The after report was collected after that cleanup.
Both rerun adapters use an empty strict MCP configuration, and the local proxy
family disables hooks in both arms. The exact adapter changes are recorded in
[the isolation patch](2026-09-16-benchmark-isolation.patch).

## Daemon and privacy findings

Doctor passed before and after the change; schema remains 11. The initial
scheduler counter was 53,594 submitted/completed and 12 failed mutations, with
no rejected admissions. `jobs list --status failed` was empty. The scheduler
counter includes rejected interactive mutations and keeps no failure history;
auto-started daemon stderr went to `/dev/null`, and the only log file was empty.
It is therefore impossible to attribute all 12 historical failures from the
retained daemon data. Learning validation errors are confirmed in prior task
transcripts, but their timestamps do not justify equating them to this daemon
lifetime's counter.

Two older durable jobs were **dead**, both `vault_export` for tombstoned/unknown
projects, with recorded unknown-project errors and 8/5 attempts. Their dates
precede the measured daemon lifetime; they are a separate failure population.
Neither result indicates SQLite corruption or a backlog in the canonical writer.

The identified credential-bearing prompt was removed while the daemon was
stopped, with secure deletion and a successful WAL truncate, then doctor passed
and `engram backup create` produced a post-scrub snapshot. The shipping purge
command now performs this through the scheduler, also removes a pending hook
copy, and reports checkpoint contention. Tests inspect database, WAL, and
sidecar bytes; older backups and independent captures are explicitly outside
that command's scope.

## Live worktree proof and hermetic checks

The four redundant, untracked `.engram/project.toml` worktree files were removed
only after verifying that each matched its primary checkout's ID and that no
other metadata files or tracked paths would be removed. Installed MCP status
for the agents worktree returned matching branch/index HEAD
`ef3a5438d210e652aeaa9271ee05d19ac7649923` and
`index_matches_checkout=true`. The workspace retained the primary agents
checkout rather than the linked worktree.

`cargo fmt --all -- --check`, strict all-target/all-feature clippy, and all
249 Rust tests passed. The Python capture metric test passed. Existing compact
prefix/byte stability, token bounds, and brain-off tests remain green. New tests
exercise secret shapes and purge persistence, linked and bare-parent worktree
identity, real MCP error→observation→learn calls with unknown acceptance, and
CLI hooks across two projects (one JSON envelope, deferred tasks, proper evidence
scope, context-served attribution, relay, and SessionEnd).
