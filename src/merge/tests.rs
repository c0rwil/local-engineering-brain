//! Project merge tests. Every fixture is synthetic.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use rusqlite::{Connection, params};
use tempfile::TempDir;

use super::*;
use crate::events::{EventStore, ProjectProjection};
use crate::ids::{AttemptId, ContextRunId, EdgeId, EvidenceId, NodeId, TaskId};
use crate::memory::{MemoryDraft, MemoryStore};

const REMOTE: &str = "example.com/org/app";
const ROOT: &str = "0123456789abcdef0123456789abcdef01234567";

struct Fixture {
    _temp: TempDir,
    database: Database,
    survivor: ProjectId,
    retired: ProjectId,
    other: ProjectId,
    client: ProjectId,
    view: ProjectId,
    /// Retired node whose stable key the survivor also indexes.
    shared_node: NodeId,
    /// The survivor's node with that stable key.
    survivor_node: NodeId,
    /// Retired node only the retired index has.
    retired_only_node: NodeId,
    anchored: MemoryId,
    pending: MemoryId,
    duplicate_survivor: MemoryId,
    duplicate_retired: MemoryId,
    retired_task: TaskId,
}

fn id(raw: &str) -> ProjectId {
    ProjectId::parse(raw).expect("project id")
}

fn draft(project: &ProjectId, claim: &str, nodes: Vec<NodeId>) -> MemoryDraft {
    MemoryDraft {
        scope_type: "project".to_owned(),
        scope_id: project.to_string(),
        kind: "lesson".to_owned(),
        claim: claim.to_owned(),
        rationale: "synthetic fixture".to_owned(),
        confidence: 0.4,
        importance: 0.5,
        tags: vec!["fixture".to_owned()],
        valid_from_commit: None,
        valid_to_commit: None,
        trust_source: "human_stated".to_owned(),
        evidence_ids: Vec::new(),
        source_task_ids: Vec::new(),
        node_ids: nodes,
    }
}

fn append(database: &Database, event: &DomainEvent) {
    EventStore::new(database.clone())
        .append(event)
        .expect("append fixture event");
}

fn register(database: &Database, project: &ProjectId, name: &str) {
    append(
        database,
        &DomainEvent::ProjectRegistered {
            project_id: project.clone(),
            name: name.to_owned(),
            canonical_remote: None,
            root_commit: None,
            parent_project_id: None,
        },
    );
}

fn fact(database: &Database, project: &ProjectId, key: &str, value: &str) {
    append(
        database,
        &DomainEvent::ProjectFactSet {
            project_id: project.clone(),
            key: key.to_owned(),
            value: value.to_owned(),
            note: None,
        },
    );
}

fn link(database: &Database, from: &ProjectId, to: &ProjectId, relation: &str) {
    append(
        database,
        &DomainEvent::ProjectLinked {
            project_id: from.clone(),
            linked_project_id: to.clone(),
            relation: relation.to_owned(),
            note: None,
        },
    );
}

fn node(connection: &Connection, project: &ProjectId, key: &str, digest: &str) -> NodeId {
    let node = NodeId::new();
    connection
        .execute(
            "INSERT INTO nodes(id, project_id, kind, stable_key, label, file_path,
                 current_digest, source_kind, source_ref, extractor_version, observed_at_ms)
             VALUES (?1, ?2, 'Function', ?3, ?3, 'src/lib.rs', ?4, 'native', 'head',
                     'tree-sitter-v2', 0)",
            params![node.as_str(), project.as_str(), key, digest],
        )
        .expect("node");
    node
}

