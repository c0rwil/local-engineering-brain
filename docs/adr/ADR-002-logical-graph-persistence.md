# ADR-002: Logical graph and physical persistence

- Status: accepted
- Date: 2026-08-28

## Decision

Expose a provider-neutral property graph and persist normalized nodes/edges in SQLite using typed
external IDs and integer-backed indexed relations. Tree-sitter is the native provider; bounded JSON
Graphify snapshots are an external adapter. Provider output is validated and parsed outside the
canonical writer, then applied atomically. Provider-native stores are rebuildable accelerators only.

## Consequences

Cross-file relationships survive provider changes, query behavior is deterministic, and no graph
server is required. Deep graph analytics remain explicit tools rather than prompt-path work.
