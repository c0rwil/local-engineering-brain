-- Observed resource usage is separate from the memory provenance ledger.
-- Each sample is an additive delta, never a cumulative session snapshot.
CREATE TABLE resource_usage (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    sample_id TEXT NOT NULL,
    source TEXT NOT NULL,
    task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
    context_run_id TEXT REFERENCES context_runs(id) ON DELETE SET NULL,
    session_id TEXT,
    provider TEXT,
    model TEXT,
    mode TEXT CHECK (mode IN ('off','repo','brain')),
    input_tokens INTEGER CHECK (input_tokens >= 0),
    output_tokens INTEGER CHECK (output_tokens >= 0),
    cache_read_tokens INTEGER CHECK (cache_read_tokens >= 0),
    cache_write_tokens INTEGER CHECK (cache_write_tokens >= 0),
    tool_calls INTEGER CHECK (tool_calls >= 0),
    context_bytes INTEGER CHECK (context_bytes >= 0),
    context_tokens INTEGER CHECK (context_tokens >= 0),
    cost_usd REAL CHECK (cost_usd >= 0),
    request_json TEXT NOT NULL CHECK (json_valid(request_json)),
    observed_at_ms INTEGER NOT NULL CHECK (observed_at_ms >= 0),
    recorded_at_ms INTEGER NOT NULL,
    UNIQUE(project_id, source, sample_id)
) STRICT;
CREATE INDEX resource_usage_project_time ON resource_usage(project_id, observed_at_ms);
CREATE INDEX resource_usage_task ON resource_usage(task_id);
CREATE INDEX resource_usage_context ON resource_usage(context_run_id);
