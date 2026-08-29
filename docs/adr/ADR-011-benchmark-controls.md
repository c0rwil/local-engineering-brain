# ADR-011: Benchmark isolation and controls

- Status: accepted
- Date: 2026-08-28

## Decision

Require exactly one off and one brain run for every task/seed with a pinned model hash and normalized
config hash. Run each arm in a fresh bounded repository copy, alternate order, share the same command
and wall limit, and retain per-arm artifacts. Freeze one verified standalone brain backup (or an
empty migrated baseline), copy it separately for every arm, and assign distinct database, Unix
socket, state-root, and proxy-port environment values. Include the snapshot digest in the effective
configuration hash. Launch commands in isolated process groups and terminate descendants after each
phase. Reports expose raw JSONL and do not reinterpret failed targets after the fact.
