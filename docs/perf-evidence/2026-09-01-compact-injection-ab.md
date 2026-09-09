# Compact-injection A/B (Opus, brain-only) — 2026-09-01

Question: does the deterministic compact rendering (`context.compact_injection
= true`) preserve outcomes while shrinking the injected payload?

Design: brain arm only, both renderings, tasks t07/t09/t13 (capability probes
with meaningful memory payloads), seeds 1-2, claude-opus-4-8 via the LEB
proxy, pinned project identity, configs `opus-compact-verbose.yaml` /
`opus-compact-compact.yaml`.

## Result

| arm | runs | successes | mean wall | mean injected bytes |
|---|---|---|---|---|
| verbose | 6 | 6 | 54 s | 15,808 B |
| compact | 6 | 6 | 52 s | 10,549 B |

Per-run injected bytes (verbose → compact): t07s1 15,808→11,776; t07s2
29,640→13,248 (−55%); t09s1 11,856→10,304; t09s2 11,856→8,832; t13s1
13,832→8,832; t13s2 11,856→10,304.

## Verdict

**Success parity at −33% mean injected payload** (up to −55% on the heaviest
run). Wall delta is noise at frontier tier; the byte reduction is the lever
that matters for local prefill cost. Default stays **opt-in** for now: the
flagship fidelity result (2026-08-31, 24/24) was measured with verbose
rendering, and preference/fidelity memories are the most compressible — a
fidelity-family (G/G2) compact A/B must pass before flipping the default.
