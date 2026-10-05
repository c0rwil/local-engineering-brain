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
    let partial_budget: i64 = connection.query_row(
        "SELECT min(c.token_budget) FROM context_runs c JOIN projects p ON p.id=c.project_id WHERE p.name='two'",
        [], |row| row.get(0)).unwrap();
    assert!(
        partial_budget < 256,
        "the second repository gets only the remaining budget"
    );
    let retry = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
        paths.last().unwrap().display()
    );
    let output = cli(
        root,
        &["hook", "pretool", "--codex"],
        Some(payload(retry.clone())),
    );
    let result: Value = serde_json::from_str(&output).unwrap();
    let context = result["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.len() <= 1024);
    // A newly eligible source map can consume the remaining shape budget.
    // Its identical repeat is silent, leaving room for the whole file brief.
    let output = if context.contains("two_long_function_name_19") {
        output
    } else {
        cli(root, &["hook", "pretool", "--codex"], Some(payload(retry)))
    };
    let result: Value = serde_json::from_str(&output).unwrap();
    let context = result["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.len() <= 1024);
    assert!(
        context.contains("two_long_function_name_19"),
        "omitted briefs must remain eligible: {context}"
    );
    assert_eq!(connection.query_row(
        "SELECT max(c.token_budget) FROM context_runs c JOIN projects p ON p.id=c.project_id WHERE p.name='two'",
        [], |row| row.get::<_, i64>(0)).unwrap(), 256,
        "the same paths must be reevaluated when the full budget is available");
    cli(root, &["daemon", "stop"], None);
}

#[test]
#[allow(clippy::too_many_lines)]
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
    // Codex 0.155 reports a shell call as `Bash` with the command text; the
    // rollout records where it ran and its exit code. `cd` is relative to
    // that directory.
    let rollout = root.join("rollout-native.jsonl");
    std::fs::write(
        &rollout,
        format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":"native-shell",
                "session_id":"native-shell","source":"vscode","history_mode":"paginated"}}),
            json!({"type":"event_msg","payload":{"type":"item_completed",
                "item":{"type":"CommandExecution","id":"exec-native","status":"completed",
                    "exit_code":0,"cwd":format!("file://{}", one.display())}}})
        ),
    )
    .unwrap();
    cli(
        root,
        &["hook", "prompt", "--codex"],
        Some(
            json!({"session_id":"native-shell","cwd":one,"transcript_path":rollout,
            "prompt":"Verify compute"}),
        ),
    );
    let shell = json!({"session_id":"native-shell","turn_id":"turn-1","cwd":one,
        "hook_event_name":"PreToolUse","model":"gpt-test","permission_mode":"default",
        "transcript_path":rollout,"tool_name":"Bash","tool_use_id":"exec-native",
        "tool_input":{"command":"cd ../two && cargo test"}});
    let output = cli(root, &["hook", "pretool", "--codex"], Some(shell.clone()));
    assert!(
        serde_json::from_str::<Value>(&output).unwrap()["hookSpecificOutput"]["additionalContext"]
            .is_string()
    );
    let mut finished = shell;
    finished["hook_event_name"] = json!("PostToolUse");
    finished["tool_response"] = json!("test result: ok");
    cli(root, &["hook", "bash", "--codex"], Some(finished));
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
    assert_eq!(
        connection
            .query_row(
                "SELECT p.name FROM evidence e JOIN projects p ON p.id=e.project_id",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "two"
    );
    cli(
        root,
        &["hook", "session-end", "--codex"],
        Some(json!({"session_id":"native-shell","cwd":one,"transcript_path":rollout})),
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
        "only the emitted original prompt and cross-repo context attach to their episodes"
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

#[cfg(unix)]
#[test]
fn session_start_names_an_unreadable_repository_instead_of_staying_silent() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = TempDir::new().expect("temp");
    let root = temp.path();
    let repo = root.join("locked");
    std::fs::create_dir(&repo).unwrap();
    let init = Command::new("git")
        .args(["init", "-q"])
        .arg(&repo)
        .output()
        .expect("git init");
    assert!(init.status.success());
    std::fs::set_permissions(&repo, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(&repo).is_ok() {
        // Running as root: file modes are not enforced, nothing to observe.
        std::fs::set_permissions(&repo, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let brief = cli(
        root,
        &["hook", "session-start"],
        Some(json!({"session_id":"locked-session", "cwd":repo})),
    );
    std::fs::set_permissions(&repo, std::fs::Permissions::from_mode(0o755)).unwrap();
    cli(root, &["daemon", "stop"], None);
    assert!(
        brief.contains("Engram cannot read this repository"),
        "{brief}"
    );
    assert!(brief.contains(repo.to_str().unwrap()), "{brief}");
    assert!(brief.contains("engram doctor"), "{brief}");
}

#[test]
fn hooks_count_real_tool_ids_and_final_usage_once_without_guessing_cost() {
    let temp = TempDir::new().expect("temp");
    let root = temp.path();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("lib.rs"), "pub fn compute() {}\n").unwrap();
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    cli(
        root,
        &["hook", "session-start"],
        Some(json!({
            "session_id":"usage-session", "cwd":repo, "transcript_path":root.join("usage.jsonl")
        })),
    );
    cli(
        root,
        &["hook", "prompt"],
        Some(json!({
            "session_id":"usage-session", "cwd":repo, "prompt":"Inspect compute"
        })),
    );
    let read = json!({"session_id":"usage-session", "cwd":repo, "tool_name":"Read",
        "tool_use_id":"actual-tool-1", "tool_input":{"file_path":repo.join("lib.rs"),"command":"cargo test"},
        "tool_response":{"exit_code":0}});
    cli(root, &["hook", "pretool"], Some(read.clone()));
    cli(root, &["hook", "bash"], Some(read.clone()));
    cli(root, &["hook", "bash"], Some(read));
    // Failure hooks supply an error, not necessarily an observed exit code.
    // Count the actual call once without inventing verification evidence.
    let failed = json!({"session_id":"usage-session", "cwd":repo,
        "hook_event_name":"PostToolUseFailure", "tool_name":"Bash",
        "tool_use_id":"actual-failed-tool-2", "tool_input":{"command":"cargo test"},
        "error":"Tool execution was interrupted", "is_interrupt":true});
    cli(root, &["hook", "bash"], Some(failed.clone()));
    cli(root, &["hook", "bash"], Some(failed));
    cli(
        root,
        &["hook", "bash"],
        Some(json!({"session_id":"usage-session", "cwd":repo,
        "tool_name":"Read", "tool_input":{"file_path":repo.join("lib.rs")}})),
    );
    let transcript = root.join("usage.jsonl");
    std::fs::write(&transcript, concat!(
        "{\"type\":\"assistant\",\"message\":{\"id\":\"msg-final\",\"model\":\"local-model\",\"usage\":{\"input_tokens\":100,\"output_tokens\":2}}}\n",
        "{\"type\":\"assistant\",\"message\":{\"id\":\"msg-final\",\"model\":\"local-model\",\"usage\":{\"input_tokens\":100,\"output_tokens\":10}}}\n"
    )).unwrap();
    let stop = json!({"session_id":"usage-session", "cwd":repo, "transcript_path":transcript});
    cli(root, &["hook", "stop"], Some(stop.clone()));
    cli(root, &["hook", "stop"], Some(stop.clone()));
    cli(root, &["hook", "session-end"], Some(stop));
    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
    let totals: (i64, i64, i64, i64, i64, i64) = connection.query_row(
        "SELECT count(*),sum(tool_calls),sum(input_tokens),sum(output_tokens),count(cost_usd),count(context_run_id) FROM resource_usage",
        [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?)),
    ).unwrap();
    assert_eq!(totals, (3, 2, 100, 10, 0, 2));
    let historical_links: (i64, i64) = connection.query_row(
        "SELECT count(context_run_id),count(mode) FROM resource_usage WHERE input_tokens IS NOT NULL",
        [], |row| Ok((row.get(0)?,row.get(1)?)),
    ).unwrap();
    assert_eq!(
        historical_links,
        (0, 0),
        "historical samples cannot prove context or mode attribution"
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM evidence", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0,
        "neither reading a command nor a failure without an exit code proves verification"
    );
    cli(root, &["daemon", "stop"], None);
}

#[test]
fn multi_repo_session_does_not_duplicate_or_guess_provider_usage_attribution() {
    let temp = TempDir::new().expect("temp");
    let root = temp.path();
    let first = root.join("first");
    let second = root.join("second");
    for repo in [&first, &second] {
        std::fs::create_dir(repo).unwrap();
        std::fs::write(repo.join("lib.rs"), "pub fn compute() {}\n").unwrap();
        cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    }
    cli(
        root,
        &["hook", "session-start", "--codex"],
        Some(json!({
            "session_id":"multi-usage", "cwd":first, "transcript_path":root.join("usage.jsonl")
        })),
    );
    cli(
        root,
        &["hook", "prompt", "--codex"],
        Some(json!({
            "session_id":"multi-usage", "cwd":first, "prompt":"Inspect both repositories"
        })),
    );
    for (repo, tool_id) in [(&first, "first-call"), (&second, "second-call")] {
        let read = json!({"session_id":"multi-usage", "cwd":first,"tool_name":"Read",
            "tool_use_id":tool_id,"tool_input":{"file_path":repo.join("lib.rs")}});
        cli(root, &["hook", "pretool", "--codex"], Some(read.clone()));
        cli(root, &["hook", "bash", "--codex"], Some(read));
    }
    let transcript = root.join("usage.jsonl");
    std::fs::write(
        &transcript,
        json!({"type":"event_msg","payload":{"type":"token_count","info":{
            "total_token_usage":{"input_tokens":100,"output_tokens":10,"cached_input_tokens":50}
        }}})
        .to_string(),
    )
    .unwrap();
    cli(
        root,
        &["hook", "stop", "--codex"],
        Some(json!({"session_id":"multi-usage", "cwd":first,"transcript_path":transcript})),
    );
    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
    let totals: (i64, i64, i64) = connection.query_row(
        "SELECT count(DISTINCT project_id),sum(tool_calls),count(input_tokens) FROM resource_usage",
        [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
    ).unwrap();
    assert_eq!(totals, (2, 2, 0));
    cli(root, &["daemon", "stop"], None);
}

#[test]
fn hook_templates_capture_all_tools_and_stop_usage() {
    for hooks in [
        engram::agenthooks::claude_hooks_json("engram"),
        engram::agenthooks::codex_hooks_json("engram"),
    ] {
        assert!(hooks["PostToolUse"][0].get("matcher").is_none());
        assert!(
            hooks["Stop"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("hook stop")
        );
    }
    let claude = engram::agenthooks::claude_hooks_json("engram");
    assert_eq!(
        claude["PostToolUseFailure"][0]["hooks"][0]["command"],
        "engram hook bash"
    );
    assert!(claude["PostToolUseFailure"][0].get("matcher").is_none());
    assert!(
        engram::agenthooks::codex_hooks_json("engram")
            .get("PostToolUseFailure")
            .is_none()
    );
}

#[test]
fn claude_failure_hook_upgrade_preserves_foreign_handlers_and_is_idempotent() {
    let original = json!({"hooks":{"PostToolUseFailure":[{"matcher":"Bash","hooks":[
        {"type":"command","command":"/old/engram hook bash"},
        {"type":"command","command":"echo foreign-failure-handler"}
    ]}]}});
    let once = engram::agenthooks::merge_claude_hooks(original, "/new/engram").unwrap();
    let groups = once["hooks"]["PostToolUseFailure"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(
        groups[0]["hooks"][0]["command"],
        "echo foreign-failure-handler"
    );
    assert_eq!(groups[1]["hooks"][0]["command"], "/new/engram hook bash");
    assert_eq!(
        engram::agenthooks::merge_claude_hooks(once.clone(), "/new/engram").unwrap(),
        once
    );
}

#[test]
fn resumed_transcript_and_missing_start_use_conservative_usage_baselines() {
    let temp = TempDir::new().expect("temp");
    let root = temp.path();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("lib.rs"), "pub fn compute() {}\n").unwrap();
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    let transcript = root.join("resumed.jsonl");
    let message = |id: &str, input: i64| {
        json!({"type":"assistant","message":{"id":id,
        "usage":{"input_tokens":input,"output_tokens":1}}})
        .to_string()
            + "\n"
    };
    std::fs::write(&transcript, message("historic-untracked", 900)).unwrap();
    cli(
        root,
        &["hook", "session-start"],
        Some(json!({"session_id":"resumed", "cwd":repo,"transcript_path":transcript})),
    );
    for session in ["resumed", "no-start"] {
        cli(
            root,
            &["hook", "prompt"],
            Some(json!({"session_id":session, "cwd":repo,"prompt":"Inspect compute"})),
        );
        cli(
            root,
            &["hook", "pretool"],
            Some(json!({"session_id":session,"cwd":repo,
            "tool_name":"Read","tool_input":{"file_path":repo.join("lib.rs")}})),
        );
    }
    std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap()
        .write_all(message("after-resume", 7).as_bytes())
        .unwrap();
    cli(
        root,
        &["hook", "stop"],
        Some(json!({"session_id":"resumed", "cwd":repo,"transcript_path":transcript})),
    );
    cli(
        root,
        &["hook", "stop"],
        Some(json!({"session_id":"no-start", "cwd":repo,"transcript_path":transcript})),
    );
    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT sum(input_tokens) FROM resource_usage WHERE session_id='resumed'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        7
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM resource_usage WHERE session_id='no-start'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap()
        .write_all(message("after-baseline", 5).as_bytes())
        .unwrap();
    cli(
        root,
        &["hook", "stop"],
        Some(json!({"session_id":"no-start", "cwd":repo,"transcript_path":transcript})),
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT sum(input_tokens) FROM resource_usage WHERE session_id='no-start'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        5
    );
    cli(root, &["daemon", "stop"], None);
}

#[test]
fn followup_intent_refreshes_context_and_reuses_the_project_episode() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    for name in ["compute", "auxiliary", "actual"] {
        std::fs::write(
            repo.join(format!("{name}.rs")),
            format!("pub fn {name}() {{}}\n"),
        )
        .unwrap();
    }
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    let prompt = |text: &str| json!({"session_id":"followup", "cwd":repo, "prompt":text});
    assert!(!cli(root, &["hook", "prompt"], Some(prompt("Inspect compute"))).is_empty());
    assert!(cli(root, &["hook", "prompt"], Some(prompt("Inspect compute"))).is_empty());
    let read = json!({"session_id":"followup", "cwd":repo, "tool_name":"Read",
        "tool_input":{"file_path":"compute.rs"}});
    assert!(!cli(root, &["hook", "pretool"], Some(read.clone())).is_empty());
    assert!(cli(root, &["hook", "pretool"], Some(read)).is_empty());
    assert!(cli(root, &["hook", "prompt"], Some(prompt("Inspect compute"))).is_empty());
    let session = engram::agenthooks::project_sessions(root, "followup")
        .pop()
        .unwrap();
    let initial_run = engram::agenthooks::context_run_id(root, &session.0).unwrap();
    let updated = cli(root, &["hook", "prompt"], Some(prompt("Inspect auxiliary")));
    assert!(updated.contains("auxiliary.rs"), "{updated}");
    assert_ne!(
        engram::agenthooks::context_run_id(root, &session.0).unwrap(),
        initial_run
    );
    assert_eq!(
        engram::agenthooks::pending_prompt(root, "followup").as_deref(),
        Some("Inspect auxiliary")
    );
    assert!(cli(root, &["hook", "prompt"], Some(prompt("Inspect auxiliary"))).is_empty());
    // A non-shell post-tool event must retain the actual file even without PreToolUse.
    cli(
        root,
        &["hook", "bash"],
        Some(json!({"session_id":"followup", "cwd":repo,
        "tool_name":"Edit", "tool_use_id":"edit-actual", "tool_input":{"file_path":"actual.rs"}})),
    );
    let context = cli(root, &["hook", "prompt"], Some(prompt("continue")));
    assert!(
        context.contains("actual.rs"),
        "recent observed file must affect retrieval: {context}"
    );
    assert!(
        !context.contains("auxiliary.rs"),
        "prompt terms are not recent tool paths: {context}"
    );
    let latest = engram::agenthooks::project_sessions(root, "followup");
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].1.task_id, session.1.task_id);
    assert_eq!(
        engram::agenthooks::recent_paths(root, &session.0),
        ["actual.rs", "compute.rs"]
    );
    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM tasks", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(cli(root, &["hook", "prompt"], Some(prompt("continue"))).is_empty());
    cli(root, &["daemon", "stop"], None);
}

#[test]
fn host_records_keep_the_request_and_deliver_nothing() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    for name in ["compute", "create_api_key"] {
        std::fs::write(
            repo.join(format!("{name}.rs")),
            format!("pub fn {name}() {{}}\n"),
        )
        .unwrap();
    }
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    let prompt = |text: &str| json!({"session_id":"shell-mode", "cwd":repo, "prompt":text});
    assert!(!cli(root, &["hook", "prompt"], Some(prompt("Inspect compute"))).is_empty());
    // Claude Code submits a `!command` turn as these blocks.
    let shell = "<bash-input>claude auth login</bash-input><bash-stdout>Opening browser \
        scope=org:create_api_key\nLogin successful.</bash-stdout><bash-stderr></bash-stderr>";
    assert!(cli(root, &["hook", "prompt"], Some(prompt(shell))).is_empty());
    assert_eq!(
        engram::agenthooks::pending_prompt(root, "shell-mode").as_deref(),
        Some("Inspect compute")
    );
    // So does a background task's completion notice.
    let notice = "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n\
        <summary>Background command \"create_api_key smoke\" completed (exit code 0)</summary>\n\
        </task-notification>";
    assert!(cli(root, &["hook", "prompt"], Some(prompt(notice))).is_empty());
    assert_eq!(
        engram::agenthooks::pending_prompt(root, "shell-mode").as_deref(),
        Some("Inspect compute")
    );
    let read = json!({"session_id":"shell-mode", "cwd":repo, "tool_name":"Read",
        "tool_input":{"file_path":"compute.rs"}});
    let context = cli(root, &["hook", "pretool"], Some(read));
    assert!(
        !context.contains("create_api_key"),
        "shell output must not steer retrieval: {context}"
    );
    // Text beyond the blocks is a request about the output.
    let question = format!("{shell}\nWhy did create_api_key appear?");
    cli(root, &["hook", "prompt"], Some(prompt(&question)));
    let pending = engram::agenthooks::pending_prompt(root, "shell-mode").unwrap();
    assert!(
        pending.contains("Why did create_api_key appear?"),
        "{pending}"
    );
    assert!(pending.contains("$ claude auth login"), "{pending}");
    assert!(
        !pending.contains("Opening browser"),
        "output is not intent: {pending}"
    );
    cli(root, &["daemon", "stop"], None);
}

