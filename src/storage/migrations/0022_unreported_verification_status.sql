-- A recognized verification run whose shell kept its exit status, but whose
-- coding client reported none (Codex without a rollout record, a failure
-- without an exit code, an interrupted call), counts as `unreported` instead
-- of recordable. SQLite cannot change a CHECK constraint in place, so the
-- table is rebuilt with its rows and indexes.
CREATE TABLE verification_observations_v2 (
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
        CHECK (hidden_reason IN
            ('syntax','background','or','sequence','pipe','directory','unreported')),
    -- 1 when this call's hook output carried a status note.
    note_delivered INTEGER NOT NULL CHECK (note_delivered IN (0,1)),
    observed_at_ms INTEGER NOT NULL CHECK (observed_at_ms >= 0),
    recorded_at_ms INTEGER NOT NULL,
    CHECK (note_delivered = 0 OR hidden_reason IS NOT NULL),
    UNIQUE(project_id, call_id)
) STRICT;

INSERT INTO verification_observations_v2(id, project_id, call_id, session_id, conversation_id,
    task_id, source, kind, hidden_reason, note_delivered, observed_at_ms, recorded_at_ms)
SELECT id, project_id, call_id, session_id, conversation_id, task_id, source, kind,
    hidden_reason, note_delivered, observed_at_ms, recorded_at_ms
FROM verification_observations;

DROP TABLE verification_observations;
ALTER TABLE verification_observations_v2 RENAME TO verification_observations;
CREATE INDEX verification_observations_project_time
    ON verification_observations(project_id, observed_at_ms);
CREATE INDEX verification_observations_conversation
    ON verification_observations(conversation_id, observed_at_ms);
CREATE INDEX verification_observations_task ON verification_observations(task_id);
CREATE INDEX verification_observations_time ON verification_observations(observed_at_ms);
