# LEB benchmark results

Every axis of the spec is measured against a true control: the identical
agent with LEB fully disabled, in isolated repository copies with per-arm
databases and sockets. The brain-off arm is semantically untouched (spec
P10). Raw evidence lives in [perf-evidence/](perf-evidence/) with
date-and-commit filenames; the synthetic `conveyor` fixture and its
fairness audit live in the benchmark target repo.

## Headline results

| Axis | Control | With LEB | Evidence |
|---|---:|---:|---|
| Tribal-knowledge fidelity (Opus, 12 probes × 2 seeds) | 5/12 | **24/24** | 2026-08-31 flagship + multiseed |
| Local-model task success (wall-calibrated, Q4 on-device) | 7/10 | **10/10**, −33% wall | 2026-09-01 |
| Preference distillation (learned conventions, unprompted) | 3/4 | **4/4**, −41% wall | 2026-09-02 |
| Real-repo recall@1 (production backend, non-verbatim queries) | — | **11/11** | 2026-09-01 |
| Compact injection (bytes at success parity) | baseline | **−33% injected bytes** | 2026-09-01 |
| Hook-pipeline fidelity (five injection moments, Opus) | see below | see below | 2026-09-03 |

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
exactly the part LEB captures.

## Method discipline (the short version)

- Paired off/brain trials, same prompts, same wall limits, same validators.
- Per-arm brains restored from a frozen snapshot; project identity pinned
  via `.leb/project.toml` (clone-identity fracture silently blanked early
  matrices — documented, regression-tested).
- Injected-bytes telemetry archived per run; a brain arm that injects
  nothing is a broken arm, not a data point.
- Frontier runs go through the operator's own `claude` login; local runs
  are wall-calibrated (≥2700 s) with a warmed model server.

## Comparison context

basemode (christopherkahler/base), the closest comparable tool, publishes
no measurements. LEB's differentiators — the evidence gate, automatic
capture, drift quarantine, team sync — are precisely the parts that
benchmarks like these exist to justify: knowledge only helps if it is
true, current, and actually delivered.