#[test]
fn compaction_delivers_context_and_file_briefs_again() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    for name in ["compute", "auxiliary"] {
        std::fs::write(
            repo.join(format!("{name}.rs")),
            format!("pub fn {name}() {{}}\n"),
        )
        .unwrap();
    }
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    let prompt =
        |session: &str, text: &str| json!({"session_id":session, "cwd":repo, "prompt":text});
    let read = |session: &str| {
        json!({"session_id":session, "cwd":repo, "tool_name":"Read",
            "tool_input":{"file_path":"compute.rs"}})
    };
    let transcript = root.join("compacted.jsonl");
    std::fs::write(&transcript, "{}\n").unwrap();
    let start = |source: &str| {
        json!({"session_id":"compacted", "cwd":repo, "source":source,
            "transcript_path":transcript})
    };
    // An earlier session leaves an open episode for briefs to name.
    cli(
        root,
        &["hook", "prompt"],
        Some(prompt("earlier", "Inspect auxiliary")),
    );
    cli(root, &["hook", "pretool"], Some(read("earlier")));
    let earlier = engram::agenthooks::project_sessions(root, "earlier")
        .pop()
        .unwrap()
        .1;

    assert!(!cli(root, &["hook", "session-start"], Some(start("startup"))).is_empty());
    assert!(
        !cli(
            root,
            &["hook", "prompt"],
            Some(prompt("compacted", "Inspect compute"))
        )
        .is_empty()
    );
    assert!(
        cli(
            root,
            &["hook", "prompt"],
            Some(prompt("compacted", "Inspect compute"))
        )
        .is_empty()
    );
    assert!(!cli(root, &["hook", "pretool"], Some(read("compacted"))).is_empty());
    assert!(cli(root, &["hook", "pretool"], Some(read("compacted"))).is_empty());
    let own = engram::agenthooks::project_sessions(root, "compacted")
        .pop()
        .unwrap()
        .1;

    // A resumed conversation still holds what was delivered.
    assert!(cli(root, &["hook", "session-start"], Some(start("resume"))).is_empty());
    assert!(
        cli(
            root,
            &["hook", "prompt"],
            Some(prompt("compacted", "Inspect compute"))
        )
        .is_empty()
    );

    // Compaction drops it: brief once across both hook levels, without this
    // session's own episode, then deliver the pack and the file brief again.
    let brief = cli(root, &["hook", "session-start"], Some(start("compact")));
    assert!(brief.contains(earlier.task_id.as_str()), "{brief}");
    assert!(!brief.contains(own.task_id.as_str()), "{brief}");
    assert!(cli(root, &["hook", "session-start"], Some(start("compact"))).is_empty());
    // A second compaction within the same minute grows the transcript and is
    // briefed again.
    std::fs::write(&transcript, "{}\n{\"type\":\"summary\"}\n").unwrap();
    assert!(!cli(root, &["hook", "session-start"], Some(start("compact"))).is_empty());
    assert!(
        !cli(
            root,
            &["hook", "prompt"],
            Some(prompt("compacted", "Inspect compute"))
        )
        .is_empty()
    );
    let again = cli(root, &["hook", "pretool"], Some(read("compacted")));
    assert!(again.contains("compute.rs"), "{again}");
    assert!(cli(root, &["hook", "pretool"], Some(read("compacted"))).is_empty());
    cli(
        root,
        &["hook", "session-end"],
        Some(json!({"session_id":"compacted", "cwd":repo})),
    );
    let leftover = std::fs::read_dir(root.join("hook-sessions"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".brief-"))
        .collect::<Vec<_>>();
    assert!(
        leftover.is_empty(),
        "compaction claims outlive the session: {leftover:?}"
    );
    cli(root, &["daemon", "stop"], None);
}

