//! Production hook input/output, repository routing, and lazy episode regression.
use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

fn cli(root: &Path, args: &[&str], input: Option<Value>) -> String {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("engram"));
    command
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
fn codex_installer_repairs_flat_hooks_preserves_foreign_handlers_and_rejects_invalid_json() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path().join(".codex");
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("hooks.json");
    let original = json!({"description":"keep me", "SessionStart":[{"hooks":[
        {"type":"command","command":"leb hook session-start --codex"},
        {"type":"command","command":"echo foreign"},
        {"type":"command","command":"echo \"/opt/engram hook prompt --codex\""}]}],
        "hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo stop"}]}]}});
    std::fs::write(&path, original.to_string()).unwrap();
    let install = || {
        Command::new(assert_cmd::cargo::cargo_bin!("engram"))
            .env("HOME", temp.path())
            .env_remove("CODEX_HOME")
            .args(["hooks", "codex", "--install"])
            .output()
            .unwrap()
    };
    assert!(install().status.success());
    let first = std::fs::read_to_string(&path).unwrap();
    let result: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(result["description"], "keep me");
    assert!(result.get("SessionStart").is_none());
    assert!(first.contains("echo foreign") && first.contains("echo stop"));
    assert_eq!(
        result["hooks"]["SessionStart"][0]["hooks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(!first.contains("leb hook"));
    assert_eq!(result["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 3);
    assert!(
        result["hooks"]["SessionStart"][1]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .starts_with("'/")
    );
    assert!(install().status.success());
    assert_eq!(first, std::fs::read_to_string(&path).unwrap());
    std::fs::write(&path, "{broken").unwrap();
    assert!(!install().status.success());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");
}

#[test]
fn claude_hook_upgrade_preserves_mixed_handlers_and_settings() {
    let original = json!({"permissions":{"allow":["Read"]},"hooks":{"PreToolUse":[
        {"matcher":"Read","hooks":[
            {"type":"command","command":"'/old path/leb' hook pretool"},
            {"type":"command","command":"echo \"/opt/engram hook pretool\""}]}]}});
    let binary = "'/new pa'\\''th/engram'";
    let merged = engram::agenthooks::merge_claude_hooks(original.clone(), binary).unwrap();
    assert_eq!(merged["permissions"], original["permissions"]);
    assert_eq!(
        merged["hooks"]["PreToolUse"][0]["hooks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        merged["hooks"]["PreToolUse"][0]["hooks"][0],
        original["hooks"]["PreToolUse"][0]["hooks"][1]
    );
    assert_eq!(
        engram::agenthooks::merge_claude_hooks(merged.clone(), binary).unwrap(),
        merged
    );
}

#[test]
fn codex_budget_preserves_capture_and_retries_omitted_file_briefs() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let config_dir = root.join(".config/engram");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[context]\nmax_injected_tokens = 256\nstable_prefix_tokens = 0\n",
    )
    .unwrap();
    let mut paths = Vec::new();
    for name in ["one", "two"] {
        let repo = root.join(name);
        std::fs::create_dir(&repo).unwrap();
        for file in ["a.rs", "b.rs"] {
            let source = (0..20).fold(String::new(), |mut source, n| {
                writeln!(source, "pub fn {name}_long_function_name_{n}() {{}}").unwrap();
                source
            });
            let path = repo.join(file);
            std::fs::write(&path, source).unwrap();
            paths.push(path);
        }
        cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    }
    let patch = format!(
        "*** Begin Patch\n{}*** End Patch",
        paths.iter().fold(String::new(), |mut patch, p| {
            writeln!(patch, "*** Update File: {}\n@@\n-old\n+new", p.display()).unwrap();
            patch
        })
    );
    let payload = |command: String| json!({"session_id":"budget","cwd":root,"tool_name":"apply_patch","tool_input":{"command":command}});
    cli(
        root,
        &["hook", "prompt", "--codex"],
        Some(json!({"session_id":"budget","cwd":root,"prompt":"Update both repositories"})),
    );
    let output = cli(root, &["hook", "pretool", "--codex"], Some(payload(patch)));
    let result: Value = serde_json::from_str(&output).unwrap();
    let context = result["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.len() <= 1024);
    assert!(!context.contains("two_long_function_name_19"));
    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(DISTINCT project_id) FROM tasks", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    let retry = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
        paths.last().unwrap().display()
    );
    let output = cli(root, &["hook", "pretool", "--codex"], Some(payload(retry)));
    let result: Value = serde_json::from_str(&output).unwrap();
    let context = result["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.len() <= 1024);
    assert!(
        context.contains("two_long_function_name_19"),
        "omitted briefs must remain eligible: {context}"
    );
    cli(root, &["daemon", "stop"], None);
}

#[test]
fn native_codex_shell_and_multi_repo_patch_open_scoped_episodes() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let one = root.join("one");
    let two = root.join("two");
    for repo in [&one, &two] {
        std::fs::create_dir(repo).unwrap();
        std::fs::write(repo.join("lib.rs"), "pub fn compute() -> u32 { 1 }\n").unwrap();
        cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    }
    cli(
        root,
        &["hook", "prompt", "--codex"],
        Some(
            json!({"session_id":"native-codex","cwd":one,"prompt":"Update compute in both repositories"}),
        ),
    );
    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-old\n+new\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
        one.join("lib.rs").display(),
        two.join("lib.rs").display()
    );
    let output = cli(
        root,
        &["hook", "pretool", "--codex"],
        Some(
            json!({"session_id":"native-codex","cwd":root,"tool_name":"apply_patch","tool_input":{"command":patch}}),
        ),
    );
    let result: Value = serde_json::from_str(&output).unwrap();
    let context = result["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.contains("compute"));
    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(DISTINCT project_id) FROM tasks", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    // Native unified-exec is normalized to Bash, with explicit workdir and exit code.
    cli(
        root,
        &["hook", "prompt", "--codex"],
        Some(json!({"session_id":"native-shell","cwd":one,"prompt":"Verify compute"})),
    );
    let shell = json!({"session_id":"native-shell","cwd":one,"tool_name":"Bash","tool_input":{"command":"cargo test","cwd":two},"tool_response":{"exit_code":0,"stdout":"test result: ok"}});
    let output = cli(root, &["hook", "pretool", "--codex"], Some(shell.clone()));
    assert!(
        serde_json::from_str::<Value>(&output).unwrap()["hookSpecificOutput"]["additionalContext"]
            .is_string()
    );
    cli(root, &["hook", "bash", "--codex"], Some(shell));
    assert_eq!(
        connection
            .query_row(
                "SELECT json_extract(metadata_json,'$.source') FROM evidence",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "codex-hook"
    );
    cli(
        root,
        &["hook", "session-end", "--codex"],
        Some(json!({"session_id":"native-shell","cwd":one})),
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM tasks WHERE status='success' AND accepted_by_user IS NULL",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    cli(root, &["daemon", "stop"], None);
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
