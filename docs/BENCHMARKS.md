# Engram benchmark results

Injection benchmarks use a true control: the identical
agent with Engram fully disabled, in isolated repository copies with per-arm
databases and sockets. The brain-off arm is semantically untouched (spec
P10). Raw evidence lives in [perf-evidence/](perf-evidence/) with
date-and-commit filenames; the synthetic `conveyor` fixture and its
fairness audit live in the benchmark target repo. Capture measurements and
historical recovery replay are reported separately from paired agent trials.

## Real-repository task pilot

The [real-repository task catalog](../benchmarks/real-repo-v1/README.md) specifies
nine historical engineering replays with verified parent/fix commits, separate
agent prompts and grading contracts. The first pilot is four tasks × three
repetitions × Off/Brain, or 24 agent runs. Fixture preparation and hidden-test
calibration passed, and all 24 isolated Claude Max attempts are complete. The
[pilot report](perf-evidence/2026-09-24-real-repo-pilot/README.md) records completed
attempts, delivery receipts, correctness, provider tokens, tool calls and timing.
Paired outcomes are four Brain-only passes, one Off-only pass, one both-pass pair,
and six unscorable pairs. That single both-pass pair shows 34.1% fewer processed
tokens and 22.7% fewer tool calls with Brain, with 1.3% more agent time. It does not
establish general savings. Five grader-contract ambiguities and one provider
runtime error are retained; the grading audit explains the exclusions.
The protocol separates coding-agent savings from improvements to the products
being edited, and retains failed and interrupted attempts.

The [context follow-up](perf-evidence/2026-09-25-context-followup/README.md) tests
the resulting identifier, field-owner and delivery-suppression fixes. It compares
the previous and updated binaries on frozen navigation probes and full historical
task descriptions, including reordered requirements. These are local retrieval
diagnostics; the original 24 agent attempts remain unchanged.

