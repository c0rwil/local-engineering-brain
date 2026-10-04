-- A model decision is an audit record, never source evidence or user acceptance.
CREATE TABLE lesson_judgments (
    id TEXT PRIMARY KEY NOT NULL,
    review_id TEXT NOT NULL REFERENCES lesson_reviews(id) ON DELETE CASCADE,
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
CREATE INDEX lesson_judgments_review_packet ON lesson_judgments(review_id, packet_digest, attempt DESC);
CREATE INDEX lesson_judgments_time ON lesson_judgments(recorded_at_ms DESC);
