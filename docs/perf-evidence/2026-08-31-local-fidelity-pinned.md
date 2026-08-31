# Family-G v2 fidelity at local tier, pinned identity — 2026-08-31

t20-t24 × seed 1 × off/brain, Speed model, caffeinated, pinned-identity
g2 snapshot. Injection verified per run: brain arms injected 35-50 KB per
session with near-perfect session-cache hit rates (16/17, 12/13, 11/12).

## Scoreboard

brain 3/5, off 1/5; brain mean wall 1363 s vs off 1659 s (−18%).
Brain wins: t20, t21 (off timed out both), t24 (both passed, brain 24%
faster). Both arms timed out on t22-brain/t23; off also timed out t22.

## The replay finding that reframes it

Transcript-replay of every failure shows FIVE of six produced patches
that pass BOTH the functional and style gates — the agents were done or
nearly done and the 1800 s wall expired before/during verification. Only
one failure (t23-off) was genuinely incomplete work. At local tier the
fidelity probes are therefore WALL-LIMITED, not competence-limited: the
slow model induces the unwritten conventions (as family-G v1 showed) but
barely fits task + compliance inside 30 minutes.

Interpretation: locally, memory's contribution to fidelity tasks is TIME
— the oriented arm finishes in time more often (3/5 vs 1/5) — while at
frontier tier its contribution is COMPLIANCE (Opus: 18/18 vs 2/6, speed
never in question). Same product mechanism, opposite bottlenecks.

Method note for future local fidelity runs: raise the wall (>=2400 s) or
score time-to-compliant-patch, otherwise verification races the wall and
success rates measure timing noise.

## Re-validation program: complete

With this run, every behavior claim in the evidence trail now rests on
verified-injection data at both tiers. Prior prefix-only results remain
annotated void. Recommended next defaults work: review-standards
rendering (best Opus arm), wall calibration for local fidelity, then
data-driven default flips with multi-seed confirmation.
