//! Cursor hooks and Codex sub-session routing, with synthetic payloads only.
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

/// These tests spawn CLIs that auto-start daemons. On macOS a child spawned
/// while another thread creates its stdio pipes can inherit them, and a daemon
/// that child starts then holds them open: the other test's hook never sees
/// end of input and hangs. Running the tests one at a time keeps every pipe
/// private to its own test.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn run(root: &Path, args: &[&str], input: Option<&Value>) -> Output {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("engram"));
    command
        .env("HOME", root)
        .env_remove("ENGRAM_CURSOR_HOME")
        .args([
            "--db",
            root.join("brain.sqlite").to_str().unwrap(),
            "--socket",
            root.join("engramd.sock").to_str().unwrap(),
        ])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("cli");
    let mut stdin = child.stdin.take().expect("stdin");
    if let Some(input) = input {
        stdin
            .write_all(input.to_string().as_bytes())
            .expect("payload");
    }
    drop(stdin);
    child.wait_with_output().expect("output")
}

/// Stops a test's daemon when the test ends, also after a failed assertion.
struct DaemonGuard<'a>(&'a Path);

impl Drop for DaemonGuard<'_> {
    fn drop(&mut self) {
        let _ = run(self.0, &["daemon", "stop"], None);
    }
}

fn cli(root: &Path, args: &[&str], input: Option<&Value>) -> String {
    let output = run(root, args, input);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf8")
}

/// A Cursor hook answers with exactly one JSON object and never asks Cursor
/// to block, deny or continue on its own.
fn cursor_hook(root: &Path, event: &str, input: &Value) -> Value {
    let stdout = cli(root, &["hook", event, "--cursor"], Some(input));
    let mut lines = stdout.lines();
    let response: Value = serde_json::from_str(lines.next().expect("one response line"))
        .unwrap_or_else(|error| panic!("{event}: {error}: {stdout}"));
    assert!(lines.next().is_none(), "{event} printed more than one line");
    let fields = response.as_object().expect("JSON object");
    for key in fields.keys() {
        assert!(
            matches!(key.as_str(), "additional_context" | "continue"),
            "{event}: undocumented or unsafe field {key}"
        );
    }
    if let Some(value) = fields.get("continue") {
        assert_eq!(value, &json!(true));
    }
    response
}

fn count(connection: &Connection, sql: &str) -> i64 {
    connection.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn indexed_repo(root: &Path) -> std::path::PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("lib.rs"), "pub fn compute() -> u32 { 42 }\n").unwrap();
    // Setup only: on a heavily loaded machine the first daemon launch after a
    // rebuild can outlast the CLI's 5 s auto-start wait. A later command
    // reuses the daemon once it is ready; any other failure fails the test.
    for attempt in 1..=3 {
        let output = run(root, &["index", repo.to_str().unwrap(), "--json"], None);
        if output.status.success() {
            return repo;
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            attempt < 3 && stderr.contains("did not become ready"),
            "index: {stderr}"
        );
    }
    unreachable!("index either succeeds or fails the test")
}

