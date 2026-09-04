# Hook-pipeline fidelity on Opus 4.8 — five injection moments — 2026-09-03

First benchmark of the production HOOK pipeline (SessionStart brief,
UserPromptSubmit context, PreToolUse file shapes, PostToolUse evidence,
SessionEnd auto-close) replacing the proxy as the delivery path. Family G2
undocumented-preference probes (t20-t24 × seeds 1-2), stock Claude Code +
claude-opus-4-8, per-arm brains from family-g2-snapshot.sqlite under pinned
project identity.

## Result

| Arm | Passes | Success rate | Mean wall |
|---|---:|---:|---:|
| off (stock Claude Code, disableAllHooks) | 6/10 | 60% | 116.9 s |
| brain (five-moment hook pipeline) | **10/10** | **100%** | **94.6 s (−19%)** |

Per-probe: the control fails 0/4 on t20 (terminal transitions bump their
Metrics counter) and t21 (behaviour changes ship an in-file unit test) —
the two preferences least inducible from surrounding code — and passes
t22/t23/t24. The brain arm passes everything, faster on 8 of 10 pairs.

Consistent with the 2026-08-31 proxy-era flagship (control 5/12 on the
t20-t22 subset); the hook pipeline reproduces the fidelity effect at
lower wall cost with no proxy in the path.

## Method

- Brain arm: the arm repo's `.leb/config.toml` routes the machine's
  user-level hooks at the arm's isolated daemon — the exact production
  mechanism, per-arm brains, no bench-specific injection code.
- Off arm: `.claude/settings.json` sets `disableAllHooks` — verified by
  transcript probes to fully suppress user-level hooks (and, positively
  controlled, that WITHOUT the flag user-level hooks inject headless).
- Clean environment for the agent (`env -i`): no inherited ANTHROPIC_*/
  CLAUDE_* variables reach either arm.
- Injection telemetry archived per brain run (e.g. t20-brain-1:
  injected_tokens 556, context_compiles_brain 1); a brain arm that
  injects nothing is a broken arm, not a data point.
- Harness: `leb eval run bench.g2-opus-hooks.yaml`, benchmark_id
  conveyor-g2-opus-hooks-v1, artifacts under
  `leb-bench-target/g2-opus-hooks-20260903-173715`. LEB commit b33105a.

## Raw summary

```json

```