The v2 runner can also run both arms with the Codex CLI (`--agent codex`), with
isolated per-arm Codex homes and a model-free loopback preflight; see the
[v2 protocol](../benchmarks/real-repo-v2/README.md#codex-arm). No Codex collection
has been run, so there are no Codex results.

## Retrieval regression gate

[`tests/retrieval_regression.rs`](../tests/retrieval_regression.rs) is a hermetic
check of source retrieval that runs with `cargo test` and as a named step in CI.
The frozen-snapshot navigation harness above needs a private brain; this gate
does not. An isolated `engramd` indexes the committed synthetic repository in
`tests/fixtures/retrieval_repo`. That repository has Rust, Python and TypeScript
files: helpers, call sites, and a shared environment guard with three users.
The gate then compiles context for 16 authored probes.

| Check | Threshold |
|---|---|
| Intended file in the pack | Every positive probe |
| Ranking | First pointer for exact identifiers and paths; first two for natural language |
| Definition in the first five pointers | Every probe marked required (10); one known gap is reported only |
| Filler | Unmatched, missing-path, stop-word and partial-identifier probes add no items or dynamic context; positive probes stay within a pointer bound |
| Budget | `selected_tokens` and the rendered estimate stay within budget, including a 64–1,600 token sweep |
| Off mode | Exactly zero injection for every probe |
| Determinism | A repeated compilation selects the same sources |

Assertions resolve each item to its indexed file path and symbol instead of
parsing pointer text, so formats such as line ranges can change without
breaking the gate. Thresholds are floors. They pass on the current compiler.
A local check made them fail under six deliberate regressions: reversed
ranking, disabled identifier scoring, removed relevance filters, removed
exact-path boost, unbounded source budgets and off-mode injection. Two gaps
are reported but not gated. The prose probe for the TypeScript session module
selects the file rather than `refreshSessionToken`. The shared-lock probe
selects none of the guard's three call sites, which a related-use cue would
need to cover. Run `cargo test --test retrieval_regression -- --nocapture` to
print the per-probe report. Passing this gate is retrieval evidence only; it
does not measure agent outcomes or provider usage.

## Headline results

| Axis | Control | With Engram | Evidence |
|---|---:|---:|---|
| Tribal-knowledge fidelity (Opus, 12 probes × 2 seeds) | 5/12 | **24/24** | 2026-08-31 flagship + multiseed |
| Local-model task success (wall-calibrated, Q4 on-device) | 2/10 | 2/10; runtime errors dominate | [2026-09-16 rerun](perf-evidence/2026-09-16-local-regression.md) |
| Preference distillation (learned conventions, unprompted) | 3/4 | **4/4**, −41% wall | 2026-09-02 |
| Real-repo recall@1 (production backend, non-verbatim queries) | — | **11/11** | 2026-09-01 |
| Compact injection (bytes at success parity) | baseline | **−33% injected bytes** | 2026-09-01 |
| Hook-pipeline fidelity (five injection moments, Opus) | 8/10 | **10/10** | [2026-09-17 origin-refresh regression](perf-evidence/2026-09-17-hooks-origin-refresh.md) |
| Living product / onboarding contracts (synthetic correctness checks) | — | **21/21**; no human speed claim | [2026-09-17 product evaluation](perf-evidence/2026-09-17-living-product.md) |
| Operational-lesson replay (historical repair, six real-session probes) | 0/6 before recovery | **1/6**, unchanged after repository refresh | [2026-09-17 diagnosis](perf-evidence/2026-09-17-repository-refresh.md) |

The [living-product report](perf-evidence/2026-09-17-living-product.md) adds a
source-linked real-flow check and identifies internal documentation synthesis as
a source of historical abandoned episodes. The unchanged six-case replay remains
1/6; prospective capture and onboarding gains are not yet established.

The [repository refresh diagnosis](perf-evidence/2026-09-17-repository-refresh.md)
separates source indexing from lesson delivery: three replay lessons were never
captured, one awaits review and one is quarantined for historical source drift.
The remaining lesson injects. Refreshing all three relevant repositories did not
close that historical evidence gap.

The [native-agent integration check](perf-evidence/2026-09-17-native-agents.md)
verifies five Codex hook definitions, 23 MCP tools and 270 passing tests on
`280ef5c`. Codex hook execution still requires operator trust. Its hook regression
is running and its local matrix is queued; neither has a published score yet.
The headline injection results above remain tied to their earlier builds.

The capture report also measures conversion and validation latency per session,
acceptance coverage, and abandoned-task rate. It does not claim prospective
improvement from one historical recovery. Brief false positives remain unscored
until actual emitted items have relevance labels; unknown acceptance remains
unknown.

The [2026-09-03 hook matrix](perf-evidence/2026-09-03-opus-hooks-pipeline.md)
reported 6/10 versus 10/10 at −19% mean wall. The September 16 rerun was 8/10
versus 10/10. The product-build rerun was 7/10 versus 10/10; the subsequent
origin-refresh fix rerun is 8/10 versus 10/10. Its mean wall is lower but its
median is slightly higher, so no general speedup is claimed. These are fresh paired runs;
differences across dates are not attributed to the capture changes.

The [2026-09-01 local matrix](perf-evidence/2026-09-01-local-multiseed-wall-calibrated.md)
reported 7/10 versus 10/10 at −33% mean wall. The latest local rerun had twelve
API cancellations/stream-idle errors and four wall timeouts. It shows no success
gain and does not support a speedup claim; runtime reliability must be resolved
before drawing an injection-quality comparison. The [September 17 final-build
local attempt](perf-evidence/2026-09-17-local-product-runtime-blocked.md) is blocked
before warmup by the local Metal/MLX runtime and has no score.

## What the fidelity probes measure

The `conveyor` fixture embeds **undocumented team preferences** — rules the
code exhibits but no document states (terminal transitions bump a metrics
counter; behaviour changes ship an in-file test; rejections name the
offending input; state enumerations read in lifecycle order). A memoryless
agent can only induce them from surrounding code; the brain arm holds each
as a validated preference memory seeded through the real
episode→learn→promote pathway. Verification is two-stage and byte-identical
across arms: the full functional test suite plus a per-task style check,
with per-task discrimination proven mechanically (HEAD fails; a naive
functionally-correct patch fails style; the reference solution passes
both).

The flagship reading: probes that went **0-for-10** across memoryless Opus
arms pass **12-for-12** with injection — unwritten team standards are the
part of engineering that frontier models cannot induce reliably, and
exactly the part Engram captures.

## Method discipline (the short version)

- Paired off/brain trials, same prompts, same wall limits, same validators.
- Per-arm brains restored from a frozen snapshot; project identity pinned
  via `.engram/project.toml` (clone-identity fracture silently blanked early
  matrices — documented, regression-tested).
- Per-run context telemetry with measurement scope stated in each report; the
  hook collector measures compiled prefix/context bytes, excluding additional
  hook payloads. A zero-context brain arm is a broken arm, not a data point.
- Frontier runs go through the operator's own `claude` login; local runs
  are wall-calibrated (≥2700 s) with a warmed model server.

## Comparison context

basemode (christopherkahler/base), the closest comparable tool, publishes
no measurements. Engram's differentiators — the evidence gate, automatic
capture, drift quarantine, team sync — are precisely the parts that
benchmarks like these exist to justify: knowledge only helps if it is
true, current, and actually delivered.
