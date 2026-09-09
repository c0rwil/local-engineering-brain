# ADR-001: Canonical SQLite and event model

- Status: accepted
- Date: 2026-08-28

## Context

LEB needs a zero-daemon canonical store whose mutations are auditable, crash-safe, locally
inspectable, and portable across macOS and Linux. Derived graph/vector indexes must never become the
only copy of learned state. The implementation also needs a small idle footprint and a clean path to
a long-running local daemon.

## Decision

Use Rust for the core and bundled SQLite as canonical storage. Every learning-relevant mutation is a
typed, schema-versioned immutable event. The event append and its materialized projection execute in
one `BEGIN IMMEDIATE` transaction. Event rows are protected against update/delete by database
triggers, IDs are idempotent only when all stored content matches, and replay rejects unknown event
types, invalid/mismatched project scopes, or schema versions explicitly.

Every connection enables WAL, foreign keys, a bounded busy timeout, and `synchronous=FULL`.
Migrations are embedded in the binary and applied under one writer transaction. Recorded names and
SHA-256 checksums must match, and a binary refuses schemas newer than it understands. Database and
newly created containing-directory permissions are restricted on Unix without changing arbitrary
pre-existing directories. Backups use SQLite `VACUUM INTO` for a consistent snapshot.

## Consequences

- One long-lived daemon owns a bounded priority writer and a fixed reusable reader pool; logical
  writers from concurrent agents receive explicit backpressure instead of opening ad hoc writers.
- Event history can reconstruct project projections and diagnose divergent materialized state.
- Schema changes require a numbered migration and compatible event decoder.
- Memory purge removes content and derived retrieval state while retaining a content-free audit event.
- Bundled SQLite increases build time and binary size but removes a runtime system dependency.
