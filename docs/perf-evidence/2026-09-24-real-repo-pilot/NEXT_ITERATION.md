# Next iteration

These are proposed engineering tasks, grounded in all 12 Brain attempts in
[CONTEXT_AUDIT.md](CONTEXT_AUDIT.md), at its **2026-09-25 07:08:46 UTC** cutoff
(23 total completed records; final Off attempt unfinished). No implementation or
new runtime evaluation is included here. Preserve the frozen pilot and its raw outcomes; future work
gets new versioned fixtures and results.

## Prerequisite: repair the evaluation contract

The [grading audit](GRADING_AUDIT.md) identifies three hidden constraints that
exceed the delivered tasks. Before another agent benchmark:

- **r01:** exercise policy enforcement with the production context retained.
  Require request-only policy propagation separately only if the task explicitly
  specifies it. An alternative implementation that enforces the restriction in
  the resolver must be accepted; one that ignores enforcing policy must fail.
- **r02:** permit additive optional v1 fixture fields, matching the repository
  policy. Still reject removal or incompatible type changes and omitted history
  opt-in in the step-consuming script.
- **r06:** specify permitted timeout status codes and test bounded, fail-closed
  gate and welcome behavior independently of the status assertion. Explicitly
  decide whether the deadline includes body consumption, then cover a stalled
  body as well as stalled headers.

Preflight each revised grader against the base, reference, legitimate alternative
implementations, and incomplete fixes. Publish the acceptance matrix and frozen
hashes before new trials. Do not retroactively rerun or rescore this pilot.

## Prioritized implementation tasks

| Priority / task | Demonstrated evidence and bounded change | Acceptance checks and measurements |
| --- | --- | --- |
| 1. Preserve identifiers throughout the prompt | [SourceQuery::parse](../../../src/repository_context.rs#L540) spends its 20-term budget in input order; later policy and consumer requirements disappear. Extract explicit paths and code identifiers across the whole task before selecting natural-language terms. Distinguish precise identifiers from generic exact-label matches in [ranking](../../../src/repository_context.rs#L283). | Move an unchanged requirement between the start and end of a prompt: preserve its identifier candidates. Cover snake_case, camelCase, qualified names, quoted names, and paths. At the same pointer/token budget, measure required-definition recall, required-file recall, generic-label occupancy, and compilation latency. Improved task outcomes remain a hypothesis. |
| 2. Retrieve fields through their owners | The [source index](../../../src/storage/migrations/0015_source_search.sql#L6) stores labels and paths; indexed request types do not expose the existing `use_app_guides` field. Add field-to-owner lookup and resolve qualified identifiers such as `Settings::from_env` to the existing owner/member definitions. | The historical field query finds `ActionRequest` and `SessionActionRequest`; the qualified query finds `Settings`/`from_env` in `src/config.rs`. Disambiguate equal field names by repository and owner. A field absent at the historical revision stays absent. Measure owner recall, false matches, index size/time, and compile latency. |
| 3. Deduplicate delivered source content | [Delivery suppression](../../../src/agenthooks.rs#L312) hashes rendered bytes. Twelve order-only deliveries emitted 4,335 estimated context tokens in the audited sample. Use source identity plus content/revision and relevant fact freshness, independent of ranking order, scoped to task and workspace. | Reordering unchanged pointers emits nothing. A new source, changed content at an existing identity, or changed relevant fact remains deliverable. Independent tasks/workspaces retain their first delivery. Handle removal, invalidation, and returning to an older set explicitly; avoid permanent ID-only suppression. Measure order-only packs, novel/repeated pointers, estimated emitted tokens, and false suppression. These text estimates are not provider savings. |
| 4. Measure requirement coverage and impacted consumers | The packs omit indexed resolver, policy, lock-site, gate, and consumer functions. r02 repetition 3 succeeds with byte-identical packs to two earlier failures, so omission alone cannot explain the outcomes. Add an offline task-to-required-component evaluation; test bounded caller/consumer expansion separately. | Score existing required definitions and files, including the step-consuming script and all three lock acquisitions. Record unique delivered coverage and novelty at fixed budgets. Include unchanged supporting files and legitimate alternative implementations; do not define relevance solely by the reference diff. Compare direct retrieval with consumer expansion for recall, irrelevant occupancy, and latency. |

## Evidence-gated follow-up: source lifecycle relevance

Two high-ranked r02 `Step` types live under `delete/src/`; the needed handler and
consumer are absent. This demonstrates weak task relevance, **not** that every
file in that directory is retired. First verify lifecycle boundaries from
repository documentation, build inputs, or explicit configuration. Only then test
a default ranking penalty with explicit historical queries still able to retrieve
those sources. Measure required-component recall and lifecycle-related misses;
do not exclude files by directory name alone.

## Evaluation design

Start with offline retrieval fixtures using historical snapshots, full task text,
and equivalent prompts with reordered requirements. Keep budgets fixed; compare
one change at a time with the frozen retrieval version. Add held-out tasks to
check that improvements extend beyond these four audited examples.

After grader preflight, run a new paired agent evaluation with pinned model,
effort, prompts, repository revisions, dependency caches, isolation, and budgets.
Balance arm order and predeclare repetitions and stopping rules. Compare the
selected retrieval change against current Brain; retain Off as the zero-context
control. Do not select attempts based on whether their patches pass.

Report all-attempt outcomes and observed resources with missing/partial coverage,
pair correctness counts excluding infrastructure/unscorable cases, and resource
deltas only where both attempts cleanly pass with complete measurements. Keep
provider input/output/cache categories, tool calls, agent wall time, setup/index
costs, compilation work, and durable delivery receipts distinct. A retrieval
recall gain or reduction in repeated text is a mechanism result; completed-task
benefit requires the new controlled agent results.