#[test]
fn failed_and_empty_context_attempts_retry_and_keep_the_last_delivered_run() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("compute.rs"), "pub fn compute() {}\n").unwrap();
    std::fs::write(repo.join("auxiliary.rs"), "pub fn auxiliary() {}\n").unwrap();
    // The daemon retains its default 6000-token ceiling while each hook loads
    // the changed caller config. 7000 deterministically fails compilation.
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    let config_dir = root.join(".config/engram");
    std::fs::create_dir_all(&config_dir).unwrap();
    let budget = |tokens: u32| {
        std::fs::write(
            config_dir.join("config.toml"),
            format!("[context]\nmax_injected_tokens = {tokens}\nstable_prefix_tokens = 0\n"),
        )
        .unwrap();
    };
    let prompt =
        |session: &str, text: &str| json!({"session_id":session, "cwd":repo, "prompt":text});
    for (session, tokens) in [("empty-retry", 1), ("failed-retry", 7000)] {
        budget(tokens);
        assert!(
            cli(
                root,
                &["hook", "prompt"],
                Some(prompt(session, "Inspect compute"))
            )
            .is_empty()
        );
        budget(6000);
        assert!(
            !cli(
                root,
                &["hook", "prompt"],
                Some(prompt(session, "Inspect compute"))
            )
            .is_empty()
        );
        assert!(
            cli(
                root,
                &["hook", "prompt"],
                Some(prompt(session, "Inspect compute"))
            )
            .is_empty()
        );
    }
    cli(
        root,
        &["hook", "pretool"],
        Some(json!({"session_id":"failed-retry", "cwd":repo,
        "tool_name":"Read", "tool_input":{"file_path":"compute.rs"}})),
    );
    let (scope, session) = engram::agenthooks::project_sessions(root, "failed-retry")
        .pop()
        .unwrap();
    let previous = engram::agenthooks::context_run_id(root, &scope).unwrap();
    budget(7000);
    assert!(
        cli(
            root,
            &["hook", "prompt"],
            Some(prompt("failed-retry", "Inspect auxiliary"))
        )
        .is_empty()
    );
    assert_eq!(
        engram::agenthooks::pending_prompt(root, "failed-retry").as_deref(),
        Some("Inspect auxiliary")
    );
    assert_eq!(
        engram::agenthooks::context_run_id(root, &scope).unwrap(),
        previous
    );
    budget(6000);
    assert!(
        !cli(
            root,
            &["hook", "prompt"],
            Some(prompt("failed-retry", "Inspect auxiliary"))
        )
        .is_empty()
    );
    assert_ne!(
        engram::agenthooks::context_run_id(root, &scope).unwrap(),
        previous
    );
    assert_eq!(
        engram::agenthooks::load_session(root, &scope)
            .unwrap()
            .task_id,
        session.task_id
    );
    cli(root, &["daemon", "stop"], None);
}

