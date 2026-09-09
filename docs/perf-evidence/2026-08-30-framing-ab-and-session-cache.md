# Framing A/B + session-cache confirmation — 2026-08-30

24 runs: t07/t10/t14 × seeds 6-7 × off/brain, once per framing (reference,
then directive), caffeinated, metrics archived via `metrics_file`.

## Session-pinned injection: CONFIRMED LIVE

Every brain run shows exactly 1 session-cache miss (the first compile) and
N hits — e.g. 16 hits / 1 miss over a 17-request t10 session. The injected
block is compiled once and byte-stable for the whole agentic session, which
is what lets the runtime prefix cache hold. This closes the hypothesis left
open by diagnosis-v3. (Metrics are missing for wall-limit-killed runs: the
harness kills the process group before the wrapper's capture runs — known
limitation, noted for interpretation.)

## Framing verdict: no measurable difference; reference stays default

Brain arm: reference 5/6, directive 4/6 — but the directive block ran
second on an aging model server and BOTH its arms show inflated walls
(off t07: 1773 s + a timeout vs reference's 513/698 s), so the comparison
is confounded by server state, not framing. Additionally, the deliberation
signature that motivated the A/B (+3.7 brain tool calls in diagnosis-v3)
did NOT replicate at seeds 6-7: brain used FEWER mean tool calls than off
in BOTH framings (13.7 vs 14.2; 16.5 vs 18.0). Verdict: keep `reference`
as default; the feature stays as an ablation lever; a properly interleaved
rerun is queued behind higher-value work. Process rule added: restart the
model server BETWEEN benchmark configs, and interleave A/B blocks.

## Cumulative post-fix scoreboard (all paired runs, artifacts excluded)

brain 17/22 (77%) vs off 15/21 (71%) — directionally positive, not yet
significant; the dataset keeps growing with each matrix.

Raw: ~/lebbench-framing-ref/, ~/lebbench-framing-dir/.
