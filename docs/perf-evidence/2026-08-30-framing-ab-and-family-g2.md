# Framing A/B (null) and Family-G v2 (tribal probes) — 2026-08-30

## Injection framing A/B: NULL — reference stays default

t07/t10/t14 × seeds 6-7, reference vs directive heading, brain arms
compared across configs (off arms as replicate controls):

- reference brain 5/6 (t07 2/2 at 675/805 s, t10 1/2, t14 2/2)
- directive brain 4/6, consistently slower on t07/t14
- off arms swung 5/6 vs 4/6 across identical configs — session variance
  is the same order as the framing delta, so no framing effect is
  detectable at this n, and the direction is unfavorable. The
  "deliberation is caused by the preamble stance" hypothesis is dead;
  the deliberation cost tracks INJECTION VOLUME, not wording.

## Family-G v2 (undocumented tribal preferences): tie with structure

t20-t24 × seed 1: off 4/5, brain 4/5, brain mean wall +21%.

1. **Induction ceiling, honestly measured**: off-arm patches genuinely
   exhibit the unwritten conventions (t20-off bumps the metrics counter,
   t22-off interpolates the offending id). On a 15-file fixture the
   exemplars sit next to every edit site; a competent model induces the
   house style without memory. Fixture SIZE is now the known confound —
   tribal-knowledge payoff should scale with exemplar distance, which a
   small fixture cannot represent.
2. **Re-encounter acceleration is real**: on G6's second task (t24) the
   brain arm was 2.1x FASTER than off (776 vs 1616 s) with 8 fewer tool
   calls, and same-calls-half-time vs its own first encounter (t20).
   Memory of a convention family pays on repetition, not first sight.
3. **First-encounter tax persists**: brain 2.2-2.4x slower on t20/t22,
   and its one failure (t21) was a mid-work wall timeout on the
   test-writing preference — the heaviest compliance obligation.
4. **Session pinning verified in production numbers**: every brain run
   shows exactly 1 session-cache miss and 13-24 hits; the injected block
   was byte-stable across whole agentic sessions (6.8-12.4 KB total).

## Next lever under test

`context.max_injected_memories = 2` (selective) vs uncapped (inclusive)
on the v2 first-encounter tasks: if the tax is volume-driven, selectivity
should cut brain wall time without costing the style passes.
