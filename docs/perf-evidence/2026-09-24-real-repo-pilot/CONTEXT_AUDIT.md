# Context delivery audit

The first completed Brain run solved the guide hot-path task, but its context
missed two important implementation files and repeated the same source pointers.
This supports specific retrieval and delivery experiments. It does not establish
task-level savings or a causal slowdown: the corresponding Off run was interrupted
by a provider error before it completed its own validation.

Coverage cutoff: **2026-09-25 07:08:46 UTC**, or **2026-09-25 00:08:46 MST**.
This snapshot covers all 12 planned Brain trials: r01, r02, r05, and r06
repetitions 1–3. Twenty-three total trial records were complete at the cutoff;
r06 repetition 3 Off was still unfinished. It uses the
finished trials' provider records, tool events, candidate diffs, and read-only
SQLite context and delivery records. Candidate source, private identifiers, and
raw agent reasoning are intentionally excluded. The broader pilot protocol and
results are in [README.md](README.md).

Across these 12 runs, **199 compilations produced 36 durable deliveries**.
There were 432 delivered pointer occurrences, of which 278 repeated a source
already delivered within that trial (64.4%). Twelve deliveries changed only the
order of the immediately preceding pack. Ten deliveries after the initial
packs introduced a new source, one each. These are context-quality observations,
not evidence that a particular delivery caused a tool call, success, or failure.

## Audit method

The frozen databases store delivery receipts as `usage_events` with
`usage_kind=context_served`, rather than in a separate `context_receipts` table.
This audit joins those events to their task and `context_runs` rows, selects
distinct nonempty Brain context runs, and reads their `context_items`. Receipt
counts, delivered token estimates, compilation counts, compiled token estimates,
and compiled source-item counts agree with every audited trial's recorded
delivery summary. A compiled but unacknowledged pack is not counted as delivered.
Each completed database is opened in read-only mode inside a consistent read
transaction, including its SQLite WAL. An immutable-file connection that would
ignore the WAL is not used. Only attempts with a completed record at the cutoff
enter this snapshot; unfinished attempts are not queried.

Source-set identity ignores ranking order. A repeated pointer is one already
delivered earlier in the same trial. An order-only pack has the same stable prefix
and rendered pointer lines as the immediately preceding delivery, with only order
changed. Returning to an older source set after an intervening different set is
reported separately. All uniqueness and repetition counts reset between trials;
the aggregate sums those within-trial counts.

The 199 compilations selected 2,388 pointer occurrences and an estimated 73,453
context tokens. The 36 deliveries emitted 432 occurrences and an estimated 13,080
context tokens. These estimates describe compiled or emitted text, not provider
input, cache usage, billing, or savings. Compiler time totaled 1,493 ms across the
12 runs; the full hook and daemon path is not measured by that number.

## Observed work and its limits

| Measure | Off, provider interrupted | Brain, clean completion |
| --- | ---: | ---: |
| Hidden behavioral checks passed | 14/14 | 14/14 |
| Tool calls | 27 | 74 |
| Read / Grep calls | 11 / 7 | 28 / 27 |
| Read + Grep calls before first edit | 16 | 39 |
| Provider total tokens | 955,615 | 5,646,988 |
| Agent wall seconds | 250.648 | 785.035 |
| Durable context delivery receipts | 0 | 4 |
| Estimated delivered context tokens | 0 | 1,620 |

Provider total tokens include input, output, cache creation, and cache reads across
the run. They are not unique prompt tokens or a direct measure of injection cost.
The table retains observed usage from the interrupted run; it is not a clean
paired savings comparison.

Both patches changed `src/models/api.rs`, `src/services/guides.rs`, and
`src/services/partner_runtime.rs`. Brain also added approximately 272 lines of
regression tests, out of 299 added lines. It ran nine new tests and a broader
135-test selection successfully. Off completed a build, then received a provider
server error and exited 1, without adding regression tests. Its final provider
record combined `subtype=success` with `is_error=true`; the latter, the nonzero
exit, and the explicit error event establish interrupted execution despite the
behavioral grader passing the saved patch.

The pre-edit exploration difference occurs before Brain's test additions, but one
pair still cannot separate context effects from agent variation. Neither the
lower Off totals nor the higher Brain totals should be presented as a treatment
effect.

## What the context actually delivered

