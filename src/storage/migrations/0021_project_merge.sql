-- Project merge (`engram project merge`). A merged project stays as a
-- tombstone that names its survivor: later uses of its id resolve to the
-- survivor, and replaying PROJECTS_MERGED reproduces the pairing.
ALTER TABLE projects ADD COLUMN merged_into TEXT REFERENCES projects(id);
CREATE INDEX projects_merged_into ON projects(merged_into)
    WHERE merged_into IS NOT NULL;

-- Memory anchors on symbols of a merged project's structural index. A merge
-- drops that derived index instead of moving it, so each anchor waits here
-- until the survivor is re-indexed: it is then re-attached to the symbol with
-- the same stable identity (path and qualified name), or its memory is marked
-- for revalidation. Rows follow their project if it is merged again.
CREATE TABLE project_merge_anchors (
    project_id      TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    memory_id       TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    stable_key      TEXT NOT NULL,
    kind            TEXT NOT NULL,
    label           TEXT NOT NULL,
    file_path       TEXT,
    observed_digest TEXT NOT NULL,
    merge_event_id  TEXT NOT NULL,
    recorded_at_ms  INTEGER NOT NULL,
    PRIMARY KEY (memory_id, stable_key)
) STRICT;
CREATE INDEX project_merge_anchors_project ON project_merge_anchors(project_id);

-- Deleting a node makes SQLite look up every row that references it. Without
-- these indexes each lookup scans the whole edge table, so dropping a merged
-- project's index (tens of thousands of nodes) would take hours.
CREATE INDEX edges_source ON edges(source_id);
CREATE INDEX edges_target ON edges(target_id);
CREATE INDEX memory_nodes_node ON memory_nodes(node_id);
CREATE INDEX indexed_files_scan ON indexed_files(scan_id);
