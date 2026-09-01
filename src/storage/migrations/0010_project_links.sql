-- Explicit directed links between registered projects. A link makes the
-- linked project's validated memories eligible for bounded, relevance-gated,
-- clearly-labeled cross-repository context injection (multi-repo/microservice
-- workspaces: a task in one repo can see the tribal knowledge of the repos it
-- integrates with). Links are always an explicit operator action; nothing is
-- inferred.
CREATE TABLE project_links (
    project_id TEXT NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    linked_project_id TEXT NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    relation TEXT NOT NULL,
    note TEXT,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (project_id, linked_project_id),
    CHECK (project_id <> linked_project_id)
);

CREATE INDEX idx_project_links_linked ON project_links (linked_project_id);
