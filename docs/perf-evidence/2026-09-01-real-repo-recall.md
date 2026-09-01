# Real-repository recall test (private, local-only) — 2026-09-01

Target: the user's production Rust backend (92 tracked source files,
20,556 graph nodes / 34,442 edges — indexed by LEB in 1.6 s). All work
strictly local: registration, indexing, seeding, and probing ran against a
dedicated local state root; nothing left the machine.

Ground truth: the repository's own operator-guide "Gotchas" section — 11
verified tribal facts (integer-width column reads, pooler modes,
append-only tables, auth-cache revocation lag, billing semantics, JWT
fallback flags, extractor parity, stale-branch notes). Each was seeded as
a validated lesson through the real episode→learn→promote pathway, with
claims written in reviewer language, then probed with natural developer
queries phrased as someone would actually ask ("customer says a revoked
api key still works minutes after they disabled it") — no query shares
wording with its claim.

## Result: recall@1 = 11/11

Every probe ranked its target memory FIRST among the 11 candidates under
a 6,000-token budget. This validates the claim-coverage retrieval
(migration 9) on real-world content at realistic claim density, per the
original benchmark-design recommendation to use this repo's gotchas as a
memory-recall ground-truth set.

Operational note: registration wrote `.leb/project.toml` into the target
repository's working tree (standard LEB behavior — keep it for stable
identity, or gitignore it).