#[allow(clippy::too_many_lines)]
impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("temp dir");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let survivor = id("prj_aaaaaaaa-0000-4000-8000-000000000001");
        let retired = id("prj_bbbbbbbb-0000-4000-8000-000000000002");
        let other = id("prj_cccccccc-0000-4000-8000-000000000003");
        let client = id("prj_dddddddd-0000-4000-8000-000000000004");
        let view = id("prj_eeeeeeee-0000-4000-8000-000000000005");
        register(&database, &survivor, "app");
        register(&database, &retired, "app-copy");
        register(&database, &other, "lib");
        register(&database, &client, "client");
        register(&database, &view, "app-copy@origin");
        for (project, path) in [
            (&survivor, "/work/app"),
            (&survivor, "/work/app-worktree"),
            (&retired, "/work/app-copy"),
        ] {
            append(
                &database,
                &DomainEvent::ProjectAliasAdded {
                    project_id: project.clone(),
                    path: path.to_owned(),
                },
            );
        }
        fact(&database, &survivor, "hosting.base_url", "a.example");
        fact(&database, &survivor, "internal.hidden", "true");
        fact(&database, &retired, "hosting.base_url", "b.example");
        fact(&database, &retired, "storage.db", "sqlite");
        fact(&database, &retired, "internal.hidden", "false");
        fact(&database, &retired, "internal.origin_view", view.as_str());
        fact(&database, &view, "internal.view_of", retired.as_str());
        link(&database, &retired, &other, "uses");
        link(&database, &survivor, &client, "calls-api-of");
        link(&database, &retired, &client, "uses");
        link(&database, &client, &retired, "calls-api-of");
        link(&database, &other, &retired, "uses");
        link(&database, &other, &survivor, "uses");
        link(&database, &survivor, &retired, "sibling");

        let connection = database.connect().expect("connection");
        let survivor_node = node(
            &connection,
            &survivor,
            "symbol:src/lib.rs:Function:run",
            "d1",
        );
        let shared_node = node(
            &connection,
            &retired,
            "symbol:src/lib.rs:Function:run",
            "d1",
        );
        let retired_only_node = node(
            &connection,
            &retired,
            "symbol:src/old.rs:Function:legacy",
            "d2",
        );
        connection
            .execute_batch(&format!(
                "INSERT INTO edges(id, project_id, source_id, relation, target_id, confidence,
                     source_kind, source_ref, content_digest, extractor_version, observed_at_ms)
                 VALUES ('{edge}', '{retired}', '{shared}', 'Calls', '{only}', 1.0, 'native',
                         'head', 'e', 'tree-sitter-v2', 0);
                 INSERT INTO structural_scans(id, project_id, provider, provider_version, head,
                     diff_fingerprint, started_at_ms, completed_at_ms, file_count, node_count,
                     edge_count, incremental)
                 VALUES ('scn_retired', '{retired}', 'tree_sitter', 'tree-sitter-v2', NULL,
                         'fp', 0, 0, 1, 2, 1, 0);
                 INSERT INTO indexed_files(project_id, path, language, content_digest,
                     size_bytes, scan_id, indexed_at_ms)
                 VALUES ('{retired}', 'src/lib.rs', 'rust', 'f', 10, 'scn_retired', 0);
                 INSERT INTO commits(project_id, hash, parents_json, authored_at_ms, subject)
                 VALUES ('{retired}', '{ROOT}', '[]', 0, 'initial');",
                edge = EdgeId::new(),
                retired = retired,
                shared = shared_node,
                only = retired_only_node,
            ))
            .expect("structural rows");

        let memories = MemoryStore::new(database.clone());
        let anchored = memories
            .create_candidate(
                &retired,
                &draft(&retired, "Run spawns the worker", vec![shared_node.clone()]),
            )
            .expect("anchored memory")
            .id;
        let pending = memories
            .create_candidate(
                &retired,
                &draft(
                    &retired,
                    "Legacy keeps the old path",
                    vec![retired_only_node.clone()],
                ),
            )
            .expect("pending memory")
            .id;
        let duplicate_retired = memories
            .create_candidate(
                &retired,
                &draft(&retired, "Writes go through the scheduler", vec![]),
            )
            .expect("retired duplicate")
            .id;
        let duplicate_survivor = memories
            .create_candidate(
                &survivor,
                &draft(&survivor, "Writes go through the scheduler", vec![]),
            )
            .expect("survivor duplicate")
            .id;

        let retired_task = TaskId::new();
        let survivor_task = TaskId::new();
        let attempt = AttemptId::new();
        let survivor_run = ContextRunId::new();
        let retired_run = ContextRunId::new();
        let other_run = ContextRunId::new();
        connection
            .execute_batch(&format!(
                "INSERT INTO tasks(id, project_id, prompt, prompt_digest, status, started_at_ms)
                 VALUES ('{retired_task}', '{retired}', 'p', 'd', 'open', 0),
                        ('{survivor_task}', '{survivor}', 'p', 'd', 'open', 0);
                 INSERT INTO attempts(id, task_id, ordinal, strategy_summary, outcome,
                     files_json, created_at_ms)
                 VALUES ('{attempt}', '{retired_task}', 1, 's', 'success', '[]', 0);
                 INSERT INTO evidence(id, project_id, task_id, attempt_id, kind, strength,
                     command, exit_code, artifact_digest, metadata_json, observed_at_ms)
                 VALUES ('{evidence}', '{retired}', '{retired_task}', '{attempt}',
                         'test_result', 'deterministic', 'cargo test', 0, 'a', '{{}}', 0);
                 INSERT INTO lesson_reviews(id, task_id, project_id, claim, normalized_claim,
                     rationale, evidence_ids_json, status, created_at_ms, updated_at_ms)
                 VALUES ('review_retired', '{retired_task}', '{retired}', 'c', 'c', 'r', '[]',
                         'pending', 0, 0);
                 INSERT INTO context_runs(id, request_id, project_id, request_fingerprint, mode,
                     task_text_digest, token_budget, selected_tokens, candidate_count,
                     stable_prefix_hash, stable_prefix, dynamic_context, compile_ms,
                     created_at_ms)
                 VALUES ('{survivor_run}', 'req-1', '{survivor}', 'f', 'brain', 'd', 100, 1, 1,
                         'h', '', '', 0, 0),
                        ('{retired_run}', 'req-1', '{retired}', 'f', 'brain', 'd', 100, 1, 1,
                         'h', '', '', 0, 0),
                        ('{other_run}', 'req-2', '{retired}', 'f', 'brain', 'd', 100, 1, 1,
                         'h', '', '', 0, 0);
                 INSERT INTO context_items(context_run_id, ordinal, source_type, source_id,
                     score, tokens, why_json)
                 VALUES ('{retired_run}', 0, 'memory', '{anchored}', 1.0, 1, '{{}}');
                 INSERT INTO resource_usage(id, project_id, sample_id, source, request_json,
                     observed_at_ms, recorded_at_ms)
                 VALUES ('use_s1', '{survivor}', 's1', 'claude_code',
                         '{{\"project_id\":\"{survivor}\"}}', 0, 0),
                        ('use_r1', '{retired}', 's1', 'claude_code',
                         '{{\"project_id\":\"{retired}\"}}', 0, 0),
                        ('use_r2', '{retired}', 's2', 'claude_code',
                         '{{\"project_id\":\"{retired}\"}}', 0, 0);
                 INSERT INTO tool_observations(id, project_id, call_id, session_id, source,
                     tool_name, action, paths_json, observed_at_ms, recorded_at_ms,
                     updated_at_ms)
                 VALUES ('obs_s1', '{survivor}', 'tool:c1', 's', 'claude_code', 'Read', 'read',
                         '[\"a.rs\"]', 0, 0, 0),
                        ('obs_r1', '{retired}', 'tool:c1', 's', 'claude_code', 'Read', 'read',
                         '[\"b.rs\",\"a.rs\"]', 0, 0, 0),
                        ('obs_r2', '{retired}', 'tool:c2', 's', 'claude_code', 'Read', 'read',
                         '[\"c.rs\"]', 0, 0, 0);
                 INSERT INTO verification_observations(id, project_id, call_id, session_id,
                     conversation_id, source, kind, note_delivered, observed_at_ms,
                     recorded_at_ms)
                 VALUES ('ver_s1', '{survivor}', 'tool:v1', 's', 'c', 'claude_code',
                         'test_result', 0, 0, 0),
                        ('ver_r1', '{retired}', 'tool:v1', 's', 'c', 'claude_code',
                         'test_result', 0, 0, 0);
                 INSERT INTO jobs(id, project_id, job_type, priority, status, payload_json,
                     dedupe_key, max_attempts, not_before_ms, created_at_ms, updated_at_ms)
                 VALUES ('job_pending', '{retired}', 'semantic_rebuild', 20, 'pending', '{{}}',
                         'pending-key', 5, 0, 0, 0),
                        ('job_done', '{retired}', 'vault_export', 5, 'succeeded', '{{}}',
                         'done-key', 5, 0, 0, 0);
                 INSERT INTO product_records(id, kind, version, document_json, stamps_json,
                     archived, updated_at_ms)
                 VALUES ('cat_section', 'section', 1,
                         '{{\"type\":\"section\",\"value\":{{\"entity_id\":\"repo:{retired}\",\
                          \"anchors\":[{{\"project_id\":\"{retired}\",\"node_id\":\"{shared}\"}}]}}}}',
                         '[{{\"anchor\":{{\"project_id\":\"{retired}\",\"node_id\":\"{shared}\"}}}}]',
                         0, 0);",
                evidence = EvidenceId::new(),
                shared = shared_node,
            ))
            .expect("domain rows");
        Self {
            _temp: temp,
            database,
            survivor,
            retired,
            other,
            client,
            view,
            shared_node,
            survivor_node,
            retired_only_node,
            anchored,
            pending,
            duplicate_survivor,
            duplicate_retired,
            retired_task,
        }
    }

    fn execution(&self) -> MergeExecution {
        MergeExecution {
            survivor: self.survivor.clone(),
            retired: self.retired.clone(),
            identity: evidence(),
            backup_path: Some("/backups/pre-merge.sqlite".to_owned()),
        }
    }

    fn merge(&self) -> MergeOutcome {
        execute(&self.database, &self.execution()).expect("merge")
    }

    fn connection(&self) -> Connection {
        self.database.connect().expect("connection")
    }

    fn scalar<T: rusqlite::types::FromSql>(&self, sql: &str) -> T {
        self.connection()
            .query_row(sql, [], |row| row.get(0))
            .expect("scalar")
    }

    fn plan(&self) -> MergePlan {
        let connection = self.connection();
        let survivor = party(&connection, &self.survivor).expect("survivor");
        let retired = party(&connection, &self.retired).expect("retired");
        plan(&connection, &survivor, &retired, evidence()).expect("plan")
    }
}