#[test]
#[allow(clippy::too_many_lines)]
fn cursor_installer_merges_previews_and_removes_only_engram_entries() {
    let _serial = serial();
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let cursor_home = root.join("cursor-home");
    std::fs::create_dir(&cursor_home).unwrap();
    let hooks_path = cursor_home.join("hooks.json");
    let mcp_path = cursor_home.join("mcp.json");
    let original_hooks = json!({"version":1,"hooks":{
        "afterFileEdit":[{"command":"./hooks/format.sh"}],
        "postToolUse":[
            {"command":"leb hook bash --cursor","timeout":5},
            {"command":"./hooks/audit.sh"}
        ]
    }});
    std::fs::write(&hooks_path, original_hooks.to_string()).unwrap();
    let original_mcp = json!({"mcpServers":{"docs":{"url":"https://mcp.example.test/sse"}}});
    std::fs::write(&mcp_path, original_mcp.to_string()).unwrap();
    let home_flag = ["--cursor-home", cursor_home.to_str().unwrap()];
    let setup = |extra: &[&str]| {
        let mut args = vec!["hooks", "cursor"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&home_flag);
        run(root, &args, None)
    };
    let read = |path: &Path| std::fs::read_to_string(path).unwrap();

    // An installed engram without the adapter would exit 2 on `--cursor`,
    // which Cursor treats as a block; the installer refuses to point at it.
    let stale = root.join(".local/bin/engram");
    std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
    std::fs::write(&stale, "#!/bin/sh\nexit 2\n").unwrap();
    std::fs::set_permissions(&stale, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let refused = setup(&["--install"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("does not support Cursor hooks"));
    assert_eq!(
        serde_json::from_str::<Value>(&read(&hooks_path)).unwrap(),
        original_hooks
    );
    std::fs::remove_file(&stale).unwrap();

    let preview = setup(&["--print", "--mcp"]);
    assert!(preview.status.success());
    let preview: Value = serde_json::from_slice(&preview.stdout).unwrap();
    let hooks_key = hooks_path.display().to_string();
    let mcp_key = mcp_path.display().to_string();
    assert_eq!(preview[&hooks_key]["version"], 1);
    assert!(
        preview[&mcp_key]["mcpServers"]["engram"]["command"]
            .as_str()
            .unwrap()
            .ends_with("engram-mcp")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&read(&hooks_path)).unwrap(),
        original_hooks,
        "--print writes nothing"
    );

    assert!(setup(&["--install"]).status.success());
    let installed = read(&hooks_path);
    let hooks: Value = serde_json::from_str(&installed).unwrap();
    assert_eq!(
        hooks["hooks"]["afterFileEdit"],
        original_hooks["hooks"]["afterFileEdit"]
    );
    let post = hooks["hooks"]["postToolUse"].as_array().unwrap();
    assert_eq!(post.len(), 2);
    assert_eq!(post[0]["command"], "./hooks/audit.sh");
    let command = post[1]["command"].as_str().unwrap();
    assert!(command.starts_with("'/") && command.ends_with("' hook bash --cursor"));
    for event in [
        "sessionStart",
        "beforeSubmitPrompt",
        "postToolUseFailure",
        "stop",
        "sessionEnd",
    ] {
        assert_eq!(
            hooks["hooks"][event].as_array().unwrap().len(),
            1,
            "{event}"
        );
    }
    for permission_hook in ["preToolUse", "beforeReadFile", "beforeShellExecution"] {
        assert!(hooks["hooks"].get(permission_hook).is_none());
    }
    assert_eq!(read(&mcp_path), original_mcp.to_string(), "MCP is opt-in");
    assert!(setup(&["--install"]).status.success());
    assert_eq!(read(&hooks_path), installed, "install is idempotent");

    assert!(setup(&["--mcp"]).status.success());
    let mcp: Value = serde_json::from_str(&read(&mcp_path)).unwrap();
    assert_eq!(
        mcp["mcpServers"]["docs"],
        original_mcp["mcpServers"]["docs"]
    );
    assert_eq!(mcp["mcpServers"]["engram"]["type"], "stdio");
    let mcp_command = mcp["mcpServers"]["engram"]["command"].as_str().unwrap();
    assert!(Path::new(mcp_command).is_absolute() && Path::new(mcp_command).is_file());
    assert_eq!(
        read(&hooks_path),
        installed,
        "--mcp alone leaves hooks alone"
    );

    assert!(setup(&["--uninstall", "--mcp"]).status.success());
    assert_eq!(
        serde_json::from_str::<Value>(&read(&hooks_path)).unwrap(),
        json!({"version":1,"hooks":{
            "afterFileEdit":[{"command":"./hooks/format.sh"}],
            "postToolUse":[{"command":"./hooks/audit.sh"}]
        }})
    );
    assert_eq!(
        serde_json::from_str::<Value>(&read(&mcp_path)).unwrap(),
        original_mcp
    );

    // Unreadable or foreign documents are refused and left untouched.
    std::fs::write(&hooks_path, "{broken").unwrap();
    assert!(!setup(&["--install"]).status.success());
    assert_eq!(read(&hooks_path), "{broken");
    let foreign = json!({"mcpServers":{"engram":{"command":"npx","args":["other-engram"]}}});
    std::fs::write(&mcp_path, foreign.to_string()).unwrap();
    assert!(!setup(&["--mcp"]).status.success());
    assert_eq!(read(&mcp_path), foreign.to_string());

    // The environment override applies when no flag is given.
    let env_home = root.join("env-cursor");
    let output = Command::new(assert_cmd::cargo::cargo_bin!("engram"))
        .env("HOME", root)
        .env("ENGRAM_CURSOR_HOME", &env_home)
        .args(["hooks", "cursor", "--install"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(env_home.join("hooks.json").is_file());
    assert!(
        !root.join(".cursor").exists(),
        "overrides keep the default Cursor home untouched"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn cursor_hooks_capture_once_and_answer_with_documented_fields() {
    let _serial = serial();
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let _daemon = DaemonGuard(root);
    let repo = indexed_repo(root);
    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
    // Cursor adds `session_id` (its conversation id) to every agent hook input.
    let base = |event: &str| {
        json!({"conversation_id":"cursor-conv","session_id":"cursor-conv","generation_id":"gen-1",
            "model":"test-model","hook_event_name":event,"cursor_version":"3.17.19",
            "workspace_roots":[repo],"user_email":null,"transcript_path":null})
    };
    let with = |event: &str, fields: Value| {
        let mut payload = base(event);
        for (key, value) in fields.as_object().unwrap() {
            payload[key] = value.clone();
        }
        payload
    };

    let start = cursor_hook(
        root,
        "session-start",
        &with(
            "sessionStart",
            json!({"session_id":"cursor-conv","is_background_agent":false,"composer_mode":"agent"}),
        ),
    );
    assert!(start.get("continue").is_none());

    let prompt = with(
        "beforeSubmitPrompt",
        json!({"prompt":"Inspect compute","attachments":[]}),
    );
    assert_eq!(
        cursor_hook(root, "prompt", &prompt),
        json!({"continue":true})
    );
    assert_eq!(
        engram::agenthooks::pending_prompt(root, "cursor-conv").as_deref(),
        Some("Inspect compute")
    );
    assert_eq!(count(&connection, "SELECT count(*) FROM tasks"), 0);
    assert_eq!(
        count(&connection, "SELECT count(*) FROM context_runs"),
        0,
        "nothing is compiled for a prompt Cursor cannot return context for"
    );

    // Cursor also feeds its input to imported Claude Code hooks; those stay silent.
    let imported = with("beforeSubmitPrompt", json!({"prompt":"Imported duplicate"}));
    assert!(cli(root, &["hook", "prompt"], Some(&imported)).is_empty());
    assert_eq!(
        engram::agenthooks::pending_prompt(root, "cursor-conv").as_deref(),
        Some("Inspect compute")
    );

    cli(
        root,
        &["relay", "send", "cursor-conv", "fixture relay message"],
        None,
    );
    let read = with(
        "postToolUse",
        json!({"tool_name":"Read","tool_use_id":"cursor-read-1",
            "tool_input":{"file_path":repo.join("lib.rs")},
            "tool_output":"{\"file_path\":\"lib.rs\"}","duration":3}),
    );
    let delivered = cursor_hook(root, "bash", &read);
    let context = delivered["additional_context"].as_str().expect("context");
    assert!(context.contains("compute"), "{context}");
    assert!(context.contains("fixture relay message"), "{context}");
    assert!(
        context.len()
            <= engram::agenthooks::cursor::CONTEXT_BYTES + engram::agenthooks::cursor::RELAY_BYTES
    );
    assert_eq!(count(&connection, "SELECT count(*) FROM tasks"), 1);
    assert_eq!(
        count(
            &connection,
            "SELECT count(*) FROM usage_events WHERE usage_kind='context_served'"
        ),
        1
    );
    // Identical context stays silent; the repeated tool id is counted once.
    assert_eq!(cursor_hook(root, "bash", &read), json!({}));
    assert_eq!(
        count(
            &connection,
            "SELECT count(*) FROM resource_usage WHERE source='cursor' AND tool_calls=1"
        ),
        1
    );

    let shell = with(
        "postToolUse",
        json!({"tool_name":"Shell","tool_use_id":"cursor-shell-1","cwd":repo,
            "tool_input":{"command":"cargo test","cwd":repo},
            "tool_output":"{\"exitCode\":0,\"output\":\"test result: ok\"}","duration":900}),
    );
    cursor_hook(root, "bash", &shell);
    let (source, exit_code): (String, i64) = connection
        .query_row(
            "SELECT json_extract(metadata_json,'$.source'), exit_code FROM evidence",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((source.as_str(), exit_code), ("cursor-hook", 0));
    // A failure without an observed exit code is a tool call, not evidence,
    // and it leaves the latest verification result unknown.
    let failed = with(
        "postToolUseFailure",
        json!({"tool_name":"Shell","tool_use_id":"cursor-shell-2",
            "tool_input":{"command":"cargo test","cwd":""},
            "error_message":"test result: FAILED","failure_type":"error","is_interrupt":false,
            "duration":800}),
    );
    cursor_hook(root, "bash", &failed);
    assert_eq!(count(&connection, "SELECT count(*) FROM evidence"), 1);
    let sessions = engram::agenthooks::project_sessions(root, "cursor-conv");
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].1.last_verification_exit, None,
        "an older pass must not close the episode as a success"
    );
    // The same passing command afterwards is a new observation, not a duplicate.
    let mut passed_again = shell.clone();
    passed_again["tool_use_id"] = json!("cursor-shell-4");
    cursor_hook(root, "bash", &passed_again);
    assert_eq!(count(&connection, "SELECT count(*) FROM evidence"), 2);
    assert_eq!(
        count(
            &connection,
            "SELECT sum(tool_calls) FROM resource_usage WHERE source='cursor'"
        ),
        4
    );
    // Imported Claude Code tool hooks record nothing under Cursor.
    let imported_tool = with(
        "postToolUse",
        json!({"tool_name":"Read","tool_use_id":"cursor-read-3","cwd":repo,
            "tool_input":{"file_path":repo.join("lib.rs")},"tool_output":"{}"}),
    );
    assert!(cli(root, &["hook", "bash"], Some(&imported_tool)).is_empty());
    assert_eq!(count(&connection, "SELECT count(*) FROM resource_usage"), 4);

    assert_eq!(
        cursor_hook(
            root,
            "stop",
            &with("stop", json!({"status":"completed","loop_count":0}))
        ),
        json!({})
    );
    let end = with(
        "sessionEnd",
        json!({"session_id":"cursor-conv","reason":"completed","duration_ms":1000,
            "is_background_agent":false,"final_status":"completed"}),
    );
    assert_eq!(cursor_hook(root, "session-end", &end), json!({}));
    assert_eq!(
        count(
            &connection,
            "SELECT count(*) FROM tasks WHERE status='success' AND accepted_by_user IS NULL"
        ),
        1
    );
    assert!(engram::agenthooks::pending_prompt(root, "cursor-conv").is_none());
    // Unreadable input still yields one valid response.
    let output = run(root, &["hook", "prompt", "--cursor"], None);
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"continue":true})
    );
}

fn write_rows(path: &Path, rows: &[Value]) {
    let text = rows
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, text + "\n").unwrap();
}

