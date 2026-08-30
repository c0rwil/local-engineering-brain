# Retrieval relevance root cause — 2026-08-30

Investigation of the seeds 1-3 per-task anomalies (t10 brain losses, t11
irreproducibility, brain-faster controls). Found and fixed two compounding
product defects; this doc records the causal chain because it reframes every
earlier capability result.

## Defects

1. `fts_query` truncated query terms ALPHABETICALLY to 24. Realistic tickets
   (~120 unique words) lost every late-alphabet domain term ("retry",
   "max_attempts", "exhausted"), so FTS matched only generic vocabulary and
   the lexical relevance component flattened to ~0.001 for every candidate.
   Short unit-test prompts never tripped it.
2. The context compiler capped injection at 3 memories PER KIND regardless
   of budget. With six same-kind lessons and relevance ranking reduced to
   noise, every task received the same arbitrary three lessons while >90% of
   the token budget went unused.

## What this explains

- t11's round-1 brain win: the arbitrary three included both lessons t11
  needs — luck, not retrieval. (Healthy-server reruns show t11 failing in
  BOTH arms: the task exceeds this model tier's 30-minute ceiling and
  cannot discriminate memory effects.)
- t10's brain losses: its family-D lesson was never injected; the brain arm
  carried three irrelevant lessons.
- Brain-faster controls: injection was a small constant orienting block with
  nothing at stake; per-run variance dominates wall time.

## Fix (migration 9, commit on main)

bm25/FTS demoted to candidate prefilter (6-doc corpora have flat IDF);
ranking lexical is now claim-term coverage of the task text (deterministic
light stemmer, length-weighted, [0,1]). Compiler selection is two-pass:
kind-diversity first, then score-order budget fill. Porter stemming on the
rebuilt FTS index. Golden regression test ranks the correct family lesson
for four real ticket texts; live seed-DB probes rank the correct lesson
FIRST for all six lesson-bearing tasks (t11 gets both of its lessons in the
top two).

## Measurement implication

All capability deltas measured before this fix tested "inject three
arbitrary validated lessons", not "inject relevant memory". The
post-fix diagnosis matrix (t07/t08/t10/t11/t14, seeds 4-5, instrumented
stream-json transcripts + per-arm proxy metrics) is the first honest test
of the product's central claim.
