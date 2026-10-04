# Private guide hot-path grader v2

Run after the coding agent exits, against a disposable grading copy:

```sh
python3 -B benchmarks/real-repo-v2/grading/r01_guide_hot_path/grade.py \
  --repo /private/tmp/candidate-grading-copy --output /private/tmp/r01-v2-grade.json
```

Fourteen behavioral cases exercise the real resolver, JSON request parsing,
session conversion, policy merge, HTTP search counts, deferred warming and cache
pickup. A compile check is reported separately. The v2 helper retains the same
enforcing policy context through normalization and resolution, as production
does. It tests the resulting restrictions without requiring request mutation to
encode every policy restriction. The task prompt states the required behavior.
The loopback app-guide endpoint also supplies the partner-scoped lookup, which
does not use the seeded global app-guide store.

Each case runs in a separate process with harmless synthetic settings and
in-process loopback mocks. No production service or model is called. The grader
appends private tests and replaces `.env` only in the disposable copy. It refuses
the real repository. Exit codes are 0/pass, 1/behavioral or build failure, and
2/infrastructure failure. Complete compiler and case logs stay at the paths in
the result JSON.

Cargo runs offline and locked with two jobs. A file lock protects both compilation
and execution in the shared target, default `/tmp/engram-rust-preflight-target`
or `ENGRAM_RUST_GRADER_TARGET`. Historical vendored-source timestamps are refreshed
to prevent stale artifacts across source exports. Build and grading costs are
separate from coding-agent time.

Reproduce fresh calibration from the read-only historical repository:

```sh
python3 -B benchmarks/real-repo-v2/grading/r01_guide_hot_path/preflight.py \
  --source-repo /Users/corwil/PycharmProjects/agi-backend-rust \
  --output benchmarks/real-repo-v2/grading/r01_guide_hot_path/preflight.json
```

The driver exports pinned Git objects into new private directories and saves
grader, fixture and driver hashes plus exact mutation diffs. It requires:
Every control must compile and execute all fourteen cases; failures must include
the named behavioral checks, so build errors cannot count as successful rejection.
Calibration stops on the first unexpected control outcome.

| Control | Required result |
| --- | --- |
| Historical base `bbce775ef3252e4ad882ac43e6564263302f8076` | Reject, including actual search on caller opt-out |
| Reference `ff515c8a0b8fedc4757209ce3291ee61d0f5aa7d` | Pass all fourteen behavioral cases |
| Reference with task-policy enforcement only in request normalization | Pass all fourteen |
| Reference with task-policy enforcement only in the resolver | Pass all fourteen |
| Reference that performs task work and discards only the returned text | Reject search and later cache pickup |
| Reference that ignores task policy in both layers | Reject policy restriction |

These alternates vary task-family enforcement only; other historical policy
normalization remains intact. The new `preflight.json` reports observed outcomes
and compiler version. Calibration results are not agent outcomes. Never expose
these controls or reference revisions to the coding agent.