fn token_count(input: i64, output: i64) -> Value {
    json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{
        "input_tokens":input,"output_tokens":output,"cached_input_tokens":0}}}})
}

#[test]
#[allow(clippy::too_many_lines)]
fn codex_guardian_and_worker_hooks_stay_attributed_to_the_parent() {
    let _serial = serial();
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let _daemon = DaemonGuard(root);
    let repo = indexed_repo(root);
    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
    // Synthetic Codex 0.155 rollouts. Every thread shares the root session id.
    let parent_transcript = root.join("rollout-parent.jsonl");
    let parent_meta = json!({"type":"session_meta","payload":{"id":"codex-root",
        "session_id":"codex-root","source":"vscode","thread_source":"user"}});
    write_rows(
        &parent_transcript,
        &[parent_meta.clone(), token_count(100, 10)],
    );
    let guardian_transcript = root.join("rollout-guardian.jsonl");
    write_rows(
        &guardian_transcript,
        &[
            json!({"type":"session_meta","payload":{"id":"guardian-thread",
                "session_id":"codex-root","parent_thread_id":"codex-root",
                "thread_source":"guardian_review","source":{"subagent":{"other":"guardian"}}}}),
            token_count(5_000, 50),
            token_count(9_000, 90),
        ],
    );
    let hook = |event: &str, payload: &Value| cli(root, &["hook", event, "--codex"], Some(payload));

    hook(
        "session-start",
        &json!({"session_id":"codex-root","cwd":repo,"source":"startup",
            "transcript_path":parent_transcript}),
    );
    hook(
        "prompt",
        &json!({"session_id":"codex-root","cwd":repo,"prompt":"Inspect compute",
            "transcript_path":parent_transcript}),
    );
    let read = |tool_id: &str| {
        json!({"session_id":"codex-root","cwd":repo,"tool_name":"Read","tool_use_id":tool_id,
            "tool_input":{"file_path":repo.join("lib.rs")},"transcript_path":parent_transcript})
    };
    hook("pretool", &read("parent-read"));
    hook("bash", &read("parent-read"));
    assert_eq!(count(&connection, "SELECT count(*) FROM tasks"), 1);
    let usage_rows = count(&connection, "SELECT count(*) FROM resource_usage");

    // The guardian judges a plan: no context, prompt capture, episode or usage.
    let guardian = |fields: Value| {
        let mut payload = json!({"session_id":"codex-root","cwd":repo,
            "transcript_path":guardian_transcript});
        for (key, value) in fields.as_object().unwrap() {
            payload[key] = value.clone();
        }
        payload
    };
    let review = guardian(json!({"prompt":"Review whether this command is safe to run"}));
    assert!(hook("prompt", &review).is_empty());
    assert_eq!(
        engram::agenthooks::pending_prompt(root, "codex-root").as_deref(),
        Some("Inspect compute"),
        "a guardian review must not replace the user's task prompt"
    );
    let guardian_read = guardian(json!({"tool_name":"Read","tool_use_id":"guardian-read",
        "tool_input":{"file_path":repo.join("lib.rs")}}));
    assert!(hook("pretool", &guardian_read).is_empty());
    assert!(hook("bash", &guardian_read).is_empty());
    assert!(hook("stop", &guardian(json!({}))).is_empty());
    assert!(hook("session-end", &guardian(json!({"reason":"other"}))).is_empty());
    assert_eq!(
        count(&connection, "SELECT count(*) FROM resource_usage"),
        usage_rows
    );
    assert_eq!(
        count(
            &connection,
            "SELECT count(*) FROM tasks WHERE status='open'"
        ),
        1,
        "a sub-session never closes the parent's episode"
    );

    // A spawned worker writes code for the same task: its prompt (written by the
    // parent agent) is not the task prompt, but its tool calls count once.
    let worker = |fields: Value| {
        let mut payload = json!({"session_id":"codex-root","cwd":repo,
            "agent_id":"worker-thread","agent_type":"worker"});
        for (key, value) in fields.as_object().unwrap() {
            payload[key] = value.clone();
        }
        payload
    };
    assert!(
        hook(
            "prompt",
            &worker(json!({"prompt":"Check the compute helper"}))
        )
        .is_empty()
    );
    assert_eq!(
        engram::agenthooks::pending_prompt(root, "codex-root").as_deref(),
        Some("Inspect compute")
    );
    hook(
        "bash",
        &worker(json!({"tool_name":"Read","tool_use_id":"worker-read",
            "tool_input":{"file_path":repo.join("lib.rs")}})),
    );
    assert_eq!(count(&connection, "SELECT count(*) FROM tasks"), 1);
    assert_eq!(
        count(
            &connection,
            "SELECT count(DISTINCT task_id) FROM resource_usage WHERE session_id='codex-root'"
        ),
        1
    );
    assert_eq!(
        count(&connection, "SELECT count(*) FROM resource_usage"),
        usage_rows + 1
    );

    // Only the parent's own transcript feeds token usage.
    write_rows(
        &parent_transcript,
        &[parent_meta, token_count(100, 10), token_count(160, 25)],
    );
    hook(
        "stop",
        &json!({"session_id":"codex-root","cwd":repo,"transcript_path":parent_transcript}),
    );
    assert_eq!(
        count(
            &connection,
            "SELECT coalesce(sum(input_tokens),0) FROM resource_usage"
        ),
        60
    );
    hook(
        "session-end",
        &json!({"session_id":"codex-root","cwd":repo,"transcript_path":parent_transcript}),
    );
    assert_eq!(
        count(
            &connection,
            "SELECT count(*) FROM tasks WHERE status='open'"
        ),
        0
    );
}

