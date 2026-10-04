-- Hook-observed agent tool calls, used to associate delivered source pointers
-- with later reads and edits and to measure Read volume. One row per agent
-- tool call and repository. Rows keep repository-relative paths, flags and
-- sizes only: never file content, shell commands or error text.
CREATE TABLE tool_observations (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    -- Digest of the agent's session and tool-use IDs; one call, many hooks.
    call_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
    -- Latest context delivered to this session and repository when the call
    -- was first observed. NULL means no delivered context was attributable.
    context_run_id TEXT REFERENCES context_runs(id) ON DELETE SET NULL,
    mode TEXT CHECK (mode IN ('off','repo','brain')),
    source TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('read','edit','search','shell','other')),
    paths_json TEXT NOT NULL CHECK (json_valid(paths_json) AND json_type(paths_json) = 'array'),
    -- Read calls only. NULL is unknown, never zero or false.
    ranged INTEGER CHECK (ranged IN (0,1)),
    result_bytes INTEGER CHECK (result_bytes >= 0),
    limit_hit INTEGER CHECK (limit_hit IN (0,1)),
    -- NULL until a post-tool hook reports the result.
    outcome TEXT CHECK (outcome IN ('ok','failed')),
    observed_at_ms INTEGER NOT NULL CHECK (observed_at_ms >= 0),
    recorded_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE(project_id, call_id)
) STRICT;
CREATE INDEX tool_observations_project_time ON tool_observations(project_id, observed_at_ms);
CREATE INDEX tool_observations_session_time ON tool_observations(session_id, observed_at_ms);
CREATE INDEX tool_observations_task ON tool_observations(task_id);
CREATE INDEX tool_observations_context ON tool_observations(context_run_id);
CREATE INDEX tool_observations_time ON tool_observations(observed_at_ms);
