# ADR-006: Context insertion and order

- Status: accepted
- Date: 2026-08-28

## Decision

Inject deterministic stable project architecture first, followed by task-specific validated memory.
Every selected item carries source ID, token cost, score, and feature-level `why`. Repository-derived
content is labeled reference data, not executable instruction. Off mode injects exactly zero.
Rendered bytes contain no request IDs or timestamps, and every query/rank has a stable final order.
The opt-in derived compact form and its prefix-stability contract are specified in ADR-014.
