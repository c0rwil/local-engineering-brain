# Follow-up real-repository task benchmark

**Aborted: 4/24 trial records completed.** 5 trial attempts started; 4 coding runs started; 0 attempts remain active; 1 were interrupted without a final record. Completed outcomes include 4 infrastructure/unscorable outcomes.

Pinned agent: `claude-opus-4-8[1m]`, effort `high`, Claude Code `2.1.212 (Claude Code)`. Protocol: 3 repetitions per task and arm, Engram off versus source-only automatic hooks.

All tables below use completed records only. Started or interrupted runs without final records can have additional unmeasured resource use. Token totals are provider input + cache creation + cache reads + output; they are processed tokens, not an estimate from text length.

## Correctness and observed resources

Clean validated completions require behavioral acceptance, valid controls and grading, and no explicit agent runtime error or exhausted budget. A patch can pass the behavioral grader while its agent run fails; the behavioral grade is retained separately. Success fractions use completed attempts as the denominator, including behavioral failures and infrastructure/unscorable outcomes. Each cell shows off → brain. Resources include unsuccessful attempts; partial measurements are marked.

| Task | Behavioral passes (off; brain) | Clean validated completions (off; brain) | Provider tokens | Agent tool calls | Agent seconds |
|---|---|---|---:|---:|---:|
| Skip task guides in the action hot path | 1/1; 1/1 | 0/1; 0/1 | 5,049,273 → 3,842,474 | 76 → 50 | 559.2 → 518.2 |
| Lean phone-task status polling | 1/1; 1/1 | 0/1; 0/1 | 5,979,013 → 3,391,985 | 67 → 69 | 746.4 → 729.5 |
| Isolate tests from developer environment files | 0/0; 0/0 | 0/0; 0/0 | unavailable → unavailable | unavailable → unavailable | unavailable → unavailable |
| Bound onboarding outage handling | 0/0; 0/0 | 0/0; 0/0 | unavailable → unavailable | unavailable → unavailable | unavailable → unavailable |

| Arm | Clean passes | Failures | Infrastructure / unscorable | Unknown | Attempts |
|---|---:|---:|---:|---:|---:|
| off | 0 | 0 | 2 | 0 | 2 |
| brain | 0 | 0 | 2 | 0 | 2 |

Unscorable attempts remain in resource totals but cannot establish a correctness win. Differences in overall clean-completion fractions alone are not an effectiveness estimate.

| All completed attempts | Provider tokens | Agent tool calls | Agent seconds | Reported USD estimate |
|---|---:|---:|---:|---:|
| off | 11,028,286 [2/2 reported] | 143 [2/2 reported] | 1,305.5 [2/2 reported] | 10.249 [2/2 reported] |
| brain | 7,234,459 [2/2 reported] | 119 [2/2 reported] | 1,247.7 [2/2 reported] | 7.576 [2/2 reported] |

Provider token categories remain separate because cache reads, cache writes, ordinary input and output have different prices. These are all-attempt totals, including reported usage from interrupted runs.

| Arm | Ordinary input | Cache creation | Cache reads | Output |
|---|---:|---:|---:|---:|
| off | 290 [2/2 reported] | 255,693 [2/2 reported] | 10,678,241 [2/2 reported] | 94,062 [2/2 reported] |
| brain | 220 [2/2 reported] | 218,018 [2/2 reported] | 6,939,218 [2/2 reported] | 77,003 [2/2 reported] |

## Paired results

Complete pairs: 2; both pass: 0; only off passes: 0; only brain passes: 0; neither passes: 0; infrastructure/unscorable: 2; incomplete: 0; ambiguous duplicates: 0.

Pairs with an infrastructure or unknown outcome are unscorable and do not count as a correctness win for either arm. Reductions below compare only pairs where both arms have clean validated completions and both measurements are available. Positive percentages mean fewer resources with Engram; negative percentages mean more. Percentages use sums over matched pairs, not the average of percentages. A cheaper failed run is never counted as savings.

