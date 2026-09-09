CREATE TABLE projects (
    id                  TEXT PRIMARY KEY NOT NULL,
    name                TEXT NOT NULL,
    canonical_remote    TEXT,
    root_commit         TEXT,
    parent_project_id   TEXT REFERENCES projects(id),
    created_at_ms       INTEGER NOT NULL,
    tombstoned_at_ms    INTEGER
) STRICT;

CREATE UNIQUE INDEX projects_canonical_remote_active
    ON projects(canonical_remote)
    WHERE canonical_remote IS NOT NULL AND tombstoned_at_ms IS NULL;

CREATE INDEX projects_root_commit
    ON projects(root_commit)
    WHERE root_commit IS NOT NULL;

CREATE TABLE project_aliases (
    project_id      TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    path            TEXT NOT NULL UNIQUE,
    observed_at_ms  INTEGER NOT NULL,
    PRIMARY KEY (project_id, path)
) STRICT;

-- Events intentionally do not foreign-key project_id: audit history must survive
-- logical tombstones and future scope-purge policies.
CREATE TABLE events (
    sequence        INTEGER PRIMARY KEY AUTOINCREMENT,
    id              TEXT NOT NULL UNIQUE,
    project_id      TEXT,
    event_type      TEXT NOT NULL,
    schema_version  INTEGER NOT NULL CHECK (schema_version > 0),
    payload_json    TEXT NOT NULL CHECK (json_valid(payload_json)),
    created_at_ms   INTEGER NOT NULL
) STRICT;

CREATE INDEX events_project_sequence ON events(project_id, sequence);

CREATE TRIGGER events_are_immutable_on_update
BEFORE UPDATE ON events
BEGIN
    SELECT RAISE(ABORT, 'events are immutable');
END;

CREATE TRIGGER events_are_immutable_on_delete
BEFORE DELETE ON events
BEGIN
    SELECT RAISE(ABORT, 'events are immutable');
END;