#[test]
fn relative_tool_workdirs_route_paths_and_count_multi_repo_tools_once() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let one = root.join("one");
    let two = root.join("two");
    for (repo, name) in [(&one, "first_marker"), (&two, "second_marker")] {
        std::fs::create_dir(repo).unwrap();
        std::fs::write(repo.join("lib.rs"), format!("pub fn {name}() {{}}\n")).unwrap();
        std::fs::write(repo.join("actual.rs"), "pub fn actual() {}\n").unwrap();
        cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    }
    cli(
        root,
        &["hook", "prompt"],
        Some(json!({"session_id":"relative", "cwd":one, "prompt":"Inspect the implementation"})),
    );
    let read = json!({"session_id":"relative", "cwd":one, "tool_name":"Read", "tool_use_id":"relative-read",
        "tool_input":{"workdir":"../two", "file_path":"lib.rs"}});
    let output = cli(root, &["hook", "pretool"], Some(read.clone()));
    assert!(output.contains("second_marker"), "{output}");
    assert!(!output.contains("first_marker"), "{output}");
    assert!(output.contains("Indexed reference data") && output.contains("Verify current source"));
    cli(root, &["hook", "bash"], Some(read));
    cli(
        root,
        &["hook", "bash"],
        Some(json!({"session_id":"relative", "cwd":one,
        "tool_name":"Edit", "tool_use_id":"relative-edit", "tool_input":{"cwd":two,"file_path":"actual.rs"}})),
    );
    let sessions = engram::agenthooks::project_sessions(root, "relative");
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        engram::agenthooks::recent_paths(root, &sessions[0].0),
        ["actual.rs", "lib.rs"]
    );
    let context = cli(
        root,
        &["hook", "prompt"],
        Some(json!({"session_id":"relative", "cwd":two, "prompt":"continue"})),
    );
    assert!(context.contains("actual.rs"), "{context}");
    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-old\n+new\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
        one.join("lib.rs").display(),
        two.join("lib.rs").display()
    );
    let tool = json!({"session_id":"relative", "cwd":one,"tool_name":"apply_patch", "tool_use_id":"multi-repo-patch",
        "tool_input":{"command":patch}});
    cli(root, &["hook", "bash"], Some(tool.clone()));
    cli(root, &["hook", "bash"], Some(tool));
    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
    let totals: (i64, i64) = connection
        .query_row(
            "SELECT count(*),sum(tool_calls) FROM resource_usage",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        totals,
        (3, 3),
        "one usage sample per real tool, including a multi-repo patch"
    );
    assert_eq!(
        engram::agenthooks::project_sessions(root, "relative").len(),
        2
    );
    cli(root, &["daemon", "stop"], None);
}

