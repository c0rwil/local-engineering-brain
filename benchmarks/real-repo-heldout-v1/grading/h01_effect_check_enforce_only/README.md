# Private effect-check mode grader (h01, draft)

```sh
python3 -B benchmarks/real-repo-heldout-v1/grading/h01_effect_check_enforce_only/grade.py \
  --repo /private/tmp/candidate-grading-copy --output /private/tmp/h01-grade.json
```

The grader appends `hidden_tests.rs` to `tests/http_contract.rs` in a
disposable grading copy and reuses that suite's in-process mock model server,
LoopGuard session fixtures and effect-check screenshots. Requests go through the
real router to `/sessions/{id}/action`. Six behavioral cases plus a compile
check:

| Case | Contract |
| --- | --- |
| `shadow_mode_sends_no_effect_check_note_to_the_model` | A dead tap under shadow sends no `[effect-check]` note to the model. |
| `off_mode_sends_no_effect_check_note_to_the_model` | The same under off. |
| `non_enforcing_modes_record_no_effect_check_note_in_the_step` | Shadow and off record no note in any stored step prompt. |
| `enforce_mode_still_delivers_the_note_for_a_dead_tap` | Enforce still delivers the note with its wording. |
| `enforce_mode_stays_silent_when_the_screen_moved` | Enforce sends no note when the screenshot changed. |
| `shadow_mode_still_scores_unchanged_screens` | Two identical taps on an unchanged screen under shadow produce a `loopguard_shadow` INFO line with `rung="inject"` (the screen-confirmed dead-button rung), and both recorded steps keep a screen fingerprint. |

The last case captures this test thread's INFO logs with a local
`tracing_subscriber` writer. It observes LoopGuard's existing shadow log line
and the persisted `screen_fingerprint` field. Both are internal observables and
are listed for operator review. Trace-upload stamps are not used: in this
scenario the service uploaded no new trace after the first step, so a
trace-based version of the check failed every control during development.

No database, Redis, live model or network is used; loopback mocks only. Each
case runs in a fresh process with one test thread. Exit codes and cargo
behavior are as described in the h05 README (shared `_shared/rust_grader.py`).

## Calibration

Base `f566a1c2`, reference `c24e931d`. Recorded in `preflight.json`:

| Control | Kind | Expected |
| --- | --- | --- |
| `base` | base | The three note cases fail. |
| `reference` | reference | All pass. |
| `gate-evidence-on-enforce` | mutant | Skips the screen evidence outside enforce: shadow scoring case fails. |
| `blind-loopguard-outside-enforce` | mutant | Keeps fingerprints but hides the effect reading from shadow LoopGuard: shadow scoring case fails. |
| `silence-shadow-only` | mutant | Suppresses the note only for shadow: off and stored-prompt cases fail. |
| `drop-note-everywhere` | mutant | Removes the note in every mode: enforce delivery case fails. |
| `mode-flag-before-note` | alternative | Decides delivery once from the mode, case-insensitively: passes. |
