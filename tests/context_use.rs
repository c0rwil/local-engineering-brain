//! Hook-observed context use: synthetic Claude Code hook sequences through the
//! real CLI and daemon, measured by `engram effectiveness`.
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

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

#[test]
#[allow(clippy::too_many_lines)]
fn hook_observations_attribute_delivered_pointers_and_measure_reads() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    for name in ["alpha", "beta", "gamma"] {
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

    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
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
    assert_eq!(usage["observed_tool_calls"], 5);
    assert_eq!(usage["observed_sessions"], 1);
    assert_eq!(usage["observed_calls_with_paths"], 4);
    // Every read followed the prompt delivery in this session.
    let reads = &usage["reads_by_mode"]["brain"];
    assert_eq!(reads["reads"], 3);
    assert_eq!(reads["ranged"], 1);
    assert_eq!(reads["whole_file"], 2);
    assert_eq!(reads["sized_reads"], 2);
    assert_eq!(reads["result_bytes"], alpha_bytes + beta_bytes);
    assert_eq!(reads["limit_hits"], 1);
    assert_eq!(reads["failed"], 1);
    assert!(usage["reads_by_mode"].get("unattributed").is_none());
    let pointers = &usage["pointers_by_mode"]["brain"];
    assert!(pointers["observed_contexts"].as_u64().unwrap() >= 1);
    assert!(
        pointers["touched_later"].as_u64().unwrap() >= 1,
        "beta.rs was pointed to and read later: {pointers}"
    );
    assert!(pointers["precision"].is_f64());
    assert!(pointers["first_touch_calls"]["median"].as_f64().unwrap() >= 1.0);
    assert_eq!(usage["touched_files"], 3);
    assert_eq!(usage["touched_files_before_context"], 0);
    assert!(pointers["touched_files_pointed"].as_u64().unwrap() >= 1);
    let text = cli(root, &["effectiveness"], None);
    assert!(
        text.contains("context use (association, not causation): observed tool calls 5"),
        "{text}"
    );
    assert!(
        text.contains("brain reads: 3 (1 ranged, 2 whole-file"),
        "{text}"
    );
    cli(root, &["daemon", "stop"], None);
}
