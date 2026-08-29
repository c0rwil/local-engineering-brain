# ADR-006: Context insertion and order

- Status: accepted
- Date: 2026-08-28

## Decision

Inject deterministic stable project architecture first, followed by task-specific validated memory.
Every selected item carries source ID, token cost, score, and feature-level `why`. Repository-derived
content is labeled reference data, not executable instruction. Off mode injects exactly zero.