fn evidence() -> IdentityEvidence {
    IdentityEvidence {
        same_repository: true,
        canonical_remote: Some(REMOTE.to_owned()),
        root_commit: Some(ROOT.to_owned()),
        checkouts: Vec::new(),
        refusal: None,
    }
}

/// Every row of every table, for "nothing changed" comparisons.
fn table_dump(connection: &Connection) -> BTreeMap<String, Vec<String>> {
    let tables = {
        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table'
                 AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '%_fts_%' ORDER BY name",
            )
            .expect("tables");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("names")
    };
    tables
        .into_iter()
        .map(|table| {
            let mut statement = connection
                .prepare(&format!("SELECT * FROM {}", quote(&table)))
                .expect("select");
            let width = statement.column_count();
            let mut rows = statement
                .query_map([], |row| {
                    let mut values = Vec::with_capacity(width);
                    for index in 0..width {
                        values.push(value_text(row.get_ref(index)?));
                    }
                    Ok(values.join("|"))
                })
                .expect("rows")
                .collect::<std::result::Result<Vec<_>, _>>()
                .expect("collect");
            rows.sort();
            (table, rows)
        })
        .collect()
}

#[test]
fn every_project_keyed_table_has_a_merge_role() {
    let temp = TempDir::new().expect("temp dir");
    let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
    let connection = database.connect().expect("connection");
    let keyed = project_keyed_tables(&connection)
        .expect("introspect")
        .into_iter()
        .collect::<BTreeSet<_>>();
    let classified = TABLE_ROLES
        .iter()
        .map(|(table, _)| (*table).to_owned())
        .collect::<BTreeSet<_>>();
    // A migration that adds a project-keyed table must give it a role here;
    // a role for a table that no longer exists is stale.
    assert_eq!(
        keyed.difference(&classified).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "project-keyed tables without a merge role"
    );
    assert_eq!(
        classified.difference(&keyed).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "merge roles for tables that are not project-keyed"
    );
    verify_roles(&connection).expect("every table classified");
    for (table, role) in TABLE_ROLES {
        let TableRole::Moved(rule) = role else {
            continue;
        };
        let keys = project_unique_keys(&connection, table).expect("unique keys");
        match rule {
            CollisionRule::Impossible => {
                assert!(keys.is_empty(), "{table} can collide on {keys:?}");
            }
            CollisionRule::RekeyRetired(column) => {
                assert!(
                    keys.iter().all(|key| key.iter().any(|name| name == column)),
                    "{table}: every colliding key must contain {column}: {keys:?}"
                );
            }
            CollisionRule::KeepSurvivor | CollisionRule::UnionPaths => {
                assert!(!keys.is_empty(), "{table} cannot collide; use Impossible");
            }
        }
        if *rule == CollisionRule::UnionPaths {
            assert!(
                table_columns(&connection, table)
                    .expect("columns")
                    .contains(&"paths_json".to_owned())
            );
        }
    }
}

#[test]
fn an_unclassified_project_keyed_table_blocks_the_merge() {
    let fixture = Fixture::new();
    let connection = fixture.connection();
    connection
        .execute_batch(
            "CREATE TABLE future_rows (id TEXT PRIMARY KEY, project_id TEXT NOT NULL);
             CREATE TABLE future_children (id TEXT PRIMARY KEY,
                 task_id TEXT REFERENCES tasks(id));",
        )
        .expect("future migration");
    let keyed = project_keyed_tables(&connection).expect("introspect");
    assert!(keyed.contains(&"future_rows".to_owned()));
    assert!(keyed.contains(&"future_children".to_owned()));
    let before = table_dump(&connection);
    let error = execute(&fixture.database, &fixture.execution()).expect_err("refused");
    let message = error.to_string();
    assert!(
        message.contains("future_children, future_rows"),
        "{message}"
    );
    assert_eq!(table_dump(&connection), before, "nothing may change");
}

