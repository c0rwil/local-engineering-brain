ALTER TABLE context_runs RENAME TO context_runs_legacy;
ALTER TABLE context_items RENAME TO context_items_legacy;
ALTER TABLE usage_events RENAME TO usage_events_legacy;

CREATE TABLE context_runs (
    id                  TEXT PRIMARY KEY NOT NULL,
    request_id          TEXT NOT NULL,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    request_fingerprint TEXT NOT NULL,
    mode                TEXT NOT NULL CHECK (mode IN ('brain','off')),
    task_text_digest    TEXT NOT NULL,
    token_budget        INTEGER NOT NULL,
    selected_tokens     INTEGER NOT NULL,
    candidate_count     INTEGER NOT NULL,
    stable_prefix_hash  TEXT NOT NULL,
    stable_prefix       TEXT NOT NULL,
    dynamic_context     TEXT NOT NULL,
    compile_ms          INTEGER NOT NULL,
    created_at_ms       INTEGER NOT NULL,
    UNIQUE (project_id, request_id)
) STRICT;

INSERT INTO context_runs (
    id, request_id, project_id, request_fingerprint, mode, task_text_digest,
    token_budget, selected_tokens, candidate_count, stable_prefix_hash,
    stable_prefix, dynamic_context, compile_ms, created_at_ms
)
SELECT
    id, request_id, project_id,
    'legacy:' || task_text_digest || ':' || mode || ':' || CAST(token_budget AS TEXT),
    mode, task_text_digest, token_budget, selected_tokens, candidate_count,
    stable_prefix_hash, stable_prefix, dynamic_context, compile_ms, created_at_ms
FROM context_runs_legacy;

CREATE TABLE context_items (
    context_run_id  TEXT NOT NULL REFERENCES context_runs(id) ON DELETE CASCADE,
    ordinal         INTEGER NOT NULL,
    source_type     TEXT NOT NULL,
    source_id       TEXT NOT NULL,
    score           REAL NOT NULL,
    tokens          INTEGER NOT NULL,
    why_json        TEXT NOT NULL CHECK (json_valid(why_json)),
    PRIMARY KEY (context_run_id, ordinal)
) STRICT;

INSERT INTO context_items
SELECT context_run_id, ordinal, source_type, source_id, score, tokens, why_json
FROM context_items_legacy;

CREATE TABLE usage_events (
    id              TEXT PRIMARY KEY NOT NULL,
    source_type     TEXT NOT NULL,
    source_id       TEXT NOT NULL,
    context_run_id  TEXT REFERENCES context_runs(id) ON DELETE SET NULL,
    usage_kind      TEXT NOT NULL,
    outcome         TEXT,
    created_at_ms   INTEGER NOT NULL
) STRICT;

INSERT INTO usage_events
SELECT id, source_type, source_id, context_run_id, usage_kind, outcome, created_at_ms
FROM usage_events_legacy;

DROP TABLE context_items_legacy;
DROP TABLE usage_events_legacy;
DROP TABLE context_runs_legacy;

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
  );