| Both-pass comparison | Token reduction | Tool-call reduction | Agent-time reduction |
|---|---:|---:|---:|
| All tasks | undefined (n=0) | undefined (n=0) | undefined (n=0) |

## All-attempt resource per clean validated completion

These quotients charge each arm's observed failed and infrastructure attempts to its clean validated completions. They are descriptive, not paired savings estimates; no successes means undefined. Partial coverage means the numerator is only observed usage.

| Arm | Tokens / success | Tool calls / success | Agent seconds / success | USD estimate / success |
|---|---:|---:|---:|---:|
| off | undefined | undefined | undefined | undefined |
| brain | undefined | undefined | undefined | undefined |

## Setup, indexing, and delivery

Collection fixture setup measures cloning the prepared seed caches and reindexing/snapshot work. The index/snapshot portion overlaps setup and must not be added to it. Inherited seed private-cache preparation belongs to the earlier pilot, not this collection.

| Task | Collection clone/reindex seconds | Index/snapshot seconds | New dependency preparation seconds | Inherited seed private-cache seconds | Indexed source nodes |
|---|---:|---:|---:|---:|---:|
| Skip task guides in the action hot path | 19.4 | 4.4 | 0.0 | 223.6 | 20,924 |
| Lean phone-task status polling | 8.3 | 6.3 | 0.0 | unavailable | 15,751 |
| Isolate tests from developer environment files | 18.9 | 5.0 | 0.0 | 256.3 | 22,248 |
| Bound onboarding outage handling | 38.8 | 2.5 | 0.0 | unavailable | 10,389 |

Measured collection fixture setup: 85.5 seconds; index/snapshot portion: 18.1 seconds. Inherited seed private-cache preparation (earlier pilot): 479.9 seconds [2/4 fixtures recorded].

- off: 197.5 seconds of per-trial setup, 48.3 seconds of grading; 0 delivered contexts and 0 context compilations.
- brain: 242.2 seconds of per-trial setup, 30.2 seconds of grading; 2 delivered contexts and 81 context compilations.

Agent wall time includes hook processing; fixture setup, per-trial preparation, and private grading are reported separately. Agent tool-call counts do not include internal Engram hook/daemon operations. Context estimates and delivery counts are separate from provider token usage.

## Separate infrastructure attempts

0 recorded attempts are outside the planned matrix. Their observed resource use is retained separately and is not assigned to the paired comparisons. Missing final provider telemetry is unknown, not zero.

| Arm | Attempts | Observed provider tokens | Observed tool calls | Agent seconds | Reported USD estimate |
|---|---:|---:|---:|---:|---:|

## Runtime, authentication, isolation, and toolchain preflights

0 recorded attempts are outside the planned matrix. Their observed resource use is retained separately and is not assigned to the paired comparisons. Missing final provider telemetry is unknown, not zero.

| Arm | Attempts | Observed provider tokens | Observed tool calls | Agent seconds | Reported USD estimate |
|---|---:|---:|---:|---:|---:|

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
- Development tasks previously used to improve retrieval; not held-out generalization evidence.
- V2 clarifies grading contracts. Compare Off/Brain within this collection; no causal old-versus-new comparison.
- Provider prompt caching remains enabled and is not reset. Report all token categories; estimated USD is not subscription cash billing.
- Single agent, six core tools, source-only hooks. Does not measure learned memories, automatic judging, or MCP tools.
- Three repetitions per task are diagnostic, not statistically decisive.

**Collection aborted: the generated sandbox prevented shell commands from starting (E2BIG).** Raw grades and usage are preserved, but affected attempts are unscorable and excluded from savings. This collection will not be resumed or mixed with a repaired protocol.

Machine-readable evidence: [redacted results](results.json) and [aggregate summary](summary.json).

Additional evidence: [collection controls audit](COLLECTION_AUDIT.md).
