# Local fidelity, wall-calibrated (2700 s) — 2026-08-31

t20-t24 × seed 1 × off/brain, Speed model, pinned identity, recommended
defaults (review-standards rendering ON — this run is also its first
local-tier outing). Raw: brain 4/5, off 0/5; brain mean wall 1228 s vs
off 1578 s.

## Honest per-run classification (transcript replay + artifacts)

- THREE no-edit flameouts clustered in the first four runs (off-t20 301 s,
  brain-t20 484 s, off-t21 537 s): sessions with ~500 output tokens and
  zero edits, both arms affected, immediately after a model-server
  restart. This is a COLD-START run-quality artifact, not memory signal.
  Method rule going forward: fire one disposable warmup task at the server
  before any matrix.
- Valid runs: brain 4/4 (t21 2507 s, t22 474 s, t23 1634 s, t24 1039 s
  — all functional+style gates); off 0/3 (t22 wall timeout, t24
  functional-fail at wall, t23 completed-but-failed verification in
  harness; replay of t23 diverges from the harness verdict and is
  recorded as inconclusive rather than claimed either way).
- Longer walls confirmed the earlier diagnosis: with time pressure eased,
  off-arm failures shift from "correct but late" to a mix of timeout,
  functional, and verification failures, while the brain arm completes
  well inside the wall (3 of 4 valid brain runs under 1700 s).

n remains small and one brain flameout is excluded alongside off's two;
the directional read — oriented arm finishes and complies, memoryless arm
struggles to do both within any reasonable wall — is consistent with
every post-pinning result at both tiers.
