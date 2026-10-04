//! Cursor hooks and Codex sub-session routing, with synthetic payloads only.
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

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
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    repo
}

#[test]
#[allow(clippy::too_many_lines)]
fn cursor_installer_merges_previews_and_removes_only_engram_entries() {
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
    let temp = TempDir::new().unwrap();
    let root = temp.path();
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
    cli(root, &["daemon", "stop"], None);
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
    let temp = TempDir::new().unwrap();
    let root = temp.path();
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
    cli(root, &["daemon", "stop"], None);
}