/// Cursor input for one conversation, as the shipped client sends it.
fn cursor_event(repo: &Path, event: &str, fields: &Value) -> Value {
    let mut payload = json!({"conversation_id":"cursor-conv","session_id":"cursor-conv",
        "generation_id":"gen-1","model":"test-model","hook_event_name":event,
        "cursor_version":"3.17.19","workspace_roots":[repo],"user_email":null,
        "transcript_path":null});
    for (key, value) in fields.as_object().unwrap() {
        payload[key] = value.clone();
    }
    payload
}

#[test]
fn cursor_failures_are_observed_as_failed_calls() {
    let _serial = serial();
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let _daemon = DaemonGuard(root);
    let repo = indexed_repo(root);
    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
    cursor_hook(
        root,
        "prompt",
        &cursor_event(
            &repo,
            "beforeSubmitPrompt",
            &json!({"prompt":"Inspect compute"}),
        ),
    );
    let failed_read = cursor_event(
        &repo,
        "postToolUseFailure",
        &json!({"tool_name":"Read","tool_use_id":"cursor-big-read",
            "tool_input":{"file_path":repo.join("lib.rs")},
            "error_message":"File content (300KB) exceeds maximum allowed size (256KB)",
            "failure_type":"error","is_interrupt":false,"duration":12}),
    );
    cursor_hook(root, "bash", &failed_read);
    let (outcome, limit_hit): (String, i64) = connection
        .query_row(
            "SELECT outcome, limit_hit FROM tool_observations WHERE tool_name='Read'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((outcome.as_str(), limit_hit), ("failed", 1));
    // A failed Engram MCP call does not route later evidence to the task it names.
    let task: String = connection
        .query_row("SELECT id FROM tasks", [], |row| row.get(0))
        .unwrap();
    cursor_hook(
        root,
        "bash",
        &cursor_event(
            &repo,
            "postToolUseFailure",
            &json!({"tool_name":"MCP:engram_record_attempt","tool_use_id":"cursor-mcp-1",
                "tool_input":{"task_id":task,"summary":"tried the fix"},"cwd":repo,
                "error_message":"MCP error","failure_type":"error","is_interrupt":false}),
        ),
    );
    let sessions = engram::agenthooks::project_sessions(root, "cursor-conv");
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        engram::agenthooks::declared_task(root, &sessions[0].0),
        None,
        "a failed call declares nothing"
    );
}

