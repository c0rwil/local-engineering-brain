//! Status notes, end to end through the hook binary and a temporary daemon:
//! when the shell hides a recognized verification's exit status, the
//! post-tool hook says so once per conversation and cause in its own output,
//! never in brain-off mode, and counts every recognized run for
//! `engram effectiveness`. Payloads use the real client shapes.
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

/// These tests spawn CLIs that auto-start daemons. On macOS a child spawned
/// while another thread creates its stdio pipes can inherit them, so the
/// tests run one at a time (see `agent_integrations.rs`).
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

const NOTE: &str =
    "# Engram verification note (reference data)\nEngram did not record this verification's result";

/// Run the CLI with `ENGRAM_MODE` unset unless `mode` names it.
fn cli(root: &Path, args: &[&str], input: Option<&Value>, mode: Option<&str>) -> String {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("engram"));
    command
        .env("HOME", root)
        .env_remove("ENGRAM_MODE")
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
    if let Some(mode) = mode {
        command.env("ENGRAM_MODE", mode);
    }
    let mut child = command.spawn().expect("cli");
    let mut stdin = child.stdin.take().expect("stdin");
    if let Some(input) = input {
        stdin
            .write_all(input.to_string().as_bytes())
            .expect("payload");
    }
    drop(stdin);
    let output = child.wait_with_output().expect("output");
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf8")
}

struct Fixture {
    temp: TempDir,
    _serial: std::sync::MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Self {
        let serial = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fixture = Self {
            temp: TempDir::new().unwrap(),
            _serial: serial,
        };
        let repo = fixture.repo();
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join("lib.rs"), "pub fn compute() -> u32 { 1 }\n").unwrap();
        fixture.cli(&["index", repo.to_str().unwrap(), "--json"], None);
        fixture
    }

    fn root(&self) -> &Path {
        self.temp.path()
    }

    fn repo(&self) -> PathBuf {
        self.root().join("repo")
    }

    fn cli(&self, args: &[&str], input: Option<&Value>) -> String {
        cli(self.root(), args, input, None)
    }

    fn prompt(&self, args: &[&str], session: &str) {
        let mut args = args.to_vec();
        args.insert(0, "prompt");
        args.insert(0, "hook");
        self.cli(
            &args,
            Some(&json!({"session_id":session,"cwd":self.repo(),"prompt":"Make compute return 2"})),
        );
    }

    /// A Claude Code Bash call that passed (`PostToolUse`) or failed
    /// (`PostToolUseFailure`, exit code in its error text); returns the
    /// hook's stdout.
    fn claude_bash(
        &self,
        session: &str,
        id: &str,
        command: &str,
        exit: i32,
        mode: Option<&str>,
    ) -> String {
        let mut payload = if exit == 0 {
            json!({"session_id":session,"cwd":self.repo(),
                "hook_event_name":"PostToolUse","tool_name":"Bash","tool_use_id":id,
                "tool_input":{"command":command},
                "tool_response":{"stdout":"test result: ok","stderr":"","interrupted":false,"isImage":false}})
        } else {
            json!({"session_id":session,"cwd":self.repo(),
                "hook_event_name":"PostToolUseFailure","tool_name":"Bash","tool_use_id":id,
                "tool_input":{"command":command},
                "error":format!("Exit code {exit}\ntest result: FAILED"),"is_interrupt":false})
        };
        if let Some(agent) = id.strip_prefix("agent-") {
            payload["agent_id"] = json!(agent);
            payload["agent_type"] = json!("general-purpose");
        }
        cli(self.root(), &["hook", "bash"], Some(&payload), mode)
    }

    fn db(&self) -> Connection {
        Connection::open(self.root().join("brain.sqlite")).unwrap()
    }

    /// Ledger rows (cause, note delivered), oldest first, once `expected`
    /// have committed: the daemon acknowledges them before they are written.
    fn ledger(&self, expected: usize) -> Vec<(Option<String>, i64)> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let rows: Vec<(Option<String>, i64)> = self
                .db()
                .prepare(
                    "SELECT hidden_reason, note_delivered FROM verification_observations
                     ORDER BY observed_at_ms, rowid",
                )
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            if rows.len() >= expected {
                assert_eq!(rows.len(), expected, "{rows:?}");
                return rows;
            }
            assert!(
                Instant::now() < deadline,
                "expected {expected} rows: {rows:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = cli(self.root(), &["daemon", "stop"], None, None);
    }
}

