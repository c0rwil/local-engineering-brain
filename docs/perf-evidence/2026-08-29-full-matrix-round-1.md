# Paired benchmark, full matrix round 1 — 2026-08-29

Config `c7233b22…`, model `mtplx-2.9.2-qwen3.8-27b-optimized-speed-q4-turbo-medium`,
8 paired tasks, seed 1, wall 1800 s, `--max-turns 60`. Brain snapshot carries
six validated lessons (all six trap families) recorded through the real
episode/promotion pathway. Same isolation discipline as the pilot.

## Aggregate

| Mode | Successes | Success rate | Mean wall |
|---|---:|---:|---:|
| off | 7/8 | 0.875 | 870 s |
| brain | 7/8 | 0.875 | 1086 s (+24.8%) |

Success-rate delta: 0.0 at n=8, seed 1. **No aggregate claim is supported.**

## Per-task

| Task | Family | off | brain |
|---|---|---|---|
| t07 paused flag | A re-encounter | pass 740s | pass 1479s |
| t08 control | fresh | pass 681s | pass 348s |
| t09 snapshot drain | C re-encounter | pass 1008s | pass 1455s |
| t10 retry budget | D re-encounter | **pass 1038s** | **timeout** |
| t11 cancel-by-key | B+F compound | **timeout** | **pass 1739s** |
| t12 jitter field | A re-encounter | pass 482s | pass 833s |
| t13 deadline round-up | E re-encounter | pass 858s | pass 827s |
| t14 control | fresh | pass 355s | pass 210s |

## Observations (hypotheses, not conclusions)

1. The two discriminating pairs split 1–1. The brain win landed on t11 — the
   suite's designed strongest known-bad-repeat probe — and the brain arm's
   final report explicitly reasons about the exact hidden acceptance trap
   (normalizing both sides of the dedupe-key comparison for a padded
   uppercase lookup) and cross-references the seeded family episodes. The
   off arm timed out. This is the effect the product predicts, observed
   once; it needs replication across seeds before it counts.
2. The brain loss (t10) was a timeout, not a wrong answer that verification
   caught. Whether injection distracted the agent or the task is
   wall-limited needs the run transcript studied and more seeds.
3. Injection costs real wall time: +25% mean. Off arms never received
   context; brain arms carry ~6 lessons plus stable prefix per request on a
   ~25 tok/s local model. Both controls were *faster* in the brain arm,
   suggesting per-task variance dominates the mean at n=1 seed.
4. No harmful stale injection on either control.

## Next measurement

Seeds 2 and 3 over the same 8 tasks (32 more runs) to estimate variance;
study the t10 brain transcript; consider budget-tuning injected tokens for
speed-sensitive tasks. Raw artifacts: `runs.jsonl`, `comparison.{json,md}`
under the operator's `~/lebbench-full/` output directory.
