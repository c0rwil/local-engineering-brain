# Compact-injection fidelity gate (Opus, family G2) — 2026-09-01

The 2026-09-01 capability A/B showed compact rendering at success parity
with −33% injected bytes, but the flagship fidelity result was measured
verbose, so the default flip was gated on a fidelity-family A/B.

Design: brain-only, t20/t22 + t21 (G2 tribal probes with mechanical style
verifiers), seeds 1-2, review-standards composition (the shipping
default), claude-opus-4-8, pinned identity.

## Result

| arm | fidelity passes | mean injected bytes |
|---|---|---|
| verbose | 6/6 | 15,788 B |
| compact | 6/6 | 16,155 B |

**No fidelity regression under compact rendering.** No byte reduction on
this family either — expected in hindsight: with
`review_standards_section` ON, preference memories already render as
one-line `- MUST:` entries, so the compact projection has nothing left to
shrink; the −33% capability-family saving comes from lesson blocks
(claim + rationale) collapsing. Byte impact is memory-mix-dependent;
fidelity impact is nil in both families.

## Decision

`context.compact_injection` now defaults **ON**: parity in both measured
families, strictly fewer tokens where memories are lesson-heavy, and the
explain path still references canonical text. Ablation remains one flag.
