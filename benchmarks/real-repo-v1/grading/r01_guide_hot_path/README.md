# Private guide hot-path grader

Run after the coding agent exits, against a disposable copy of its candidate:

```sh
caffeinate -dims python3 benchmarks/real-repo-v1/grading/r01_guide_hot_path/grade.py \
  --repo /tmp/candidate-grading-copy --output /tmp/r01-grade.json
```

Exit codes: 0 = pass, 1 = semantic/build failure, 2 = infrastructure failure.
The JSON contains named checks and paths to complete local compiler/test logs.
Do not expose the grader, reference commit or preflight mutations to the coding
agent. The grader appends private Rust tests and overwrites `.env` with harmless
synthetic configuration in the supplied copy; it refuses the real repository.

Fourteen behavioral cases invoke the real guide resolver, JSON request parsing,
session request conversion and partner-policy merge. They count HTTP search
requests, exercise deferred app-open warming with positive/negative controls,
and count mock cache reads during a continuation. These detect a patch which
only discards the returned guide after performing retrieval. App-guide results
and the master switch have independent assertions.

All services are in-process loopback mocks; cache state is in-memory. Settings
and subprocess environments contain no developer credentials. Each case runs
in its own test process. Cargo is offline/locked and uses two build jobs. The
shared private build target is `/tmp/engram-rust-preflight-target`; set
`ENGRAM_RUST_GRADER_TARGET` to override it. A file lock protects the executable
through the entire grade. Vendored Rust source timestamps are refreshed because
old `git archive` timestamps otherwise allowed Cargo to reuse a stale vendor
artifact between historical snapshots.

Preflight used Homebrew Rust 1.96.0, not the catalog's suggested 1.91.1. No source
prerequisite changes were needed. `git archive COMMIT` exported these fixtures
from the read-only `agi-backend-rust` repository:

| Fixture | Commit / mutation | Result |
|---|---|---|
| Base | `bbce775ef3252e4ad882ac43e6564263302f8076` | Fails 7 of 14 behavioral checks |
| Reference | `ff515c8a0b8fedc4757209ce3291ee61d0f5aa7d` | Passes 14 of 14 |
| Incomplete fix | Reference with the task predicate restored to its old behavior, then task text discarded at the return | Fails search/deferred/cache checks |

Saved preflight JSON and per-case logs are in `preflight/`. Full compiler
JSON streams remain at their recorded `/tmp/engram-rust-preflight-*` paths.
Preflight runtime is setup/validation overhead, not coding-agent benchmark time.
