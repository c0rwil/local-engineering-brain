-- Operator-run expiry of pending lesson reviews that only new verification
-- work can unblock. `expired` is a terminal status distinct from a rejection,
-- and a person can reopen an expired review. SQLite cannot change a CHECK
-- constraint in place, so `lesson_reviews` is rebuilt. `lesson_judgments`
-- references it with ON DELETE CASCADE, and dropping a referenced table first
-- deletes its rows, so the judgments table is rebuilt against the new reviews
-- table before the old one is dropped. Every review and judgment row is kept.
CREATE TABLE lesson_reviews_v2 (
    id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(id),
    project_id TEXT NOT NULL REFERENCES projects(id),
    claim TEXT NOT NULL,
    normalized_claim TEXT NOT NULL,
    rationale TEXT NOT NULL,
    evidence_ids_json TEXT NOT NULL CHECK (json_valid(evidence_ids_json)),
    status TEXT NOT NULL CHECK (status IN ('pending','accepted','rejected','expired')),
    memory_id TEXT REFERENCES memories(id) ON DELETE SET NULL,
    expires_at_ms INTEGER,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE(task_id, normalized_claim)
) STRICT;

INSERT INTO lesson_reviews_v2(id, task_id, project_id, claim, normalized_claim, rationale,
    evidence_ids_json, status, memory_id, expires_at_ms, created_at_ms, updated_at_ms)
SELECT id, task_id, project_id, claim, normalized_claim, rationale, evidence_ids_json, status,
    memory_id, expires_at_ms, created_at_ms, updated_at_ms
FROM lesson_reviews ORDER BY rowid;

CREATE TABLE lesson_judgments_v2 (
    id TEXT PRIMARY KEY NOT NULL,
    review_id TEXT NOT NULL REFERENCES lesson_reviews_v2(id) ON DELETE CASCADE,
    packet_digest TEXT NOT NULL,
    attempt INTEGER NOT NULL CHECK (attempt > 0),
    model TEXT NOT NULL,
    outcome TEXT NOT NULL CHECK (outcome IN ('accepted','rejected','deferred','runtime_error','stale')),
    reason TEXT NOT NULL,
    result_json TEXT NOT NULL CHECK (json_valid(result_json)),
    input_tokens INTEGER CHECK (input_tokens >= 0),
    output_tokens INTEGER CHECK (output_tokens >= 0),
    reported_cost_usd REAL CHECK (reported_cost_usd >= 0),
    duration_ms INTEGER NOT NULL CHECK (duration_ms >= 0),
    recorded_at_ms INTEGER NOT NULL,
    retry_after_ms INTEGER,
    UNIQUE (review_id, packet_digest, attempt)
) STRICT;

INSERT INTO lesson_judgments_v2(id, review_id, packet_digest, attempt, model, outcome, reason,
    result_json, input_tokens, output_tokens, reported_cost_usd, duration_ms, recorded_at_ms,
    retry_after_ms)
SELECT id, review_id, packet_digest, attempt, model, outcome, reason, result_json, input_tokens,
    output_tokens, reported_cost_usd, duration_ms, recorded_at_ms, retry_after_ms
FROM lesson_judgments ORDER BY rowid;

-- The old judgments go first, so the implicit delete of the old reviews
-- cascades into nothing.
DROP TABLE lesson_judgments;
DROP TABLE lesson_reviews;
-- Renaming also points the judgments' foreign key at the new name.
ALTER TABLE lesson_reviews_v2 RENAME TO lesson_reviews;
ALTER TABLE lesson_judgments_v2 RENAME TO lesson_judgments;
CREATE INDEX lesson_judgments_review_packet ON lesson_judgments(review_id, packet_digest, attempt DESC);
CREATE INDEX lesson_judgments_time ON lesson_judgments(recorded_at_ms DESC);

-- Operator status changes of a review: an expiry with the judge's gate that
-- held at the time, and a reopening. Rows are an audit trail; nothing reads
-- them to decide.
CREATE TABLE lesson_review_actions (
    id TEXT PRIMARY KEY NOT NULL,
    review_id TEXT NOT NULL REFERENCES lesson_reviews(id) ON DELETE CASCADE,
    action TEXT NOT NULL CHECK (action IN ('expired','reopened')),
    gate_class TEXT
        CHECK (gate_class IN ('no_verified_evidence','unanchored_files','revision_changed')),
    gate_reason TEXT,
    -- The age threshold of the expiry run.
    older_than_ms INTEGER CHECK (older_than_ms >= 0),
    recorded_at_ms INTEGER NOT NULL,
    CHECK ((action = 'expired') = (gate_class IS NOT NULL AND gate_reason IS NOT NULL))
) STRICT;
CREATE INDEX lesson_review_actions_review ON lesson_review_actions(review_id, recorded_at_ms);
