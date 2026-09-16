//! Production hook input/output, repository routing, and lazy episode regression.
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

fn cli(root: &Path, args: &[&str], input: Option<Value>) -> String {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("engram"));
    command
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

#[test]
#[allow(clippy::too_many_lines)]
fn lazy_hooks_route_edits_and_verification_to_target_and_close_every_project() {
    let temp = TempDir::new().expect("temp");
    let state = temp.path();
    let first = state.join("first");
    let second = state.join("second");
    for repo in [&first, &second] {
        std::fs::create_dir(repo).expect("repo");
        std::fs::write(repo.join("lib.rs"), "pub fn compute() -> u32 { 42 }\n").expect("source");
        cli(state, &["index", repo.to_str().unwrap(), "--json"], None);
    }
    let c = Connection::open(state.join("brain.sqlite")).expect("read db");
    let count = || {
        c.query_row("SELECT count(*) FROM tasks", [], |r| r.get::<_, i64>(0))
            .expect("count")
    };
    let payload =
        |input: Value| json!({"session_id":"capture-test", "cwd":first, "tool_input":input});
    cli(
        state,
        &["hook", "prompt"],
        Some(json!({"session_id":"capture-test", "cwd":first,
        "prompt":"Fix compute using sk_test_fakeCapture123"})),
    );
    assert_eq!(count(), 0, "prompts alone do not open episodes");
    let pending = engram::agenthooks::pending_prompt(state, "capture-test").expect("pending");
    assert!(!pending.contains("sk_test_fakeCapture123"));
    let output = cli(
        state,
        &["hook", "pretool"],
        Some(payload(json!({"file_path":second.join("lib.rs")}))),
    );
    let envelope: Value = serde_json::from_str(&output).expect("one complete JSON envelope");
    assert!(
        envelope["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("compute")
    );
    assert_eq!(count(), 1);
    let project: String = c
        .query_row(
            "SELECT p.name FROM tasks t JOIN projects p ON p.id=t.project_id",
            [],
            |r| r.get(0),
        )
        .expect("target project");
    assert_eq!(project, "second");
    // A duplicate pretool hook must not open another task or inject twice.
    assert!(
        cli(
            state,
            &["hook", "pretool"],
            Some(payload(json!({"file_path":second.join("lib.rs")})))
        )
        .is_empty()
    );
    cli(
        state,
        &["hook", "pretool"],
        Some(payload(json!({"file_path":first.join("lib.rs")}))),
    );
    assert_eq!(count(), 2);
    let linked: i64 = c
        .query_row(
            "SELECT count(*) FROM usage_events WHERE usage_kind='context_served'",
            [],
            |r| r.get(0),
        )
        .expect("usage linkage");
    assert_eq!(
        linked, 2,
        "both original prompt and cross-repo injection attach to their episode"
    );
    let mut verification =
        payload(json!({"command":format!("cd '{}' && cargo test", second.display())}));
    verification["tool_response"] = json!({"exit_code":0,"stdout":"fixture passed"});
    cli(state, &["hook", "bash"], Some(verification));
    let evidence_project: String = c
        .query_row(
            "SELECT p.name FROM evidence e JOIN projects p ON p.id=e.project_id",
            [],
            |r| r.get(0),
        )
        .expect("evidence project");
    assert_eq!(evidence_project, "second");
    cli(
        state,
        &["relay", "send", "all", "capture fixture relay"],
        None,
    );
    let relay = cli(
        state,
        &["hook", "prompt"],
        Some(json!({"session_id":"capture-test", "cwd":first, "prompt":"continue"})),
    );
    assert!(relay.contains("capture fixture relay"));
    cli(state, &["hook", "session-end"], Some(payload(json!({}))));
    let outcome: String = c.query_row("SELECT t.status FROM tasks t JOIN projects p ON p.id=t.project_id WHERE p.name='second'", [], |r| r.get(0)).expect("outcome");
    assert_eq!(outcome, "success");
    let open: i64 = c
        .query_row("SELECT count(*) FROM tasks WHERE status='open'", [], |r| {
            r.get(0)
        })
        .expect("open");
    assert_eq!(open, 0);
    assert!(engram::agenthooks::pending_prompt(state, "capture-test").is_none());
    cli(state, &["daemon", "stop"], None);
}
