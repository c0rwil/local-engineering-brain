# ADR-007: Token accounting

- Status: accepted
- Date: 2026-08-28

## Decision

V1 uses a deterministic conservative UTF-8 byte estimate (ceil bytes/4), persisted per memory and
rechecked for assembled blocks. It is runtime-neutral and enforces a hard configured ceiling.
Runtime-specific tokenizers may later tighten estimates but may never relax the final limit.
