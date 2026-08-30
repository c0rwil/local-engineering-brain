# Paired benchmark, seeds 2-3 — 2026-08-29/30

Same 8 tasks and pairing discipline as round 1 (`full.yaml`, wall 1800 s,
local MTPLX Speed model, pre-upgrade LEB binaries held constant all night).
32 runs. **Raw aggregate is NOT usable as-is** — two disclosed defects below.

## Integrity disclosures

1. **Mid-matrix fixture drift (integrator error).** Merging the family-G
   probe suite fast-forwarded the fixture repo (`6bd762a` → `bfc3c87`,
   additive commits; all 14 original tasks re-validated as solvable at the
   new HEAD). Runs from ordinal 17 (t11, seed 2) onward copied the new
   HEAD. Results are therefore reported per-fixture-segment, not pooled
   with round 1. Rule going forward: the fixture repo is frozen while any
   matrix references it; benchmark configs should pin a fixture commit.
2. **Model-server memory-pressure incident (ordinals 17-21 contaminated).**
   After ~20 long runs, MTPLX's RAM session bank (~7 GB) plus desktop apps
   drove the machine to heavy memory pressure; MTPLX deferred generation
   60 s, exceeding Claude Code's stream-idle timeout. Ordinals 18-21 died
   in 6-84 s without doing work; ordinal 17 ran full-length on the degraded
   server. All five are excluded as invalid. Fixed by restarting the model
   server (fresh bank; TTFT back to sub-second) and re-running the affected
   pairs on the same binaries (patch evidence to follow).

## Valid per-task results, seeds 2-3

| Task | off s2 | brain s2 | off s3 | brain s3 |
|---|---|---|---|---|
| t07 paused flag | pass 1381s | pass 1156s | **fail 750s** | pass 1277s |
| t08 control | pass 839s | pass 563s | pass 374s | pass 503s |
| t09 snapshot | pass 571s | pass 576s | pass 702s | pass 1126s |
| t10 retry budget | pass 1129s | pass 869s | pass 1511s | **timeout** |
| t11 compound | invalid | invalid | invalid | invalid |
| t12 jitter | pass 774s | invalid | pass 437s | pass 596s |
| t13 deadline | pass 796s | pass 1137s | pass 708s | pass 814s |
| t14 control | pass 253s | pass 416s | pass 281s | pass 199s |

Valid-run tallies: off 13/14, brain 12/13. No aggregate separation.

## Emerging per-task signals across all seeds so far (hypotheses)

- **t07: first genuine off-arm capability failure** (seed 3, wrong solution
  at 750 s) while brain passed all three seeds. Family-A re-encounter; the
  serde-default lesson is in the snapshot. 3/3 vs 2/3, brain ahead.
- **t10: brain arm now 1/3 vs off 3/3** (r1 timeout, s2 pass-and-faster,
  s3 timeout). If the pattern holds it suggests injection cost/distraction
  on this task specifically; the t10 transcripts are preserved this time.
- **t11: still only round 1's single data point (brain win)** — the seeds
  2-3 pairs were destroyed by the incident; patch reruns in progress.
- Controls (t08, t14): 8/8 across arms, no stale-injection harm; brain was
  faster than off in 5 of 8 control pairs — the +25% round-1 wall-cost
  claim does not survive more sampling on this build.

Raw artifacts: `~/lebbench-full-seeds-2-3/` (runs.jsonl, comparison.json,
comparison.md, full per-run transcripts including both t10 brain arms).
