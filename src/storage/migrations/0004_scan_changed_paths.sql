-- Record the working-tree paths that were dirty when a structural scan
-- committed. Incremental freshness planning needs the prior dirty set to
-- rescan files whose local edits were later reverted; NULL means the set is
-- unknown (pre-migration scans) and forces a conservative full rescan.
ALTER TABLE structural_scans
    ADD COLUMN changed_paths_json TEXT
    CHECK (changed_paths_json IS NULL OR json_valid(changed_paths_json));
