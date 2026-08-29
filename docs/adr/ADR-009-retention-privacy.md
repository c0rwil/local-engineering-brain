# ADR-009: Transcript retention and privacy

- Status: accepted
- Date: 2026-08-28

## Decision

Do not persist full model transcripts by default. Store compact tasks/attempts, evidence artifact
digests, and redacted authored text. Recursively redact common credential fields/tokens before
durable persistence. Large future blobs are content-addressed and quota/retention controlled.
