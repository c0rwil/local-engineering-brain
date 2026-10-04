# What this pilot establishes

Engram helped on some of these tasks, but this pilot does not establish broad
token savings or consistently expert repository context. All 24 authorized Claude
Max attempts completed: four historical tasks, three repetitions, Off and
source-only Brain. See the [full measurements](README.md).

## Correctness

Of twelve matched pairs, four have a clean Brain pass and an Off failure, one has
an Off pass and a Brain failure, one has both passing, and six are unscorable.
Neither arm receives a correctness win from an unscorable pair.

The strongest repeated observation is environment isolation: Brain passed all
five checks in all three repetitions; Off passed three of five each time,
leaving two poisoned-lock acquisitions unrepaired. The status-polling task is
mixed: Brain missed a required consumer in the first two repetitions and fixed it
in the third. The [grading audit](GRADING_AUDIT.md) links these observations to
saved candidates and logs; it does not attribute a fix to a particular pointer.

Five individual attempts have verified grader-contract ambiguities, and one
has a provider runtime error. Original grades and resource measurements remain
intact. Ambiguous requirements are excluded from effectiveness claims rather
than changed into passes. The graders need revised contracts and alternative
implementation preflights before another collection.

## Completed-work resource comparison

Only guide retrieval, repetition 2, has clean passes in both arms:

| Measurement | Off | Brain | Brain change |
| --- | ---: | ---: | ---: |
| Provider processed tokens | 5,197,522 | 3,423,249 | −34.1% |
| Model-issued tool calls | 75 | 58 | −22.7% |
| Agent seconds | 566.385 | 573.584 | +1.3% |
| Provider USD estimate | 4.4812 | 3.2863 | −26.7% |

This is one pair, not an overall savings estimate. Processed tokens include
cache reads and writes; dollar estimates are not subscription cash charges.
Provider caching was enabled and not reset between trials.

Across all attempts, Brain used 542 tool calls and 5,679.4 agent seconds; Off
used 561 calls and 5,138.8 seconds. Those totals include unsuccessful and
unscorable work, and Off's reported provider usage is partial. They do not support
a general speedup or a clean cost comparison. Setup, indexing, preflights and
external grading are reported separately.

## Context quality

The [delivery audit](CONTEXT_AUDIT.md) verifies all 36 emitted packs across twelve
Brain attempts. Of 432 delivered source-pointer occurrences, 278 repeat earlier
pointers within the same attempt (64.4%). Twelve packs merely reorder the prior
source set, accounting for 4,335 estimated emitted context tokens. These are
context estimates, not measured provider savings from a hypothetical fix.

The retrieval audit also finds missing required definitions, loss of identifiers
late in long prompts, and absent field-to-owner lookup. Some successful and
unsuccessful attempts received identical context, so pointer omissions alone do
not explain correctness differences.

The [next implementation tasks](NEXT_ITERATION.md) prioritize preserving precise
identifiers across the whole prompt, finding fields through their owner types,
and suppressing unchanged reordered source packs. Each has acceptance checks
and measurements. Adding another model is not required to test those mechanisms.

This pilot disables accumulated memories, automatic learning, the local judge,
linked-repository retrieval and MCP. It tests source context delivery, not the
complete engineering-memory configuration. A future evaluation needs repaired
graders and held-out tasks before any broader effectiveness claim.

The [native tracking verification](SYSTEM_VERIFICATION.md) records installation,
test results, live numeric cross-checks, repeat-import behavior, and browser QA.
