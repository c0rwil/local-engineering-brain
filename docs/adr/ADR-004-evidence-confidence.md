# ADR-004: Evidence confidence policy

- Status: accepted
- Date: 2026-08-28

## Decision

Classify evidence as deterministic, observational, or inferred. Agent inference is capped at 0.50.
Technical memory cannot reach supported/validated without deterministic or observational evidence;
explicit human override is recorded as its trust source. Repetition by the same model is not new
evidence.
