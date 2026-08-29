-- Reshape the V1 `jobs` table into the durable background job queue.
--
-- The table was created by migration 0002 but no code path ever wrote to it,
-- so dropping and recreating it loses no data. The new shape adds idempotent
-- enqueue identity (job_type, dedupe_key), bounded-retry bookkeeping
-- (max_attempts, last_error, not_before_ms backoff), and a recorded result.

DROP INDEX jobs_ready;
DROP TABLE jobs;

CREATE TABLE jobs (
    id              TEXT PRIMARY KEY NOT NULL,
    project_id      TEXT REFERENCES projects(id) ON DELETE CASCADE,
    job_type        TEXT NOT NULL,
    priority        INTEGER NOT NULL,
    status          TEXT NOT NULL
                    CHECK (status IN ('pending','running','succeeded','failed','dead')),
    payload_json    TEXT NOT NULL CHECK (json_valid(payload_json)),
    dedupe_key      TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    max_attempts    INTEGER NOT NULL CHECK (max_attempts > 0),
    last_error      TEXT,
    result_json     TEXT CHECK (result_json IS NULL OR json_valid(result_json)),
    not_before_ms   INTEGER NOT NULL,
    created_at_ms   INTEGER NOT NULL,
    updated_at_ms   INTEGER NOT NULL,
    completed_at_ms INTEGER,
    UNIQUE (job_type, dedupe_key)
) STRICT;

CREATE INDEX jobs_ready ON jobs(status, priority DESC, not_before_ms);
