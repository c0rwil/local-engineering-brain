//! Hook-observed context use: synthetic Claude Code hook sequences through the
//! real CLI and daemon, measured by `engram effectiveness`.
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

fn cli(root: &Path, args: &[&str], input: Option<Value>) -> String {
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("engram"))
        .env("HOME", root)
        .args([
            "--db",
            root.join("brain.sqlite").to_str().unwrap(),
            "--socket",
            root.join("engramd.sock").to_str().unwrap(),
        ])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cli");
    if let Some(input) = input {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.to_string().as_bytes())
            .expect("payload");
    }
    let output = child.wait_with_output().expect("output");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf8")
}

/// Action, paths, ranged, result bytes, limit hit and outcome of one stored call.
type ObservationRow = (
    String,
    String,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

/// One native tool call: `PreToolUse` (when matched) and its post-tool event.
fn tool_call(root: &Path, pre: bool, payload: &Value, post: &Value) {
    if pre {
        cli(root, &["hook", "pretool"], Some(payload.clone()));
    }
    let mut event = payload.clone();
    for (key, value) in post.as_object().expect("post fields") {
        event[key] = value.clone();
    }
    cli(root, &["hook", "bash"], Some(event));
}

/// Observations commit after the daemon acknowledges them; wait for the rows.
fn wait_for_observations(connection: &Connection, expected: i64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let count: i64 = connection
            .query_row("SELECT count(*) FROM tool_observations", [], |row| {
                row.get(0)
            })
            .unwrap();
        if count == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "expected {expected} observations, found {count}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

#[test]
#[allow(clippy::too_many_lines)]
fn hook_observations_attribute_delivered_pointers_and_measure_reads() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    for name in ["alpha", "beta", "gamma", "delta"] {
        std::fs::write(
            repo.join(format!("{name}.rs")),
            format!("pub fn {name}_marker() -> u32 {{ 1 }}\n"),
        )
        .unwrap();
    }
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    let session = "context-use-session";
    let base = |tool: &str, id: &str, input: Value| json!({"session_id":session,"cwd":repo,"tool_name":tool,"tool_use_id":id,"tool_input":input});
    cli(
        root,
        &["hook", "session-start"],
        Some(json!({"session_id":session,"cwd":repo})),
    );
    cli(
        root,
        &["hook", "prompt"],
        Some(json!({"session_id":session,"cwd":repo,
            "prompt":"Fix beta_marker in beta.rs"})),
    );
    let alpha_text = "pub fn alpha_marker() -> u32 { 1 }\n";
    tool_call(
        root,
        true,
        &base(
            "Read",
            "read-alpha",
            json!({"file_path":repo.join("alpha.rs"),"offset":1,"limit":20}),
        ),
        &json!({"hook_event_name":"PostToolUse","tool_response":{"type":"text",
            "file":{"filePath":repo.join("alpha.rs"),"content":alpha_text,
                "numLines":1,"startLine":1,"totalLines":1}}}),
    );
    let beta_text = "pub fn beta_marker() -> u32 { 1 }\n";
    tool_call(
        root,
        true,
        &base(
            "Read",
            "read-beta",
            json!({"file_path":repo.join("beta.rs")}),
        ),
        &json!({"hook_event_name":"PostToolUse","tool_response":{"type":"text",
            "file":{"filePath":repo.join("beta.rs"),"content":beta_text,
                "numLines":1,"startLine":1,"totalLines":1}}}),
    );
    tool_call(
        root,
        true,
        &base(
            "Read",
            "read-gamma",
            json!({"file_path":repo.join("gamma.rs")}),
        ),
        &json!({"hook_event_name":"PostToolUseFailure",
            "error":"File content (300KB) exceeds maximum allowed size (256KB).","is_interrupt":false}),
    );
    tool_call(
        root,
        false,
        &base("Bash", "shell-ls", json!({"command":"ls"})),
        &json!({"hook_event_name":"PostToolUse","tool_response":{"stdout":"alpha.rs","exit_code":0}}),
    );
    tool_call(
        root,
        true,
        &base(
            "Edit",
            "edit-beta",
            json!({"file_path":repo.join("beta.rs"),"old_string":"1","new_string":"2"}),
        ),
        &json!({"hook_event_name":"PostToolUse","tool_response":{"filePath":repo.join("beta.rs"),"success":true}}),
    );
    // Without an agent tool-use ID a call cannot be identified idempotently.
    cli(
        root,
        &["hook", "bash"],
        Some(json!({"session_id":session,"cwd":repo,"tool_name":"Read",
            "tool_input":{"file_path":repo.join("alpha.rs")}})),
    );
    // A denied call reaches the pre-tool hook only.
    cli(
        root,
        &["hook", "pretool"],
        Some(base(
            "Read",
            "read-delta-denied",
            json!({"file_path":repo.join("delta.rs")}),
        )),
    );

    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
    wait_for_observations(&connection, 6);
    let rows: Vec<ObservationRow> = connection
        .prepare(
            "SELECT action, paths_json, ranged, result_bytes, limit_hit, outcome
                 FROM tool_observations ORDER BY observed_at_ms, call_id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let alpha_bytes = i64::try_from(alpha_text.len()).unwrap();
    let beta_bytes = i64::try_from(beta_text.len()).unwrap();
    assert_eq!(
        rows,
        vec![
            (
                "read".into(),
                "[\"alpha.rs\"]".into(),
                Some(1),
                Some(alpha_bytes),
                Some(0),
                Some("ok".into())
            ),
            (
                "read".into(),
                "[\"beta.rs\"]".into(),
                Some(0),
                Some(beta_bytes),
                Some(0),
                Some("ok".into())
            ),
            (
                "read".into(),
                "[\"gamma.rs\"]".into(),
                Some(0),
                None,
                Some(1),
                Some("failed".into())
            ),
            (
                "shell".into(),
                "[]".into(),
                None,
                None,
                None,
                Some("ok".into())
            ),
            (
                "edit".into(),
                "[\"beta.rs\"]".into(),
                None,
                None,
                None,
                Some("ok".into())
            ),
            (
                "read".into(),
                "[\"delta.rs\"]".into(),
                Some(0),
                None,
                None,
                None
            ),
        ]
    );
    // Sizes and flags only: no file content, command or error text is stored.
    let stored: String = connection
        .query_row(
            "SELECT group_concat(id || call_id || session_id || coalesce(task_id,'') ||
                coalesce(context_run_id,'') || source || tool_name || paths_json, '|')
             FROM tool_observations",
            [],
            |row| row.get(0),
        )
        .unwrap();
    for secret in [
        "alpha_marker",
        "beta_marker",
        "300KB",
        "stdout",
        "read-alpha",
    ] {
        assert!(!stored.contains(secret), "{secret} must not be stored");
    }
    // The pre-tool hook records the touch before compiling the context it
    // triggers, so the trigger can never count as following that context.
    let alpha_at: i64 = connection
        .query_row(
            "SELECT observed_at_ms FROM tool_observations WHERE paths_json = '[\"alpha.rs\"]'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let compiled_before: i64 = connection
        .query_row(
            "SELECT count(*) FROM context_runs WHERE created_at_ms < ?1",
            [alpha_at],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        compiled_before, 1,
        "only the prompt context precedes the read"
    );
    let prompt_points_to_beta: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM context_runs c
               JOIN context_items i ON i.context_run_id = c.id
               JOIN nodes n ON n.id = i.source_id
               WHERE c.created_at_ms < ?1 AND n.file_path = 'beta.rs')",
            [alpha_at],
            |row| row.get(0),
        )
        .unwrap();
    assert!(prompt_points_to_beta, "the prompt context names beta.rs");

    let report: Value =
        serde_json::from_str(&cli(root, &["effectiveness", "--json"], None)).expect("report json");
    let project = report["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|project| project["project_name"] == "repo")
        .expect("repo report");
    let usage = &project["context_use"];
    assert_eq!(usage["observed_tool_calls"], 6);
    assert_eq!(usage["observed_sessions"], 1);
    assert_eq!(usage["observed_calls_with_paths"], 5);
    // Every read followed the prompt delivery in this session. The denied read
    // never executed: it is unfinished, not a read of delta.rs.
    let reads = &usage["reads_by_mode"]["brain"];
    assert_eq!(reads["reads"], 3);
    assert_eq!(reads["ranged"], 1);
    assert_eq!(reads["whole_file"], 2);
    assert_eq!(reads["sized_reads"], 2);
    assert_eq!(reads["result_bytes"], alpha_bytes + beta_bytes);
    assert_eq!(reads["limit_hits"], 1);
    assert_eq!(reads["failed"], 1);
    assert_eq!(reads["unfinished"], 1);
    assert!(usage["reads_by_mode"].get("unattributed").is_none());
    let pointers = &usage["pointers_by_mode"]["brain"];
    assert!(pointers["observed_contexts"].as_u64().unwrap() >= 1);
    assert!(
        pointers["touched_later"].as_u64().unwrap() >= 1,
        "beta.rs was pointed to and read later: {pointers}"
    );
    assert!(pointers["precision"].is_f64());
    assert!(pointers["first_touch_calls"]["median"].as_f64().unwrap() >= 1.0);
    // Completed calls only: the failed read of gamma.rs and the denied read of
    // delta.rs are not touches.
    assert_eq!(usage["touched_files"], 2);
    assert_eq!(usage["touched_files_before_context"], 0);
    assert!(pointers["touched_files_pointed"].as_u64().unwrap() >= 1);
    let text = cli(root, &["effectiveness"], None);
    assert!(
        text.contains("context use (association, not causation): observed tool calls 6"),
        "{text}"
    );
    assert!(
        text.contains("brain reads: 3 (1 ranged, 2 whole-file"),
        "{text}"
    );
    assert!(text.contains("failed 1; unfinished 1"), "{text}");
    cli(root, &["daemon", "stop"], None);
}

/// The pre-tool hook waits for this acknowledgement before compiling context.
/// It must not wait for the observation's lowest-priority write, and the
/// receipt time must still order the touch before a later compilation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn observation_ack_does_not_wait_for_a_blocked_writer() {
    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("alpha.rs"), "pub fn alpha_marker() {}\n").unwrap();
    let mut config = engram::config::Config::default();
    config.storage.db_path = temp.path().join("brain.sqlite");
    config.daemon.socket_path = temp.path().join("engramd.sock");
    config.daemon.request_timeout_ms = 10_000;
    config.web.enabled = false;
    config.watch.enabled = false;
    config.jobs.enabled = false;
    let client =
        engram::daemon::DaemonClient::new(&config.daemon.socket_path, Duration::from_secs(10));
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(engram::daemon::serve_until(config.clone(), async move {
        let _ = stopped.await;
    }));
    for _ in 0..200 {
        if client.health().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let project = client
        .resolve_project(&repo)
        .await
        .unwrap()
        .resolved
        .project
        .id;
    client.index_project(&repo, None).await.unwrap();

    // Hold SQLite's write lock so every daemon write stalls, then occupy the
    // single writer with a pending higher-priority write.
    let blocker = Connection::open(&config.storage.db_path).unwrap();
    blocker.busy_timeout(Duration::from_secs(5)).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let busy = {
        let client = client.clone();
        let project = project.clone();
        tokio::spawn(async move {
            client
                .start_task(&engram::daemon::TaskStartRequest {
                    context_request_id: None,
                    project_id: project,
                    prompt: "Synthetic writer load".to_owned(),
                    session_id: Some("other-session".to_owned()),
                })
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let request = engram::effectiveness::ToolObservationRequest {
        project_id: project.clone(),
        session_id: "latency-session".to_owned(),
        tool_use_id: "pre-read".to_owned(),
        task_id: None,
        context_run_id: None,
        source: "test".to_owned(),
        tool_name: "Read".to_owned(),
        phase: engram::effectiveness::ToolPhase::Pre,
        paths: vec!["alpha.rs".to_owned()],
        ranged: Some(false),
        result_bytes: None,
        limit_hit: None,
    };
    let started = Instant::now();
    let receipt = client
        .record_tool_observation(&request)
        .await
        .expect("acknowledged while the writer is blocked");
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_secs(1),
        "acknowledgement waited {waited:?} for a blocked writer"
    );
    assert!(receipt.queued);
    // Malformed requests are still rejected (HTTP 422) before anything is queued.
    for path in ["/absolute/alpha.rs", "../../outside/secret.txt"] {
        let mut invalid = request.clone();
        invalid.tool_use_id = format!("invalid {path}");
        invalid.paths = vec![path.to_owned()];
        let error = client
            .record_tool_observation(&invalid)
            .await
            .expect_err("invalid path");
        assert!(error.to_string().contains("invalid_request"), "{error}");
    }

    // Compile after the acknowledgement, as the pre-tool hook does. Its
    // higher-priority write can commit before the queued observation.
    let compile = {
        let client = client.clone();
        let project = project.clone();
        let repo = repo.clone();
        tokio::spawn(async move {
            client
                .compile_context(&engram::context::ContextCompileRequest {
                    request_id: "after-observation".to_owned(),
                    project_id: project,
                    task_text: "Inspect alpha_marker".to_owned(),
                    current_paths: vec!["alpha.rs".to_owned()],
                    token_budget: 512,
                    mode: engram::context::ContextMode::Brain,
                    repository_root: Some(repo),
                    head: None,
                    task_id: None,
                    framing: None,
                })
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let released_at = now_ms();
    blocker.execute_batch("ROLLBACK").unwrap();
    busy.await.unwrap().expect("pending write completes");
    let context = compile.await.unwrap().expect("context compiles");

    let reader = Connection::open(&config.storage.db_path).unwrap();
    wait_for_observations(&reader, 1);
    let (observed_at, recorded_at): (i64, i64) = reader
        .query_row(
            "SELECT observed_at_ms, recorded_at_ms FROM tool_observations",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let compiled_at: i64 = reader
        .query_row(
            "SELECT created_at_ms FROM context_runs WHERE id = ?1",
            [context.id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(observed_at, receipt.observed_at_ms);
    assert!(
        observed_at < released_at && recorded_at >= released_at,
        "stamped on receipt ({observed_at}), written after the lock ({recorded_at})"
    );
    assert!(
        observed_at <= compiled_at,
        "the touch ({observed_at}) sorts before the later context ({compiled_at})"
    );
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}
