-- Agents and hooks record revisions as full or abbreviated hashes. Evidence
-- recorded at `abc1234` supports a task that finished at `abc1234def...`:
-- the shorter revision (at least 7 characters) must be a case-insensitive
-- prefix of the longer one. Everything else is unchanged from 0012.
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
      OR (
          min(length(e.commit_hash), length(t.final_commit)) >= 7
          AND lower(substr(e.commit_hash, 1, min(length(e.commit_hash), length(t.final_commit))))
              = lower(substr(t.final_commit, 1, min(length(e.commit_hash), length(t.final_commit))))
      )
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