#[test]
#[allow(clippy::too_many_lines)]
fn merge_moves_rebuilds_and_records_every_collision() {
    let fixture = Fixture::new();
    let (s, r) = (fixture.survivor.as_str(), fixture.retired.as_str());
    let outcome = fixture.merge();
    let audit = &outcome.audit;

    // Moved rows: nothing project-keyed is left under the retired id except
    // history.
    for table in TABLE_ROLES.iter().filter_map(|(table, role)| {
        matches!(role, TableRole::Moved(_) | TableRole::Rebuilt).then_some(*table)
    }) {
        let left: i64 = fixture.scalar(&format!(
            "SELECT COUNT(*) FROM {} WHERE project_id = '{r}'",
            quote(table)
        ));
        assert_eq!(left, 0, "{table} keeps rows of the retired project");
    }
    assert_eq!(audit.moved["tasks"], 1);
    assert_eq!(audit.moved["memories"], 3);
    assert_eq!(audit.moved["context_runs"], 2);
    assert_eq!(audit.moved["resource_usage"], 1);
    assert_eq!(audit.moved["tool_observations"], 1);
    assert_eq!(audit.moved.get("verification_observations"), Some(&0));
    // Both jobs stay as history; the unfinished one is cancelled first.
    assert_eq!(audit.kept["jobs"], 2);
    assert_eq!(audit.cancelled_jobs, ["job_pending"]);
    assert!(audit.kept["events"] > 0);
    assert_eq!(audit.rebuilt["nodes"], 2);
    assert_eq!(audit.rebuilt["edges"], 1);
    assert_eq!(audit.rebuilt["indexed_files"], 1);
    assert_eq!(audit.rebuilt["structural_scans"], 1);
    assert_eq!(audit.rebuilt["commits"], 1);
    let task_project: String = fixture.scalar(&format!(
        "SELECT project_id FROM tasks WHERE id = '{}'",
        fixture.retired_task
    ));
    assert_eq!(task_project, s);

    // Collisions, each by its explicit rule.
    let collisions = audit
        .collisions
        .iter()
        .map(|collision| (collision.table.as_str(), collision.key.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        collisions,
        [
            ("context_runs", "request_id=req-1"),
            ("resource_usage", "source=claude_code, sample_id=s1"),
            ("tool_observations", "call_id=tool:c1"),
            ("verification_observations", "call_id=tool:v1"),
        ]
    );
    let request_ids: Vec<String> = {
        let connection = fixture.connection();
        let mut statement = connection
            .prepare("SELECT request_id FROM context_runs ORDER BY request_id")
            .expect("runs");
        statement
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<std::result::Result<_, _>>()
            .expect("collect")
    };
    assert_eq!(request_ids, ["req-1", "req-1~merged-bbbbbbbb", "req-2"]);
    let usage: String = fixture
        .scalar("SELECT group_concat(id, ',') FROM (SELECT id FROM resource_usage ORDER BY id)");
    assert_eq!(
        usage, "use_r2,use_s1",
        "the retired duplicate sample is dropped"
    );
    let moved_request: String =
        fixture.scalar("SELECT request_json FROM resource_usage WHERE id = 'use_r2'");
    assert_eq!(moved_request, format!("{{\"project_id\":\"{s}\"}}"));
    let paths: String =
        fixture.scalar("SELECT paths_json FROM tool_observations WHERE id = 'obs_s1'");
    assert_eq!(paths, "[\"a.rs\",\"b.rs\"]");
    let verification: String =
        fixture.scalar("SELECT group_concat(id) FROM verification_observations");
    assert_eq!(verification, "ver_s1");

    // Facts: the survivor wins, internal facts never move, the retired
    // values are in the audit.
    let facts: String = fixture.scalar(&format!(
        "SELECT group_concat(key || '=' || value, ';') FROM
         (SELECT key, value FROM project_facts WHERE project_id = '{s}' ORDER BY key)"
    ));
    assert_eq!(
        facts,
        format!(
            "hosting.base_url=a.example;internal.hidden=true;internal.origin_view={};\
             storage.db=sqlite",
            fixture.view
        )
    );
    let retired_facts: i64 = fixture.scalar(&format!(
        "SELECT COUNT(*) FROM project_facts WHERE project_id = '{r}'"
    ));
    assert_eq!(retired_facts, 0);
    let base_url = audit
        .facts
        .iter()
        .find(|fact| fact.key == "hosting.base_url")
        .expect("fact decision");
    assert_eq!(base_url.retired_value, "b.example");
    assert_eq!(base_url.survivor_value.as_deref(), Some("a.example"));

    // Links: union, dedupe, nothing points at the retired project.
    let links: String = fixture.scalar(
        "SELECT group_concat(project_id || '>' || linked_project_id || ':' || relation, ';')
         FROM (SELECT * FROM project_links ORDER BY project_id, linked_project_id)",
    );
    let (o, c) = (fixture.other.as_str(), fixture.client.as_str());
    assert_eq!(
        links,
        format!("{s}>{o}:uses;{s}>{c}:calls-api-of;{o}>{s}:uses;{c}>{s}:calls-api-of")
    );
    assert_eq!(audit.links.len(), 5, "{:?}", audit.links);

    // The retired project's origin view is paired with the survivor, which
    // had none.
    assert_eq!(outcome.repaired_view.as_ref(), Some(&fixture.view));
    let view_of: String = fixture.scalar(&format!(
        "SELECT value FROM project_facts WHERE project_id = '{}' AND key = 'internal.view_of'",
        fixture.view
    ));
    assert_eq!(view_of, s);

    // Anchors: one re-attached by stable identity, one waits for re-index.
    let anchor: String = fixture.scalar(&format!(
        "SELECT node_id FROM memory_nodes WHERE memory_id = '{}'",
        fixture.anchored
    ));
    assert_eq!(anchor, fixture.survivor_node.as_str());
    assert_eq!((audit.anchors_remapped, audit.anchors_pending), (1, 1));
    let waiting: String = fixture.scalar(
        "SELECT memory_id || '|' || stable_key || '|' || project_id FROM project_merge_anchors",
    );
    assert_eq!(
        waiting,
        format!("{}|symbol:src/old.rs:Function:legacy|{s}", fixture.pending)
    );
    let _ = (&fixture.shared_node, &fixture.retired_only_node);

    // Memories keep both identical claims; derived rows follow.
    assert_eq!(
        audit.duplicate_claims,
        [DuplicateClaim {
            survivor_memory_id: fixture.duplicate_survivor.clone(),
            retired_memory_id: fixture.duplicate_retired.clone(),
        }]
    );
    let scopes: i64 = fixture.scalar(&format!(
        "SELECT COUNT(*) FROM memories WHERE project_id = '{s}' AND scope_id = '{s}'"
    ));
    assert_eq!(scopes, 4);
    let text: i64 = fixture.scalar(&format!(
        "SELECT COUNT(*) FROM memories_fts WHERE project_id = '{s}'"
    ));
    assert_eq!(text, 4);
    let vectors: i64 = fixture.scalar(&format!(
        "SELECT COUNT(*) FROM memory_vectors WHERE project_id = '{s}'"
    ));
    assert_eq!(vectors, 4);

    // Jobs: unfinished ones are cancelled, finished ones stay as history.
    let jobs: String = fixture.scalar(
        "SELECT group_concat(id || ':' || status || ':' || project_id, ';')
         FROM (SELECT * FROM jobs ORDER BY id)",
    );
    assert_eq!(jobs, format!("job_done:succeeded:{r};job_pending:dead:{r}"));

    // Product knowledge is re-anchored in a new revision.
    let (version, document): (i64, String) = fixture
        .connection()
        .query_row(
            "SELECT version, document_json FROM product_records WHERE id = 'cat_section'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("record");
    assert_eq!(version, 2);
    assert!(!document.contains(r), "{document}");
    assert!(document.contains(&format!("repo:{s}")), "{document}");
    assert!(
        document.contains(fixture.survivor_node.as_str()),
        "{document}"
    );
    let revisions: i64 =
        fixture.scalar("SELECT COUNT(*) FROM product_revisions WHERE record_id = 'cat_section'");
    assert_eq!(revisions, 1);

    // The tombstone names the survivor, which records the live identity.
    let tombstone: (bool, String) = fixture
        .connection()
        .query_row(
            &format!(
                "SELECT tombstoned_at_ms IS NOT NULL, merged_into FROM projects WHERE id = '{r}'"
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("tombstone");
    assert_eq!(tombstone, (true, s.to_owned()));
    let identity: (String, String) = fixture
        .connection()
        .query_row(
            &format!("SELECT canonical_remote, root_commit FROM projects WHERE id = '{s}'"),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("identity");
    assert_eq!(identity, (REMOTE.to_owned(), ROOT.to_owned()));
    let aliases: String = fixture.scalar(&format!(
        "SELECT group_concat(path) FROM (SELECT path FROM project_aliases
         WHERE project_id = '{s}' ORDER BY path)"
    ));
    assert_eq!(aliases, "/work/app,/work/app-copy,/work/app-worktree");
    assert_eq!(outcome.checkouts_to_repin, ["/work/app-copy"]);
    // The fixture's checkout paths do not exist, so nothing is re-indexed.
    assert_eq!(outcome.reindex_path, None);
    assert_eq!(outcome.reindexed, None);

    // The audit event is the record of all of it.
    let payload: String = fixture.scalar(&format!(
        "SELECT payload_json FROM events WHERE id = '{}'",
        outcome.merge_event_id
    ));
    let DomainEvent::ProjectsMerged {
        audit: recorded, ..
    } = serde_json::from_str(&payload).expect("event")
    else {
        panic!("not a merge event");
    };
    assert_eq!(&*recorded, audit);
    assert_eq!(
        recorded.backup_path.as_deref(),
        Some("/backups/pre-merge.sqlite")
    );
    let violations: i64 = fixture.scalar("SELECT COUNT(*) FROM pragma_foreign_key_check");
    assert_eq!(violations, 0);
}

#[test]
fn the_plan_is_deterministic_and_matches_the_merge() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    assert_eq!(plan, fixture.plan());
    let text = render::plan_text(&plan, &[]);
    assert_eq!(text, render::plan_text(&fixture.plan(), &[]));
    assert!(
        text.contains("context_runs request_id=req-1: keep both"),
        "{text}"
    );
    let planned = plan
        .tables
        .iter()
        .map(|table| (table.table.clone(), table.rows))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        plan.collisions.len(),
        4,
        "every collision is listed: {:?}",
        plan.collisions
    );
    assert_eq!(plan.anchors.total, 2);
    assert_eq!(plan.anchors.remap_now, 1);
    assert_eq!(plan.views.repaired.as_ref(), Some(&fixture.view));
    assert_eq!(plan.cancelled_jobs, ["job_pending"]);
    assert_eq!(plan.product_records, ["cat_section"]);
    let outcome = fixture.merge();
    for (table, rows) in &outcome.audit.rebuilt {
        if table.ends_with("_fts") || table == "memory_vectors" {
            continue;
        }
        assert_eq!(planned[table], *rows, "{table}");
    }
    // Moved counts exclude the dropped duplicates the plan lists as collisions.
    assert_eq!(planned["tasks"], outcome.audit.moved["tasks"]);
    assert_eq!(
        planned["resource_usage"],
        outcome.audit.moved["resource_usage"] + 1
    );
}

#[test]
fn a_failure_inside_the_merge_changes_nothing() {
    let fixture = Fixture::new();
    let connection = fixture.connection();
    // Stand-in for a crash halfway: the transaction fails after facts,
    // anchors and several tables were already rewritten in it.
    connection
        .execute_batch(
            "CREATE TRIGGER injected_failure BEFORE DELETE ON commits
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        )
        .expect("trigger");
    let before = table_dump(&connection);
    let error = execute(&fixture.database, &fixture.execution()).expect_err("failure");
    assert!(error.to_string().contains("injected failure"), "{error}");
    assert_eq!(table_dump(&connection), before);
}

#[test]
fn a_repeated_merge_reports_the_earlier_one_and_changes_nothing() {
    let fixture = Fixture::new();
    let first = fixture.merge();
    let connection = fixture.connection();
    let before = table_dump(&connection);
    let again = fixture.merge();
    assert_eq!(again.merge_event_id, first.merge_event_id);
    assert_eq!(again.audit, first.audit);
    assert_eq!(table_dump(&connection), before);
    let refused = check_parties(&connection, &fixture.survivor, &fixture.retired)
        .expect_err("the retired project is no longer active");
    assert!(refused.to_string().contains("already merged"), "{refused}");
}

#[test]
fn replaying_the_log_reproduces_the_merged_projection() {
    let fixture = Fixture::new();
    fixture.merge();
    let connection = fixture.connection();
    let replayed = EventStore::new(fixture.database.clone())
        .replay_project_projection()
        .expect("replay");
    assert_eq!(replayed, materialized_projection(&connection));
    let retired = &replayed.projects[&fixture.retired];
    assert!(retired.tombstoned);
    assert_eq!(retired.merged_into.as_ref(), Some(&fixture.survivor));
    assert!(retired.aliases.is_empty());

    // Re-applying every event to an empty database rebuilds the same project
    // records, links and facts.
    let temp = TempDir::new().expect("temp dir");
    let fresh = Database::open(temp.path().join("replayed.sqlite")).expect("fresh database");
    let fresh_events = EventStore::new(fresh.clone());
    for envelope in EventStore::new(fixture.database.clone())
        .replay()
        .expect("events")
    {
        fresh_events
            .append_with_id(&envelope.id, envelope.schema_version, &envelope.event)
            .expect("re-append");
    }
    let fresh_connection = fresh.connect().expect("connection");
    assert_eq!(
        projection_rows(&fresh_connection),
        projection_rows(&connection)
    );
}

fn materialized_projection(connection: &Connection) -> ProjectProjection {
    let mut projection = ProjectProjection::default();
    let mut statement = connection
        .prepare(
            "SELECT id, name, canonical_remote, root_commit, parent_project_id,
                    tombstoned_at_ms IS NOT NULL, merged_into FROM projects",
        )
        .expect("projects");
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, bool>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })
        .expect("query")
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("rows");
    for (raw, name, canonical_remote, root_commit, parent, tombstoned, merged_into) in rows {
        let project = id(&raw);
        let mut aliases = alias_paths(connection, &project).expect("aliases");
        aliases.sort();
        projection.projects.insert(
            project,
            crate::events::ProjectProjectionEntry {
                name,
                canonical_remote,
                root_commit,
                parent_project_id: parent.as_deref().map(id),
                aliases,
                tombstoned,
                merged_into: merged_into.as_deref().map(id),
            },
        );
    }
    projection
}