/// The note a Claude Code or Codex post-tool hook printed, from its
/// documented `hookSpecificOutput.additionalContext` envelope.
fn hook_note(stdout: &str, event: &str) -> String {
    let output: Value =
        serde_json::from_str(stdout.trim()).unwrap_or_else(|error| panic!("{error}: {stdout}"));
    let specific = &output["hookSpecificOutput"];
    assert_eq!(specific["hookEventName"], event, "{stdout}");
    let note = specific["additionalContext"].as_str().expect("context");
    assert!(note.starts_with(NOTE), "{note}");
    assert!(note.len() <= 900, "{note}");
    note.to_owned()
}

fn share(report: &Value, group: &str) -> (i64, i64) {
    let share = &report["verification_status"][group];
    (
        share["runs"].as_i64().unwrap(),
        share["status_bearing"].as_i64().unwrap(),
    )
}

#[test]
fn claude_code_gets_one_note_per_conversation_and_cause() {
    let f = Fixture::new();
    f.prompt(&[], "notes");
    let first = f.claude_bash("notes", "t1", "cargo test 2>&1 | tail -5", 0, None);
    let note = hook_note(&first, "PostToolUse");
    assert!(
        note.contains("a pipe reports its last command's exit status"),
        "{note}"
    );
    assert!(
        note.contains("`set -o pipefail; cargo test 2>&1 | tail -5`"),
        "{note}"
    );
    // The same cause again, in another command: no repeat.
    assert_eq!(
        f.claude_bash("notes", "t2", "cargo clippy 2>&1 | grep warning", 1, None),
        ""
    );
    // A different cause gets its own note, on the event that reported it
    // (here `grep` found nothing and failed the call).
    let sequence = f.claude_bash(
        "notes",
        "t3",
        "cargo build; cargo test 2>&1 | grep FAILED",
        1,
        None,
    );
    let sequence = hook_note(&sequence, "PostToolUseFailure");
    assert!(
        sequence.contains("joined by `;` or a newline, sets the exit status"),
        "{sequence}"
    );
    assert!(
        sequence.contains("run `set -o pipefail; cargo build && cargo test 2>&1 | tail -n 40`"),
        "{sequence}"
    );
    // A status-bearing run records evidence and needs no note.
    assert_eq!(f.claude_bash("notes", "t4", "cargo test", 0, None), "");
    // A subagent thread has its own context, so it gets its own note.
    hook_note(
        &f.claude_bash("notes", "agent-worker-1", "cargo test | tail", 0, None),
        "PostToolUse",
    );
    let evidence: i64 = f
        .db()
        .query_row("SELECT count(*) FROM evidence", [], |row| row.get(0))
        .unwrap();
    assert_eq!(evidence, 1, "only the status-bearing run is evidence");
    let pipe = Some("pipe".to_owned());
    assert_eq!(
        f.ledger(5),
        vec![
            (pipe.clone(), 1),
            (pipe.clone(), 0),
            (Some("sequence".to_owned()), 1),
            (None, 0),
            (pipe, 1),
        ]
    );
    let report: Value =
        serde_json::from_str(&f.cli(&["effectiveness", "--json"], None)).expect("report");
    let status = &report["verification_status"];
    assert_eq!(
        (
            status["notes"].as_i64(),
            status["noted_conversations"].as_i64()
        ),
        (Some(3), Some(2))
    );
    // Each conversation splits at its first note; the noted runs come first.
    assert_eq!(share(&report, "runs"), (5, 1));
    assert_eq!(share(&report, "before_note"), (2, 0));
    assert_eq!(share(&report, "after_note"), (3, 1));
    assert_eq!(share(&report, "without_note"), (0, 0));
    assert_eq!(status["hidden_by_reason"], json!({"pipe":3,"sequence":1}));
    assert_eq!(
        report["projects"][0]["verification_status"]["runs"],
        status["runs"]
    );
    let text = f.cli(&["effectiveness"], None);
    assert!(
        text.contains("Verification runs with a recordable exit status: 1 of 5 (20.0%)"),
        "{text}"
    );
}

