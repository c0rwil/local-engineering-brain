-- Product intent and cited documentation are distinct from injectable memory.
CREATE TABLE product_records (
    id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (version > 0),
    document_json TEXT NOT NULL CHECK (json_valid(document_json)),
    stamps_json TEXT NOT NULL CHECK (json_valid(stamps_json)),
    archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0,1)),
    updated_at_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX product_records_kind ON product_records(kind, archived);
CREATE TABLE product_revisions (
    record_id TEXT NOT NULL REFERENCES product_records(id),
    version INTEGER NOT NULL,
    document_json TEXT NOT NULL CHECK (json_valid(document_json)),
    stamps_json TEXT NOT NULL CHECK (json_valid(stamps_json)),
    archived INTEGER NOT NULL CHECK (archived IN (0,1)),
    recorded_at_ms INTEGER NOT NULL,
    PRIMARY KEY (record_id, version)
) STRICT;
CREATE TABLE lesson_reviews (
    id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(id),
    project_id TEXT NOT NULL REFERENCES projects(id),
    claim TEXT NOT NULL,
    normalized_claim TEXT NOT NULL,
    rationale TEXT NOT NULL,
    evidence_ids_json TEXT NOT NULL CHECK (json_valid(evidence_ids_json)),
    status TEXT NOT NULL CHECK (status IN ('pending','accepted','rejected')),
    memory_id TEXT REFERENCES memories(id) ON DELETE SET NULL,
    expires_at_ms INTEGER,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE(task_id, normalized_claim)
) STRICT;
-- Signals are provenance records, never implicit human acceptance.
CREATE TABLE task_signals (
    task_id TEXT NOT NULL REFERENCES tasks(id),
    kind TEXT NOT NULL CHECK (kind IN ('verification','commit_landed','pr_merged','user_acceptance')),
    reference TEXT NOT NULL,
    evidence_id TEXT REFERENCES evidence(id),
    recorded_at_ms INTEGER NOT NULL,
    PRIMARY KEY(task_id, kind, reference)
) STRICT;
CREATE TABLE memory_expiry (
    memory_id TEXT PRIMARY KEY NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    expires_at_ms INTEGER NOT NULL
) STRICT;

-- Purged evidence has no surviving observation; a digest alone is not support.
DROP VIEW verified_supporting_evidence;
CREATE VIEW verified_supporting_evidence AS
SELECT
    e.id AS evidence_id,
    e.project_id AS project_id,
    e.task_id AS task_id
FROM evidence e
JOIN tasks t ON t.id = e.task_id AND t.project_id = e.project_id
LEFT JOIN attempts a ON a.id = e.attempt_id
WHERE t.status = 'success'
  AND (
      e.attempt_id IS NULL
      OR (a.task_id = e.task_id AND a.outcome = 'success')
  )
  AND (
      e.commit_hash IS NULL
      OR t.final_commit IS NULL
      OR e.commit_hash = t.final_commit
  )
  AND (
      (
          e.kind IN (
              'test_result', 'build_result', 'lint_result',
              'typecheck_result', 'benchmark_result'
          )
          AND e.strength = 'deterministic'
          AND e.exit_code = 0
          AND e.command IS NOT NULL
          AND length(trim(e.command)) > 0
      )
      OR (
          e.kind IN ('user_acceptance', 'runtime_observation', 'trace', 'metric')
          AND e.strength = 'observational'
          AND (e.exit_code IS NULL OR e.exit_code = 0)
      )
  )
  AND (e.artifact_text IS NOT NULL OR e.artifact_blob_digest IS NOT NULL);
