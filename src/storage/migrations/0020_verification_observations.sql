-- Hook-observed verification runs (tests, lints, type checks, builds), used to
-- report how many carry their own exit status and how that share moves after
-- Engram's status note. One row per agent tool call and repository. Rows keep
-- the verification kind and the label of the cause that hid the status only:
-- never commands, output, error text or paths.
CREATE TABLE verification_observations (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    -- Digest of the agent's session and tool-use IDs; one call, many hooks.
    call_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    -- The conversation a status note is limited to: the agent session, or a
    -- subagent thread within it. Notes go once per conversation and cause.
    conversation_id TEXT NOT NULL,
    task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
    source TEXT NOT NULL,
    kind TEXT NOT NULL
        CHECK (kind IN ('test_result','lint_result','typecheck_result','build_result')),
    -- Why the exit status could not be recorded; NULL when it could.
    hidden_reason TEXT
        CHECK (hidden_reason IN ('syntax','background','or','sequence','pipe','directory')),
    -- 1 when this call's hook output carried a status note.
    note_delivered INTEGER NOT NULL CHECK (note_delivered IN (0,1)),
    observed_at_ms INTEGER NOT NULL CHECK (observed_at_ms >= 0),
    recorded_at_ms INTEGER NOT NULL,
    CHECK (note_delivered = 0 OR hidden_reason IS NOT NULL),
    UNIQUE(project_id, call_id)
) STRICT;
CREATE INDEX verification_observations_project_time
    ON verification_observations(project_id, observed_at_ms);
CREATE INDEX verification_observations_conversation
    ON verification_observations(conversation_id, observed_at_ms);
CREATE INDEX verification_observations_task ON verification_observations(task_id);
CREATE INDEX verification_observations_time ON verification_observations(observed_at_ms);
