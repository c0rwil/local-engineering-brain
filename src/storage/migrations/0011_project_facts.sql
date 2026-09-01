-- Curated per-project architecture facts: deployment hostnames, hosting
-- provider, storage backends, service role. A living, machine-readable
-- knowledge base maintained via `leb fact` — rendered into the stable
-- context prefix, into linked-repository context sections, and into the
-- Obsidian vault, and consumed by link inference (a repo that references
-- another service's declared hostname is declaring an API dependency).
CREATE TABLE project_facts (
    project_id TEXT NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    note TEXT,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (project_id, key)
);