#[test]
fn changed_paths_with_identical_context_preserve_the_emitted_receipt() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("compute.rs"), "pub fn compute() {}\n").unwrap();
    std::fs::write(repo.join("auxiliary.rs"), "pub fn auxiliary() {}\n").unwrap();
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    let prompt = json!({"session_id":"same-text", "cwd":repo, "prompt":"Inspect compute"});
    assert!(!cli(root, &["hook", "prompt"], Some(prompt.clone())).is_empty());
    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
    let project: String = connection
        .query_row("SELECT id FROM projects LIMIT 1", [], |row| row.get(0))
        .unwrap();
    let scope = engram::agenthooks::scoped_session_id(
        "same-text",
        &engram::ids::ProjectId::parse(&project).unwrap(),
    );
    let run_id = engram::agenthooks::context_run_id(root, &scope).unwrap();
    let request_id = engram::agenthooks::context_request_id(root, &scope).unwrap();
    // Newly observed paths without indexed knowledge leave the compiled source
    // map identical; neither attempt should reemit the previous text.
    for path in ["new-a.txt", "new-b.txt"] {
        let tool = json!({"session_id":"same-text", "cwd":repo, "tool_name":"Write",
            "tool_input":{"file_path":path}});
        assert!(cli(root, &["hook", "pretool"], Some(tool.clone())).is_empty());
        assert_eq!(
            engram::agenthooks::context_run_id(root, &scope).unwrap(),
            run_id
        );
        assert_eq!(
            engram::agenthooks::context_request_id(root, &scope).unwrap(),
            request_id
        );
        let count: i64 = connection
            .query_row("SELECT count(*) FROM context_runs", [], |row| row.get(0))
            .unwrap();
        assert!(cli(root, &["hook", "pretool"], Some(tool)).is_empty());
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM context_runs", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            count + 1,
            "unchanged inputs must recheck source revisions and mutable knowledge"
        );
    }
    assert_eq!(
        engram::agenthooks::recent_paths(root, &scope),
        ["new-b.txt", "new-a.txt"]
    );
    let served = || {
        connection
            .query_row(
                "SELECT count(*) FROM usage_events WHERE usage_kind='context_served'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
    };
    assert_eq!(
        served(),
        1,
        "suppressed compilations must not create context_served rows"
    );
    assert_eq!(connection.query_row(
        "SELECT count(*) FROM usage_events WHERE usage_kind='context_served' AND context_run_id<>?1",
        [run_id.as_str()], |row| row.get::<_, i64>(0)).unwrap(), 0);
    let changed = cli(
        root,
        &["hook", "prompt"],
        Some(json!({"session_id":"same-text", "cwd":repo, "prompt":"Inspect auxiliary"})),
    );
    assert!(changed.contains("auxiliary.rs"), "{changed}");
    let changed_run = engram::agenthooks::context_run_id(root, &scope).unwrap();
    assert_ne!(changed_run, run_id);
    assert_eq!(
        served(),
        2,
        "changed context is acknowledged after successful emission"
    );
    assert_eq!(connection.query_row(
        "SELECT count(*) FROM usage_events WHERE usage_kind='context_served' AND context_run_id=?1",
        [changed_run.as_str()], |row| row.get::<_, i64>(0)).unwrap(), 1);
    cli(root, &["daemon", "stop"], None);
}