fn projection_rows(connection: &Connection) -> Vec<String> {
    let mut rows = Vec::new();
    for sql in [
        "SELECT 'project', id, name, coalesce(canonical_remote, ''), coalesce(root_commit, ''),
                coalesce(parent_project_id, ''), tombstoned_at_ms IS NOT NULL,
                coalesce(merged_into, '') FROM projects",
        "SELECT 'alias', project_id, path, '', '', '', '', '' FROM project_aliases",
        "SELECT 'link', project_id, linked_project_id, relation, coalesce(note, ''), '', '', ''
         FROM project_links",
        "SELECT 'fact', project_id, key, value, coalesce(note, ''), '', '', '' FROM project_facts",
    ] {
        let mut statement = connection.prepare(sql).expect("projection query");
        rows.extend(
            statement
                .query_map([], |row| {
                    let mut values = Vec::new();
                    for index in 0..8 {
                        values.push(value_text(row.get_ref(index)?));
                    }
                    Ok(values.join("|"))
                })
                .expect("rows")
                .collect::<std::result::Result<Vec<_>, _>>()
                .expect("collect"),
        );
    }
    rows.sort();
    rows
}

#[test]
fn finalize_attaches_waiting_anchors_or_marks_their_memories_for_revalidation() {
    let fixture = Fixture::new();
    fixture.merge();
    let s = fixture.survivor.as_str();
    // Without anchor resolution (the re-index failed) the anchor keeps waiting.
    let waiting = finalize(&fixture.database, &fixture.survivor, &sweep()).expect("sweep");
    assert_eq!(waiting.pending_anchors, 1);
    assert!(waiting.revalidate.is_empty());

    // The survivor's re-index indexed the symbol: the anchor is re-attached.
    let connection = fixture.connection();
    let reindexed = node(
        &connection,
        &fixture.survivor,
        "symbol:src/old.rs:Function:legacy",
        "d2",
    );
    let attached = finalize(&fixture.database, &fixture.survivor, &resolve()).expect("finalize");
    assert_eq!(attached.anchors_remapped, 1);
    assert_eq!(attached.pending_anchors, 0);
    let anchor: String = fixture.scalar(&format!(
        "SELECT node_id FROM memory_nodes WHERE memory_id = '{}'",
        fixture.pending
    ));
    assert_eq!(anchor, reindexed.as_str());

    // An anchor whose symbol the survivor does not index marks its memory
    // for revalidation (suspect, with the reason), never promotes it.
    connection
        .execute(
            "INSERT INTO project_merge_anchors(project_id, memory_id, stable_key, kind, label,
                 file_path, observed_digest, merge_event_id, recorded_at_ms)
             VALUES (?1, ?2, 'symbol:src/gone.rs:Function:gone', 'Function', 'gone',
                     'src/gone.rs', 'd3', 'evt_fixture', 0)",
            params![s, fixture.anchored.as_str()],
        )
        .expect("waiting anchor");
    let marked = finalize(&fixture.database, &fixture.survivor, &resolve()).expect("finalize");
    assert_eq!(marked.revalidate, std::slice::from_ref(&fixture.anchored));
    let (status, epistemic): (String, String) = connection
        .query_row(
            "SELECT status, epistemic_state FROM memories WHERE id = ?1",
            [fixture.anchored.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("memory");
    assert_eq!(
        (status.as_str(), epistemic.as_str()),
        ("suspect", "candidate")
    );
    let reason: String = fixture.scalar(&format!(
        "SELECT json_extract(payload_json, '$.data.reason') FROM events
         WHERE event_type = 'MEMORY_STATE_CHANGED'
           AND json_extract(payload_json, '$.data.memory_id') = '{}'
         ORDER BY sequence DESC LIMIT 1",
        fixture.anchored
    ));
    assert_eq!(reason, ANCHOR_UNRESOLVED_REASON);
}

#[test]
fn rows_written_under_a_retired_id_after_the_merge_are_swept() {
    let fixture = Fixture::new();
    fixture.merge();
    let straggler = TaskId::new();
    fixture
        .connection()
        .execute(
            "INSERT INTO tasks(id, project_id, prompt, prompt_digest, status, started_at_ms)
             VALUES (?1, ?2, 'p', 'd', 'open', 0)",
            params![straggler.as_str(), fixture.retired.as_str()],
        )
        .expect("stale write");
    let outcome = finalize(&fixture.database, &fixture.survivor, &sweep()).expect("finalize");
    assert_eq!(outcome.stragglers.get("tasks"), Some(&1));
    let project: String = fixture.scalar(&format!(
        "SELECT project_id FROM tasks WHERE id = '{straggler}'"
    ));
    assert_eq!(project, fixture.survivor.as_str());

    // The sweep is audited, and a later finalize reports it as well.
    let (trigger, swept): (String, i64) = fixture
        .connection()
        .query_row(
            "SELECT json_extract(payload_json, '$.data.trigger'),
                    json_extract(payload_json, '$.data.stragglers.tasks')
             FROM events WHERE event_type = 'PROJECT_MERGE_FINALIZED'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("finalize event");
    assert_eq!((trigger.as_str(), swept), ("after_merge", 1));
    let later = finalize(&fixture.database, &fixture.survivor, &resolve()).expect("finalize");
    assert!(later.stragglers.is_empty());
    assert_eq!(later.stragglers_since_merge.get("tasks"), Some(&1));
    let events: i64 =
        fixture.scalar("SELECT COUNT(*) FROM events WHERE event_type = 'PROJECT_MERGE_FINALIZED'");
    assert_eq!(
        events, 2,
        "the anchor resolution is a change worth recording"
    );
}

fn sweep() -> FinalizeOptions {
    FinalizeOptions {
        trigger: "after_merge".to_owned(),
        ..FinalizeOptions::default()
    }
}

fn resolve() -> FinalizeOptions {
    FinalizeOptions {
        resolve_anchors: true,
        reindexed: None,
        trigger: "client".to_owned(),
    }
}

#[test]
fn anchors_on_tombstoned_symbols_are_unresolved() {
    let fixture = Fixture::new();
    // The survivor's copy of the anchored symbol was removed from its tree.
    fixture
        .connection()
        .execute(
            "UPDATE nodes SET tombstoned_at_ms = 1 WHERE id = ?1",
            [fixture.survivor_node.as_str()],
        )
        .expect("tombstone");
    let outcome = fixture.merge();
    assert_eq!(
        (
            outcome.audit.anchors_remapped,
            outcome.audit.anchors_pending
        ),
        (0, 2),
        "nothing is re-attached to a tombstoned symbol"
    );
    let marked = finalize(&fixture.database, &fixture.survivor, &resolve()).expect("finalize");
    assert_eq!(marked.anchors_remapped, 0);
    let mut revalidate = marked.revalidate.clone();
    revalidate.sort();
    let mut expected = vec![fixture.anchored.clone(), fixture.pending.clone()];
    expected.sort();
    assert_eq!(revalidate, expected);
    let anchors: i64 = fixture.scalar(&format!(
        "SELECT COUNT(*) FROM memory_nodes WHERE node_id = '{}'",
        fixture.survivor_node
    ));
    assert_eq!(anchors, 0);
}

#[test]
fn a_completed_follow_up_is_recorded_for_a_repeated_merge() {
    let fixture = Fixture::new();
    fixture.merge();
    let unresolved = finalize(
        &fixture.database,
        &fixture.survivor,
        &FinalizeOptions {
            resolve_anchors: false,
            reindexed: Some("/work/app".to_owned()),
            trigger: "client".to_owned(),
        },
    )
    .expect("finalize");
    assert_eq!(unresolved.pending_anchors, 1);
    // An anchor still waits, so a repeated merge must re-index again.
    assert_eq!(fixture.merge().reindexed, None);
    finalize(
        &fixture.database,
        &fixture.survivor,
        &FinalizeOptions {
            resolve_anchors: true,
            reindexed: Some("/work/app".to_owned()),
            trigger: "client".to_owned(),
        },
    )
    .expect("finalize");
    assert_eq!(fixture.merge().reindexed.as_deref(), Some("/work/app"));
}

#[test]
fn parties_must_be_distinct_active_projects_that_are_not_views() {
    let fixture = Fixture::new();
    let connection = fixture.connection();
    let refusal = |survivor: &ProjectId, retired: &ProjectId| {
        check_parties(&connection, survivor, retired)
            .expect_err("refused")
            .to_string()
    };
    assert!(refusal(&fixture.survivor, &fixture.survivor).contains("same project"));
    assert!(refusal(&fixture.survivor, &fixture.view).contains("origin view"));
    let unknown = id("prj_ffffffff-0000-4000-8000-000000000009");
    assert!(refusal(&fixture.survivor, &unknown).contains("unknown project"));
    append(
        &fixture.database,
        &DomainEvent::ProjectTombstoned {
            project_id: fixture.other.clone(),
        },
    );
    assert!(refusal(&fixture.other, &fixture.survivor).contains("retired"));
    check_parties(&connection, &fixture.survivor, &fixture.retired).expect("valid parties");
}

#[test]
fn a_survivor_with_its_own_origin_view_retires_the_other() {
    let fixture = Fixture::new();
    let own_view = id("prj_99999999-0000-4000-8000-000000000010");
    register(&fixture.database, &own_view, "app@origin");
    fact(
        &fixture.database,
        &own_view,
        "internal.view_of",
        fixture.survivor.as_str(),
    );
    fact(
        &fixture.database,
        &fixture.survivor,
        "internal.origin_view",
        own_view.as_str(),
    );
    let outcome = fixture.merge();
    assert_eq!(outcome.repaired_view, None);
    assert_eq!(outcome.retired_views, std::slice::from_ref(&fixture.view));
    let tombstoned: bool = fixture.scalar(&format!(
        "SELECT tombstoned_at_ms IS NOT NULL FROM projects WHERE id = '{}'",
        fixture.view
    ));
    assert!(tombstoned);
    let survivor_view: String = fixture.scalar(&format!(
        "SELECT value FROM project_facts WHERE project_id = '{}' AND key = 'internal.origin_view'",
        fixture.survivor
    ));
    assert_eq!(survivor_view, own_view.as_str());
}

const RETIRED: &str = "prj_bbbbbbbb-0000-4000-8000-000000000002";
const SURVIVOR: &str = "prj_aaaaaaaa-0000-4000-8000-000000000001";

fn redirected(body: &serde_json::Value) -> serde_json::Value {
    let redirects = BTreeMap::from([(RETIRED.to_owned(), SURVIVOR.to_owned())]);
    let bytes = serde_json::to_vec(body).expect("body");
    redirect_request_body(&bytes, &redirects).map_or_else(
        || body.clone(),
        |rewritten| serde_json::from_slice(&rewritten).expect("json"),
    )
}

#[test]
fn only_typed_project_references_are_redirected() {
    let quoted = format!("see \"{RETIRED}\"");
    // Typed references move to the survivor.
    let typed = redirected(&serde_json::json!({
        "project_id": RETIRED,
        "linked_project_id": RETIRED,
        "project_ids": [RETIRED, "prj_other"],
        "memory": {"scope_type": "project", "scope_id": RETIRED, "claim": RETIRED},
        "query": {"entity_id": format!("repo:{RETIRED}")},
        "item": {"value": {"parent_id": format!("repo:{RETIRED}"),
                           "anchors": [{"project_id": RETIRED, "path": RETIRED}]}},
        "projects": [{"project_id": RETIRED,
                      "links": [{"target_project_id": RETIRED, "note": RETIRED}]}],
    }));
    assert_eq!(
        typed,
        serde_json::json!({
            "project_id": SURVIVOR,
            "linked_project_id": SURVIVOR,
            "project_ids": [SURVIVOR, "prj_other"],
            "memory": {"scope_type": "project", "scope_id": SURVIVOR, "claim": RETIRED},
            "query": {"entity_id": format!("repo:{SURVIVOR}")},
            "item": {"value": {"parent_id": format!("repo:{SURVIVOR}"),
                               "anchors": [{"project_id": SURVIVOR, "path": RETIRED}]}},
            "projects": [{"project_id": SURVIVOR,
                          "links": [{"target_project_id": SURVIVOR, "note": RETIRED}]}],
        })
    );
    // The reviewer's cases: text that is, or quotes, a retired id keeps its
    // bytes; only the typed project_id moves.
    for text_value in [RETIRED.to_owned(), quoted.clone()] {
        let evidence = serde_json::json!({
            "project_id": RETIRED,
            "kind": "test_result",
            "command": text_value,
            "artifact_text": text_value,
            "metadata": {"source": text_value, "nested": [text_value]},
        });
        let rewritten = redirected(&evidence);
        assert_eq!(rewritten["project_id"], SURVIVOR);
        for field in ["command", "artifact_text"] {
            assert_eq!(rewritten[field], text_value.as_str(), "{field}");
        }
        assert_eq!(rewritten["metadata"]["source"], text_value.as_str());
        assert_eq!(rewritten["metadata"]["nested"][0], text_value.as_str());
        let task = redirected(&serde_json::json!({"project_id": RETIRED, "prompt": text_value}));
        assert_eq!(task["prompt"], text_value.as_str());
        let fact = redirected(&serde_json::json!({
            "project_id": RETIRED, "key": "hosting.base_url",
            "value": text_value, "note": text_value,
        }));
        assert_eq!(fact["project_id"], SURVIVOR);
        assert_eq!(
            (&fact["value"], &fact["note"]),
            (
                &serde_json::json!(text_value),
                &serde_json::json!(text_value)
            )
        );
    }
    // A scope id of another scope type is not a project reference.
    let file_scope = redirected(&serde_json::json!({
        "memory": {"scope_type": "file", "scope_id": RETIRED},
    }));
    assert_eq!(file_scope["memory"]["scope_id"], RETIRED);
    // Bodies without a retired id are not parsed at all.
    let redirects = BTreeMap::from([(RETIRED.to_owned(), SURVIVOR.to_owned())]);
    assert!(redirect_request_body(br#"{"project_id":"prj_other"}"#, &redirects).is_none());
    assert!(redirect_request_body(b"not json", &redirects).is_none());
    let text_only = serde_json::to_vec(&serde_json::json!({"prompt": RETIRED})).expect("body");
    assert!(redirect_request_body(&text_only, &redirects).is_none());
}

#[test]
fn product_text_that_names_the_retired_id_is_left_alone() {
    let fixture = Fixture::new();
    let r = fixture.retired.as_str();
    fixture
        .connection()
        .execute(
            "INSERT INTO product_records(id, kind, version, document_json, stamps_json,
                 archived, updated_at_ms)
             VALUES ('cat_text', 'section', 1, ?1, '[]', 0, 0)",
            [format!(
                "{{\"type\":\"section\",\"value\":{{\"entity_id\":\"cat_entity\",\
                 \"curated\":\"{r}\",\"title\":\"see \\\"{r}\\\"\"}}}}"
            )],
        )
        .expect("record");
    let plan = fixture.plan();
    assert_eq!(
        plan.product_records,
        ["cat_section"],
        "text-only mentions are not anchors"
    );
    fixture.merge();
    let (version, document): (i64, String) = fixture
        .connection()
        .query_row(
            "SELECT version, document_json FROM product_records WHERE id = 'cat_text'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("record");
    assert_eq!(version, 1);
    assert!(
        document.contains(&format!("\"curated\":\"{r}\"")),
        "{document}"
    );
}

#[test]
fn observations_and_usage_queued_before_a_merge_land_with_the_survivor() {
    use crate::effectiveness::{
        ToolObservationRequest, ToolPhase, UsageRecordRequest, VerificationObservationRequest,
    };
    let fixture = Fixture::new();
    // A usage sample recorded before the merge, and resent with the retired
    // id afterwards (a client retrying), stays one sample.
    let store = crate::effectiveness::EffectivenessStore::new(fixture.database.clone());
    let sample: UsageRecordRequest = serde_json::from_value(serde_json::json!({
        "project_id": fixture.retired.as_str(),
        "sample_id": "sample-queued",
        "source": "claude_code",
        "metrics": {"input_tokens": 10, "output_tokens": 5},
    }))
    .expect("usage request");
    let first = store.record(&sample).expect("usage before the merge");
    assert!(first.inserted);
    fixture.merge();
    let resent = store.record(&sample).expect("usage resent after the merge");
    assert!(!resent.inserted, "the moved sample matches byte for byte");
    assert_eq!(resent.id, first.id);
    let verification = store
        .record_verification_observation(&VerificationObservationRequest {
            project_id: fixture.retired.clone(),
            session_id: "queued-session".to_owned(),
            tool_use_id: "call-2".to_owned(),
            conversation_id: "queued-session".to_owned(),
            task_id: Some(fixture.retired_task.clone()),
            source: "claude_code".to_owned(),
            kind: "test_result".to_owned(),
            hidden: None,
            note_delivered: false,
        })
        .expect("queued verification observation");
    assert!(verification);
    let verification_project: String = fixture.scalar(
        "SELECT project_id FROM verification_observations WHERE session_id = 'queued-session'",
    );
    assert_eq!(verification_project, fixture.survivor.as_str());
    let observed = store
        .record_tool_observation(&ToolObservationRequest {
            project_id: fixture.retired.clone(),
            session_id: "queued-session".to_owned(),
            tool_use_id: "call-1".to_owned(),
            task_id: Some(fixture.retired_task.clone()),
            context_run_id: None,
            source: "claude_code".to_owned(),
            tool_name: "Read".to_owned(),
            phase: ToolPhase::Post,
            paths: vec!["src/lib.rs".to_owned()],
            ranged: None,
            result_bytes: None,
            limit_hit: None,
        })
        .expect("queued tool observation");
    let project: String = fixture.scalar(&format!(
        "SELECT project_id FROM tool_observations WHERE id = '{}'",
        observed.id
    ));
    assert_eq!(project, fixture.survivor.as_str());
}

#[test]
fn redirects_follow_chains_of_merges() {
    let fixture = Fixture::new();
    fixture.merge();
    let next = id("prj_12345678-0000-4000-8000-000000000011");
    register(&fixture.database, &next, "app-final");
    execute(
        &fixture.database,
        &MergeExecution {
            survivor: next.clone(),
            retired: fixture.survivor.clone(),
            identity: evidence(),
            backup_path: None,
        },
    )
    .expect("second merge");
    let redirects = redirects(&fixture.connection()).expect("redirects");
    assert_eq!(redirects[fixture.retired.as_str()], next.as_str());
    assert_eq!(redirects[fixture.survivor.as_str()], next.as_str());
    let pending: String = fixture.scalar("SELECT project_id FROM project_merge_anchors");
    assert_eq!(
        pending,
        next.as_str(),
        "waiting anchors follow their project"
    );
}

fn git(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repository(path: &Path, remote: &str, content: &str) {
    std::fs::create_dir_all(path).expect("repository");
    git(path, &["init", "-q"]);
    git(path, &["config", "user.email", "engram@example.invalid"]);
    git(path, &["config", "user.name", "Engram Tests"]);
    std::fs::write(path.join("README.md"), content).expect("file");
    git(path, &["add", "README.md"]);
    git(
        path,
        &["-c", "commit.gpgsign=false", "commit", "-qm", "initial"],
    );
    git(path, &["remote", "add", "origin", remote]);
}

fn checkout_party(project: &str, name: &str, paths: &[&Path]) -> MergeParty {
    MergeParty {
        id: id(project),
        name: name.to_owned(),
        hidden: false,
        canonical_remote: None,
        root_commit: None,
        checkouts: paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
    }
}

#[test]
fn identity_evidence_needs_one_repository_seen_by_both() {
    let temp = TempDir::new().expect("temp dir");
    let first = temp.path().join("first");
    repository(&first, "git@example.com:org/shared.git", "initial\n");
    let second = temp.path().join("second");
    git(
        temp.path(),
        &["clone", "-q", first.to_str().unwrap(), "second"],
    );
    git(
        &second,
        &[
            "remote",
            "set-url",
            "origin",
            "git@example.com:org/shared.git",
        ],
    );
    let unrelated = temp.path().join("unrelated");
    repository(&unrelated, "git@example.com:org/shared.git", "other\n");
    let gone = temp.path().join("gone");

    let survivor = checkout_party(
        "prj_aaaaaaaa-0000-4000-8000-000000000001",
        "app",
        &[&first, &gone],
    );
    let retired = checkout_party(
        "prj_bbbbbbbb-0000-4000-8000-000000000002",
        "copy",
        &[&second],
    );
    let same = identity_evidence(&survivor, &retired);
    assert!(same.same_repository, "{same:?}");
    assert_eq!(
        same.canonical_remote.as_deref(),
        Some("example.com/org/shared")
    );
    assert_eq!(same.checkouts[1].state, "missing");

    let other = checkout_party(
        "prj_bbbbbbbb-0000-4000-8000-000000000002",
        "other",
        &[&unrelated],
    );
    let different = identity_evidence(&survivor, &other);
    assert!(!different.same_repository);
    assert!(
        different
            .refusal
            .as_deref()
            .is_some_and(|refusal| refusal.contains("not one repository")),
        "{different:?}"
    );

    let absent = checkout_party(
        "prj_bbbbbbbb-0000-4000-8000-000000000002",
        "absent",
        &[&gone],
    );
    let unproven = identity_evidence(&survivor, &absent);
    assert!(
        unproven
            .refusal
            .as_deref()
            .is_some_and(|refusal| refusal.contains("no checkout of absent")),
        "{unproven:?}"
    );

    let mut recorded = survivor.clone();
    recorded.root_commit = Some("feedface".to_owned());
    let contradicted = identity_evidence(&recorded, &retired);
    assert!(
        contradicted
            .refusal
            .as_deref()
            .is_some_and(|refusal| refusal.contains("records root commit feedface")),
        "{contradicted:?}"
    );
}

#[test]
fn selectors_resolve_ids_names_and_recorded_paths() {
    let fixture = Fixture::new();
    let registry = ProjectRegistry::new(fixture.database.clone());
    assert_eq!(
        resolve_selector(&registry, fixture.retired.as_str()).expect("id"),
        fixture.retired
    );
    assert_eq!(
        resolve_selector(&registry, "app-copy").expect("name"),
        fixture.retired
    );
    assert_eq!(
        resolve_selector(&registry, "APP-COPY").expect("case-insensitive name"),
        fixture.retired
    );
    assert_eq!(
        resolve_selector(&registry, "/work/app-copy").expect("recorded path"),
        fixture.retired
    );
    fixture.merge();
    // After the merge the name still selects the retired project, so a
    // repeated merge reports "already merged".
    assert_eq!(
        resolve_selector(&registry, "app-copy").expect("retired name"),
        fixture.retired
    );
    let connection = fixture.connection();
    assert_eq!(
        merge_that_moved(&connection, &fixture.survivor, "/work/app-copy").expect("moved"),
        Some(fixture.retired.clone())
    );
    assert!(resolve_selector(&registry, "nothing-by-this-name").is_err());
}
