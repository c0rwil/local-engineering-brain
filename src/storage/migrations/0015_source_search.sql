-- Source search is a local reference index, separate from validated memories.
-- Keep real definitions and document sections; reference expressions can be
-- enormous and are not useful destinations for an agent to inspect.
-- Project IDs are one alphanumeric scope token so FTS does not repeatedly
-- evaluate UUID components as a positional phrase for every source match.
CREATE VIRTUAL TABLE source_node_fts USING fts5(
    project_id, label, file_path,
    tokenize = 'porter unicode61'
);

CREATE INDEX nodes_source_label ON nodes(project_id, label COLLATE NOCASE)
    WHERE tombstoned_at_ms IS NULL AND file_path IS NOT NULL
      AND kind <> 'ExternalReference';

INSERT INTO source_node_fts(rowid, project_id, label, file_path)
SELECT rowid, replace(replace(project_id, '-', ''), '_', ''), label, file_path FROM nodes
WHERE tombstoned_at_ms IS NULL AND file_path IS NOT NULL
  AND kind <> 'ExternalReference';

CREATE TRIGGER source_node_fts_insert AFTER INSERT ON nodes
WHEN new.tombstoned_at_ms IS NULL AND new.file_path IS NOT NULL
  AND new.kind <> 'ExternalReference'
BEGIN
    INSERT INTO source_node_fts(rowid, project_id, label, file_path)
    VALUES(new.rowid, replace(replace(new.project_id, '-', ''), '_', ''), new.label, new.file_path);
END;

CREATE TRIGGER source_node_fts_delete AFTER DELETE ON nodes
BEGIN
    DELETE FROM source_node_fts WHERE rowid = old.rowid;
END;

CREATE TRIGGER source_node_fts_update
AFTER UPDATE OF project_id, label, file_path, kind, tombstoned_at_ms ON nodes
BEGIN
    DELETE FROM source_node_fts WHERE rowid = old.rowid;
    INSERT INTO source_node_fts(rowid, project_id, label, file_path)
    SELECT new.rowid, replace(replace(new.project_id, '-', ''), '_', ''), new.label, new.file_path
    WHERE new.tombstoned_at_ms IS NULL AND new.file_path IS NOT NULL
      AND new.kind <> 'ExternalReference';
END;
