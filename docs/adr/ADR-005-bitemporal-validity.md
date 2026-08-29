# ADR-005: Bitemporal validity

- Status: accepted
- Date: 2026-08-28

## Decision

Track software-valid commit intervals separately from knowledge-valid timestamps. Automatic
retrieval requires active+validated state, Git ancestry applicability, and unchanged linked symbol
digests. Drift quarantines memory as suspect; stale/superseded records remain historically
inspectable until explicit purge.
