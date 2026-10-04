-- Derived semantic sidecar, version 2: one row per (memory, embedder).
-- Rows of the default hashed embedder stay maintained inside canonical
-- transactions and remain the fallback. Rows from an optional local embedding
-- model are written by `engram semantic rebuild` (or the semantic_rebuild
-- job) outside the canonical writer and are searched only under the
-- configured model identity, so vectors from different models never mix.
-- `source_digest` is the SHA-256 of the exact text a model embedded, letting a
-- rebuild skip unchanged memories; it is NULL for hashed rows. Still
-- rebuildable derived state: safe to drop and regenerate from `memories`.
CREATE TABLE memory_vectors_v2 (
    memory_id       TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    project_id      TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    embedder        TEXT NOT NULL,
    dimension       INTEGER NOT NULL CHECK (dimension > 0),
    vector          BLOB NOT NULL CHECK (length(vector) = 4 * dimension),
    updated_at_ms   INTEGER NOT NULL,
    source_digest   TEXT,
    PRIMARY KEY (memory_id, embedder)
) STRICT;

INSERT INTO memory_vectors_v2(memory_id, project_id, embedder, dimension, vector, updated_at_ms)
SELECT memory_id, project_id, embedder, dimension, vector, updated_at_ms
FROM memory_vectors
WHERE length(vector) = 4 * dimension;

DROP TABLE memory_vectors;
ALTER TABLE memory_vectors_v2 RENAME TO memory_vectors;
CREATE INDEX memory_vectors_project ON memory_vectors(project_id, embedder);