| Delivery measure | First r01 Brain run |
| --- | ---: |
| Compiled context runs | 18 |
| Distinct selected source sets | 1 |
| Unique source pointers | 12 |
| Distinct rendered packs / durable deliveries | 4 / 4 |
| Delivered pointer occurrences | 48 |
| Repeated pointer occurrences after the first pack | 36 (75%) |
| Estimated tokens per complete pack | 405 |
| Estimated tokens in later packs with the same information | 1,215 |
| Changed production files represented by delivered pointers | 1 of 3 |

All 18 compilations selected the same 12 source identities. The four delivered
packs contained the same stable prefix and pointer lines, with only their order
changing. Subsequent deliveries introduced no new source identities or pointer
text. The 1,215-token figure counts three complete repeated packs; it is an
estimate of emitted text, not provider billing attribution. The record's
`source_items=216` counts all compiled items, not the 48 delivered occurrences.

The pack did locate `src/services/guides.rs`, but its selected symbol was
`skip_task_guides_for_short_follow_up`, rather than `resolve_for_action`.
Neither `src/models/api.rs` nor `src/services/partner_runtime.rs` appeared.
`ActionRequest`, `SessionActionRequest`, `into_action_request`, and
`apply_guide_policy_to_request` were indexed in the frozen database but were not
delivered. This is a selection failure for those symbols, rather than evidence
that those files were never indexed.

The first two ranked pointers were the `guides` module in `src/services/mod.rs`
and an `action` function in `src/services/loopguard_enforcer.rs`, scoring 75 and
66. Other pointers included a Raindrop session-ID preservation test, a 32 MiB
request-limit test, loopguard tests, and an empty-assistant-output history test.
These illustrate matches to generic task words with weak relation to the required
request switches, conversion, and policy behavior.

Three of the 12 supplied files later received a native Read call; five received a
Read or a Grep explicitly scoped to that file. Seven received neither. These are
observable inspection counts, not a semantic relevance score: repository-wide
Grep could traverse additional files, and useful context need not trigger a read.

## Confirmed mechanisms

