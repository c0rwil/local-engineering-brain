# Private multi-byte truncation grader (h05, draft)

```sh
python3 -B benchmarks/real-repo-heldout-v1/grading/h05_multibyte_truncation/grade.py \
  --repo /private/tmp/candidate-grading-copy --output /private/tmp/h05-grade.json
```

The grader writes `hidden_tests.rs` as a new integration-test crate,
`tests/engram_heldout_h05.rs`, in a disposable grading copy. The tests call only
the public human-response context and prompt builders, never the private
truncation helper that a fix is likely to change or rename. Five behavioral
cases plus a compile check:

- a task prompt whose historical byte cut lands inside a two-byte character;
- desired outcome, human context and latest question with three- and four-byte
  characters;
- earlier-step transcript entries (step prompt and step output), including a
  multi-byte entry within its limit;
- ASCII parity with the historical output, including trailing-whitespace trim;
- a field of 3001 characters (6001 bytes) that is within the character limit.

The expected value is computed by the contract (limits in characters, longest
leading prefix before the existing suffix, trailing whitespace trimmed), which
equals the historical output for ASCII. Synthetic text only; no model, database
or network is used. Each case runs in a fresh process.

Exit codes are 0 pass, 1 behavioral or compile failure, 2 infrastructure
failure. Cargo runs offline and locked with `ENGRAM_RUST_GRADER_JOBS` jobs
(default 2) in `ENGRAM_RUST_GRADER_TARGET` (default
`/tmp/engram-heldout-rust-target`) under a file lock. Every source file in the
grading copy is touched first, because historical archives carry commit-time
mtimes and a reused target directory could otherwise serve stale artifacts. A
root `.env` in the grading copy is removed. The grader refuses paths under
`~/PycharmProjects`.

## Calibration

```sh
python3 -B benchmarks/real-repo-heldout-v1/grading/h05_multibyte_truncation/preflight.py \
  --source-repo /path/to/agi-backend-rust \
  --output benchmarks/real-repo-heldout-v1/grading/h05_multibyte_truncation/preflight.json
```

Base `f566a1c2` must fail the four multi-byte cases, reference `dee1f322` must
pass, and these controls must behave as recorded in `preflight.json`:

| Control | Kind | Expected |
| --- | --- | --- |
| `char-count-only` | mutant | Counts characters for the limit but still slices bytes: three panicking cases fail. |
| `byte-budget-at-char-boundary` | mutant | No panic, but a byte budget: all four multi-byte cases fail. |
| `prompt-fields-only` | mutant | Fixes the top-level fields, keeps byte truncation for transcript history: only the transcript case fails. |
| `char-indices-cut` | alternative | Different implementation of the same contract: passes. |
