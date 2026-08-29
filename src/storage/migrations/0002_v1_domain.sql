CREATE TABLE commits (
    project_id      TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    hash            TEXT NOT NULL,
    parents_json    TEXT NOT NULL CHECK (json_valid(parents_json)),
    authored_at_ms  INTEGER,
    subject         TEXT,
    PRIMARY KEY (project_id, hash)
) STRICT;

CREATE TABLE structural_scans (
    id                  TEXT PRIMARY KEY NOT NULL,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    provider            TEXT NOT NULL,
    provider_version    TEXT NOT NULL,
    head                TEXT,
    diff_fingerprint    TEXT NOT NULL,
    started_at_ms       INTEGER NOT NULL,
    completed_at_ms     INTEGER NOT NULL,
    file_count          INTEGER NOT NULL CHECK (file_count >= 0),
    node_count          INTEGER NOT NULL CHECK (node_count >= 0),
    edge_count          INTEGER NOT NULL CHECK (edge_count >= 0),
    incremental         INTEGER NOT NULL CHECK (incremental IN (0, 1))
) STRICT;

CREATE INDEX structural_scans_project_time
    ON structural_scans(project_id, completed_at_ms DESC);

CREATE TABLE indexed_files (
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    path                TEXT NOT NULL,
    language            TEXT NOT NULL,
    content_digest      TEXT NOT NULL,
    size_bytes          INTEGER NOT NULL CHECK (size_bytes >= 0),
    scan_id             TEXT NOT NULL REFERENCES structural_scans(id),
    indexed_at_ms       INTEGER NOT NULL,
    PRIMARY KEY (project_id, path)
) STRICT;

CREATE TABLE nodes (
    id                  TEXT PRIMARY KEY NOT NULL,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    kind                TEXT NOT NULL,
    stable_key          TEXT NOT NULL,
    label               TEXT NOT NULL,
    file_path           TEXT,
    language            TEXT,
    start_byte          INTEGER,
    end_byte            INTEGER,
    current_digest      TEXT NOT NULL,
    source_kind         TEXT NOT NULL,
    source_ref          TEXT NOT NULL,
    extractor_version   TEXT NOT NULL,
    observed_at_ms      INTEGER NOT NULL,
    tombstoned_at_ms    INTEGER,
    UNIQUE(project_id, stable_key)
) STRICT;

CREATE INDEX nodes_project_label ON nodes(project_id, label);
CREATE INDEX nodes_project_file ON nodes(project_id, file_path);
CREATE INDEX nodes_project_active ON nodes(project_id, kind)
    WHERE tombstoned_at_ms IS NULL;

CREATE TABLE edges (
    id                  TEXT PRIMARY KEY NOT NULL,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    source_id           TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    relation            TEXT NOT NULL,
    target_id           TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    confidence          REAL NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),
    source_kind         TEXT NOT NULL,
    source_ref          TEXT NOT NULL,
    content_digest      TEXT NOT NULL,
    extractor_version   TEXT NOT NULL,
    observed_at_ms      INTEGER NOT NULL,
    tombstoned_at_ms    INTEGER,
    UNIQUE(project_id, source_id, relation, target_id)
) STRICT;

CREATE INDEX edges_source_active ON edges(project_id, source_id, relation)
    WHERE tombstoned_at_ms IS NULL;
CREATE INDEX edges_target_active ON edges(project_id, target_id, relation)
    WHERE tombstoned_at_ms IS NULL;

CREATE TABLE tasks (
    id                  TEXT PRIMARY KEY NOT NULL,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    session_id          TEXT,
    prompt              TEXT NOT NULL,
    prompt_digest       TEXT NOT NULL,
    status              TEXT NOT NULL CHECK (status IN ('open','success','partial','failure','abandoned')),
    accepted_by_user    INTEGER CHECK (accepted_by_user IN (0, 1)),
    started_at_ms       INTEGER NOT NULL,
    completed_at_ms     INTEGER,
    final_commit        TEXT
) STRICT;

CREATE INDEX tasks_project_time ON tasks(project_id, started_at_ms DESC);

CREATE TABLE attempts (
    id                  TEXT PRIMARY KEY NOT NULL,
    task_id             TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    ordinal             INTEGER NOT NULL CHECK (ordinal > 0),
    strategy_summary    TEXT NOT NULL,
    outcome             TEXT NOT NULL CHECK (outcome IN ('unknown','success','failure','partial')),
    files_json          TEXT NOT NULL CHECK (json_valid(files_json)),
    created_at_ms       INTEGER NOT NULL,
    UNIQUE(task_id, ordinal)
) STRICT;

CREATE TABLE evidence (
    id                  TEXT PRIMARY KEY NOT NULL,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    task_id             TEXT REFERENCES tasks(id) ON DELETE SET NULL,
    attempt_id          TEXT REFERENCES attempts(id) ON DELETE SET NULL,
    kind                TEXT NOT NULL,
    strength            TEXT NOT NULL CHECK (strength IN ('deterministic','observational','inferred')),
    commit_hash         TEXT,
    command             TEXT,
    exit_code           INTEGER,
    artifact_digest     TEXT NOT NULL,
    metadata_json       TEXT NOT NULL CHECK (json_valid(metadata_json)),
    observed_at_ms      INTEGER NOT NULL
) STRICT;

CREATE INDEX evidence_project_time ON evidence(project_id, observed_at_ms DESC);

