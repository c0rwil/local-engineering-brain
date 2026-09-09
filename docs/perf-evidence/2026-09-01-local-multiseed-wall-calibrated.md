# Local multi-seed fidelity matrix, wall-calibrated — 2026-09-01

The final backlog item: family-G2 tribal-fidelity probes (t20–t24) on the
local shipping runtime (MTPLX Optimized-Speed Q4), seeds 2–3, paired
off/brain arms, 2700 s walls, pinned project identity, caffeinated, model
server warmed per the method rules.

## Result (seeds 2–3, 20 cells, all valid)

| arm | successes | mean wall |
|---|---|---|
| off | 7/10 | 1673 s |
| brain | **10/10** | **1123 s (−33%)** |

Per-task pairs (off → brain):
- t20 (metrics-counter preference): s2 pass/pass, s3 pass/pass
- t21 (in-file unit test): s2 **fail(2700 s timeout)**/pass(484 s), s3 pass/pass
- t22 (rejection names input): s2 pass(1442 s)/pass(448 s), s3 pass(2544 s)/pass(1197 s)
- t23 (lifecycle enum order): s2 **fail(2700 s)**/pass(575 s), s3 **fail(2043 s)**/pass(2605 s)
- t24 (metrics re-probe): s2 pass/pass, s3 pass/pass

## Integrity notes

- The original launch's warmup silently no-oped (`timeout` is not a command
  on macOS zsh) and the first cell (t20/off/s2) flamed out cold: 0 edit
  calls, 843 output tokens, its own stderr shows it booting the model
  server. It was re-run warm and **passed** (1090 s, 3 edits); the table
  uses the valid re-run. Scoring the flameout as a failure would have
  overstated the brain delta — disclosed and excluded instead.
- An activity sweep over all 20 transcripts confirms every counted failure
  did real work (3–6 edit calls): the off-arm failures are genuine, not
  infrastructure.
- The matrix was interrupted once by session teardown (background tasks die
  with the Claude Code process); the 13 remaining cells were re-launched
  nohup-detached with a fresh warmup. The interrupted t21/brain/s3 cell's
  partial artifact was discarded and the cell fully re-run.

## Reading

Combined with seed 1 (2026-08-31 wall-calibrated: brain 4/4 valid vs off
0/3), the local story across seeds 1–3 is **brain 14/14 valid vs off 7/13**,
with brain consistently *faster* (−33% mean wall here) because injected
tribal knowledge prevents the long exploratory spirals that burn the wall.
This matches the frontier-tier flagship (24/24 vs 5/12) in direction: at
frontier the gap shows up as fidelity; locally it shows up as both fidelity
and time-to-completion.