| Mechanism | Code and observation |
| --- | --- |
| Early generic terms consume the query budget | [SourceQuery::parse](../../../src/repository_context.rs#L540) keeps the first 20 unique accepted terms; the [limit is applied during input traversal](../../../src/repository_context.rs#L590). In this task, the retained set includes `both`, `existing`, `must`, `neither`, `optional`, `supplied`, and `turning`. Later requirements containing `use_guide`, `session-action`, `request`, `conversion`, `partner`, `policy`, and `restrictions` miss that term budget. The budget fills within the task itself, before runner instructions. |
| Generic exact labels outrank more specific partial matches | [rank_sources](../../../src/repository_context.rs#L283) gives an exact label match +64, a label substring +8, and a path substring +4. It does not distinguish a generic word such as `action` from a precise code identifier. The delivered top-ranked `guides` and `action` labels exhibit this weighting. |
| Field names do not necessarily lead to their owning types | The [source FTS index](../../../src/storage/migrations/0015_source_search.sql#L6) stores project, label, and file path. The frozen index has the request types but no source label for existing field `use_app_guides`. Searching the field therefore cannot use an indexed field-to-owner association. The new `use_task_guides` field is naturally absent at the historical base. |
| Reordering bypasses unchanged-content suppression | [Current paths receive a +4 score boost](../../../src/repository_context.rs#L276), changing pointer order as the agent reads files. [ContextDeliveryClaim::should_emit](../../../src/agenthooks.rs#L312) compares a digest of rendered bytes. The same source set produced exactly four different orderings and four delivered packs, confirming that order-only differences passed this suppression check. |

The duplication mechanism is directly supported by the delivered payloads and
implementation. The query and ranking observations identify concrete retrieval
weaknesses. They do not prove which pointer affected the agent's decisions or how
many tool calls any single weakness caused.

Compiler time was 7–9 ms per run, totaling 135 ms across 18 runs. This excludes
hook, daemon, and transcript-capture overhead. The task episode in the Brain
database remained marked abandoned despite the clean agent completion and passing
external grade, so that episode status alone cannot stand in for this benchmark's
ground-truth outcome.

## Experiments after the frozen matrix

1. **Preserve identifiers across the whole task.** Extract explicit paths and
   snake_case, camelCase, qualified, or quoted identifiers before applying a term
   cap. Rank remaining natural-language terms by specificity. Test equivalent
   prompts with critical requirements moved to the end; measure retrieval of
   request types, conversion, policy, and resolver symbols.
2. **Index fields with their owners.** Associate configuration keys and request
   fields with the containing type and relevant definitions. Test existing field
   queries such as `use_app_guides` against the historical base. Measure whether
   the needed files enter the bounded pack without adding large source dumps.
3. **Suppress semantic repeats.** Compare selected source identities, content
   digests, facts, and freshness, so pure reordering is suppressed while changed
   source or facts can still be delivered. Include controls for reordered items,
   one new item, changed content under an existing identity, and changed facts.
   Alternatively deliver only the new or changed items.
4. **Measure task coverage and novelty alongside costs.** Record coverage of
   required components, unique versus repeated delivered pointers, inspection
   before the first edit, and completed regression validation. Keep compiled,
   emitted, provider-observed, and outcome-validated quantities separate.
5. **Run a new paired comparison after these changes.** Preserve the current
   matrix unchanged. Compare retrieval versions with the same historical bases,
   model, instructions, dependency caches, and graders, retaining failed attempts.
   A small local reranker or judge is a later candidate if it improves measured
   component coverage and completed-task cost beyond these deterministic fixes;
   this run alone does not establish that a model is needed.

The r02 follow-up also motivates testing repository-declared active and retired
source boundaries, and coverage of callers that depend on an altered contract.
Such boundaries should come from repository evidence or configuration; a directory
name alone is not sufficient grounds to exclude source.

## Follow-up coverage

The next completed Brain run, r02 repetition 1, exited cleanly and received two
failed checks out of 23. One is a genuine omission: a user-control consumer still
accesses absent `steps`. The other is an overly strict hidden byte-preservation
check on additive v1 fixture changes. The agent-visible
`contract_test_fixtures/v1/README.md:21–23` explicitly permits optional fields and
requires v2 for removal or type changes. The hidden byte rule therefore exceeds
the visible contract. The independent consumer omission still supports the
behavioral failure; see [GRADING_AUDIT.md](GRADING_AUDIT.md). The run used 53 tools,
2,793,161 provider total tokens, and 475.785 agent wall seconds.
Before its first edit it made 13 Read and seven Grep calls.

| Brain run | Compilations | Distinct source sets | Deliveries | Unique pointers within run | Delivered occurrences | Repeated occurrences | Order-only packs | Estimated delivered / order-only pack tokens |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| r01, repetition 1 | 18 | 1 | 4 | 12 | 48 | 36 | 3 | 1,620 / 1,215 |
| r01, repetition 2 | 17 | 2 | 3 | 13 | 36 | 23 | 0 | 1,201 / 0 |
| r01, repetition 3 | 15 | 2 | 4 | 13 | 48 | 35 | 1 | 1,606 / 405 |
| r02, repetition 1 | 24 | 1 | 2 | 12 | 24 | 12 | 1 | 796 / 398 |
| r02, repetition 2 | 21 | 1 | 2 | 12 | 24 | 12 | 1 | 796 / 398 |
| r02, repetition 3 | 21 | 1 | 2 | 12 | 24 | 12 | 1 | 796 / 398 |
| r05, repetition 1 | 10 | 2 | 2 | 13 | 24 | 11 | 0 | 768 / 0 |
| r05, repetition 2 | 7 | 2 | 2 | 13 | 24 | 11 | 0 | 768 / 0 |
| r05, repetition 3 | 5 | 2 | 2 | 13 | 24 | 11 | 0 | 768 / 0 |
| r06, repetition 1 | 18 | 2 | 4 | 13 | 48 | 35 | 2 | 1,224 / 612 |
| r06, repetition 2 | 18 | 3 | 5 | 14 | 60 | 46 | 2 | 1,517 / 603 |
| r06, repetition 3 | 25 | 3 | 4 | 14 | 48 | 34 | 1 | 1,220 / 306 |
| Sum within runs | 199 | 22 | 36 | 154 | 432 | 278 | 12 | 13,080 / 4,335 |

All 24 r02 compilations selected the same 12 source identities. Its two delivered
packs differ only in pointer order, providing a second observation of reorder-only
delivery. Repetition 2 of r02 exhibits the same pattern: 21 compilations, one
source set, and two orderings delivered. Every distinct compiled source set also
appeared in a delivered pack in these 12 runs. Unlike the first r01 run and all
three r02 runs, each r05 run introduces one new pointer in its second pack, so
those packs are not order-only repeats.

Of the 24 deliveries after the 12 initial packs, twelve are order-only repeats,
ten introduce one new pointer each, and two return to an older set without
introducing a new pointer. The last category comprises r01 repetitions 2 and 3,
with 405 estimated tokens each of previously delivered information. These 810
tokens are not included in the order-only column. No consecutive byte-identical pack
was delivered. These aggregates concern delivery novelty, not task success or
provider token savings.

The r02 pack contains `src/agency/api/services/mcp_session_service.py`, one of the
two changed production files, and the corresponding handler test file. Those are
also the only two delivered files later targeted by native Read or file-scoped
Grep. It omits the indexed `handle_phone_task_status` function in
`src/agency/api/services/mcp_tool_handlers.py`, and the indexed user-control
consumer at `scripts/mcp_user_control_integration_test.py`. The latter contains
`poll_task`, `last_step`, and `verify_user_control`; its missing update is the
genuine consumer failure.

Generic `Request` documentation sections and three types named `Step` dominate
the first five ranks. Two of those types live beneath `delete/src/`, while one is
the relevant session-service type. The query budget retains `include_steps` and
`since_step`, but fills before later requirements concerning Redis, false values,
invalid parameters, preserving contracts, and updating consumers. The task thus
provides another concrete instance of early terms excluding later obligations.

The agent inspected the v1 fixtures and contract test before making the additive
edits. Those edits should not be treated as evidence of an engineering failure
when the visible policy permits them. The consumer omission cannot be causally
attributed to retrieval from this trace. A targeted experiment should separately
measure whether context identifies impacted consumers and whether acceptance
checks match the contract visible to the agent.

Both r02 repetitions delivered the same 12 unique sources. The second again
omitted the handler implementation and user-control consumer. Its two raw failed
checks comprise the same genuine consumer omission and overly strict fixture
byte check. It made 18 native Read/Grep calls before its first edit, compared with
20 in repetition 1. Repeated observations support the retrieval diagnosis but do
not establish that the omitted pointers caused the consumer omissions.

The updated grading adjudication also excludes r02 repetition 2 Off from
effectiveness scoring: its sole raw failure is the byte check, while all other
checks pass. That correction concerns grading validity and does not change this
audit's Brain context sample or delivery counts.

Repetition 3 of r02 passed all 23 frozen checks while receiving the same two
ordered packs as repetitions 1 and 2: 21 compilations, 12 unique pointers, and
one order-only delivery. The indexed handler and user-control consumer remained
absent from context. Its saved patch nevertheless updates the consumer to request
`include_steps: true` and preserves the v1 fixture bytes. It made 14 native
Read/Grep calls before its first edit. This outcome under the same delivered
sources is direct evidence against inferring a deterministic failure from the
missing consumer pointer. It does not establish that the context was helpful.

### r05: success without useful lock-site pointers

Brain r05 repetition 1 passed compilation and all five frozen behavioral checks.
It correctly handles test-mode dotenv isolation, preserves development loading,
and recovers all three existing shared-lock acquisition sites. Its paired Off
run passed the first three behavioral checks but left the cluster and Upstash
test acquisition sites unprotected. Inspection of those saved failures confirmed
real `PoisonError` cascades at the unchanged sites; see
[GRADING_AUDIT.md](GRADING_AUDIT.md).

| Measure | Off, incomplete behavior | Brain, all checked behavior passes |
| --- | ---: | ---: |
| Tool calls | 24 | 35 |
| Read + Grep calls before first edit | 8 | 11 |
| Provider total tokens | 1,376,214 | 1,221,845 |
| Agent wall seconds | 338.058 | 395.570 |
| Agent execution | Clean | Clean |

Both agents exited 0 with no provider error, timeout, or permission denial. Each
also encountered a recoverable native Read error: Off requested a file exceeding
the read limit; Brain supplied a string where the line offset required a number.
These are included in the tool counts and did not invalidate execution. The
different behavioral outcomes preclude presenting this single pair as equivalent
completed work with established savings.

The ten Brain compilations selected 120 total source items and an estimated
3,858 tokens. Only two packs were durably delivered: 24 pointer occurrences and
768 estimated tokens. The first pack had 381 tokens; the second had 387. There
were two distinct source sets. Eleven pointers were shared, one was removed, and
one was added, leaving 13 unique pointers across the two deliveries. No additional
order-only pack was emitted in this run. Compiler time totaled 59 ms, again
excluding the rest of the hook path.

The first pack contained neither `src/config.rs` nor `tests/http_contract.rs`,
the two files actually changed. The second added `tests/http_contract.rs`, but
selected `get_action_failed_action_emits_failure_session_end`, which does not
identify the shared-lock repair. File coverage therefore rose from zero of two
to one of two. `Settings`, `from_env`, `settings_for_test`,
`redis_cluster_mode_does_not_default_to_local_redis_url`, and
`upstash_mode_does_not_default_to_local_redis_url` all existed in the index; none
was delivered. Coverage of the three required lock acquisition functions was
zero of three. The HTTP contract file was the only delivered file later targeted
by native Read or file-scoped Grep; all 11 Reads were confined to the two changed
files.

Two functions named `contains`, in `src/services/action_parser.rs` and
`vendor/llmapi-rust/src/media.rs`, led both packs with scores of 75. Generic task
word `contains` was captured by the query, while the first-20 budget excluded
later requirements about normal startup, loading, lock recovery, and regression
coverage. The query did retain `settings::from_env` as one token, but the index
labels `Settings` and `from_env` separately. This adds a focused query experiment:
resolve qualified identifiers to their indexed owner and member while retaining
the original identifier, instead of relying only on whole-label or substring
matches to the unsplit token.

The successful patch does not establish that these weak pointers helped find
the missed Off sites. Brain made a direct global replacement of the three known
lock acquisitions and validated both Redis-related cases. That is observable
agent behavior; attributing it to context requires a controlled comparison.

Repetition 2 also passed all five frozen checks and received the same two source
sets as repetition 1. Seven compilations emitted two packs totaling 768 estimated
tokens. `src/config.rs` and all three shared-lock acquisition functions again
remained absent; the only additional pointer named the same unrelated test in
`tests/http_contract.rs`. It made 11 native Read/Grep calls before its first edit,
matching repetition 1. The repeat success does not turn the missing lock-site
coverage into evidence of useful retrieval.

Repetition 3 again passed all five checks with the same two source sets: five
compilations and two deliveries, totaling 768 estimated emitted tokens. Neither
the configuration file nor any shared-lock acquisition function was delivered,
despite those definitions being indexed. It made seven native Read/Grep calls
before its first edit. All three Brain repetitions repaired all three existing
acquisitions while the Off candidates missed the same two sites; the
[grading audit](GRADING_AUDIT.md) verifies those outcomes. The repeated difference
does not establish that the supplied pointers caused the better task coverage.

### r01 repetitions 2 and 3: a temporary additional file pointer

The second r01 Brain run passed the frozen behavioral grader. Its first pack was
the same 12-source set seen in repetition 1. The second replaced an action-engine
history-test pointer with `add_optional_strings` in `src/models/api.rs`; the third
returned to the original source set. Thus the API file appeared once, but its
request types and conversion method still did not. `src/services/partner_runtime.rs`
and its policy function remained absent. Coverage of the three changed production
files rose from one to two across all deliveries, without supplying the central
symbols for the task.

This run made 14 native Read/Grep calls before its first edit, compared with 39 in
r01 repetition 1. Four delivered files received a native Read or file-scoped Grep.
The differing trajectories under the same retrieval version are another reason
not to infer a causal tool-call cost from a single trace.

Repetition 3 repeated the same temporary API-file addition and return to the
original source set, then emitted one order-only pack. Its 15 compilations led to
four deliveries with 1,606 estimated tokens, 13 unique pointers, and 35 repeated
occurrences. None of the five named components appeared, and the policy file
remained absent. It made 21 native Read/Grep calls before its first edit, and
five delivered files received a Read or file-scoped Grep.

Its raw 13/14 failure has a separate verified grader-contract exclusion: the
hidden policy check replaces the enforcing context with a default context,
while the production resolver retains it and combines it with the caller's
switch. The visible prompt does not require that extra context-discarding
behavior. This audit therefore does not treat that result as a genuine policy
bypass, a pass, or evidence that a missing policy pointer caused failure; see
[GRADING_AUDIT.md](GRADING_AUDIT.md).

### r06: supporting navigation context, missing gate functions

Brain r06 repetition 1 passed its frozen behavioral grader. Its 18 compilations
produced two source sets and four deliveries. The first two packs had the same
12 pointers in different orders. The third added `app/org/invite/page.tsx` and
removed a payment backend pointer; the fourth reordered that new set. The only
new source after the first pack was therefore the invite page.

The pack included `features/onboarding/actions/_call-backend.ts`, one of the three
changed production files, via its `BackendRequest` type. It also supplied the CLI
authentication page, later the invite page, and a repository lesson about backend
soft failures. All four were subsequently read. Those are plausible supporting
references for the navigation and outage requirements, although the trace cannot
establish their incremental benefit.

The indexed `callBackend`, `enforceOnboardingGate`, and `getCurrentUser` functions
were not delivered. Neither `features/onboarding/actions/enforce-onboarding-gate.ts`
nor `features/onboarding/actions/get-current-user.ts` appeared, and the welcome
page was absent too. A generic function named `Page` in
`app/sentry-example-page/page.tsx` led all four packs with a score of 75. Several
other `BackendRequest` types from unrelated feature directories also occupied the
pack. The agent independently inspected the missing gate and current-user files,
and made 13 native Read/Grep calls before its first edit.

Repetition 2 produced three source sets and five deliveries from 18 compilations.
It added `features/onboarding/utils/safe-next-path.ts` in the second pack and the
invite page in the fourth; the third and fifth packs changed only order. The
safe-next-path file is relevant supporting context for safe return destinations
and was inspected, alongside the four supporting files read in repetition 1.
The gate and current-user files and all three named functions were still absent.
It made 15 native Read/Grep calls before its first edit. Its raw 25/26 failure
has a verified grader-contract exclusion concerning timeout status 504 versus
503, detailed separately in [GRADING_AUDIT.md](GRADING_AUDIT.md). Neither the
supporting pointer addition nor the raw grade establishes a context effect.

Repetition 3 selected the same three source sets as repetition 2, with four
deliveries from 25 compilations. It introduced the safe-next-path file in the
second pack and the invite page in the fourth; the third was an order-only
repeat. The delivered files inspected were the backend wrapper, CLI page, invite
page, and safe-next-path file. Its 14 unique pointers still omitted the gate and
current-user files and all three named functions. It made 17 native Read/Grep
calls before its first edit. The raw grader reports 25/26, with the timeout-status
check failing. The separate audit subsequently verified the same 504-versus-503
grader-contract exclusion and preserved the raw failure; see
[GRADING_AUDIT.md](GRADING_AUDIT.md). It is unscorable for effectiveness, and no
success or failure is attributed to its context delivery.

### Coverage of the required components

This table uses named existing implementation components implicated by each task,
not every changed file or new symbol an agent might legitimately add. File
coverage means at least one pointer anywhere in that file appeared across the
run's deliveries; it does not imply that the relevant definition was supplied.

| Task and repetition | Required component files represented | Selected existing symbols represented |
| --- | --- | --- |
| r01, repetition 1 | 1/3: guide resolver file only | 0/5: request types, request conversion, resolver, and policy function |
| r01, repetitions 2 and 3 | 2/3 each: resolver and API files; policy file absent | 0/5 each: the additional API pointer names a different helper |
| r02, repetitions 1–3 | 1/3 each: session service; handler and step-consuming script absent | 1/3 each: runtime `Step` type present; `handle_phone_task_status` and consumer `poll_task` absent |
| r05, repetitions 1–3 | 1/2 each: HTTP contract file; configuration file absent | 0/3 each shared-lock acquisition functions; `from_env` also absent |
| r06, repetitions 1–3 | 1/3 each: onboarding backend wrapper; gate and current-user files absent | 0/3 each: `callBackend`, `enforceOnboardingGate`, and `getCurrentUser` |

All planned Brain deliveries are covered. The complete paired effectiveness
analysis remains separate: the final Off attempt was unfinished at this cutoff,
and this context audit does not substitute for the final matrix report.
