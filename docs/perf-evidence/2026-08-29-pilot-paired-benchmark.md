# Paired benchmark pilot — 2026-08-29

First real-model paired evaluation of the central claim, against the
`conveyor` fixture suite (leb-bench-target). Recorded honestly: this pilot
establishes the measurement pipeline; it does not yet establish an effect.

## Setup

- Model: MTPLX 2.9.2 serving `Youssofal/Qwen3.8-27B-MTPLX-Optimized-Speed`
  (Q4, turbo profile, medium reasoning), Apple M5 Pro 48 GB.
- Agent: Claude Code headless (`--max-turns 40`, skip-permissions) through
  the per-arm LEB proxy via the `claude-mtplx` launcher.
- Arms: byte-identical snapshot baseline per arm; brain arm injects, off arm
  is the zero-injection control. Counterbalanced order, isolated DB/socket/
  state/port per run, process-group cleanup.
- Brain seed: three validated lessons (families B, C, F) recorded through the
  real `leb task`/`leb learn`/promotion pathway with deterministic evidence,
  anchored to touched-file symbols, snapshot via `leb backup create`.

## Results (3 paired tasks, seed 1, wall limit 1500 s)

| Task | off | brain | off wall | brain wall |
|---|---|---|---:|---:|
| t09 re-encounter (drain trap) | pass | pass | 512 s | 601 s |
| t11 re-encounter (compound) | timeout | timeout | 1500 s | 1500 s |
| t14 fresh control | pass | pass | 395 s | 464 s |

Success delta 0.0; brain wall +6.5% (compile + larger prompts).

## What the pilot established

1. The full measurement pipeline works end-to-end with a real agent and
   model. Getting here surfaced and fixed two release-grade bugs: the proxy
   rejected every multi-turn agentic conversation (task extraction required
   text in the last user message), and harness arm sockets exceeded the
   macOS `sun_path` limit, so per-arm daemons could never start.
2. Retrieval correctness at the point of injection was verified directly:
   the t11 prompt compiles a 316-token context containing both directly
   relevant seeded lessons.
3. No harmful stale injection on the fresh control.

## Why no effect yet, and what changes

- t09's trap did not bite the memoryless agent (both arms solved it), so the
  pair carried no discriminating power.
- t11 was too hard for both arms within 25 minutes.
- n=3 tasks x 1 seed cannot resolve small effects.

Next recorded run: 8 tasks (6 re-encounters spanning all six trap families +
2 fresh controls), all six families seeded, wall 1800 s, `--max-turns 60`,
same model and pairing discipline.
