# Real-repository task pilot

**Complete: 24/24 trial records completed.** 24 trial attempts started; 24 coding runs started; 0 started attempts have no completed record. Completed outcomes include 6 infrastructure/unscorable outcomes.

Pinned agent: `claude-opus-4-8[1m]`, effort `high`, Claude Code `2.1.212 (Claude Code)`. Protocol: 3 repetitions per task and arm, Engram off versus source-only automatic hooks.

All tables below use completed records only. Started or interrupted runs without final records can have additional unmeasured resource use. Token totals are provider input + cache creation + cache reads + output; they are processed tokens, not an estimate from text length.

## Correctness and observed resources

Clean validated completions require behavioral acceptance, valid controls and grading, and no explicit agent runtime error or exhausted budget. A patch can pass the behavioral grader while its agent run fails; the behavioral grade is retained separately. Success fractions use completed attempts as the denominator, including behavioral failures and infrastructure/unscorable outcomes. Each cell shows off → brain. Resources include unsuccessful attempts; partial measurements are marked.

| Task | Behavioral passes (off; brain) | Clean validated completions (off; brain) | Provider tokens | Agent tool calls | Agent seconds |
|---|---|---|---:|---:|---:|
| Skip task guides in the action hot path | 3/3; 2/3 | 2/3; 2/3 | 13,036,512 (partial) → 12,852,483 | 183 → 194 | 1,529.6 → 1,975.9 |
| Lean phone-task status polling | 1/3; 1/3 | 1/3; 1/3 | 10,771,597 → 10,382,639 | 215 → 152 | 1,755.4 → 1,503.3 |
| Isolate tests from developer environment files | 0/3; 3/3 | 0/3; 3/3 | 3,862,065 → 3,125,003 | 78 → 84 | 1,082.0 → 1,061.3 |
| Bound onboarding outage handling | 2/3; 1/3 | 2/3; 1/3 | 2,570,718 → 3,699,826 | 85 → 112 | 771.8 → 1,138.8 |

| Arm | Clean passes | Failures | Infrastructure / unscorable | Unknown | Attempts |
|---|---:|---:|---:|---:|---:|
| off | 5 | 4 | 3 | 0 | 12 |
| brain | 7 | 2 | 3 | 0 | 12 |

Unscorable attempts remain in resource totals but cannot establish a correctness win. Differences in overall clean-completion fractions alone are not an effectiveness estimate.

| All completed attempts | Provider tokens | Agent tool calls | Agent seconds | Reported USD estimate |
|---|---:|---:|---:|---:|
| off | 30,240,892 (partial) [12/12 reported] | 561 [12/12 reported] | 5,138.8 [12/12 reported] | 30.885 (partial) [12/12 reported] |
| brain | 30,059,951 [12/12 reported] | 542 [12/12 reported] | 5,679.4 [12/12 reported] | 32.095 [12/12 reported] |

Provider token categories remain separate because cache reads, cache writes, ordinary input and output have different prices. These are all-attempt totals, including reported usage from interrupted runs.

| Arm | Ordinary input | Cache creation | Cache reads | Output |
|---|---:|---:|---:|---:|
| off | 1,124 (partial) [12/12 reported] | 836,357 (partial) [12/12 reported] | 29,085,999 (partial) [12/12 reported] | 317,412 (partial) [12/12 reported] |
| brain | 988 [12/12 reported] | 928,801 [12/12 reported] | 28,793,948 [12/12 reported] | 336,214 [12/12 reported] |

## Paired results

Complete pairs: 12; both pass: 1; only off passes: 1; only brain passes: 4; neither passes: 0; infrastructure/unscorable: 6; incomplete: 0; ambiguous duplicates: 0.

Pairs with an infrastructure or unknown outcome are unscorable and do not count as a correctness win for either arm. Reductions below compare only pairs where both arms have clean validated completions and both measurements are available. Positive percentages mean fewer resources with Engram; negative percentages mean more. Percentages use sums over matched pairs, not the average of percentages. A cheaper failed run is never counted as savings.

| Both-pass comparison | Token reduction | Tool-call reduction | Agent-time reduction |
|---|---:|---:|---:|
| All tasks | +34.1% (n=1) | +22.7% (n=1) | -1.3% (n=1) |
| Skip task guides in the action hot path | +34.1% (n=1) | +22.7% (n=1) | -1.3% (n=1) |

## All-attempt resource per clean validated completion

These quotients charge each arm's observed failed and infrastructure attempts to its clean validated completions. They are descriptive, not paired savings estimates; no successes means undefined. Partial coverage means the numerator is only observed usage.

| Arm | Tokens / success | Tool calls / success | Agent seconds / success | USD estimate / success |
|---|---:|---:|---:|---:|
| off | 6,048,178 (partial coverage) | 112.2 | 1,027.8 | 6.177 (partial coverage) |
| brain | 4,294,279 | 77.4 | 811.3 | 4.585 |

## Setup, indexing, and delivery

Original fixture setup includes dependency preparation and indexing/snapshot work; these columns overlap and must not be added together. Additional private dependency preparation is recorded separately.