#[test]
fn brain_off_mode_gives_no_note() {
    let f = Fixture::new();
    f.prompt(&[], "off");
    let command = "cargo test 2>&1 | tail -5";
    assert_eq!(f.claude_bash("off", "t1", command, 0, Some("off")), "");
    // Off mode consumed nothing: the same session with notes on gets one.
    hook_note(
        &f.claude_bash("off", "t2", command, 0, Some("brain")),
        "PostToolUse",
    );
    let pipe = Some("pipe".to_owned());
    assert_eq!(f.ledger(2), vec![(pipe.clone(), 0), (pipe, 1)]);
}

#[test]
fn codex_post_tool_use_carries_the_note() {
    let f = Fixture::new();
    f.prompt(&["--codex"], "codex-notes");
    let bash = |id: &str, command: &str, exit: i32| {
        f.cli(
            &["hook", "bash", "--codex"],
            Some(
                &json!({"session_id":"codex-notes","turn_id":"turn-1","cwd":f.repo(),
                "hook_event_name":"PostToolUse","model":"gpt-test","tool_name":"Bash",
                "tool_use_id":id,"tool_input":{"command":command},
                "tool_response":{"exit_code":exit,"stdout":"test result: ok"}}),
            ),
        )
    };
    let note = hook_note(&bash("c1", "cargo test -q || true", 0), "PostToolUse");
    assert!(note.contains("`||` replaces a failure"), "{note}");
    assert!(note.contains("run `cargo test -q`"), "{note}");
    assert_eq!(bash("c2", "cargo build || echo failed", 0), "");
    assert_eq!(bash("c3", "cargo test", 0), "");
    let or = Some("or".to_owned());
    assert_eq!(f.ledger(3), vec![(or.clone(), 1), (or, 0), (None, 0)]);
}

