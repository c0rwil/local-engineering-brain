# Held-out real-repository task benchmark

**Held-out suite `real-repo-heldout-v1`.** Its five historical tasks were chosen outside the development tasks that informed Engram, to test whether its context changes generalize. It was collected while its catalog was still `draft_requires_operator_review`, with `--allow-draft-suite`: the operator review in its README, including confirmation that no task informed Engram development, was still pending, so the held-out status is provisional.

**Complete: 30/30 trial records completed.** 30 trial attempts started; 30 coding runs started; 0 attempts remain active; 0 were interrupted without a final record. Completed outcomes include 0 infrastructure/unscorable outcomes.

Pinned agent: `claude-opus-4-8[1m]`, effort `high`, Claude Code `2.1.212 (Claude Code)`. Protocol: 3 repetitions per task and arm, Engram off versus source-only automatic hooks.

All tables below use completed records only. Started or interrupted runs without final records can have additional unmeasured resource use. Token totals are provider input + cache creation + cache reads + output; they are processed tokens, not an estimate from text length.

## Correctness and observed resources

Clean validated completions require behavioral acceptance, valid controls and grading, and no explicit agent runtime error or exhausted budget. A patch can pass the behavioral grader while its agent run fails; the behavioral grade is retained separately. Success fractions use completed attempts as the denominator, including behavioral failures and infrastructure/unscorable outcomes. Each cell shows off → brain. Resources include unsuccessful attempts; partial measurements are marked.

| Task | Behavioral passes (off; brain) | Clean validated completions (off; brain) | Provider tokens | Agent tool calls | Agent seconds |
|---|---|---|---:|---:|---:|
| Deliver the effect-check note only when LoopGuard enforces | 3/3; 3/3 | 3/3; 3/3 | 4,630,172 → 5,439,413 | 117 → 86 | 871.6 → 1,045.8 |
| Tag model-failure and reaction analytics with the frozen origin | 3/3; 3/3 | 3/3; 3/3 | 7,401,360 → 8,519,235 | 131 → 125 | 1,192.0 → 975.4 |
| Default scheme-less navigation targets to https on every agent path | 3/3; 3/3 | 3/3; 3/3 | 3,705,384 → 3,574,984 | 153 → 131 | 1,124.5 → 756.8 |
| Resolve Play subscription base-plan product ids at every plan lookup | 3/3; 3/3 | 3/3; 3/3 | 2,439,977 → 1,961,311 | 63 → 52 | 884.2 → 710.0 |
| Truncate human-response prompt fields on character boundaries | 3/3; 3/3 | 3/3; 3/3 | 1,352,309 → 818,164 | 68 → 31 | 658.7 → 489.4 |

| Arm | Clean passes | Failures | Infrastructure / unscorable | Unknown | Attempts |
|---|---:|---:|---:|---:|---:|
| off | 15 | 0 | 0 | 0 | 15 |
| brain | 15 | 0 | 0 | 0 | 15 |

Unscorable attempts remain in resource totals but cannot establish a correctness win. Differences in overall clean-completion fractions alone are not an effectiveness estimate.

| All completed attempts | Provider tokens | Agent tool calls | Agent seconds | Reported USD estimate |
|---|---:|---:|---:|---:|
| off | 19,529,202 [15/15 reported] | 532 [15/15 reported] | 4,731.0 [15/15 reported] | 23.284 [15/15 reported] |
| brain | 20,313,107 [15/15 reported] | 425 [15/15 reported] | 3,977.5 [15/15 reported] | 24.219 [15/15 reported] |

Provider token categories remain separate because cache reads, cache writes, ordinary input and output have different prices. These are all-attempt totals, including reported usage from interrupted runs.

| Arm | Ordinary input | Cache creation | Cache reads | Output |
|---|---:|---:|---:|---:|
| off | 1,046 [15/15 reported] | 715,178 [15/15 reported] | 18,538,663 [15/15 reported] | 274,315 [15/15 reported] |
| brain | 740 [15/15 reported] | 859,029 [15/15 reported] | 19,212,588 [15/15 reported] | 240,750 [15/15 reported] |

## Paired results

Complete pairs: 15; both pass: 15; only off passes: 0; only brain passes: 0; neither passes: 0; infrastructure/unscorable: 0; incomplete: 0; ambiguous duplicates: 0.

Pairs with an infrastructure or unknown outcome are unscorable and do not count as a correctness win for either arm. Reductions below compare only pairs where both arms have clean validated completions and both measurements are available. Positive percentages mean fewer resources with Engram; negative percentages mean more. Percentages use sums over matched pairs, not the average of percentages. A cheaper failed run is never counted as savings.

