# ADR-008: Vector backend abstraction

- Status: accepted
- Date: 2026-08-28

## Decision

Semantic retrieval is optional and accelerator-only. Canonical text/provenance stays in SQLite.
V1 remains correct with vectors disabled through exact, FTS5, structural, validity, and utility
features. Any future local vector backend must be bounded, rebuildable, and must not block the hot
path when unavailable.
