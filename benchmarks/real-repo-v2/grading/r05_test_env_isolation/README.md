# Private test-environment grader v2

The prompt, `grade.py` and `hidden_tests.rs` are byte-identical to v1. V2 runs
fresh calibration; it does not relabel saved v1 evidence.

```sh
python3 -B benchmarks/real-repo-v2/grading/r05_test_env_isolation/grade.py \
  --repo /private/tmp/candidate-grading-copy --output /private/tmp/r05-v2-grade.json
```

Five behavioral cases check test-mode scrubbing, normal development dotenv
loading, and poison recovery at all three existing shared-lock acquisition sites.
The deliberate original panic remains visible. Each assertion runs in a fresh
process; no database, Redis or model starts. A separate compile check is reported.
The grader appends private tests and replaces `.env` with harmless synthetic
values only in a disposable grading copy. It refuses the real repository.

Exit codes are 0/pass, 1/behavioral or build failure, and 2/infrastructure failure.
The result links complete local logs. Cargo is offline and locked with two jobs,
and a file lock serializes build and execution in
`/tmp/engram-rust-preflight-target` (`ENGRAM_RUST_GRADER_TARGET` overrides it).
Historical vendored-source timestamps are refreshed to prevent stale artifacts.

Fresh calibration uses the shared v2 Rust preflight driver:

```sh
python3 -B benchmarks/real-repo-v2/grading/r01_guide_hot_path/preflight.py \
  --source-repo /Users/corwil/PycharmProjects/agi-backend-rust \
  --task r05_test_env_isolation \
  --output benchmarks/real-repo-v2/grading/r05_test_env_isolation/preflight.json
```

It exports base `cae95f3086d7320551ab02ba4ba0fc105dc13342` and reference
`30cc0acfcb7766e8ff0de47119d072b6f46543a8` from read-only Git objects. The base must
fail test-mode isolation, the reference must pass all five cases, and a reference
mutant removing dotenv globally must fail normal development loading.
`preflight.json` records new results, hashes, mutation diffs and compiler version.
Calibration and grading time are excluded from coding-agent time. Keep all
private tests, references and mutations outside agent workspaces.
