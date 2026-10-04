# Private failure and feedback origin grader (h02, draft)

```sh
python3 -B benchmarks/real-repo-heldout-v1/grading/h02_failure_feedback_origin/grade.py \
  --repo /private/tmp/candidate-grading-copy --output /private/tmp/h02-grade.json
```

The grader appends `hidden_tests.rs` to `tests/http_contract.rs` in a
disposable grading copy and reuses that suite's in-process mock model servers,
Raindrop bridge capture and session fixtures. Requests go through the real
router; analytics are observed at the mock bridge, so any implementation
location is graded by what it emits. Eight behavioral cases plus a compile check:

- a first-step model failure carries the request's declared origin;
- first-step normalization matches session creation (` MCP ` becomes `mcp`, an
  open-vocabulary value passes, blank and e-mail-shaped values stay absent);
- a failure on an existing session keeps its frozen stamp over a divergent
  request declaration;
- a failure on a session created without origin stays untagged;
- the reaction `user_feedback` event carries the session origin;
- the reaction `user_feedback` signal and companion `task_failed` or
  `task_completed` signal carry it (thumbs-down and thumbs-up sessions);
- reaction records of an origin-less session omit the key;
- successful step and `session_end` events keep their existing origin stamp.

The model-failure `task_failed` and `error` signals are not graded for origin;
the historical fix tags only the failure event. Values are synthetic. No
database, Redis, live model or network is used; loopback mocks only. Each case
runs in a fresh process with one test thread. Exit codes and cargo behavior are
as described in the h05 README (shared `_shared/rust_grader.py`).

## Calibration

Base `2b690115`, reference `c67bbf4d`. Recorded in `preflight.json`:

| Control | Kind | Expected |
| --- | --- | --- |
| `base` | base | Five tagging cases fail. |
| `reference` | reference | All pass. |
| `request-origin-wins` | mutant | Uses the request origin even when a session exists: frozen-stamp and untagged-session cases fail. |
| `raw-request-origin` | mutant | First-step origin without session-creation normalization: normalization case fails. |
| `failure-events-only` | mutant | Fixes failure events, not reactions: both reaction tagging cases fail. |
| `feedback-only` | mutant | Fixes reactions, not failure events: three failure cases fail. |
| `equivalent-expression` | alternative | Same contract written differently in both files: passes. |