/// Codex 0.155 runs no hook for a code-mode `exec` cell and reports each
/// shell command the cell runs as its own `Bash` call, like a direct
/// `exec_command`: `tool_input.command`, and the output text as
/// `tool_response`, without an exit code. Those runs get notes and ledger
/// rows, but no status is inferred, so none becomes evidence and the episode
/// cannot close as a success.
#[test]
fn codex_code_mode_commands_are_noted_but_never_inferred_to_pass() {
    let f = Fixture::new();
    f.prompt(&["--codex"], "codex-code-mode");
    let post = |tool: &str, id: &str, command: &str| {
        f.cli(
            &["hook", "bash", "--codex"],
            Some(
                &json!({"session_id":"codex-code-mode","turn_id":"turn-1","cwd":f.repo(),
                "hook_event_name":"PostToolUse","model":"gpt-test","permission_mode":"default",
                "transcript_path":null,"tool_name":tool,"tool_use_id":id,
                "tool_input":{"command":command},"tool_response":"test result: ok"}),
            ),
        )
    };
    let note = hook_note(
        &post("Bash", "exec-1", "cargo test 2>&1 | tail -5"),
        "PostToolUse",
    );
    assert!(
        note.contains("`set -o pipefail; cargo test 2>&1 | tail -5`"),
        "{note}"
    );
    assert_eq!(post("Bash", "exec-2", "cargo test"), "");
    // The cell's JavaScript, were a build to report it, is not shell text.
    let cell = "const r = await tools.exec_command({ cmd: \"cargo test || true\" });";
    assert_eq!(post("exec", "cell-1", cell), "");
    let pipe = Some("pipe".to_owned());
    assert_eq!(f.ledger(2), vec![(pipe, 1), (None, 0)]);
    let evidence: i64 = f
        .db()
        .query_row("SELECT count(*) FROM evidence", [], |row| row.get(0))
        .unwrap();
    assert_eq!(evidence, 0, "output text is not an exit status");
    f.cli(
        &["hook", "session-end", "--codex"],
        Some(&json!({"session_id":"codex-code-mode","cwd":f.repo()})),
    );
    let status: String = f
        .db()
        .query_row(
            "SELECT status FROM tasks WHERE session_id='codex-code-mode'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "abandoned");
}

#[test]
fn cursor_shell_results_carry_the_note_in_their_single_response() {
    let f = Fixture::new();
    let base = |event: &str, fields: &Value| {
        let mut payload = json!({"conversation_id":"cursor-notes","session_id":"cursor-notes",
            "generation_id":"gen-1","model":"test-model","hook_event_name":event,
            "cursor_version":"3.17.19","workspace_roots":[f.repo()],"user_email":null,
            "transcript_path":null});
        for (key, value) in fields.as_object().unwrap() {
            payload[key] = value.clone();
        }
        payload
    };
    let hook = |event: &str, payload: &Value| -> Value {
        let stdout = f.cli(&["hook", event, "--cursor"], Some(payload));
        let mut lines = stdout.lines();
        let response = serde_json::from_str(lines.next().expect("one response line"))
            .unwrap_or_else(|error| panic!("{error}: {stdout}"));
        assert!(
            lines.next().is_none(),
            "more than one response line: {stdout}"
        );
        response
    };
    hook(
        "prompt",
        &base(
            "beforeSubmitPrompt",
            &json!({"prompt":"Make compute return 2","attachments":[]}),
        ),
    );
    let shell = |event: &str, id: &str, command: &str| {
        let result = if event == "postToolUse" {
            json!({"tool_output":"{\"exitCode\":0,\"output\":\"test result: ok\"}"})
        } else {
            json!({"error_message":"test result: FAILED","failure_type":"error","is_interrupt":false})
        };
        let mut fields = json!({"tool_name":"Shell","tool_use_id":id,"cwd":f.repo(),
            "tool_input":{"command":command,"cwd":f.repo()},"duration":900});
        for (key, value) in result.as_object().unwrap() {
            fields[key] = value.clone();
        }
        hook("bash", &base(event, &fields))
    };
    // The first shell result also delivers repository context: one response
    // carries both, within the size Cursor accepts.
    let first = shell("postToolUse", "s1", "cargo test 2>&1 | tail -5");
    let context = first["additional_context"].as_str().expect("context");
    let (repository, note) = context.split_once(NOTE).expect("note after the context");
    assert!(repository.contains("compute"), "{context}");
    assert!(note.contains("`set -o pipefail; cargo test 2>&1 | tail -5`"));
    assert!(
        context.len()
            <= engram::agenthooks::cursor::CONTEXT_BYTES
                + engram::agenthooks::cursor::RELAY_BYTES
                + 900
    );
    let second = shell("postToolUse", "s2", "cargo test | tail -3");
    assert!(
        !second["additional_context"]
            .as_str()
            .unwrap_or_default()
            .contains(NOTE),
        "{second}"
    );
    // A failure reported without an exit code, with another cause.
    let failed = shell("postToolUseFailure", "s3", "cargo test; false");
    let note = failed["additional_context"].as_str().expect("note");
    assert!(
        note.contains(NOTE) && note.contains("run `cargo test`"),
        "{note}"
    );
    let plain = shell("postToolUse", "s4", "cargo test");
    assert!(
        !plain["additional_context"]
            .as_str()
            .unwrap_or_default()
            .contains(NOTE),
        "{plain}"
    );
    let pipe = Some("pipe".to_owned());
    assert_eq!(
        f.ledger(4),
        vec![
            (pipe.clone(), 1),
            (pipe, 0),
            (Some("sequence".to_owned()), 1),
            (None, 0)
        ]
    );
}