| Task | Original setup seconds | Index/snapshot seconds | Original dependency seconds | Additional private dependency seconds | Indexed source nodes |
|---|---:|---:|---:|---:|---:|
| Skip task guides in the action hot path | 148.8 | 2.8 | 145.8 | 223.6 | 20,924 |
| Lean phone-task status polling | 11.3 | 3.2 | 0.0 | unavailable | 15,751 |
| Isolate tests from developer environment files | 154.4 | 3.2 | 150.8 | 256.3 | 22,248 |
| Bound onboarding outage handling | 24.7 | 1.5 | 22.4 | unavailable | 10,389 |

Observed one-time fixture setup: 339.2 seconds; index/snapshot portion: 10.6 seconds. Additional private dependency preparation: 479.9 seconds [2/4 fixtures recorded].

- off: 153.7 seconds of per-trial setup, 149.4 seconds of grading; 0 delivered contexts and 0 context compilations.
- brain: 167.4 seconds of per-trial setup, 129.3 seconds of grading; 36 delivered contexts and 199 context compilations.

Agent wall time includes hook processing; fixture setup, per-trial preparation, and private grading are reported separately. Agent tool-call counts do not include internal Engram hook/daemon operations. Context estimates and delivery counts are separate from provider token usage.

## Separate infrastructure attempts

1 recorded attempts are outside the planned matrix. Their observed resource use is retained separately and is not assigned to the paired comparisons. Missing final provider telemetry is unknown, not zero.

| Arm | Attempts | Observed provider tokens | Observed tool calls | Agent seconds | Reported USD estimate |
|---|---:|---:|---:|---:|---:|
| off | 1 | unavailable [0/1 reported] | 36 (partial) [1/1 reported] | unavailable [0/1 reported] | unavailable [0/1 reported] |

## Runtime, authentication, isolation, and toolchain preflights

17 recorded attempts are outside the planned matrix. Their observed resource use is retained separately and is not assigned to the paired comparisons. Missing final provider telemetry is unknown, not zero.

| Arm | Attempts | Observed provider tokens | Observed tool calls | Agent seconds | Reported USD estimate |
|---|---:|---:|---:|---:|---:|
| off | 12 | 330,198 [12/12 reported] | 36 [12/12 reported] | 206.7 [9/12 reported] | 1.712 [12/12 reported] |
| brain | 5 | 152,281 [5/5 reported] | 15 [5/5 reported] | 65.0 [5/5 reported] | 0.962 [5/5 reported] |

## Scope and limitations

- This is a four-task, three-repetition diagnostic, not a general productivity estimate or a statistically conclusive trial.
- The model and effort are pinned. Model drift or missing brain delivery is classified as infrastructure failure rather than a successful Engram trial.
- Claude-reported USD figures are usage estimates under a subscription, not incremental cash charges or billing receipts.
- Platform dependencies use the prepared installed cache and differ from the historical lockfile; both arms use the same prepared environment.
- Engram uses source-only historical indexes with automatic hooks, no accumulated learned memories, and no MCP tools. This does not measure the full second-brain configuration.
- Provider cache reads and cache creation are included in processed-token totals. Cache pricing differs, so processed-token reduction and estimated USD reduction need not agree.
- Derived completion classification uses revision `agent-completion-v2`. Raw records and behavioral grades are preserved; runtime errors are infrastructure outcomes and explicit budget exhaustion is failure unless infrastructure already invalidates the trial.
- Manifest runner/metrics hashes identify frozen collection-start sources. The separate analysis_provenance hashes identify the scripts used to generate this report; post-collection classification does not replace raw measurements.
- Reported usage from error or interrupted runs may be partial even when final usage fields are present. Measurement coverage counts reported values, not complete error-run billing. All reported values remain in all-attempt totals; none of these runs contributes to both-pass savings.
- Incomplete tool traces retain their observed call counts as lower bounds in all-attempt totals, with partial coverage marked. They are excluded from paired tool-call deltas.
- Private source, prompts, patches, tool inputs, raw grader output, session identifiers, and trace identifiers are excluded from these artifacts.
- r02 validates cursor conversion errors, not strict rejection of bool/fractional JSON numbers
- r06 malformed JSON syntax is covered, schema-invalid valid JSON is not
- single agent with six core tools; source-only Engram, not accumulated memory or MCP
- Provider prompt caching is enabled and was not reset; arm order alternates. The aborted first Off attempt may have warmed the r01 prefix. Token categories are reported separately; CLI USD estimates are not subscription cash charges.
- Post-collection adjudication (5 attempts): A reviewed grader requirement was not established by the delivered task and repository contract. The original behavioral result is preserved, but this attempt is unscorable for effectiveness; its observed resource use remains included. Original frozen grades are unchanged. See the grading audit for the reviewed ambiguity.

Machine-readable evidence: [redacted results](results.json) and [aggregate summary](summary.json).

Additional evidence: [findings and interpretation](FINDINGS.md); [context delivery audit](CONTEXT_AUDIT.md); [grading audit and uncovered cases](GRADING_AUDIT.md); [collection controls audit](COLLECTION_AUDIT.md); [measured follow-up tasks](NEXT_ITERATION.md); [native-tool workflow diagnostics](workflow.json).
