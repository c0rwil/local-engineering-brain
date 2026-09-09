# Local-tier re-validation with pinned identity — 2026-08-31

t07-t14 × seeds 1-2 × off/brain, Speed model, fresh server, caffeinated
(power-manager assertion verified mid-run), full-snapshot re-seeded under
the pinned project id. Injection integrity pre-verified by reproduction
(repo copy + snapshot → all 6 lessons injected, t10's prompt ranking its
family lesson first); per-run injected-bytes telemetry was not archived
for this config (metrics_file absent in the legacy task blocks) — noted,
not repeated.

## Result: first local-tier separation, both axes

| | off | brain |
|---|---|---|
| Successes | 10/16 (62.5%) | **12/16 (75%)** |
| Mean wall | 1016 s | **874 s (−14%)** |

Per-task: brain is never worse than off on any task, and the wins align
with the seeded lesson families:

- **t07 (family A): brain 2/2, off 0/2** — both off arms timed out; brain
  finished at 1464/1140 s. The matrix's entire success-rate delta.
- t09 (family C): both pass; brain faster both seeds (721/804 vs 1184/1022).
- t13 (family E): both pass; brain faster both seeds (680/581 vs 889/1028).
- t10, t11: fail in both arms — above this tier's ceiling; no separation.
- Controls (t08, t14): 8/8, walls within noise — no stale-injection harm.

Attribution caution: brain transcripts do not verbally cite the lessons;
the gains are consistent and family-aligned at n=2 seeds, with injection
mechanically verified — directional evidence, not per-run causal proof.

## Cross-tier story (with perf-evidence 2026-08-31 Opus flagship)

- Frontier tier: memory buys STYLE FIDELITY (18/18 vs 2/6) at ~zero wall
  cost — fast models skip unwritten conventions without it.
- Local tier: memory buys COMPLETION AND SPEED on lesson-relevant tasks
  (+12.5 pp success, −14% wall) — slower models benefit from orientation.
- Both with the same snapshot pathway, honest controls, and verified
  injection. The prefix-only-era numbers are superseded.
