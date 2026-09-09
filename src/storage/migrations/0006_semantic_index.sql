-- Rebuildable derived semantic sidecar. Rows are deterministic projections of
-- canonical memory text (see src/semantic.rs) and are never canonical truth:
-- the table is safe to drop and regenerate from `memories` at any time via
-- `leb semantic rebuild <path>`. Vectors are little-endian f32 BLOBs;
-- `embedder` records the producing embedder identity so incompatible vectors
-- can be detected and rebuilt after a model swap (ADR-008).
CREATE TABLE memory_vectors (
    memory_id       TEXT PRIMARY KEY NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    project_id      TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    embedder        TEXT NOT NULL,
    dimension       INTEGER NOT NULL CHECK (dimension > 0),
    vector          BLOB NOT NULL,
    updated_at_ms   INTEGER NOT NULL
) STRICT;

CREATE INDEX memory_vectors_project ON memory_vectors(project_id);