| Both-pass comparison | Token reduction | Tool-call reduction | Agent-time reduction |
|---|---:|---:|---:|
| All tasks | -4.0% (n=15) | +20.1% (n=15) | +15.9% (n=15) |
| Deliver the effect-check note only when LoopGuard enforces | -17.5% (n=3) | +26.5% (n=3) | -20.0% (n=3) |
| Tag model-failure and reaction analytics with the frozen origin | -15.1% (n=3) | +4.6% (n=3) | +18.2% (n=3) |
| Default scheme-less navigation targets to https on every agent path | +3.5% (n=3) | +14.4% (n=3) | +32.7% (n=3) |
| Resolve Play subscription base-plan product ids at every plan lookup | +19.6% (n=3) | +17.5% (n=3) | +19.7% (n=3) |
| Truncate human-response prompt fields on character boundaries | +39.5% (n=3) | +54.4% (n=3) | +25.7% (n=3) |

The same both-pass pairs by provider token category. Cache reads dominate processed tokens and are priced differently, so they are compared separately.

| Both-pass comparison | Ordinary input | Cache creation | Cache reads | Output |
|---|---:|---:|---:|---:|
| All tasks | +29.3% (n=15) | -20.1% (n=15) | -3.6% (n=15) | +12.2% (n=15) |
| Deliver the effect-check note only when LoopGuard enforces | +34.2% (n=3) | -39.7% (n=3) | -16.9% (n=3) | +6.3% (n=3) |
| Tag model-failure and reaction analytics with the frozen origin | +6.3% (n=3) | -23.1% (n=3) | -15.1% (n=3) | +15.7% (n=3) |
| Default scheme-less navigation targets to https on every agent path | +36.0% (n=3) | -32.2% (n=3) | +4.6% (n=3) | -0.3% (n=3) |
| Resolve Play subscription base-plan product ids at every plan lookup | +27.4% (n=3) | +6.3% (n=3) | +20.4% (n=3) | +24.2% (n=3) |
| Truncate human-response prompt fields on character boundaries | +50.0% (n=3) | -1.3% (n=3) | +42.2% (n=3) | +9.9% (n=3) |

## All-attempt resource per clean validated completion

These quotients charge each arm's observed failed and infrastructure attempts to its clean validated completions. They are descriptive, not paired savings estimates; no successes means undefined. Partial coverage means the numerator is only observed usage.

| Arm | Tokens / success | Tool calls / success | Agent seconds / success | USD estimate / success |
|---|---:|---:|---:|---:|
| off | 1,301,947 | 35.5 | 315.4 | 1.552 |
| brain | 1,354,207 | 28.3 | 265.2 | 1.615 |

## Setup, indexing, and delivery

Fixtures were exported from the operator's repositories at each base commit with `git archive`, committed as one-commit repositories, built offline and locked where they have Rust dependencies, and indexed by the pinned release. Export, index/snapshot and dependency-build seconds are parts of fixture setup and must not be added to it. A snapshot refreshed for a new release replaces the index/snapshot time only.

| Task | Fixture setup seconds | Index/snapshot seconds | Dependency build seconds | Export seconds | Indexed source nodes |
|---|---:|---:|---:|---:|---:|
| Deliver the effect-check note only when LoopGuard enforces | 121.1 | 3.2 | 94.9 | 0.4 | 25,226 |
| Tag model-failure and reaction analytics with the frozen origin | 106.1 | 2.8 | 77.1 | 0.4 | 22,250 |
| Default scheme-less navigation targets to https on every agent path | 11.2 | 4.7 | 0.0 | 8.5 | 41,646 |
| Resolve Play subscription base-plan product ids at every plan lookup | 12.3 | 5.3 | 0.0 | 9.1 | 45,717 |
| Truncate human-response prompt fields on character boundaries | 38.3 | 3.2 | 8.9 | 0.4 | 25,226 |

Measured fixture preparation: 289.0 seconds; index/snapshot portion: 19.1 seconds. Export from the operator's repositories: 18.8 seconds [5/5 fixtures recorded].

- off: 114.4 seconds of per-trial setup, 117.8 seconds of grading; 0 delivered contexts and 0 context compilations.
- brain: 120.2 seconds of per-trial setup, 122.8 seconds of grading; 28 delivered contexts and 252 context compilations.

Agent wall time includes hook processing; fixture setup, per-trial preparation, and private grading are reported separately. Agent tool-call counts do not include internal Engram hook/daemon operations. Context estimates and delivery counts are separate from provider token usage.

## Context delivery and reads

Delivered contexts are Brain contexts confirmed by hook receipts. Pointer lines are their source-map entries; a ranged pointer names a line span. Related-use items are delivered `code_use` items. Read counts native Read requests over all completed attempts, those that named an offset or limit, and the UTF-8 bytes of text they returned; file reads through Bash are not counted. Off attempts have no Engram context by design.

| Arm | Delivered contexts | Pointer lines | Ranged pointers | Related-use pointers (items) | Read calls | Ranged Read calls | Read text bytes |
|---|---:|---:|---:|---:|---:|---:|---:|
| off | 0 | 0 | 0 (unavailable) | 0 (0) | 148 | 129 | 722,587 |
| brain | 28 | 381 | 306 (80.3%) | 45 (45) | 137 | 102 | 1,184,198 |

Read totals are complete for 15/15 off and 15/15 brain attempts; partial cells include the others as observed.

