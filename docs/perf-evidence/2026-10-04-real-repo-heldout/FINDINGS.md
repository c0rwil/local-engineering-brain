# Findings — held-out suite, 30 of 30 attempts

The collection is complete. Five held-out historical repository tasks were each run three times with Engram off and on, under a frozen protocol. Every attempt was graded and none was unscorable. One extra attempt failed before the agent started, because the Claude Code login had expired. It is kept outside the matrix as an infrastructure attempt and its slot ran again. See the [report](README.md) for every measurement and exclusion. The suite was still a draft awaiting operator review, so its held-out status is provisional.

**Result: equal correctness, less effort, slightly more tokens.**

- **Correctness:** both arms passed every attempt (Off 15/15, Engram 15/15), so all 15 pairs passed in both arms.
- **Effort:** across those pairs Engram used 20.1% fewer tool calls (532 → 425) and 15.9% less agent time.
- **Tokens and cost:** Engram processed 4.0% more provider tokens (19.5M → 20.3M), and the usage-cost estimate was 4.0% higher ($23.28 → $24.22).

In the [v2 development-task study](../2026-09-25-real-repo-followup/FINDINGS.md), Engram used 15.5% fewer calls, 17.2% more tokens and 2.1% more time, and passed one task fewer. The task sets and builds differ, so this comparison describes two studies, not the effect of one change.

| Task | Off | Engram | Both-pass pairs, Engram vs Off (sum over pairs) |
|---|---|---|---|
| Effect-check note only when enforcing | 3/3 pass | 3/3 pass | 17.5% more tokens, 26.5% fewer calls, 20.0% more time. |
| Failure analytics carry the frozen origin | 3/3 pass | 3/3 pass | 15.1% more tokens, 4.6% fewer calls, 18.2% less time. |
| Scheme-less navigation defaults to https | 3/3 pass | 3/3 pass | 3.5% fewer tokens, 14.4% fewer calls, 32.7% less time. |
| Play base-plan ids at every plan lookup | 3/3 pass | 3/3 pass | 19.6% fewer tokens, 17.5% fewer calls, 19.7% less time. |
| Truncate prompt fields on character boundaries | 3/3 pass | 3/3 pass | 39.5% fewer tokens, 54.4% fewer calls, 25.7% less time. |

By token category over the 15 pairs:

| Category | Engram vs Off |
|---|---|
| Output | 12.2% fewer |
| Cache reads | 3.6% more (19.2M against 18.5M) |
| Cache creation | 20.1% more |
| Ordinary input | Fewer, but under 0.01% of tokens |

Cache reads make up 95% of processed tokens. Estimated costs are not subscription cash charges.

## The line-range hypothesis did not hold

The changes measured here aimed for fewer Read bytes and fewer cache-read tokens at equal or better correctness. Correctness was equal. On the other two measures:

- **Read text:** Engram's arm received 63.9% more Read text (1.18M against 0.72M bytes) in fewer Read calls (137 against 148).
- **Cache reads:** 3.6% more.

80.3% of the 381 pointer lines Engram delivered named a line range, and 45 were related-use pointers.

**The Off arm already reads in ranges.** Claude Code 2.1.212 on this model named an offset or limit in 129 of its 148 Reads (87%). A ranged pointer cannot make reading ranged when it already is.

**The extra text comes from pointed files read whole.** These counts come from the private traces, reproduce the report's totals, and only the counts are published.

- **Pointed files:** 94 of Engram's 137 Reads (69%) opened a file that a delivered pointer had named. They returned 907K bytes, 77% of its Read text, averaging 9.6 KB per Read against 4.9 KB per Read in Off.
- **Whole-file reads:** 22 of those 94 Reads had no offset or limit and returned 559K bytes. That is more than the whole 461K-byte excess over Off. Off read 19 files whole, for 326K bytes.
- **Ranged reads:** when Engram's agent read a pointed file by range, it asked on average for four times the span the pointer named.

The measured build is `cf474b5` (release `71df87f2`). It predates two later changes that target this pattern: file briefs that show checked line spans and a size line for large files (`815f73e`), and briefs ordered by task relevance (`43e7b8b`). The next run should measure the current build on the same suite. If those changes work, whole-file Reads of pointed files will fall.

## What the improvement is

With Engram's source context the agent made fewer tool calls in all five tasks and finished faster in four. Processed tokens are dominated by cache reads, which grow with the number of turns times the size of the carried context. Fewer turns saved tokens, while larger Reads and Engram's own deliveries added them: 28 deliveries in 15 attempts, from 252 compilations. The net was 15–18% more tokens in two tasks and 4–40% fewer in three. The call savings match v2; the time savings are new. This is an association over 15 pairs, not a controlled causal estimate.

## Limits of this result

**Correctness.** Every attempt in both arms passed, a ceiling, so this suite cannot show a correctness gain or loss. A suite with tasks that the Off arm sometimes fails is needed for that.

**Learned memories.** This measured source-only hooks with no learned memories and no MCP tools, as in v2. It does not test lessons from earlier work, which is the core claim of the spec.

## Next experiments

1. Rerun this suite on the current build, which adds size lines and task-ranked briefs. Report whole-file Reads of pointed files alongside the paired table.
2. Make pointers to large files say how large they are and which span to read, and measure the bytes agents read after following them.
3. Add held-out tasks that Off does not always solve, so correctness can move.
4. Measure learned memories: seed validated lessons from earlier tasks in the same repositories, then compare brain-with-lessons against source-only and Off.

## Scope

- **Suite:** five tasks, three repetitions each, a diagnostic rather than a statistically decisive trial. The suite awaits the operator review its README requires, including confirmation that no task informed Engram development.
- **Pinned setup:** one agent (Claude Code 2.1.212) with `claude-opus-4-8[1m]` at high effort; fixtures rebuilt from the operator's repositories at each base commit; provider prompt caching left on.
- **Denominators:** no trial was replaced because it failed. The infrastructure attempt is reported apart from the matrix.
- **Evidence ledger:** the 30 records were imported as `real-repo-heldout-2026-10-04`.
