# Family-G v1 (documented conventions) — 2026-08-30

10 runs: t15-t19 × seed 1 × off/brain, Speed model, fresh server,
caffeinated, brain arm seeded with the five preference memories.

## Result: off 5/5, brain 3/5 — memory lost to documentation

Every off-arm run passed BOTH the functional gate and the style verifier.
Transcripts show why: the fixture documents its conventions in a README
"Code conventions" section (added for discoverability fairness), the
prompts hint that CI enforces them, and the off-arm agent simply read the
section and complied (t16-off: one README read, explicit citation of the
lowercase-error rule in its reasoning). The brain arm's two failures were
wall-limit timeouts (t16, t19), not style violations.

## Interpretation — a sharpening, not a defeat

The v1 probes measured "documented-conventions compliance", which a
competent agent achieves without memory. The product's fidelity claim
lives one level deeper: TRIBAL knowledge — standards exhibited in the code
and enforced in review but never written down. v1 accidentally proved the
control condition works: when knowledge IS written down, LEB adds latency
without lift on style adherence. That belongs in the product's honest
positioning: LEB is for what the docs don't say.

## Family-G v2 design (queued)

Same mechanical verifiers and discrimination-proof validation, but:
preferences exhibited ONLY in existing code (never documented), prompts
hint only that "the team's review standards apply", and the off arm can
still pass by induction from examples — harder, fair, realistic.

Raw: ~/lebbench-family-g/.