The line-range hypothesis is fewer Read bytes and fewer cache-read tokens with Engram at equal or better correctness. The comparison uses the both-pass pairs above, with complete Read measurements in both arms; positive percentages mean less with Engram. Correctness is in the paired results: a reduction only counts where both arms passed.

| Both-pass comparison | Read calls | Ranged Read calls | Read text bytes | Cache-read tokens |
|---|---:|---:|---:|---:|
| All tasks | +7.4% (n=15) | +20.9% (n=15) | -63.9% (n=15) | -3.6% (n=15) |
| Deliver the effect-check note only when LoopGuard enforces | +33.3% (n=3) | +43.1% (n=3) | -86.3% (n=3) | -16.9% (n=3) |
| Tag model-failure and reaction analytics with the frozen origin | -22.5% (n=3) | -17.1% (n=3) | -70.0% (n=3) | -15.1% (n=3) |
| Default scheme-less navigation targets to https on every agent path | +0.0% (n=3) | +7.7% (n=3) | -296.9% (n=3) | +4.6% (n=3) |
| Resolve Play subscription base-plan product ids at every plan lookup | -46.2% (n=3) | -300.0% (n=3) | +2.7% (n=3) | +20.4% (n=3) |
| Truncate human-response prompt fields on character boundaries | +56.2% (n=3) | +75.0% (n=3) | -85.8% (n=3) | +42.2% (n=3) |

## Separate infrastructure attempts

1 recorded attempts are outside the planned matrix. Their observed resource use is retained separately and is not assigned to the paired comparisons. Missing final provider telemetry is unknown, not zero.

| Arm | Attempts | Observed provider tokens | Observed tool calls | Agent seconds | Reported USD estimate |
|---|---:|---:|---:|---:|---:|
| off | 1 | 0 (partial) [1/1 reported] | 0 [1/1 reported] | 0.3 [1/1 reported] | 0.000 (partial) [1/1 reported] |

Recorded inspections of attempts the collector stopped on:

- `h01_effect_check_enforce_only-r1-off` (2026-10-05T02:10:06+00:00): kept here as a separate infrastructure attempt, outside the paired matrix; its slot ran again. Reason: Claude Code OAuth session expired before the agent started (0 tools, 0 tokens, 0.3 s); operator re-logged in with claude auth login

## Runtime, authentication, isolation, and toolchain preflights

0 recorded attempts are outside the planned matrix. Their observed resource use is retained separately and is not assigned to the paired comparisons. Missing final provider telemetry is unknown, not zero.

| Arm | Attempts | Observed provider tokens | Observed tool calls | Agent seconds | Reported USD estimate |
|---|---:|---:|---:|---:|---:|

## Scope and limitations

- This is a five-task, three-repetition diagnostic, not a general productivity estimate or a statistically conclusive trial.
- The model and effort are pinned. Model drift or missing brain delivery is classified as infrastructure failure rather than a successful Engram trial.
- Claude-reported USD figures are usage estimates under a subscription, not incremental cash charges or billing receipts.
- Dependencies: Rust dependencies built offline and locked from each base Cargo.lock before timing, identical in both arms; agents-repository tasks have no installed Python dependencies. Both arms use the same prepared environment.
- Engram uses source-only historical indexes with automatic hooks, no accumulated learned memories, and no MCP tools. This does not measure the full second-brain configuration.
- Provider cache reads and cache creation are included in processed-token totals. Cache pricing differs, so processed-token reduction and estimated USD reduction need not agree.
- Derived completion classification uses revision `agent-completion-v2`. Raw records and behavioral grades are preserved; runtime errors are infrastructure outcomes and explicit budget exhaustion is failure unless infrastructure already invalidates the trial.
- Manifest runner/metrics hashes identify frozen collection-start sources. The separate analysis_provenance hashes identify the scripts used to generate this report; post-collection classification does not replace raw measurements.
- Reported usage from error or interrupted runs may be partial even when final usage fields are present. Measurement coverage counts reported values, not complete error-run billing. All reported values remain in all-attempt totals; none of these runs contributes to both-pass savings.
- Incomplete tool traces retain their observed call counts as lower bounds in all-attempt totals, with partial coverage marked. They are excluded from paired tool-call deltas.
- Private source, prompts, patches, tool inputs, raw grader output, session identifiers, and trace identifiers are excluded from these artifacts.
- Held-out historical tasks for the line-ranged pointer and related-use changes. They count as held out only after the operator confirms that none informed Engram development.
- Fixtures are rebuilt from the operator's repositories at the catalog base commits, and the graders are recalibrated against them. Compare Off/Brain within this collection only.
- Collected from a suite still marked draft_requires_operator_review (--allow-draft-suite), before the operator review its README requires.
- Provider prompt caching remains enabled and is not reset. Report all token categories; estimated USD is not subscription cash billing.
- Single agent, six core tools, source-only hooks. Does not measure learned memories, automatic judging, or MCP tools.
- Three repetitions per task are diagnostic, not statistically decisive.

Machine-readable evidence: [redacted results](results.json) and [aggregate summary](summary.json).
