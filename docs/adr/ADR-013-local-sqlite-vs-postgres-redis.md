# ADR-013: Local SQLite versus PostgreSQL and Redis

- Status: accepted
- Date: 2026-08-28

## Context

Many agents may read and submit writes concurrently, and a brain may grow to millions of rows. The
architecture must not assume that all state fits in process memory. PostgreSQL and Redis are common
server architectures, but the V1 product is one user on one workstation.

## Decision

Use SQLite as the on-disk canonical engine behind a single-owner daemon. The daemon serializes only
short mutations, serves reads from a fixed pool under WAL, does parsing/retrieval outside writer
critical sections. Queries are indexed and paginated; the full
brain is never loaded into RAM. Redis is not canonical and is unnecessary for one process because a
durable request projection already supplies retry reuse without a second data lifecycle. A bounded,
project-partitioned cache primitive exists, but V1 rejects enabling it until canonical-generation
invalidation is connected end to end.

Adopt PostgreSQL when the product crosses a real boundary: multiple machines/users, remote service
availability, row-level authorization, replication, or write throughput that cannot be handled by
one local writer. The current V1 stores intentionally use concrete SQLite transactions; they are not
advertised as a drop-in PostgreSQL seam. A distributed edition must first introduce daemon-owned
repository/unit-of-work interfaces, implement PostgreSQL transaction and FTS semantics behind them,
and validate replay equivalence. Immutable typed events reduce migration risk but do not replace
that engineering work. Redis remains optional only for a distributed deployment's cross-process
ephemeral cache or coordination.

## Consequences

V1 has no database-server RAM tax or operational dependency, maintains crash-safe local semantics,
and handles concurrent subagents through explicit daemon admission. It does not claim horizontal
write scaling; team/distributed mode requires a PostgreSQL-backed storage implementation and access
control design.
