# Private test-environment grader

```sh
caffeinate -dims python3 benchmarks/real-repo-v1/grading/r05_test_env_isolation/grade.py \
  --repo /tmp/candidate-grading-copy --output /tmp/r05-grade.json
```

Run only after the coding agent exits, against a disposable grading copy. The
grader appends private tests to the existing contract-test module and replaces
`.env` with three harmless values. It never reads or copies a developer `.env`.
Keep tests, reference revisions and mutation evidence hidden from the agent.

Each assertion runs in a fresh process, preserving environment and mutex
isolation. The five cases verify test-mode scrubbing, normal development dotenv
loading, and recovery at all three existing shared-lock acquisition sites.
Poisoning comes from a caught deliberate assertion whose original panic payload
is checked and remains visible in the log; subsequent assertions must execute.
No database, Redis or model is started. Existing Redis configuration assertions
construct Settings only and make no requests.

Exit codes are 0/pass, 1/fail and 2/infra. JSON names every check and links complete
local logs. Cargo is offline/locked, uses two jobs, and defaults to the shared
private `/tmp/engram-rust-preflight-target` directory (override with
`ENGRAM_RUST_GRADER_TARGET`). A file lock serializes building and execution.
Historical vendored source mtimes are refreshed to avoid stale path-dependency
artifacts being reused across snapshots. The preflight initially exposed that
cache issue; no application source repair was necessary.

Preflight used Homebrew Rust 1.96.0, not the catalog's suggested 1.91.1. Fixtures
were exported with `git archive COMMIT` from read-only `agi-backend-rust`:

| Fixture | Commit / mutation | Result |
|---|---|---|
| Base | `cae95f3086d7320551ab02ba4ba0fc105dc13342` | Fails test-mode isolation and all poisoned-lock cases |
| Reference | `30cc0acfcb7766e8ff0de47119d072b6f46543a8` | Passes all five cases |
| Incomplete fix | Reference with `dotenvy::dotenv()` removed globally | Fails normal development loading |

Preflight JSON and per-case logs are in `preflight/`; full compiler JSON streams
remain at the recorded local paths. Grading/setup runtime is excluded from
coding-agent benchmark time.
