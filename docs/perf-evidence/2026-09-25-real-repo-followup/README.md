# Follow-up real-repository task benchmark

**Complete: 24/24 trial records completed.** 24 trial attempts started; 24 coding runs started; 0 attempts remain active; 0 were interrupted without a final record. Completed outcomes include 0 infrastructure/unscorable outcomes.

Pinned agent: `claude-opus-4-8[1m]`, effort `high`, Claude Code `2.1.212 (Claude Code)`. Protocol: 3 repetitions per task and arm, Engram off versus source-only automatic hooks.

All tables below use completed records only. Started or interrupted runs without final records can have additional unmeasured resource use. Token totals are provider input + cache creation + cache reads + output; they are processed tokens, not an estimate from text length.

## Correctness and observed resources

Clean validated completions require behavioral acceptance, valid controls and grading, and no explicit agent runtime error or exhausted budget. A patch can pass the behavioral grader while its agent run fails; the behavioral grade is retained separately. Success fractions use completed attempts as the denominator, including behavioral failures and infrastructure/unscorable outcomes. Each cell shows off → brain. Resources include unsuccessful attempts; partial measurements are marked.

| Task | Behavioral passes (off; brain) | Clean validated completions (off; brain) | Provider tokens | Agent tool calls | Agent seconds |
|---|---|---|---:|---:|---:|
| Skip task guides in the action hot path | 3/3; 3/3 | 3/3; 3/3 | 14,045,437 → 18,119,195 | 218 → 174 | 1,962.1 → 2,120.3 |
| Lean phone-task status polling | 3/3; 3/3 | 3/3; 3/3 | 11,980,560 → 11,305,501 | 225 → 191 | 1,828.1 → 1,699.0 |
| Isolate tests from developer environment files | 3/3; 2/3 | 3/3; 2/3 | 3,512,972 → 5,704,770 | 100 → 93 | 1,167.5 → 1,180.1 |
| Bound onboarding outage handling | 0/3; 0/3 | 0/3; 0/3 | 4,194,395 → 2,410,391 | 116 → 86 | 1,181.8 → 1,066.8 |

| Arm | Clean passes | Failures | Infrastructure / unscorable | Unknown | Attempts |
|---|---:|---:|---:|---:|---:|
| off | 9 | 3 | 0 | 0 | 12 |
| brain | 8 | 4 | 0 | 0 | 12 |

Unscorable attempts remain in resource totals but cannot establish a correctness win. Differences in overall clean-completion fractions alone are not an effectiveness estimate.

| All completed attempts | Provider tokens | Agent tool calls | Agent seconds | Reported USD estimate |
|---|---:|---:|---:|---:|
| off | 33,733,364 [12/12 reported] | 659 [12/12 reported] | 6,139.6 [12/12 reported] | 34.347 [12/12 reported] |
| brain | 37,539,857 [12/12 reported] | 544 [12/12 reported] | 6,066.2 [12/12 reported] | 38.044 [12/12 reported] |

Provider token categories remain separate because cache reads, cache writes, ordinary input and output have different prices. These are all-attempt totals, including reported usage from interrupted runs.

| Arm | Ordinary input | Cache creation | Cache reads | Output |
|---|---:|---:|---:|---:|
| off | 1,254 [12/12 reported] | 886,427 [12/12 reported] | 32,476,136 [12/12 reported] | 369,547 [12/12 reported] |
| brain | 956 [12/12 reported] | 1,102,555 [12/12 reported] | 36,077,327 [12/12 reported] | 359,019 [12/12 reported] |

## Paired results

Complete pairs: 12; both pass: 8; only off passes: 1; only brain passes: 0; neither passes: 3; infrastructure/unscorable: 0; incomplete: 0; ambiguous duplicates: 0.

Pairs with an infrastructure or unknown outcome are unscorable and do not count as a correctness win for either arm. Reductions below compare only pairs where both arms have clean validated completions and both measurements are available. Positive percentages mean fewer resources with Engram; negative percentages mean more. Percentages use sums over matched pairs, not the average of percentages. A cheaper failed run is never counted as savings.

| Both-pass comparison | Token reduction | Tool-call reduction | Agent-time reduction |
|---|---:|---:|---:|
| All tasks | -17.2% (n=8) | +15.5% (n=8) | -2.1% (n=8) |
| Skip task guides in the action hot path | -29.0% (n=3) | +20.2% (n=3) | -8.1% (n=3) |
| Lean phone-task status polling | +5.6% (n=3) | +15.1% (n=3) | +7.1% (n=3) |
| Isolate tests from developer environment files | -61.8% (n=2) | +1.5% (n=2) | -8.3% (n=2) |

## All-attempt resource per clean validated completion

These quotients charge each arm's observed failed and infrastructure attempts to its clean validated completions. They are descriptive, not paired savings estimates; no successes means undefined. Partial coverage means the numerator is only observed usage.

| Arm | Tokens / success | Tool calls / success | Agent seconds / success | USD estimate / success |
|---|---:|---:|---:|---:|
| off | 3,748,152 | 73.2 | 682.2 | 3.816 |
| brain | 4,692,482 | 68.0 | 758.3 | 4.756 |

## Setup, indexing, and delivery

Collection fixture setup measures cloning the prepared seed caches and reindexing/snapshot work. The index/snapshot portion overlaps setup and must not be added to it. Inherited seed private-cache preparation belongs to the earlier pilot, not this collection.

| Task | Collection clone/reindex seconds | Index/snapshot seconds | New dependency preparation seconds | Inherited seed private-cache seconds | Indexed source nodes |
|---|---:|---:|---:|---:|---:|
| Skip task guides in the action hot path | 19.4 | 4.4 | 0.0 | 223.6 | 20,924 |
| Lean phone-task status polling | 8.5 | 6.3 | 0.0 | unavailable | 15,751 |
| Isolate tests from developer environment files | 19.4 | 4.6 | 0.0 | 256.3 | 22,248 |
| Bound onboarding outage handling | 38.8 | 2.8 | 0.0 | unavailable | 10,389 |

Measured collection fixture setup: 86.1 seconds; index/snapshot portion: 18.1 seconds. Inherited seed private-cache preparation (earlier pilot): 479.9 seconds [2/4 fixtures recorded].

- off: 1,218.1 seconds of per-trial setup, 262.2 seconds of grading; 0 delivered contexts and 0 context compilations.
- brain: 1,197.5 seconds of per-trial setup, 216.8 seconds of grading; 22 delivered contexts and 323 context compilations.

Agent wall time includes hook processing; fixture setup, per-trial preparation, and private grading are reported separately. Agent tool-call counts do not include internal Engram hook/daemon operations. Context estimates and delivery counts are separate from provider token usage.

## Separate infrastructure attempts

4 recorded attempts are outside the planned matrix. Their observed resource use is retained separately and is not assigned to the paired comparisons. Missing final provider telemetry is unknown, not zero.

| Arm | Attempts | Observed provider tokens | Observed tool calls | Agent seconds | Reported USD estimate |
|---|---:|---:|---:|---:|---:|
| off | 2 | 11,028,286 [2/2 reported] | 143 [2/2 reported] | 1,305.5 [2/2 reported] | 10.249 [2/2 reported] |
| brain | 2 | 7,234,459 [2/2 reported] | 119 [2/2 reported] | 1,247.7 [2/2 reported] | 7.576 [2/2 reported] |

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

Machine-readable evidence: [redacted results](results.json) and [aggregate summary](summary.json).

Additional evidence: [findings and interpretation](FINDINGS.md); [collection controls audit](COLLECTION_AUDIT.md).
