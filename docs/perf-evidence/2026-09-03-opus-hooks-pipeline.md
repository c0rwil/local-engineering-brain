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
{
  "benchmark_id": "conveyor-g2-opus-hooks-v1",
  "model_hash": "anthropic-api-claude-opus-4-8-via-hooks",
  "config_hash": "d200deebdd4ff3f5b5c9313affe09e73ab4bfa9231196f469f15d102630ee626",
  "paired_runs": 10,
  "off": {
    "mode": "off",
    "runs": 10,
    "successes": 6,
    "success_rate": 0.6,
    "mean_wall_ms": 116931.6
  },
  "brain": {
    "mode": "brain",
    "runs": 10,
    "successes": 10,
    "success_rate": 1.0,
    "mean_wall_ms": 94604.2
  },
  "success_rate_delta": 0.4,
  "mean_wall_time_delta_pct": -19.094410749532212,
  "generated_at_ms": 1788484351515
}
```