CREATE TABLE memories (
    id                  TEXT PRIMARY KEY NOT NULL,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    scope_type          TEXT NOT NULL,
    scope_id            TEXT NOT NULL,
    kind                TEXT NOT NULL,
    epistemic_state     TEXT NOT NULL CHECK (epistemic_state IN ('candidate','observed','supported','validated','rejected')),
    status              TEXT NOT NULL CHECK (status IN ('active','suspect','stale','superseded','contradicted','archived','purged')),
    claim               TEXT NOT NULL,
    normalized_claim    TEXT NOT NULL,
    rationale           TEXT NOT NULL,
    confidence          REAL NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),
    importance          REAL NOT NULL CHECK (importance >= 0.0 AND importance <= 1.0),
    tags_json           TEXT NOT NULL CHECK (json_valid(tags_json)),
    token_estimate      INTEGER NOT NULL CHECK (token_estimate >= 0),
    valid_from_commit   TEXT,
    valid_to_commit     TEXT,
    known_from_ms       INTEGER NOT NULL,
    known_to_ms         INTEGER,
    superseded_by       TEXT REFERENCES memories(id),
    contradicted_by     TEXT REFERENCES memories(id),
    trust_source        TEXT NOT NULL,
    retrieved_count     INTEGER NOT NULL DEFAULT 0,
    injected_count      INTEGER NOT NULL DEFAULT 0,
    acknowledged_count  INTEGER NOT NULL DEFAULT 0,
    successful_count    INTEGER NOT NULL DEFAULT 0,
    harmful_count       INTEGER NOT NULL DEFAULT 0,
    updated_at_ms       INTEGER NOT NULL
) STRICT;

CREATE INDEX memories_project_state
    ON memories(project_id, status, epistemic_state);
CREATE INDEX memories_project_normalized_claim
    ON memories(project_id, normalized_claim);

CREATE TABLE memory_evidence (
    memory_id       TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    evidence_id     TEXT NOT NULL REFERENCES evidence(id) ON DELETE CASCADE,
    relation        TEXT NOT NULL CHECK (relation IN ('supports','contradicts','invalidates')),
    PRIMARY KEY (memory_id, evidence_id, relation)
) STRICT;

CREATE TABLE memory_sources (
    memory_id       TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    task_id         TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    PRIMARY KEY (memory_id, task_id)
) STRICT;

CREATE TABLE memory_nodes (
    memory_id       TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    node_id         TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    observed_digest TEXT NOT NULL,
    PRIMARY KEY (memory_id, node_id)
) STRICT;

CREATE VIRTUAL TABLE memories_fts USING fts5(
    memory_id UNINDEXED,
    project_id UNINDEXED,
    claim,
    rationale,
    tags
);

CREATE TABLE context_runs (
    id                  TEXT PRIMARY KEY NOT NULL,
    request_id          TEXT NOT NULL UNIQUE,
    project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    mode                TEXT NOT NULL CHECK (mode IN ('brain','off')),
    task_text_digest    TEXT NOT NULL,
    token_budget        INTEGER NOT NULL,
    selected_tokens     INTEGER NOT NULL,
    candidate_count     INTEGER NOT NULL,
    stable_prefix_hash  TEXT NOT NULL,
    stable_prefix       TEXT NOT NULL,
    dynamic_context     TEXT NOT NULL,
    compile_ms          INTEGER NOT NULL,
    created_at_ms       INTEGER NOT NULL
) STRICT;

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

CREATE TABLE usage_events (
    id              TEXT PRIMARY KEY NOT NULL,
    source_type     TEXT NOT NULL,
    source_id       TEXT NOT NULL,
    context_run_id  TEXT REFERENCES context_runs(id) ON DELETE SET NULL,
    usage_kind      TEXT NOT NULL,
    outcome         TEXT,
    created_at_ms   INTEGER NOT NULL
) STRICT;

CREATE TABLE eval_runs (
    id              TEXT PRIMARY KEY NOT NULL,
    benchmark_id    TEXT NOT NULL,
    mode            TEXT NOT NULL CHECK (mode IN ('brain','off')),
    model_hash      TEXT NOT NULL,
    config_hash     TEXT NOT NULL,
    metrics_json    TEXT NOT NULL CHECK (json_valid(metrics_json)),
    created_at_ms   INTEGER NOT NULL
) STRICT;

CREATE TABLE jobs (
    id              TEXT PRIMARY KEY NOT NULL,
    project_id      TEXT REFERENCES projects(id) ON DELETE CASCADE,
    job_type        TEXT NOT NULL,
    priority        INTEGER NOT NULL,
    status          TEXT NOT NULL CHECK (status IN ('queued','running','complete','failed')),
    payload_json    TEXT NOT NULL CHECK (json_valid(payload_json)),
    attempts        INTEGER NOT NULL DEFAULT 0,
    not_before_ms   INTEGER NOT NULL,
    created_at_ms   INTEGER NOT NULL,
    updated_at_ms   INTEGER NOT NULL
) STRICT;

CREATE INDEX jobs_ready ON jobs(status, priority DESC, not_before_ms);

CREATE TABLE blobs (
    digest          TEXT PRIMARY KEY NOT NULL,
    size_bytes      INTEGER NOT NULL CHECK (size_bytes >= 0),
    content_type    TEXT NOT NULL,
    created_at_ms   INTEGER NOT NULL,
    refcount        INTEGER NOT NULL CHECK (refcount >= 0)
) STRICT;
