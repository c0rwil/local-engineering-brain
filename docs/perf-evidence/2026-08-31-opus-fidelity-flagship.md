# Tribal-knowledge fidelity on Opus 4.8 — flagship result — 2026-08-31

Family-G v2 probes (t20/t21/t22 × seeds 1-2), Claude Code + claude-opus-4-8
through the LEB proxy, family-g2 snapshot seeded via the real
episode→learn→promote pathway under a PINNED project identity, per-run
injected-bytes telemetry archived in runs.jsonl.

## Result

| Arm | Style passes | Mean wall | Mean injected/session |
|---|---|---:|---:|
| off (no injection) | 2/6 | fails ran long | 0 B |
| brain, inclusive (4 prefs) | **6/6** | 57 s | ~22 KB |
| brain, selective (cap 2) | **6/6** | 71 s | ~14.6 KB |
| brain, review-standards section | **6/6** | **50 s** (38-59) | ~14.3 KB |

t20/t21 — 0-for-10 across every earlier Opus brain arm and 0-for-4 in the
off baseline — pass 12-for-12 across the three real-injection arms. Failure
-stage replay had already shown all off-arm failures are pure style
failures (functional passes, tribal-rule violations). This is the product's
central claim observed end-to-end: memory of unwritten team standards turns
a fast frontier agent from style-blind to fully compliant at negligible
wall cost.

Recommended composition from this data: preference memories rendered as the
review-standards section (fastest, tightest variance, ~35% fewer bytes than
inclusive); selectivity cap optional. Defaults stay unchanged pending local
-tier re-validation and more seeds.

## Retroactive integrity note — supersedes earlier brain-arm behavior claims

Injected-bytes telemetry exposed that every paired matrix BEFORE identity
pinning ran its brain arms with prefix-only injection (~0.5 KB/request, no
memories): project identity fractured across repo copies because clones
carry different local-path remotes, and the seeded snapshot's project id
was never resolved by the arms. Affected: rounds 1-2, patch reruns,
diagnosis-v3, family-G v1/v2 local, framing A/B, local selectivity A/B.
Mechanism-level findings from those runs stand (retrieval relevance fix,
session pinning, harness/socket/env fixes, failure-stage taxonomy); their
brain-vs-off BEHAVIOR deltas are void and are being re-measured. The
framing-A/B "null" in particular was measured with nothing injected and
says nothing about framing.

Identity contract going forward: benchmark fixtures and seed scripts pin
`.leb/project.toml`; the clone-identity limitation is documented in
CLAUDE.md and regression-tested in src/projects.rs.
