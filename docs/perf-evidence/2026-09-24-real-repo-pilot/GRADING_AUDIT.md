# Grading audit

This audit distinguishes executed acceptance checks from supplementary source
inspection. It does not change the frozen grader, prompts, candidates, or scores.
Behavioral acceptance is separate from clean agent completion and runtime validity.

## r01: task-guide request switches

Repetition 3 Brain passes compilation and 13/14 behavioral checks. Its sole
failure, `policy_restricts_task_opt_in`, reports one mock search when zero was
expected. The fixture enables guides, disables task guides through an enforcing
partner policy, and supplies `use_task_guides: true`. After calling
`apply_guide_policy_to_request`, however, the
[hidden fixture](../../../benchmarks/real-repo-v1/grading/r01_guide_hot_path/hidden_tests.rs#L60)
replaces that enforcing context with the consumer default before resolving guides.
The failed assertion therefore tests whether the request alone carries the
restriction after its policy context has been discarded.

The saved candidate leaves the policy mutator unchanged, so it does not copy the
new task-guide restriction into the request. It does enforce the restriction in
the resolver: `src/services/guides.rs:1064–1067` combines the caller's switch with
`task_guides_allowed(partner_context)`. That helper rejects task guides under the
enforcing policy at lines 4194–4198. Both fresh retrieval at line 1101 and later
cache pickup at line 1312 depend on the combined gate. In the production path,
`src/services/action_engine.rs:1158` calls the policy mutator and line 1219 passes
the **same enforcing context** to the resolver. Source inspection found no
production replacement with the default context between these calls.

The [delivered prompt](../../../benchmarks/real-repo-v1/prompts/r01_guide_hot_path.md)
requires preserving enforcing partner restrictions and caller opt-outs. It does
not require a standalone request to retain policy after discarding the context.
The hidden check thus imposes an additional implementation constraint, and its
failure does not establish a production policy bypass. Repetition 3 Off passes
14/14 and copies the restriction into the request at
`src/services/partner_runtime.rs:647–648`, satisfying that stronger constraint.

Repetition 3 Brain warrants an evidence-verified `ambiguous_grader_contract`
exclusion from effectiveness scoring. Preserve its raw 13/14 behavioral failure
and resource use; do not convert it to a pass or count it as a correctness loss.
This audit inspected saved source and execution logs only. It did **not** execute
a counterfactual check retaining the enforcing context or the full production
path, so it does not establish a counterfactual 14/14 pass.

For both arms, the saved grade matches the recorded grade, both grader hashes
match repetition 1 and the frozen files, and the delivered prompts are identical
and match their recorded hashes. Both agents exited 0 without a recorded timeout,
provider error, or validity issue. The reviewed Brain grade SHA-256 is
`9042347168b6496e055d5fb01bb907fb2ff962390815e8519e33c59065972643`.
No candidate, grader, prompt, or raw score was changed.

## r02: lean phone-task status

The private grader executes the actual status handler, Redis service, and
user-control polling consumer against isolated test doubles. Its 23 checks cover
lean polling, counts, explicit history, ordinary integer/string cursors, invalid
text/list/object cursors, preserved fields and v1 fixtures, and consumer opt-in.

| Trial | Executed behavioral result | Saved-candidate adjudication |
|---|---|---|
| Repetition 1, Brain | 21/23; fail | Genuine missing consumer history opt-in, plus an overly strict hidden check on additive v1 fixture changes. |
| Repetition 1, Off | 23/23; pass | v1 fixtures remain unchanged; the user-control consumer explicitly requests history. |
| Repetition 2, Off | 22/23; raw fail | Sole failure is the overly strict byte check on additive v1 fields; all other checks pass. Requires separate verified exclusion from effectiveness scoring. |
| Repetition 2, Brain | 21/23; fail | Genuine missing consumer history opt-in, plus the same overly strict hidden byte check. |
| Repetition 3, Off | 21/23; fail | Genuine missing consumer history opt-in, plus the same overly strict hidden byte check. |
| Repetition 3, Brain | 23/23; pass | v1 fixtures remain unchanged; the consumer explicitly requests history. |

For repetition 1 Brain, saved `contract_test_fixtures/v1/status_running.json` and
`status_completed.json` add `step_count` to the historical fixtures. This fails
the predeclared hidden byte-preservation check. The agent-visible repository
policy permits additive fields, as detailed below. This does not remove the
independent consumer failure: `scripts/mcp_user_control_integration_test.py`
remains unchanged, returns the lean terminal response without an opt-in request,
and subsequently lacks the steps its user-control checks inspect. The executed
consumer check fails with a missing `steps` field. That omission is a genuine
task failure; the separate hidden byte check is stricter than the visible policy.

For repetition 2 Off, source inspection confirms that the saved candidate adds
`step_count: 2` to `contract_test_fixtures/v1/status_completed.json` and
`step_count: 1` to `status_running.json`. The frozen base copies match the
grader-owned immutable hashes. These are the only changed v1 fixtures; their
existing fields remain intact. The grader reports the completed fixture first
and stops that check at its first mismatch.

This changes the hidden fixture hashes without violating the repository's
documented rule for additive fields. The byte-preservation failure alone does
not establish a breach of the agent-visible contract or runtime wire-field loss.
Repetition 2 Off passes all other recorded checks, including runtime serialization
and the explicit history request in the user-control consumer. Its saved grade
matches the trial record, all recorded grader hashes still match, and the agent
completed without a recorded runtime error. This review reads saved evidence and
candidate files; it does not rerun tests or alter the score.

Repetition 2 Brain makes the same two additive fixture edits as repetition 2
Off. Its `scripts/mcp_user_control_integration_test.py` remains byte-identical to
the base, so the polling consumer still returns a lean terminal response without
requesting the steps its subsequent checks need. The saved consumer check fails
with a missing `steps` field. This independent omission supports the behavioral
failure despite the overly strict check on additive fixture changes.
The saved grade matches the record, grader hashes remain unchanged, and the
agent has no recorded runtime error or timeout. These findings come from saved
results and source inspection, without another test execution.

For repetition 3 Off, saved-source inspection again finds only additive
`step_count` changes to the two v1 status fixtures and a byte-unchanged
`scripts/mcp_user_control_integration_test.py`. The recorded consumer check fails
with a missing `steps` field, independently establishing the missing history
opt-in; the known byte-policy ambiguity does not remove that behavioral failure.
Repetition 3 Brain preserves the v1 fixture hashes and adds `include_steps: True`
to the consumer's status request. All 23 recorded checks pass. Both saved grades
match their trial records and unchanged grader hashes; both agents exited 0
without a timeout or provider error. This pair was reviewed through saved
grading evidence and source inspection only, with no rerun or score change.

### Agent-visible v1 versioning boundary

The historical agents repository at base commit
`1b5c21e44eda8e68dbbe457f7e98e7a607ff3b1a` includes the relevant policy in
`contract_test_fixtures/v1/README.md:21–23`:

> Adding optional fields is non-breaking; both ends ignore unknown keys. Removing or changing the type of any existing field requires a v2/ directory and a coordinated transition.

That file identifies both status fixtures as `phone_task_status` response
examples. It does not require byte-immutable fixtures or a new version for an
added field. The delivered prompt says “Preserve contract v1” without imposing
byte immutability. The additive `step_count` fields preserve all existing keys,
types, and values. The repetition 2 Off candidate leaves the policy README
unchanged; both reviewed Brain candidates retain that policy while documenting
the new history opt-in behavior. The reviewed historical Python tests contain
no fixture-hash constraint that establishes the hidden byte rule for the agent.

This corrects the earlier interpretation of fixture changes as a genuine
omission: the hidden rule overconstrains the visible task. Repetition 2 Off's sole
failed check therefore warrants an evidence-verified `ambiguous_grader_contract`
exclusion from effectiveness scoring, retaining its original 22/23 behavioral
failure and observed resources. It must not become a pass or a correctness loss
in paired comparisons. Trials with independent consumer omissions retain those
behavioral failures. Future trials with the same sole check failure require their own
saved-source and grading-evidence review before any exclusion. No grader, raw
score, prompt, or candidate is changed by this source-only audit.

### Unscored cursor cases and prompt ambiguity

The following table is **source inspection, not additional executed checks**.
It compares reference fix `5b0c08504f191c7f9b96893764c120cb0f85efd0` with the saved
repetition 1 candidates. Assume `include_steps` is absent and a task exists.

| `since_step` value | Reference | Brain, repetition 1 | Off, repetition 1 |
|---|---|---|---|
| JSON `false` | Converts to 0; includes steps numbered above 0 | Disables cursor; omits steps | Disables cursor; omits steps |
| String `"false"` | Invalid params, `-32602` | Disables cursor; omits steps | Disables cursor; omits steps |
| JSON `true` | Converts to 1; includes steps numbered above 1 | Same as reference | Invalid params, `-32602` |
| JSON `1.8` | Converts to 1; includes steps numbered above 1 | Same as reference | Invalid params, `-32602` |

The reference applies Python `int` conversion to each non-null cursor. Brain
first applies `_flag_disabled`, then converts; Off's `_parse_since_step` first
disables false values and otherwise accepts integers and integer-valued strings.
The relevant saved source is `src/agency/api/services/mcp_tool_handlers.py`:
Brain lines 107 and 701; Off lines 659 and 687.

The prompt says “Treat a false boolean or string false as disabled” without
explicitly limiting that sentence to `include_steps`. It also requests errors
for cursors that cannot be converted to an integer. Both candidates apply the
first sentence to the cursor as well. The numeric boolean/fractional cases were
already outside the frozen grader's strict-type coverage. The cursor string
`"false"` additionally exposes the wording ambiguity.

These observations limit what a 23/23 result establishes. They do not introduce
post-hoc requirements or alter any score. A future task specification should
separately define opt-in flag parsing and cursor parsing before new trials run.

## r05: isolate test environment loading

Repetition 1 Off is a genuine incomplete fix, according to the Rust grader
auditor's review of the saved candidate and execution logs. The executed checks
for test-mode dotenv isolation, development dotenv loading, and the settings
harness's poisoned-lock recovery pass. Two other poisoned-lock checks fail.

Source inspection found that the candidate repairs `settings_for_test` through
`lock_test_env()`, while two existing tests still acquire the same lock with
`TEST_ENV_LOCK.lock().unwrap()` in saved `tests/http_contract.rs`:

- `redis_cluster_mode_does_not_default_to_local_redis_url`, line 2825.
- `upstash_mode_does_not_default_to_local_redis_url`, line 2845.

Each frozen hidden case catches a deliberate panic, verifies the original panic
payload, then invokes one of these existing functions. The saved execution logs
show the original deliberate failure followed by `PoisonError` at the remaining
`unwrap`. Thus the prompt's requirement that later tests recover the shared lock
is unmet at those acquisition sites. No grader defect or post-hoc
reclassification was identified.

Repetition 1 Brain passes compilation and all five frozen behavioral cases. The
Rust grader auditor's saved-source inspection confirms that it replaces all
three shared-lock acquisitions: the settings helper and both existing tests
listed above. It therefore addresses the two missing acquisition sites in the
Off candidate. There are no failed checks to reclassify for this trial.

Repetition 2 repeats these outcomes. Off passes compilation and three of five
behavioral cases, but still uses an unprotected lock acquisition in the cluster
and Upstash tests at saved `tests/http_contract.rs` lines 2819 and 2839. The two
saved failure logs show the deliberate original panic followed by `PoisonError`
at exactly those sites. The settings helper recovers correctly. This is another
genuine incomplete fix under the unchanged acceptance criteria, not a grader
defect.

Repetition 2 Brain passes compilation and all five cases. Source inspection
confirms poisoned-guard recovery at all three existing acquisition sites, starting
at lines 62, 2798, and 2824. Its saved cluster and Upstash logs retain the original
deliberate panic and then report a passing check, with no subsequent poison error.

For both repetition 2 arms, the saved grade JSON matches the recorded grade, and
both grader-file hashes match repetition 1 and the frozen grader files. Both
agents exited 0 without a recorded timeout, provider error, or validity issue;
grader exits are 1 for Off and 0 for Brain. This adjudication only inspected saved
source, hashes, and execution logs. It ran no new tests and changes no raw score.

Repetition 3 Off has the same genuine incomplete fix: compilation and three of
five behavioral cases pass, but the cluster and Upstash poisoned-lock checks
exit 101. The saved settings helper recovers its guard at
`tests/http_contract.rs:64–66`; the two existing tests still call
`TEST_ENV_LOCK.lock().unwrap()` at lines 2819 and 2839. Both saved failure logs
show the deliberate original panic followed by `PoisonError` at those exact
acquisition sites. This again violates the requirement that later tests recover
the shared lock; it is not a grader-contract mismatch.

The repetition 3 Off grade matches its trial record, and both grader-file hashes
match the earlier repetitions and frozen files. The agent exited 0 with no
recorded timeout, provider error, or validity issue; the grader exited 1. This
review inspected saved source and logs only, without new executions or changes
to the raw score.

Repetition 3 Brain passes compilation and all five cases. Its shared helper
`lock_test_env()` recovers a poisoned guard at `tests/http_contract.rs:59–62`,
and all three original acquisition sites use it: the settings helper at line 71,
cluster test at line 2801, and Upstash test at line 2821. Both saved Redis-related
logs show the original deliberate panic and a passing result without a secondary
poison error. The saved grade matches the record; grader hashes match repetition
1 and the frozen files. Agent and grader both exited 0, without a recorded
timeout, provider error, or validity issue. No checks were rerun.

| r05 repetition | Off behavioral result | Brain behavioral result | Adjudication |
| --- | --- | --- | --- |
| 1 | 3/5; fail | 5/5; pass | Off misses the cluster and Upstash lock recovery sites. |
| 2 | 3/5; fail | 5/5; pass | Same two Off omissions; Brain repairs all three original sites. |
| 3 | 3/5; fail | 5/5; pass | Same two Off omissions; Brain repairs all three original sites. |

All six r05 candidates compile, and no r05 result requires a grader-contract
exclusion. These observed outcomes do not identify whether context caused the
difference between arms.

## r06: bound onboarding outage handling

Repetition 1 Brain has 26 passing executable checks and no failed or unknown
checks in its saved grading result. The backend-task auditor verified that the
saved grade matches the recorded grade and that all three grader manifest hashes
still match the frozen files. The agent and grader exited successfully, with no
timeout, provider error, or recorded validity issue. This is verification of
recorded execution evidence, not a new grader execution. No failed-check
adjudication or additional candidate source inspection was necessary.

Repetition 1 Off has 25/26 passing checks. The sole failed assertion expects
timeout status `503`, while the candidate deliberately returns `504`. The saved
execution establishes that the hung-fetch calls settled within 5.5 seconds and
the profile result reported failure. Assertions about exact gate/welcome
destinations and all three abort events followed the status assertion and were
not reached. Thus this is not evidence of a deadline failure or a fail-open
decision, and it cannot establish a counterfactual 26/26 pass.

The frozen catalog/reference expects the established failure status, historically
`503`; the delivered prompt requires bounded, fail-closed behavior without naming
that status. The backend-task auditor therefore identified a grader-contract
ambiguity in this trial's sole failed assertion. The raw behavioral grade remains
fail. Any separate analysis adjudication must explicitly retain that verdict and
label the comparison unscorable rather than count a correctness loss.

The auditor also found a **source-inspection-only** limitation, separate from the
failed assertion: Off clears its abort timer after response headers arrive and
before consuming the JSON body. A stalled body therefore lacks an active timeout.
The reference retains its timeout signal through body consumption. The frozen
hung-fetch test delays headers and does not execute this body-stall case; this
observation does not change its recorded score.

Repetition 2 Brain has the same sole ambiguous assertion failure: 25/26 checks
pass, and the saved Vitest result reports `504` where the frozen timeout check
expects `503`. The check completes in 5,005.520 ms. Its preceding elapsed-time
and `result.ok === false` assertions pass; the later gate destination, welcome
destination and exact abort-count assertions are not reached. This establishes
that all three calls settled within the allowed deadline, but does not establish
a counterfactual full pass for the interrupted check.

Saved-source inspection confirms a deliberate status choice: in
`features/onboarding/actions/_call-backend.ts:81`, an aborted request produces
`504`, while other network failures produce `503`. The profile action supplies
the 5,000 ms timeout at `get-current-user.ts:12`. The delivered prompt is identical
to repetition 1 Off and specifies neither status code. The saved grade matches
the trial record, all grader hashes match the frozen files and repetition 1, and
the agent completed successfully without a timeout or recorded provider error.
Therefore this trial warrants the same evidence-verified
`ambiguous_grader_contract` exclusion: preserve its raw behavioral failure and
resource use, classify the comparison as unscorable, and do not turn it into a
pass or an effectiveness loss. The exact reviewed grade SHA-256 is
`c9f6011d61ae38a3887093e90543076e22bef36731f7632bcf47192303f8cf9f`.

Repetition 2 Brain also repeats the separate **source-inspection-only** body
timeout limitation: `_call-backend.ts:85–86` clears the abort timer after fetch
returns headers, before the JSON reads at lines 90 and 103. Its gate and welcome
functions handle failure results conservatively, but those timeout-specific
destinations were not asserted after the status mismatch. The frozen test does
not exercise a stalled response body. No new test was run, no candidate or
grader was changed, and this observation does not change the recorded score.

Repetition 3 Brain again passes 25/26 checks, with the sole failure reporting
`504` instead of the hidden check's `503`. The saved Vitest check duration is
5,006.141 ms. As in the earlier reviewed trials, the elapsed-time and failure-result
assertions pass before the status assertion; exact gate/welcome destinations and
the number of aborted fetches are not asserted after it. This is the same
contract ambiguity, not an executed deadline failure or evidence of a full
counterfactual pass.

The saved candidate makes this status choice explicit at
`features/onboarding/actions/_call-backend.ts:78–88`. It sets a 5,000 ms abort
timer at lines 15 and 55–63 and returns `504` for an aborted request, `503` for
other network failures. The delivered prompt is unchanged and names neither
code. Saved grade and native Vitest results agree on the sole failure, the grade
matches the trial record, and all grader hashes match the frozen files and
prior reviewed runs. The agent completed without a recorded timeout or provider
error. This supports an `ambiguous_grader_contract` exclusion for exactly
`r06_onboarding_outage-r3-brain`, retaining its raw failure and all observed
resources. Its reviewed grade SHA-256 is
`6c74a6e9191e6b9571947de78f569a87a2295ecf80461866f36956d61d6af196`.

The separate **source-inspection-only** limitation also recurs: the candidate
clears its abort timer at `_call-backend.ts:92–93` before consuming JSON at lines
97 and 110. The frozen hung-header check does not execute a stalled response
body, so this observation does not add a behavioral failure or alter the score.
This review used saved evidence only and ran no new tests.

Repetition 3 Off passes all 26 frozen checks in both its saved grade and native
Vitest report. The grade matches the trial record, and all grader-file hashes
match the frozen files. Agent and grader exited 0, with complete trace/usage
evidence and no recorded timeout, provider error or validity issue. Its grade
SHA-256 is
`60950d0a3a2c00b9ff22f8f964e7904e06a65dc8ddb0ec4eb8ab9189d94ca44f`.
This confirms the saved result without a rerun or further candidate inspection;
no adjudication is required for this trial.