#[test]
fn cursor_background_shell_launches_never_count_as_passes() {
    let _serial = serial();
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let _daemon = DaemonGuard(root);
    let repo = indexed_repo(root);
    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
    cursor_hook(
        root,
        "prompt",
        &cursor_event(
            &repo,
            "beforeSubmitPrompt",
            &json!({"prompt":"Verify compute"}),
        ),
    );
    let shell = |tool_id: &str, input: Value, output: &str| {
        cursor_event(
            &repo,
            "postToolUse",
            &json!({"tool_name":"Shell","tool_use_id":tool_id,"cwd":repo,
                "tool_input":input,"tool_output":output,"duration":20}),
        )
    };
    // A launch that only started the command reports no result of its own.
    cursor_hook(
        root,
        "bash",
        &shell(
            "cursor-bg-1",
            json!({"command":"cargo test","cwd":repo,"run_in_background":true}),
            "{\"exitCode\":0}",
        ),
    );
    cursor_hook(
        root,
        "bash",
        &shell(
            "cursor-bg-2",
            json!({"command":"cargo test","cwd":repo}),
            "{\"shell_id\":3,\"pid\":4242}",
        ),
    );
    assert_eq!(count(&connection, "SELECT count(*) FROM evidence"), 0);
    cursor_hook(
        root,
        "bash",
        &shell(
            "cursor-fg-1",
            json!({"command":"cargo test","cwd":repo}),
            "{\"exitCode\":0,\"output\":\"test result: ok\"}",
        ),
    );
    assert_eq!(count(&connection, "SELECT count(*) FROM evidence"), 1);
}

