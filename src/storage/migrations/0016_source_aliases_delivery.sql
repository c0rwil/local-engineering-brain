-- Owner aliases preserve the destination's identity while making named fields
-- searchable. Existing rows are populated by the version-aware source reindex.
ALTER TABLE nodes ADD COLUMN search_aliases TEXT NOT NULL DEFAULT '';
-- Persist the digest with the output it describes; replay must not relabel an
-- old context with metadata from a newer repository revision.
ALTER TABLE context_runs ADD COLUMN delivery_digest TEXT;

DROP TRIGGER source_node_fts_insert;
DROP TRIGGER source_node_fts_delete;
DROP TRIGGER source_node_fts_update;
DROP TABLE source_node_fts;

CREATE VIRTUAL TABLE source_node_fts USING fts5(
    project_id, label, file_path, search_aliases,
    tokenize = 'porter unicode61'
);

INSERT INTO source_node_fts(rowid, project_id, label, file_path, search_aliases)
SELECT rowid, replace(replace(project_id, '-', ''), '_', ''), label, file_path, search_aliases
FROM nodes
WHERE tombstoned_at_ms IS NULL AND file_path IS NOT NULL
  AND kind <> 'ExternalReference';

CREATE TRIGGER source_node_fts_insert AFTER INSERT ON nodes
WHEN new.tombstoned_at_ms IS NULL AND new.file_path IS NOT NULL
  AND new.kind <> 'ExternalReference'
BEGIN
    INSERT INTO source_node_fts(rowid, project_id, label, file_path, search_aliases)
    VALUES(new.rowid, replace(replace(new.project_id, '-', ''), '_', ''), new.label, new.file_path, new.search_aliases);
END;

CREATE TRIGGER source_node_fts_delete AFTER DELETE ON nodes
BEGIN
    DELETE FROM source_node_fts WHERE rowid = old.rowid;
END;

CREATE TRIGGER source_node_fts_update
AFTER UPDATE OF project_id, label, file_path, search_aliases, kind, tombstoned_at_ms ON nodes
BEGIN
    DELETE FROM source_node_fts WHERE rowid = old.rowid;
    INSERT INTO source_node_fts(rowid, project_id, label, file_path, search_aliases)
    SELECT new.rowid, replace(replace(new.project_id, '-', ''), '_', ''), new.label, new.file_path, new.search_aliases
    WHERE new.tombstoned_at_ms IS NULL AND new.file_path IS NOT NULL
      AND new.kind <> 'ExternalReference';
END;
