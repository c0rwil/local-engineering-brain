-- Compact memory text is a derived, rebuildable projection. Canonical claim,
-- rationale, and provenance remain in memories and its link tables.
CREATE TABLE memory_compact_injections (
    memory_id        TEXT PRIMARY KEY NOT NULL
                     REFERENCES memories(id) ON DELETE CASCADE,
    renderer_version INTEGER NOT NULL CHECK (renderer_version = 1),
    source_digest    TEXT NOT NULL CHECK (length(source_digest) = 64),
    compact_text     TEXT NOT NULL
                     CHECK (compact_text <> '')
                     CHECK (length(CAST(compact_text AS BLOB)) <= 528),
    token_estimate   INTEGER NOT NULL CHECK (token_estimate >= 0)
) STRICT;

-- Existing rows are backfilled by the migration runner with the same Rust
-- renderer used for future writes. That backfill and this schema change share
-- the migration transaction, so migration 8 is never recorded partially.