#[test]
fn cursor_tool_hooks_wait_for_an_unavailable_daemon_once() {
    let _serial = serial();
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("lib.rs"), "pub fn compute() {}\n").unwrap();
    // A socket below a regular file can never be served.
    std::fs::write(root.join("blocker"), "").unwrap();
    let socket = root.join("blocker/engramd.sock");
    let read = cursor_event(
        &repo,
        "postToolUse",
        &json!({"tool_name":"Read","tool_use_id":"cursor-read-1",
            "tool_input":{"file_path":repo.join("lib.rs")},"tool_output":"{}"}),
    );
    let started = std::time::Instant::now();
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("engram"))
        .env("HOME", root)
        .args([
            "--db",
            root.join("brain.sqlite").to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "hook",
            "bash",
            "--cursor",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(read.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let elapsed = started.elapsed();
    assert!(output.status.success(), "hooks fail open");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({})
    );
    // Each auto-start attempt waits 5 s for readiness; two would exceed 10 s.
    assert!(
        elapsed < std::time::Duration::from_secs(9),
        "waited {elapsed:?}"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn codex_worker_context_keeps_its_own_delivery_state() {
    let _serial = serial();
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let _daemon = DaemonGuard(root);
    let repo = indexed_repo(root);
    let hook = |event: &str, payload: &Value| cli(root, &["hook", event, "--codex"], Some(payload));
    hook(
        "prompt",
        &json!({"session_id":"codex-root","cwd":repo,"prompt":"Inspect compute"}),
    );
    let read = |fields: Value| {
        let mut payload = json!({"session_id":"codex-root","cwd":repo,"tool_name":"Read",
            "tool_input":{"file_path":repo.join("lib.rs")}});
        for (key, value) in fields.as_object().unwrap() {
            payload[key] = value.clone();
        }
        payload
    };
    let worker_read =
        read(json!({"tool_use_id":"worker-read","agent_id":"worker-thread","agent_type":"worker"}));
    let worker = hook("pretool", &worker_read);
    assert!(
        worker.contains("Indexed reference data: lib.rs"),
        "{worker}"
    );
    // The root conversation never saw the worker's brief, so it still gets one.
    let parent = hook("pretool", &read(json!({"tool_use_id":"parent-read"})));
    assert!(
        parent.contains("Indexed reference data: lib.rs"),
        "{parent}"
    );
    // Each conversation suppresses only its own repeats.
    assert!(hook("pretool", &worker_read).is_empty());
    assert!(hook("pretool", &read(json!({"tool_use_id":"parent-read-2"}))).is_empty());
    // Tool calls of both threads stay on the one root episode.
    hook("bash", &worker_read);
    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
    assert_eq!(count(&connection, "SELECT count(*) FROM tasks"), 1);
    let sessions = engram::agenthooks::project_sessions(root, "codex-root");
    assert_eq!(sessions.len(), 1);
    let scope = sessions[0].0.clone();
    hook(
        "session-end",
        &json!({"session_id":"codex-root","cwd":repo}),
    );
    let leftovers: Vec<_> = std::fs::read_dir(root.join("hook-sessions"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(&scope))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn codex_unreadable_transcripts_cannot_replace_an_active_prompt() {
    let _serial = serial();
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let _daemon = DaemonGuard(root);
    let repo = indexed_repo(root);
    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
    let hook = |event: &str, payload: &Value| cli(root, &["hook", event, "--codex"], Some(payload));
    let missing = root.join("rollout-not-yet-written.jsonl");
    // Nothing is active yet: a first prompt is captured even without metadata.
    hook(
        "prompt",
        &json!({"session_id":"codex-root","cwd":repo,"prompt":"Inspect compute",
            "transcript_path":missing}),
    );
    assert_eq!(
        engram::agenthooks::pending_prompt(root, "codex-root").as_deref(),
        Some("Inspect compute")
    );
    hook(
        "bash",
        &json!({"session_id":"codex-root","cwd":repo,"tool_name":"Read",
            "tool_use_id":"root-read","tool_input":{"file_path":repo.join("lib.rs")}}),
    );
    assert_eq!(count(&connection, "SELECT count(*) FROM tasks"), 1);
    // An unidentified thread cannot replace the active task prompt.
    assert!(
        hook(
            "prompt",
            &json!({"session_id":"codex-root","cwd":repo,
                "prompt":"Review whether this command is safe","transcript_path":missing}),
        )
        .is_empty()
    );
    assert_eq!(
        engram::agenthooks::pending_prompt(root, "codex-root").as_deref(),
        Some("Inspect compute")
    );
    assert!(
        hook(
            "stop",
            &json!({"session_id":"codex-root","cwd":repo,"transcript_path":missing})
        )
        .is_empty()
    );
    // Codex runs SessionEnd only for the root thread, also after the user
    // deleted its transcript, so the episode still closes.
    hook(
        "session-end",
        &json!({"session_id":"codex-root","cwd":repo,"transcript_path":missing}),
    );
    assert_eq!(
        count(
            &connection,
            "SELECT count(*) FROM tasks WHERE status='open'"
        ),
        0
    );
}
