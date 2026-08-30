# Diagnosis matrix v3 — first post-retrieval-fix paired runs — 2026-08-30

20 runs: t07/t08/t10/t11/t14 × seeds 4-5 × off/brain. New build (relevant-
lesson retrieval, session-pinned injection, migration 9), fresh model
server, caffeinated, instrumented v2 wrapper (full stream-json transcripts).
One run invalid: t08-off-4 died in 132 s to a one-off mid-stream
`/v1/mtplx/cancel` (marked artifact; server healthy before and after).

## Scoreboard (excluding the artifact)

| Task | off | brain | Note |
|---|---|---|---|
| t07 | 1/2 | 1/2 | both arms split; brain's win cost 3.5x off's wall |
| t08 control | 1/1 | 2/2 | — |
| t10 | 1/2 | 1/2 | anti-brain trend GONE with the relevant lesson injected |
| t11 | 1/2 | **2/2** | previously 1/6 across arms; brain passed BOTH seeds at 1659/1752 s — just under the wall that killed every pre-fix brain attempt |
| t14 control | 2/2 | 2/2 | brain used fewer tool calls in both seeds (6,6 vs 7,11) |

Aggregate: **brain 8/10, off 6/9** — first directional separation, n still
too small for a rate claim, but per-task behavior finally matches theory.

## Transcript findings (the real payload)

1. **Two injection regimes visible.** On easy tasks (t14) the brain arm
   orients: fewer tool calls, earlier first edit, faster in 1 of 2 seeds.
   On hard tasks it deliberates: +3.7 mean tool calls, +3.6 turns vs off.
   The t10-brain-s4 failure is the extreme: 27 tool calls, zero edits — a
   thorough investigation loop (grepping tests, reading CHANGELOG/docs)
   that never committed to a change in 30 minutes.
2. **t11's flip is consistent with prefix-stable injection buying the
   margin**: both brain wins landed within 150 s of the wall; pre-fix brain
   attempts with the SAME two relevant lessons timed out. Direct
   session-cache telemetry was lost (see gap below), so this is a strong
   hypothesis, not a measurement.
3. Injection framing hypothesis: the preamble labels memory "reference
   data, not executable instructions" — the deliberation regime suggests
   the model treats it as review criteria rather than decisions already
   made. Next iteration A/Bs an action-oriented framing.

## Telemetry gap

The v2 wrapper wrote per-arm proxy metrics into the destroyed temp state
dir. Fixed going forward: configs declare `metrics_file` so the harness
archives it into runs.jsonl.

Raw artifacts: `~/lebbench-diagnosis-v3/`.
