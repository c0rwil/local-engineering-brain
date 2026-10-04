# Findings — final, 24 of 24 attempts

The collection is complete: four historical repository tasks × three repetitions × Off/Engram under the repaired, frozen protocol, with zero infrastructure or unscorable outcomes. See the [report](README.md) for all measurements and exclusions, and the [collection audit](COLLECTION_AUDIT.md) for controls and the one operational pause.

**Result: the installed build does not improve overall effectiveness.** Correctness did not improve: Off produced 9/12 clean passes, Engram 8/12. Across the eight pairs where both modes passed, Engram used 15.5% fewer tool calls (509 → 430) but 17.2% more provider tokens (28.4M → 33.3M), a 15.7% higher usage-cost estimate and 2.1% more agent time. The effect depends on the task: status polling improved every time, while the guide and environment tasks became more expensive every time. These are ratios of paired sums, not averages of task percentages. Failed-attempt resources remain in the report. Cost estimates are not subscription cash charges.

| Task | Off | Engram | Both-pass pairs, Engram vs Off (sum over pairs) |
|---|---|---|---|
| Guide hot path | 3/3 pass | 3/3 pass | 29.0% more tokens (+15%, +28%, +44% by pair), 20.2% fewer calls, 8.1% more time. |
| Lean status polling | 3/3 pass | 3/3 pass | 5.6% fewer tokens (−4%, −0.3%, −14%), 15.1% fewer calls, 7.1% less time. |
| Environment isolation | 3/3 pass | 2/3 pass | Two eligible pairs: 61.8% more tokens (+127%, +33%), 1.5% fewer calls, 8.3% more time. Engram's first attempt missed two poisoned-lock recovery sites. |
| Onboarding outage | 0/3 pass | 0/3 pass | All six attempts failed the same 15 of 31 private checks. No savings comparison. |

All attempts, including failures: Off 33.7M tokens, 659 calls, $34.35 estimated; Engram 37.5M tokens (+11.3%), 544 calls (−17.5%), $38.04 (+10.8%). Per clean completion: Off 3.75M tokens and $3.82; Engram 4.69M tokens and $4.76.

## What drives the extra tokens

**Fewer reads, more text.** In the eight both-pass pairs, Engram made 28% fewer native Reads (128 versus 178) but received 74% more Read text (1.04M versus 0.60M characters) and 45% more tool-result text overall. It issued more whole-file Reads without an offset or limit (26 versus 16) and hit the 256 KB Read limit more often (4 versus 1; 6 versus 1 across all 24 attempts). By task, Read text rose 68% for the guide task, 216% for environment isolation and 38% for status polling, where fewer calls still reduced total tokens.

**The added cost is carried context, not the injected context.** Output tokens were unchanged (267,585 versus 267,525). The difference is input: cache reads rose 17.1% and cache creation 28.7%, consistent with larger tool results staying in the prompt for every later turn. Engram's own injected text is small. Across its 12 runs, 323 compilations produced 22 deliveries (12 initial, 10 with changed source sets), with no byte-identical or order-only repeats, totalling about 8,200 estimated tokens. That is roughly 680 per run against about 3M processed tokens per run. The costly part is how agents follow the pointers.

This is an association across 8 pairs, not a controlled causal test. It is the most consistent pattern in the collection, and it explains both the call reduction and the token increase.

## Other findings

**Finding a helper is not the same as covering all affected sites.** Engram's first environment attempt fixed the shared settings helper but missed two later lock users in the same contract-test file. Its source map named the helper, not those functions. A whole-file Read exceeded the size limit, and narrower reads never reached them. Its second and third attempts received the same initial context and passed after searching every lock reference. This is variation in exploration, not evidence that Engram caused the omission.

**Passing self-written tests does not establish task completion.** Every onboarding attempt, in both modes, kept an outage policy that breaks welcome-page usability, and all six failed the identical private checks. Several passed their own new tests, which encoded the wrong behavior. Throwing still denies gated access, so this is not a fail-open security finding. Engram's context contained source pointers, not the conflicting policy; the policy came from baseline code comments and tests.

## Next experiments

1. **Line-ranged source pointers.** Render indexed definitions as `path:start-end` so agents read the relevant span instead of whole files. This targets the measured mechanism. Success means equal or better correctness, with lower Read text and cache-read tokens, not merely fewer calls.
2. **Related-use coverage cue.** Point agents at other users and consumers of a changed definition, targeting the environment-task miss.

Test each change separately on held-out tasks, with correctness first and all token categories reported.

## Scope

These development tasks informed earlier retrieval work; they are not held-out evidence. Source-only hooks do not measure learned memories, the local judge or MCP tools. Three repetitions per task are diagnostic, not statistically decisive. Provider prompt caching and normal host background load were not reset. No trial was replaced because it failed. The earlier four-attempt collection was invalidated by shell-launch failures and remains separately archived; its resource use is preserved outside this comparison.