#[test]
fn reordered_source_maps_stay_silent_but_source_revisions_and_facts_refresh() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    for name in ["alpha", "beta"] {
        std::fs::write(
            repo.join(format!("{name}.rs")),
            format!("pub fn compute_{name}() {{}}\n"),
        )
        .unwrap();
    }
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    let prompt = json!({"session_id":"source-order", "cwd":repo, "prompt":"Inspect compute"});
    let first = cli(root, &["hook", "prompt"], Some(prompt.clone()));
    assert!(first.contains("alpha.rs") && first.contains("beta.rs"));
    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
    let project: String = connection
        .query_row("SELECT id FROM projects LIMIT 1", [], |row| row.get(0))
        .unwrap();
    let scope = engram::agenthooks::scoped_session_id(
        "source-order",
        &engram::ids::ProjectId::parse(&project).unwrap(),
    );
    let first_run = engram::agenthooks::context_run_id(root, &scope).unwrap();
    let first_dynamic: String = connection
        .query_row(
            "SELECT dynamic_context FROM context_runs WHERE id=?1",
            [first_run.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    let first_path =
        if first_dynamic.find("alpha.rs").unwrap() < first_dynamic.find("beta.rs").unwrap() {
            "beta.rs"
        } else {
            "alpha.rs"
        };
    engram::agenthooks::remember_paths(root, &scope, &[first_path.to_owned()]).unwrap();
    assert!(cli(root, &["hook", "prompt"], Some(prompt.clone())).is_empty());
    let reordered: String = connection
        .query_row(
            "SELECT dynamic_context FROM context_runs ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(
        first_dynamic, reordered,
        "the fixture must exercise an actual ranking-order change"
    );
    assert_eq!(
        engram::agenthooks::context_run_id(root, &scope),
        Some(first_run)
    );

    // A source can change while its rendered label/path and hook inputs stay fixed.
    for (column, value) in [
        ("current_digest", "new-content"),
        ("source_ref", "new-revision"),
    ] {
        connection
            .execute(
                &format!(
                    "UPDATE nodes SET {column}=?1 WHERE project_id=?2 AND file_path='alpha.rs'"
                ),
                [value, project.as_str()],
            )
            .unwrap();
        let refreshed = cli(root, &["hook", "prompt"], Some(prompt.clone()));
        assert!(
            refreshed.contains("alpha.rs"),
            "{column} must refresh: {refreshed}"
        );
        assert!(cli(root, &["hook", "prompt"], Some(prompt.clone())).is_empty());
    }
    cli(
        root,
        &[
            "fact",
            "set",
            "storage",
            "SQLite",
            "--path",
            repo.to_str().unwrap(),
        ],
        None,
    );
    assert!(cli(root, &["hook", "prompt"], Some(prompt.clone())).contains("SQLite"));
    assert!(cli(root, &["hook", "prompt"], Some(prompt)).is_empty());
    assert!(
        !cli(
            root,
            &["hook", "prompt"],
            Some(
                json!({"session_id":"another-source-task", "cwd":repo,"prompt":"Inspect compute"})
            )
        )
        .is_empty()
    );
    cli(root, &["daemon", "stop"], None);
}

#[test]
fn failed_hook_output_keeps_the_last_receipt_and_retries_the_same_pack() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("lib.rs"), "pub fn compute() {}\n").unwrap();
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    let prompt = json!({"session_id":"failed-output", "cwd":repo, "prompt":"Inspect compute"});
    assert!(!cli(root, &["hook", "prompt"], Some(prompt.clone())).is_empty());
    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
    let project: String = connection
        .query_row("SELECT id FROM projects LIMIT 1", [], |row| row.get(0))
        .unwrap();
    let scope = engram::agenthooks::scoped_session_id(
        "failed-output",
        &engram::ids::ProjectId::parse(&project).unwrap(),
    );
    let previous = engram::agenthooks::context_run_id(root, &scope).unwrap();
    cli(
        root,
        &[
            "fact",
            "set",
            "storage",
            "SQLite",
            "--path",
            repo.to_str().unwrap(),
        ],
        None,
    );
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("engram"))
        .env("HOME", root)
        .args([
            "--db",
            root.join("brain.sqlite").to_str().unwrap(),
            "--socket",
            root.join("engramd.sock").to_str().unwrap(),
            "hook",
            "prompt",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Close the reader before compilation finishes: emitting the complete JSON fails.
    drop(child.stdout.take());
    child
        .stdin
        .take()
        .unwrap()
        .write_all(prompt.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "hooks fail open: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        engram::agenthooks::context_run_id(root, &scope),
        Some(previous.clone())
    );
    assert!(cli(root, &["hook", "prompt"], Some(prompt.clone())).contains("SQLite"));
    assert_ne!(
        engram::agenthooks::context_run_id(root, &scope),
        Some(previous)
    );
    assert!(cli(root, &["hook", "prompt"], Some(prompt)).is_empty());
    cli(root, &["daemon", "stop"], None);
}

#[test]
fn larger_prompt_budget_refreshes_a_previously_partial_context() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    let config_dir = root.join(".config/engram");
    std::fs::create_dir_all(&config_dir).unwrap();
    let budget = |tokens: u32| {
        std::fs::write(
            config_dir.join("config.toml"),
            format!("[context]\nmax_injected_tokens = {tokens}\nstable_prefix_tokens = 0\n"),
        )
        .unwrap();
    };
    budget(6000);
    for index in 0..12 {
        let filename = format!("needle_{index:02}_{}.rs", "implementation_".repeat(6));
        std::fs::write(
            repo.join(filename),
            format!("pub fn needle_{index}() {{}}\n"),
        )
        .unwrap();
    }
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    let prompt = json!({"session_id":"budget-refresh", "cwd":repo, "prompt":"Inspect needle"});
    budget(256);
    let partial = cli(root, &["hook", "prompt", "--codex"], Some(prompt.clone()));
    assert!(!partial.is_empty());
    assert!(cli(root, &["hook", "prompt", "--codex"], Some(prompt.clone())).is_empty());
    let connection = Connection::open(root.join("brain.sqlite")).unwrap();
    let project: String = connection
        .query_row("SELECT id FROM projects LIMIT 1", [], |row| row.get(0))
        .unwrap();
    let scope = engram::agenthooks::scoped_session_id(
        "budget-refresh",
        &engram::ids::ProjectId::parse(&project).unwrap(),
    );
    let initial_run = engram::agenthooks::context_run_id(root, &scope).unwrap();
    budget(6000);
    let full = cli(root, &["hook", "prompt", "--codex"], Some(prompt.clone()));
    assert!(
        full.len() > partial.len(),
        "larger budget must make the omitted pointers available"
    );
    assert_ne!(
        engram::agenthooks::context_run_id(root, &scope).unwrap(),
        initial_run
    );
    assert!(cli(root, &["hook", "prompt", "--codex"], Some(prompt)).is_empty());
    cli(root, &["daemon", "stop"], None);
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

/// Claude Code reports Bash exit status through its event, not a field:
/// `PostToolUse` for a completed command and `PostToolUseFailure` with
/// "Exit code N" otherwise. Hook evidence must reach the task the agent
/// declared and stay eligible after the commit watcher re-indexes the bytes
/// the run observed; the agent's own copy of the run keeps the time rule.
#[test]
#[allow(clippy::too_many_lines)]
fn claude_code_verification_becomes_eligible_evidence_for_the_declared_task() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("lib.rs"), "pub fn compute() -> u32 { 1 }\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "lib.rs"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    let session = "claude-evidence";
    cli(
        root,
        &["hook", "prompt"],
        Some(json!({"session_id":session,"cwd":repo,"prompt":"Make compute return 2"})),
    );
    let event = |tool: &str, id: &str, input: Value, response: Value| {
        json!({"session_id":session,"cwd":repo,"hook_event_name":"PostToolUse",
            "tool_name":tool,"tool_use_id":id,"tool_input":input,"tool_response":response})
    };
    let task: Value = serde_json::from_str(&cli(
        root,
        &[
            "task",
            "start",
            "Make compute return 2",
            "--path",
            repo.to_str().unwrap(),
            "--json",
        ],
        None,
    ))
    .unwrap();
    let task_id = task["id"].as_str().unwrap().to_owned();
    // The MCP result reaches the hook as the tool's JSON text.
    cli(
        root,
        &["hook", "bash"],
        Some(event(
            "mcp__engram__engram_task_start",
            "toolu_start",
            json!({"project_id":task["project_id"],"prompt":"Make compute return 2"}),
            json!(task.to_string()),
        )),
    );
    std::fs::write(repo.join("lib.rs"), "pub fn compute() -> u32 { 2 }\n").unwrap();
    cli(
        root,
        &["hook", "bash"],
        Some(event(
            "Edit",
            "toolu_edit",
            json!({"file_path":repo.join("lib.rs")}),
            json!({"filePath":repo.join("lib.rs")}),
        )),
    );
    let tests = |command: &str, id: &str| {
        event(
            "Bash",
            id,
            json!({"command":command,"description":"Run tests"}),
            json!({"stdout":"running 1 test\ntest compute_returns_two ... ok\n\ntest result: ok. 1 passed; 0 failed",
                "stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false}),
        )
    };
    cli(
        root,
        &["hook", "bash"],
        Some(
            json!({"session_id":session,"cwd":repo,"hook_event_name":"PostToolUseFailure",
            "tool_name":"Bash","tool_use_id":"toolu_fail","tool_input":{"command":"cargo test"},
            "error":"Exit code 101\ntest compute_returns_two ... FAILED","is_interrupt":false}),
        ),
    );
    // `| tail` replaces the test's exit status with tail's: not an observation.
    cli(
        root,
        &["hook", "bash"],
        Some(tests("cargo test 2>&1 | tail -5", "toolu_masked")),
    );
    for _ in 0..2 {
        // User- and project-level hooks can both fire for one call.
        cli(
            root,
            &["hook", "bash"],
            Some(tests("cargo test", "toolu_pass")),
        );
    }
    let c = Connection::open(root.join("brain.sqlite")).unwrap();
    let evidence = || -> Vec<(String, String, i64, String)> {
        c.prepare(
            "SELECT id,task_id,exit_code,metadata_json FROM evidence ORDER BY observed_at_ms,rowid",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    };
    let rows = evidence();
    assert_eq!(
        rows.iter().map(|row| row.2).collect::<Vec<_>>(),
        vec![101, 0],
        "the masked run and the repeated fire are not new observations"
    );
    assert!(
        rows.iter().all(|row| row.1 == task_id),
        "evidence goes to the task declared in this session"
    );
    let pass: Value = serde_json::from_str(&rows[1].3).unwrap();
    assert_eq!(pass["source"], "claude-code-hook");
    assert_eq!(pass["exit_status"], "success_event");
    assert_eq!(pass["tool_use_id"], "toolu_pass");
    let observed_digest = pass["source_digests"]["lib.rs"]
        .as_str()
        .unwrap()
        .to_owned();
    // The agent completes its task and also pastes its own copy of the run.
    let completion = root.join("complete.json");
    std::fs::write(
        &completion,
        json!({"operation":"complete","task_id":task_id,"outcome":"success",
            "summary":"compute returns 2","files":["lib.rs"],
            "verifications":[{"kind":"test_result","strength":"deterministic","command":"cargo test",
                "exit_code":0,"artifact":"test result: ok. 1 passed; 0 failed"}],
            "lessons":[{"claim":"compute returns 2 for every caller","rationale":"Observed test run"}]})
        .to_string(),
    )
    .unwrap();
    let completed: Value = serde_json::from_str(&cli(
        root,
        &["product", "lesson", completion.to_str().unwrap()],
        None,
    ))
    .unwrap();
    let review_evidence = completed["reviews"][0]["evidence_ids"].as_array().unwrap();
    assert_eq!(
        review_evidence.len(),
        2,
        "the hook observation and the agent's copy, never the failure"
    );
    assert!(review_evidence.contains(&json!(rows[1].0)));
    cli(
        root,
        &["hook", "bash"],
        Some(event(
            "mcp__engram__engram_complete_task",
            "toolu_done",
            json!({"task_id":task_id}),
            json!(completed.to_string()),
        )),
    );
    cli(
        root,
        &["hook", "bash"],
        Some(tests("cargo test --lib", "toolu_after")),
    );
    let after = evidence();
    assert_eq!(after.len(), 4);
    assert_ne!(
        after[3].1, task_id,
        "after completion, runs return to the session's own episode"
    );
    // Commit and re-index: the touched file's index stamp now follows the runs.
    git(&repo, &["commit", "-q", "-am", "compute returns 2"]);
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    let (indexed_digest, indexed_at): (String, i64) = c
        .query_row(
            "SELECT content_digest,indexed_at_ms FROM indexed_files WHERE path='lib.rs'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let observed_at: i64 = c
        .query_row(
            "SELECT observed_at_ms FROM evidence WHERE id=?1",
            [&rows[1].0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(indexed_digest, observed_digest);
    assert!(indexed_at > observed_at);
    cli(
        root,
        &["hook", "session-end"],
        Some(json!({"session_id":session,"cwd":repo})),
    );
    cli(root, &["daemon", "stop"], None);
    drop(c);
    let database = engram::storage::Database::open(root.join("brain.sqlite")).unwrap();
    let packets = engram::judge::JudgeStore::new(database)
        .prepare_pending(&engram::judge::JudgeConfig {
            enabled: true,
            model: "fixture-model".to_owned(),
            ..engram::judge::JudgeConfig::default()
        })
        .unwrap();
    assert_eq!(packets.len(), 1);
    let eligibility: Vec<(String, bool)> = packets[0]
        .evidence
        .iter()
        .map(|item| (item.id.as_str().to_owned(), item.eligible))
        .collect();
    assert!(
        eligibility.contains(&(rows[1].0.clone(), true)),
        "{eligibility:?}"
    );
    assert_eq!(
        eligibility.iter().filter(|(_, eligible)| *eligible).count(),
        1,
        "the agent's own copy keeps the time rule"
    );
    assert!(
        packets[0].blocked_reason.is_none(),
        "{:?}",
        packets[0].blocked_reason
    );
}

/// A verification whose result the hook cannot observe leaves the session's
/// outcome unknown, so an earlier pass must not close the episode as success.
#[test]
fn unobserved_verification_after_a_pass_never_closes_the_episode_as_success() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("lib.rs"), "pub fn compute() -> u32 { 1 }\n").unwrap();
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    let passed = json!({"stdout":"test result: ok. 1 passed","stderr":"",
        "interrupted":false,"isImage":false});
    let later_runs = [
        // Interrupted by the user.
        Some(
            json!({"hook_event_name":"PostToolUseFailure","tool_name":"Bash",
            "tool_input":{"command":"cargo test"},
            "error":"Tool execution was interrupted","is_interrupt":true}),
        ),
        // A failure reported without an exit code.
        Some(
            json!({"hook_event_name":"PostToolUseFailure","tool_name":"Bash",
            "tool_input":{"command":"cargo test"},"error":"Command timed out","is_interrupt":false}),
        ),
        // `| tail` reports tail's status, not the tests'.
        Some(json!({"hook_event_name":"PostToolUse","tool_name":"Bash",
            "tool_input":{"command":"cargo test 2>&1 | tail -3"},"tool_response":passed})),
        // Control: the observed pass is the final result.
        None,
    ];
    for (index, later) in later_runs.into_iter().enumerate() {
        let session = format!("unknown-result-{index}");
        cli(
            root,
            &["hook", "prompt"],
            Some(json!({"session_id":session,"cwd":repo,"prompt":"Check compute"})),
        );
        cli(
            root,
            &["hook", "bash"],
            Some(
                json!({"session_id":session,"cwd":repo,"hook_event_name":"PostToolUse",
                "tool_name":"Bash","tool_input":{"command":"cargo test"},"tool_response":passed}),
            ),
        );
        if let Some(mut later) = later {
            later["session_id"] = json!(session);
            later["cwd"] = json!(repo);
            cli(root, &["hook", "bash"], Some(later));
        }
        cli(
            root,
            &["hook", "session-end"],
            Some(json!({"session_id":session,"cwd":repo})),
        );
    }
    let c = Connection::open(root.join("brain.sqlite")).unwrap();
    let statuses: Vec<String> = c
        .prepare("SELECT status FROM tasks ORDER BY started_at_ms,rowid")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        statuses,
        ["abandoned", "abandoned", "abandoned", "success"],
        "an unknown final result is never success"
    );
    let recorded_passes: i64 = c
        .query_row("SELECT count(*) FROM evidence WHERE exit_code=0", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(recorded_passes, 4, "each observed pass is still recorded");
    cli(root, &["daemon", "stop"], None);
}

/// The `additionalContext` of a Claude Code tool hook's output, or "" when the
/// hook printed nothing.
fn hook_context(output: &str) -> String {
    if output.trim().is_empty() {
        return String::new();
    }
    let result: Value = serde_json::from_str(output).unwrap();
    result["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn pretool_file_briefs_carry_checked_spans_once_per_file() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let config_dir = root.join(".config/engram");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[context]\nlarge_file_bytes = 1024\n",
    )
    .unwrap();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    let source = (0..60).fold(String::new(), |mut source, n| {
        writeln!(source, "pub fn step_{n}() -> u32 {{\n    {n}\n}}\n").unwrap();
        source
    });
    std::fs::write(repo.join("engine.rs"), &source).unwrap();
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    let read = |session: &str| {
        json!({"session_id":session,"cwd":repo,"tool_name":"Read",
            "tool_input":{"file_path":repo.join("engine.rs")}})
    };
    // A tool call opens an episode only after the session's prompt.
    let pretool = |session: &str| {
        cli(
            root,
            &["hook", "prompt"],
            Some(json!({"session_id":session,"cwd":repo,"prompt":"Inspect the engine steps"})),
        );
        hook_context(&cli(root, &["hook", "pretool"], Some(read(session))))
    };

    let first = pretool("spans");
    assert!(
        // The order follows the task (see the next test); spans are checked.
        first.contains("Defines (line spans")
            && first.contains("step_0 (Function) 1-3, step_1 (Function) 5-7, "),
        "{first}"
    );
    let size_line = format!(
        "File size: {} KB. The line spans locate the listed definitions.\n",
        source.len().div_ceil(1024)
    );
    assert!(first.contains(&size_line), "{first}");
    // Suppression is unchanged: one brief per file per conversation.
    assert!(
        !pretool("spans").contains("Indexed reference data: engine.rs"),
        "a briefed file is not briefed again"
    );

    // An edit after indexing leaves the plain list and the current size.
    let edited = format!("// edited\n{source}");
    std::fs::write(repo.join("engine.rs"), &edited).unwrap();
    let stale = pretool("stale");
    assert!(
        stale.contains("step_0 (Function), step_1 (Function), "),
        "{stale}"
    );
    assert!(
        stale.contains(&format!("File size: {} KB.\n", edited.len().div_ceil(1024))),
        "{stale}"
    );
    assert!(!stale.contains("line spans"), "{stale}");
    cli(root, &["daemon", "stop"], None);
}

#[test]
fn pretool_file_brief_lists_the_task_named_definition_first() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    let source = (0..60).fold(String::new(), |mut source, n| {
        writeln!(source, "pub fn step_{n:02}() -> u32 {{\n    {n}\n}}\n").unwrap();
        source
    });
    std::fs::write(repo.join("engine.rs"), &source).unwrap();
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    cli(
        root,
        &["hook", "prompt"],
        Some(json!({"session_id":"named","cwd":repo,"prompt":"Make step_57 return a constant"})),
    );
    let read = json!({"session_id":"named","cwd":repo,"tool_name":"Read",
        "tool_input":{"file_path":repo.join("engine.rs")}});
    let brief = hook_context(&cli(root, &["hook", "pretool"], Some(read.clone())));
    assert!(
        brief.contains(
            "Defines (line spans, task matches first): step_57 (Function) 229-231, step_00 (Function) 1-3, "
        ),
        "{brief}"
    );
    assert_eq!(brief.matches("(Function)").count(), 20, "{brief}");
    // Within one task the file is briefed once.
    let repeat = hook_context(&cli(root, &["hook", "pretool"], Some(read)));
    assert!(!repeat.contains("Defines"), "{repeat}");
    cli(root, &["daemon", "stop"], None);
}

#[test]
fn a_new_task_rebriefs_a_file_only_when_its_listed_sources_change() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    let source = (0..30).fold(String::new(), |mut source, n| {
        writeln!(source, "pub fn step_{n:02}() -> u32 {{\n    {n}\n}}\n").unwrap();
        source
    });
    std::fs::write(repo.join("engine.rs"), &source).unwrap();
    cli(root, &["index", repo.to_str().unwrap(), "--json"], None);
    let read = json!({"session_id":"tasks","cwd":repo,"tool_name":"Read",
        "tool_input":{"file_path":repo.join("engine.rs")}});
    let task = |prompt: &str| {
        cli(
            root,
            &["hook", "prompt"],
            Some(json!({"session_id":"tasks","cwd":repo,"prompt":prompt})),
        );
    };
    let brief = || {
        let context = hook_context(&cli(root, &["hook", "pretool"], Some(read.clone())));
        context
            .find("# Indexed reference data: engine.rs")
            .map(|start| context[start..].to_owned())
    };

    task("Make step_05 return a constant");
    let first = brief().expect("first brief");
    assert!(
        first.contains("task matches first): step_05 (Function) 21-23, step_00 (Function) 1-3"),
        "{first}"
    );
    assert_eq!(brief(), None, "same task: no repeat");
    // The same 20 definitions in another order are not briefed again.
    task("Make step_10 return a constant");
    assert_eq!(brief(), None, "reordering only: no re-brief");
    // A task naming a definition outside the delivered list re-briefs once.
    task("Make step_27 return a constant");
    let rebrief = brief().expect("re-brief");
    assert!(
        rebrief.contains("task matches first): step_27 (Function) 109-111, step_00 (Function) 1-3"),
        "{rebrief}"
    );
    assert_eq!(brief(), None, "once per task");
    cli(root, &["daemon", "stop"], None);
}
