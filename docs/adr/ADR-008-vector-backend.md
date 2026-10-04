# ADR-008: Vector backend abstraction

- Status: accepted
- Date: 2026-08-28

## Decision

Semantic retrieval is optional and accelerator-only. Canonical text/provenance stays in SQLite.
V1 remains correct with vectors disabled through exact, FTS5, structural, validity, and utility
features. Any future local vector backend must be bounded, rebuildable, and must not block the hot
path when unavailable.

## Addendum (2026-10-04): local embedding provider

- An optional `[semantic] provider = "local"` calls an OpenAI-compatible `/v1/embeddings` endpoint
  on a loopback address. The hashed n-gram embedder stays the default. Semantic retrieval stays
  off by default, and neither embedder has a measured task-level benefit.
- The `memory_vectors` sidecar holds one row per (memory, embedder identity). Hashed rows are still
  maintained inside canonical transactions and serve as the fallback. Model rows are embedded
  outside the canonical writer and committed in short scheduled transactions. Each model row
  carries a digest of the text that was embedded.
- Retrieval compares only rows of the configured identity and dimension. A model, prefix or
  dimension change leaves the old rows unsearched. The stage then degrades to lexical +
  structural retrieval and requests a rebuild. That rebuild deletes the other model's rows for the
  project. An unreachable endpoint falls back to the hashed rows for that request.
- Every query embedding and rebuild request is bounded in size and time. Each memory's `why` note
  names the embedder that served it and any fallback.
