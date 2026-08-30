-- Rebuild the memory FTS index with Porter stemming so inflected task text
-- ("retries", "milliseconds") matches stored claims ("retry",
-- "millisecond"). The index is derived state: dropping and refilling it from
-- canonical memories inside the migration transaction loses nothing.
DROP TABLE memories_fts;
CREATE VIRTUAL TABLE memories_fts USING fts5(
    memory_id UNINDEXED,
    project_id UNINDEXED,
    claim,
    rationale,
    tags,
    tokenize = 'porter unicode61'
);
INSERT INTO memories_fts(memory_id, project_id, claim, rationale, tags)
SELECT id, project_id, claim, rationale,
       (SELECT COALESCE(group_concat(value, ' '), '')
        FROM json_each(m.tags_json))
FROM memories m
WHERE m.status <> 'purged';
