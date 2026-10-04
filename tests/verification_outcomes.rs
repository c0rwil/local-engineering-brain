//! Hook verification capture and session outcomes, end to end through the
//! hook binary and a temporary daemon. Each test is the regression for one
//! reviewed defect; payloads use the real client shapes.
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

fn cli(root: &Path, args: &[&str], input: Option<&Value>) -> String {
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

/// How a Claude Code Bash call ended.
enum Run<'a> {
    Pass(&'a str),
    Fail(i32, &'a str),
}

struct Fixture {
    temp: TempDir,
}

impl Fixture {
    fn new(repos: &[&str]) -> Self {
        Self::with_config(repos, None)
    }

    fn with_config(repos: &[&str], config: Option<&str>) -> Self {
        let temp = TempDir::new().unwrap();
        if let Some(config) = config {
            let dir = temp.path().join(".config/engram");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("config.toml"), config).unwrap();
        }
        let fixture = Self { temp };
        for name in repos {
            let repo = fixture.repo(name);
            std::fs::create_dir(&repo).unwrap();
            std::fs::write(repo.join("lib.rs"), "pub fn compute() -> u32 { 1 }\n").unwrap();
            fixture.cli(&["index", repo.to_str().unwrap(), "--json"]);
        }
        fixture
    }

    fn root(&self) -> &Path {
        self.temp.path()
    }

    fn repo(&self, name: &str) -> PathBuf {
        self.root().join(name)
    }

    fn cli(&self, args: &[&str]) -> String {
        cli(self.root(), args, None)
    }

    fn hook(&self, args: &[&str], payload: &Value) {
        cli(self.root(), args, Some(payload));
    }

    fn prompt(&self, session: &str, cwd: &Path) {
        self.hook(
            &["hook", "prompt"],
            &json!({"session_id":session,"cwd":cwd,"prompt":"Make compute return 2"}),
        );
    }

    /// A Claude Code Bash call: `PostToolUse` when it passed, otherwise
    /// `PostToolUseFailure` with the exit code in its error text.
    fn bash(&self, session: &str, cwd: &Path, id: &str, command: &str, run: &Run<'_>) {
        let payload = match run {
            Run::Pass(stdout) => json!({"session_id":session,"cwd":cwd,
                "hook_event_name":"PostToolUse","tool_name":"Bash","tool_use_id":id,
                "tool_input":{"command":command},
                "tool_response":{"stdout":stdout,"stderr":"","interrupted":false,"isImage":false}}),
            Run::Fail(code, text) => json!({"session_id":session,"cwd":cwd,
                "hook_event_name":"PostToolUseFailure","tool_name":"Bash","tool_use_id":id,
                "tool_input":{"command":command},
                "error":format!("Exit code {code}\n{text}"),"is_interrupt":false}),
        };
        self.hook(&["hook", "bash"], &payload);
    }

    /// A Claude Code `Edit` of `file` in `repo`. The tool call opens that
    /// repository's episode before any verification there.
    fn edit(&self, session: &str, repo: &Path, id: &str, file: &str) {
        let path = repo.join(file);
        self.hook(
            &["hook", "bash"],
            &json!({"session_id":session,"cwd":repo,"hook_event_name":"PostToolUse",
                "tool_name":"Edit","tool_use_id":id,
                "tool_input":{"file_path":path,"old_string":"1","new_string":"2"},
                "tool_response":{"filePath":path}}),
        );
    }

    /// The episode sidecars of every session, as JSON.
    fn sidecars(&self) -> Vec<(PathBuf, Value)> {
        std::fs::read_dir(self.root().join("hook-sessions"))
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .filter_map(|path| {
                let sidecar: Value =
                    serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
                sidecar.get("task_id").is_some().then_some((path, sidecar))
            })
            .collect()
    }

    /// Rewrite the episode sidecars as an earlier build left them before a
    /// first resolved verification: without any checkout.
    fn forget_checkouts(&self) {
        for (path, mut sidecar) in self.sidecars() {
            let fields = sidecar.as_object_mut().unwrap();
            fields.remove("root");
            fields.remove("checkouts");
            std::fs::write(path, sidecar.to_string()).unwrap();
        }
    }

    fn end(&self, session: &str, cwd: &Path) {
        self.hook(
            &["hook", "session-end"],
            &json!({"session_id":session,"cwd":cwd}),
        );
    }

    fn db(&self) -> Connection {
        Connection::open(self.root().join("brain.sqlite")).unwrap()
    }

    fn rows(&self, sql: &str) -> Vec<(String, i64)> {
        self.db()
            .prepare(sql)
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// Recorded verification kinds and exit codes, oldest first.
    fn evidence(&self) -> Vec<(String, i64)> {
        self.rows("SELECT kind, exit_code FROM evidence ORDER BY observed_at_ms, rowid")
    }

    /// Outcome of the hook episode of `session`.
    fn outcome(&self, session: &str) -> String {
        self.db()
            .query_row(
                "SELECT status FROM tasks WHERE session_id=?1",
                [session],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn task(&self, repo: &Path) -> Value {
        serde_json::from_str(&self.cli(&[
            "task",
            "start",
            "Make compute return 2",
            "--path",
            repo.to_str().unwrap(),
            "--json",
        ]))
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new(assert_cmd::cargo::cargo_bin!("engram"))
            .env("HOME", self.root())
            .args([
                "--db",
                self.root().join("brain.sqlite").to_str().unwrap(),
                "--socket",
                self.root().join("engramd.sock").to_str().unwrap(),
                "daemon",
                "stop",
            ])
            .output();
    }
}

/// Item 1: a failed test is not masked by later passing lint, format, check
/// or build runs; formatters without `--check` and bare `make` targets are
/// not verification.
#[test]
fn passing_checks_after_a_failed_test_close_the_episode_as_failure() {
    let f = Fixture::new(&["repo"]);
    let repo = f.repo("repo");
    f.prompt("item-1", &repo);
    f.bash(
        "item-1",
        &repo,
        "t1",
        "cargo test",
        &Run::Fail(101, "test compute ... FAILED"),
    );
    for (id, command) in [
        ("t2", "cargo fmt --check"),
        ("t3", "cargo check"),
        ("t4", "cargo build"),
        ("t5", "cargo fmt"),
        ("t6", "make install"),
    ] {
        f.bash("item-1", &repo, id, command, &Run::Pass("ok"));
    }
    f.end("item-1", &repo);
    assert_eq!(f.outcome("item-1"), "failure");
    assert_eq!(
        f.evidence(),
        [
            ("test_result".to_owned(), 101),
            ("lint_result".to_owned(), 0),
            ("typecheck_result".to_owned(), 0),
            ("build_result".to_owned(), 0),
        ],
        "`cargo fmt` and `make install` are not verification"
    );
}

/// User- and project-level hooks run concurrently for one tool call; the call
/// is recorded once and its result stays known.
#[test]
fn concurrent_hooks_for_one_call_record_it_once() {
    let f = Fixture::new(&["repo"]);
    let repo = f.repo("repo");
    f.prompt("dual", &repo);
    // The episode exists before the race, as it does for every call after a
    // session's first: both hooks load the same session state at once.
    f.bash("dual", &repo, "t0", "cargo check", &Run::Pass("ok"));
    for round in 1..=5 {
        let payload = json!({"session_id":"dual","cwd":repo,
            "hook_event_name":"PostToolUse","tool_name":"Bash","tool_use_id":format!("t{round}"),
            "tool_input":{"command":"cargo test"},
            "tool_response":{"stdout":"test result: ok","stderr":"","interrupted":false,"isImage":false}});
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| {
                    barrier.wait();
                    cli(f.root(), &["hook", "bash"], Some(&payload))
                });
            }
        });
    }
    let tests = f
        .evidence()
        .into_iter()
        .filter(|(kind, _)| kind == "test_result")
        .count();
    assert_eq!(tests, 5, "each of the five calls is recorded exactly once");
    f.end("dual", &repo);
    assert_eq!(f.outcome("dual"), "success");
}

/// Item 2: every verification in a command must carry its own status; a
/// passing exit after `;` says nothing about the tests before it.
#[test]
fn verifications_joined_by_a_semicolon_are_not_recorded() {
    let f = Fixture::new(&["repo"]);
    let repo = f.repo("repo");
    f.prompt("item-2", &repo);
    f.bash(
        "item-2",
        &repo,
        "t1",
        "cargo test; cargo clippy",
        &Run::Pass("test result: FAILED. 0 passed; 1 failed"),
    );
    assert!(f.evidence().is_empty());
    f.end("item-2", &repo);
    assert_eq!(f.outcome("item-2"), "abandoned");
}

/// Item 3: the verification's directory comes from the same parse as its
/// status; `cd X; cmd` may run in the original directory and is not recorded.
#[test]
fn a_directory_change_that_may_not_apply_never_misattributes_evidence() {
    let f = Fixture::new(&["one", "two"]);
    let one = f.repo("one");
    f.prompt("item-3", &one);
    f.bash(
        "item-3",
        &one,
        "t1",
        "cd ../two; cargo test",
        &Run::Pass("test result: ok"),
    );
    assert!(f.evidence().is_empty(), "never recorded on project one");
    f.bash(
        "item-3",
        &one,
        "t2",
        "export A=1; cd ../two && cargo test",
        &Run::Pass("test result: ok"),
    );
    assert_eq!(
        f.rows("SELECT p.name, e.exit_code FROM evidence e JOIN projects p ON p.id=e.project_id"),
        [("two".to_owned(), 0)]
    );
}

/// Item 4: write-ahead. A failure the daemon refuses to record (here: larger
/// than its request limit) leaves the result unknown instead of the older pass.
#[test]
fn a_failure_the_daemon_refuses_leaves_the_result_unknown() {
    let f = Fixture::with_config(&["repo"], Some("[daemon]\nmax_request_bytes = 3000\n"));
    let repo = f.repo("repo");
    f.prompt("item-4", &repo);
    f.bash(
        "item-4",
        &repo,
        "t1",
        "cargo test",
        &Run::Pass("test result: ok"),
    );
    let output = "thread panicked\n".repeat(400);
    f.bash(
        "item-4",
        &repo,
        "t2",
        "cargo test",
        &Run::Fail(101, &output),
    );
    assert_eq!(
        f.evidence(),
        [("test_result".to_owned(), 0)],
        "the failure was not recorded"
    );
    f.end("item-4", &repo);
    assert_eq!(f.outcome("item-4"), "abandoned");
}

/// Item 5: after an unknown run, the same passing command is a new
/// observation, not a duplicate of the older pass.
#[test]
fn an_identical_pass_after_an_unknown_run_is_recorded_again() {
    let f = Fixture::new(&["repo"]);
    let repo = f.repo("repo");
    f.prompt("item-5", &repo);
    let pass = Run::Pass("test result: ok");
    f.bash("item-5", &repo, "t1", "cargo test", &pass);
    f.bash("item-5", &repo, "t2", "cargo test | tail -3", &pass);
    f.bash("item-5", &repo, "t3", "cargo test", &pass);
    assert_eq!(
        f.evidence(),
        [("test_result".to_owned(), 0), ("test_result".to_owned(), 0)]
    );
    // The same tool call delivered twice is still one observation.
    f.bash("item-5", &repo, "t3", "cargo test", &pass);
    assert_eq!(f.evidence().len(), 2);
    f.end("item-5", &repo);
    assert_eq!(f.outcome("item-5"), "success");
}

/// Item 6: a declared task closed outside the MCP hook stops receiving
/// evidence, and another MCP server's tool of the same name declares nothing.
#[test]
fn closed_or_foreign_declared_tasks_receive_no_hook_evidence() {
    let f = Fixture::new(&["repo"]);
    let repo = f.repo("repo");
    f.prompt("item-6", &repo);
    let declare = |server: &str, id: &str, task: &Value| {
        f.hook(
            &["hook", "bash"],
            &json!({"session_id":"item-6","cwd":repo,"hook_event_name":"PostToolUse",
                "tool_name":format!("mcp__{server}__engram_task_start"),"tool_use_id":id,
                "tool_input":{"project_id":task["project_id"],"prompt":"Make compute return 2"},
                "tool_response":task.to_string()}),
        );
    };
    let declared = f.task(&repo);
    let declared_id = declared["id"].as_str().unwrap().to_owned();
    declare("engram", "d1", &declared);
    let pass = Run::Pass("test result: ok");
    f.bash("item-6", &repo, "t1", "cargo test", &pass);
    f.cli(&[
        "task",
        "finish",
        &declared_id,
        "--outcome",
        "success",
        "--json",
    ]);
    f.bash("item-6", &repo, "t2", "cargo test --lib", &pass);
    let foreign = f.task(&repo);
    declare("other", "d2", &foreign);
    f.bash("item-6", &repo, "t3", "cargo test --doc", &pass);
    let tasks: Vec<String> = f
        .db()
        .prepare("SELECT task_id FROM evidence ORDER BY observed_at_ms, rowid")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let episode: String = f
        .db()
        .query_row(
            "SELECT id FROM tasks WHERE session_id='item-6'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tasks, [declared_id, episode.clone(), episode]);
}

/// Item 7: a negated or conditional `pipefail` does not make a pipe carry the
/// verification's status.
#[test]
fn negated_or_conditional_pipefail_leaves_a_pipe_unrecorded() {
    let f = Fixture::new(&["repo"]);
    let repo = f.repo("repo");
    f.prompt("item-7", &repo);
    for (id, command) in [
        (
            "t1",
            "setopt pipefail; setopt no_pipe_fail; cargo test | tail -3",
        ),
        ("t2", "false && set -o pipefail; cargo test | tail -3"),
    ] {
        f.bash("item-7", &repo, id, command, &Run::Pass("test result: ok"));
    }
    assert!(f.evidence().is_empty());
}

/// Item 8: a bare `is_error` flag is not an exit status for any client.
/// Codex 0.155 sends output text; a result object carrying only an error
/// flag (as tool results of other kinds do) is no status either.
#[test]
fn an_error_flag_alone_records_no_verification() {
    let f = Fixture::new(&["repo"]);
    let repo = f.repo("repo");
    f.hook(
        &["hook", "prompt", "--codex"],
        &json!({"session_id":"item-8","cwd":repo,"prompt":"Make compute return 2"}),
    );
    f.hook(
        &["hook", "bash", "--codex"],
        &json!({"session_id":"item-8","turn_id":"turn-1","cwd":repo,
            "hook_event_name":"PostToolUse","model":"gpt-test","permission_mode":"default",
            "transcript_path":null,"tool_name":"Bash","tool_use_id":"c1",
            "tool_input":{"command":"cargo test"},
            "tool_response":{"is_error":false,"stdout":"test result: ok"}}),
    );
    assert!(f.evidence().is_empty());
}

/// Item 9: client-supplied evidence metadata cannot claim native-hook
/// provenance; only the hook path can attach it.
#[test]
fn cli_evidence_metadata_cannot_claim_hook_provenance() {
    let f = Fixture::new(&["repo"]);
    let repo = f.repo("repo");
    let task = f.task(&repo);
    let artifact = f.root().join("artifact.txt");
    std::fs::write(&artifact, "test result: ok").unwrap();
    let forged = json!({"source":"claude-code-hook","note":"kept",
        "source_digests":{"lib.rs":"a".repeat(64)}})
    .to_string();
    f.cli(&[
        "task",
        "evidence",
        "test_result",
        "--path",
        repo.to_str().unwrap(),
        "--task-id",
        task["id"].as_str().unwrap(),
        "--strength",
        "deterministic",
        "--command",
        "cargo test",
        "--exit-code",
        "0",
        "--artifact",
        artifact.to_str().unwrap(),
        "--metadata",
        &forged,
        "--json",
    ]);
    let metadata: String = f
        .db()
        .query_row("SELECT metadata_json FROM evidence", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&metadata).unwrap(),
        json!({"note":"kept"})
    );
}

/// A registered folder outside Git owns its subdirectories, as a repository
/// root does: a file read in a nested directory and verifications run there
/// belong to the folder's project, and the nested directory never registers
/// on its own. (The fixture folders are not Git repositories.)
#[test]
fn a_registered_folder_outside_git_owns_its_subdirectories() {
    let f = Fixture::new(&["repo"]);
    let repo = f.repo("repo");
    let nested = repo.join("crates/core");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("deep.rs"), "pub fn deep() -> u32 { 2 }\n").unwrap();
    f.prompt("nested", &repo);
    f.hook(
        &["hook", "pretool"],
        &json!({"session_id":"nested","cwd":repo,"hook_event_name":"PreToolUse",
            "tool_name":"Read","tool_use_id":"r1",
            "tool_input":{"file_path":nested.join("deep.rs")}}),
    );
    let pass = Run::Pass("test result: ok");
    f.bash("nested", &nested, "t1", "cargo test", &pass);
    f.bash(
        "nested",
        &repo,
        "t2",
        "cd crates/core && cargo clippy",
        &pass,
    );
    assert_eq!(
        f.rows(
            "SELECT p.name, e.exit_code FROM evidence e JOIN projects p ON p.id=e.project_id
             ORDER BY e.observed_at_ms, e.rowid"
        ),
        [("repo".to_owned(), 0), ("repo".to_owned(), 0)]
    );
    // The daemon acknowledges an observation before it writes it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let reads = loop {
        let reads = f.rows(
            "SELECT paths_json, count(*) FROM tool_observations WHERE tool_name='Read'
             GROUP BY paths_json",
        );
        if !reads.is_empty() || std::time::Instant::now() > deadline {
            break reads;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(reads, [("[\"crates/core/deep.rs\"]".to_owned(), 1)]);
    assert_eq!(
        f.rows("SELECT name, count(*) FROM projects GROUP BY name"),
        [("repo".to_owned(), 1)]
    );
    for directory in [&nested, &repo.join("crates")] {
        assert!(
            !directory.join(".engram").exists(),
            "{}",
            directory.display()
        );
    }
    f.end("nested", &repo);
    assert_eq!(f.outcome("nested"), "success");
}

/// Configuration follows the same identity: a hook running in a subdirectory
/// of a registered folder outside Git loads the folder's project config.
#[test]
fn a_registered_folder_outside_git_configures_its_subdirectories() {
    let f = Fixture::new(&["repo"]);
    let repo = f.repo("repo");
    let nested = repo.join("docs/notes");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        repo.join(".engram/config.toml"),
        "[security]\ncustom_redaction_patterns = [\"TICKET-[0-9]+\"]\n",
    )
    .unwrap();
    f.hook(
        &["hook", "prompt"],
        &json!({"session_id":"configured","cwd":nested,"prompt":"Fix TICKET-4711 in compute"}),
    );
    let saved = engram::agenthooks::pending_prompt(f.root(), "configured").expect("prompt");
    assert!(
        saved.contains("compute") && !saved.contains("TICKET-4711"),
        "{saved}"
    );
}

// Codex 0.155 reports a shell command to `PostToolUse` as `Bash` with only
// the command text and the output text. The calling thread's paginated
// rollout records the command's exit code and the directory it ran in. The
// helpers below write synthetic rollouts in that format.

fn file_uri(path: &Path) -> String {
    use std::fmt::Write as _;
    use std::os::unix::ffi::OsStrExt as _;
    let mut uri = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(byte) {
            uri.push(char::from(*byte));
        } else {
            write!(uri, "%{byte:02X}").unwrap();
        }
    }
    uri
}

/// Start the rollout of Codex thread `thread` in session `session`.
fn codex_rollout(path: &Path, thread: &str, session: &str, source: &Value) {
    let meta = json!({"timestamp":"2026-10-04T00:00:00.000Z","type":"session_meta",
        "payload":{"id":thread,"session_id":session,"source":source,
            "history_mode":"paginated","cwd":"/synthetic","cli_version":"0.155.0-alpha.2.6"}});
    std::fs::write(path, format!("{meta}\n")).unwrap();
}

/// Append the line Codex writes when shell call `id` finishes in `cwd`.
fn codex_finished(path: &Path, id: &str, cwd: &Path, exit_code: i64) {
    let status = if exit_code == 0 {
        "completed"
    } else {
        "failed"
    };
    let line = json!({"timestamp":"2026-10-04T00:00:01.000Z","ordinal":2,"type":"event_msg",
        "payload":{"type":"item_completed","thread_id":"synthetic","turn_id":"turn-1",
            "started_at_ms":1,"completed_at_ms":2,
            "item":{"type":"CommandExecution","id":id,"command":["/bin/zsh","-lc","synthetic"],
                "cwd":file_uri(cwd),"status":status,"exit_code":exit_code,
                "source":"unified_exec_startup","process_id":"7","parsed_cmd":[],
                "stdout":"synthetic output","stderr":"","aggregated_output":"synthetic output",
                "formatted_output":"synthetic output","duration":{"secs":1,"nanos":0}}}});
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{line}").unwrap();
}

impl Fixture {
    fn codex_prompt(&self, session: &str, cwd: &Path, rollout: &Path) {
        self.hook(
            &["hook", "prompt", "--codex"],
            &json!({"session_id":session,"turn_id":"turn-1","cwd":cwd,
                "hook_event_name":"UserPromptSubmit","model":"gpt-test",
                "permission_mode":"default","transcript_path":rollout,
                "prompt":"Make compute return 2"}),
        );
    }

    /// A Codex shell call as `PostToolUse` reports it: the session's `cwd`,
    /// the command, and the output text. Returns the hook's output.
    fn codex_bash(
        &self,
        session: &str,
        cwd: &Path,
        rollout: &Path,
        id: &str,
        command: &str,
    ) -> String {
        self.codex_call(&json!({"session_id":session,"turn_id":"turn-1","cwd":cwd,
            "hook_event_name":"PostToolUse","model":"gpt-test","permission_mode":"default",
            "transcript_path":rollout,"tool_name":"Bash","tool_use_id":id,
            "tool_input":{"command":command},"tool_response":"synthetic output"}))
    }

    fn codex_call(&self, payload: &Value) -> String {
        cli(self.root(), &["hook", "bash", "--codex"], Some(payload))
    }

    fn codex_end(&self, session: &str, cwd: &Path, rollout: &Path) {
        self.hook(
            &["hook", "session-end", "--codex"],
            &json!({"session_id":session,"cwd":cwd,"transcript_path":rollout,
                "hook_event_name":"SessionEnd","reason":"other"}),
        );
    }

    /// Outcome of the session's episode for one repository.
    fn outcome_in(&self, session: &str, repo: &str) -> String {
        self.db()
            .query_row(
                "SELECT t.status FROM tasks t JOIN projects p ON p.id=t.project_id
                 WHERE t.session_id=?1 AND p.name=?2",
                [session, repo],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// Rows of `sql` once `expected` have committed: the daemon acknowledges
    /// observations before it writes them.
    fn committed(&self, sql: &str, expected: usize) -> Vec<(String, i64)> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let rows = self.rows(sql);
            if rows.len() >= expected || std::time::Instant::now() > deadline {
                return rows;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Verification ledger rows (cause or "recordable", note), oldest first.
    fn ledger(&self, expected: usize) -> Vec<(String, i64)> {
        self.committed(
            "SELECT coalesce(hidden_reason, 'recordable'), note_delivered
             FROM verification_observations ORDER BY observed_at_ms, rowid",
            expected,
        )
    }
}

/// The rollout record gives a Codex run its exit status and the directory it
/// ran in, which the payload's session `cwd` does not: here the model ran
/// the tests with `workdir` set to another folder.
#[test]
fn codex_rollout_records_become_evidence_where_the_command_ran() {
    let f = Fixture::new(&["one", "two repo"]);
    let (one, two) = (f.repo("one"), f.repo("two repo"));
    let rollout = f.root().join("rollout-root.jsonl");
    codex_rollout(&rollout, "codex-ran", "codex-ran", &json!("vscode"));
    f.codex_prompt("codex-ran", &one, &rollout);
    codex_finished(&rollout, "exec-1", &one, 101);
    assert_eq!(
        f.codex_bash("codex-ran", &one, &rollout, "exec-1", "cargo test"),
        ""
    );
    codex_finished(&rollout, "exec-2", &two, 0);
    f.codex_bash("codex-ran", &one, &rollout, "exec-2", "cargo test");
    assert_eq!(
        f.rows(
            "SELECT p.name || ' ' || json_extract(e.metadata_json,'$.exit_status') || ' ' ||
                json_extract(e.metadata_json,'$.source'), e.exit_code
             FROM evidence e JOIN projects p ON p.id=e.project_id
             ORDER BY e.observed_at_ms, e.rowid"
        ),
        [
            ("one codex_rollout codex-hook".to_owned(), 101),
            ("two repo codex_rollout codex-hook".to_owned(), 0)
        ]
    );
    // The failing command is a failed tool call too.
    assert_eq!(
        f.committed(
            "SELECT p.name || ' ' || o.outcome, count(*) FROM tool_observations o
             JOIN projects p ON p.id=o.project_id WHERE o.tool_name='Bash'
             GROUP BY o.project_id, o.outcome ORDER BY p.name",
            2
        ),
        [("one failed".to_owned(), 1), ("two repo ok".to_owned(), 1)]
    );
    assert_eq!(
        f.ledger(2),
        [("recordable".to_owned(), 0), ("recordable".to_owned(), 0)]
    );
    f.codex_end("codex-ran", &one, &rollout);
    assert_eq!(f.outcome_in("codex-ran", "two repo"), "success");
    assert_eq!(f.outcome_in("codex-ran", "one"), "failure");
}

/// The rollout writer can lag the hook; the hook waits briefly for the line.
#[test]
fn codex_waits_briefly_for_a_record_still_being_written() {
    let f = Fixture::new(&["repo"]);
    let repo = f.repo("repo");
    let rollout = f.root().join("rollout-late.jsonl");
    codex_rollout(&rollout, "codex-late", "codex-late", &json!("vscode"));
    f.codex_prompt("codex-late", &repo, &rollout);
    let writer = std::thread::spawn({
        let (rollout, repo) = (rollout.clone(), repo.clone());
        move || {
            std::thread::sleep(std::time::Duration::from_millis(600));
            codex_finished(&rollout, "exec-late", &repo, 0);
        }
    });
    f.codex_bash("codex-late", &repo, &rollout, "exec-late", "cargo test");
    writer.join().unwrap();
    assert_eq!(f.evidence(), [("test_result".to_owned(), 0)]);
}

/// Without its record a Codex run has neither a status nor a known
/// directory: nothing is recorded, the ledger says the client reported no
/// status, and no earlier pass of the same command may close an episode.
#[test]
fn a_codex_run_without_a_record_stays_unknown_everywhere() {
    let f = Fixture::new(&["one", "two"]);
    let (one, two) = (f.repo("one"), f.repo("two"));
    let rollout = f.root().join("rollout-unknown.jsonl");
    codex_rollout(&rollout, "codex-unknown", "codex-unknown", &json!("vscode"));
    f.codex_prompt("codex-unknown", &one, &rollout);
    codex_finished(&rollout, "exec-1", &one, 0);
    f.codex_bash("codex-unknown", &one, &rollout, "exec-1", "cargo test");
    codex_finished(&rollout, "exec-2", &two, 0);
    f.codex_bash("codex-unknown", &one, &rollout, "exec-2", "cargo test");
    let started = std::time::Instant::now();
    // The model ran it in either folder; Codex wrote no record of it.
    f.codex_bash("codex-unknown", &one, &rollout, "exec-3", "cargo test");
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(1_500),
        "the hook waited for the record"
    );
    assert_eq!(
        f.evidence(),
        [("test_result".to_owned(), 0), ("test_result".to_owned(), 0)]
    );
    assert_eq!(
        f.ledger(3),
        [
            ("recordable".to_owned(), 0),
            ("recordable".to_owned(), 0),
            ("unreported".to_owned(), 0)
        ]
    );
    f.codex_end("codex-unknown", &one, &rollout);
    assert_eq!(f.outcome_in("codex-unknown", "one"), "abandoned");
    assert_eq!(
        f.outcome_in("codex-unknown", "two"),
        "abandoned",
        "the unrecorded run may have failed in this folder"
    );
}

/// A pipe hides the verification's status even when the rollout has the
/// pipeline's: the run is noted, never recorded.
#[test]
fn a_piped_codex_run_is_noted_not_recorded() {
    let f = Fixture::new(&["repo"]);
    let repo = f.repo("repo");
    let rollout = f.root().join("rollout-pipe.jsonl");
    codex_rollout(&rollout, "codex-pipe", "codex-pipe", &json!("vscode"));
    f.codex_prompt("codex-pipe", &repo, &rollout);
    codex_finished(&rollout, "exec-pipe", &repo, 0);
    let output = f.codex_bash(
        "codex-pipe",
        &repo,
        &rollout,
        "exec-pipe",
        "cargo test 2>&1 | tail -5",
    );
    let note: Value = serde_json::from_str(output.trim()).expect("note envelope");
    let note = note["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("note");
    assert!(
        note.contains("`set -o pipefail; cargo test 2>&1 | tail -5`"),
        "{note}"
    );
    assert!(f.evidence().is_empty());
    assert_eq!(f.ledger(1), [("pipe".to_owned(), 1)]);
}

/// Only the calling thread's own rollout is read: a spawned worker's runs
/// count for the root session, the guardian stays silent, and a call that
/// names another thread's rollout gets no status from it.
#[test]
fn codex_reads_only_the_calling_threads_rollout() {
    let f = Fixture::new(&["repo"]);
    let repo = f.repo("repo");
    let root_rollout = f.root().join("rollout-root.jsonl");
    let worker_rollout = f.root().join("rollout-worker.jsonl");
    let guardian_rollout = f.root().join("rollout-guardian.jsonl");
    codex_rollout(&root_rollout, "codex-tree", "codex-tree", &json!("vscode"));
    codex_rollout(
        &worker_rollout,
        "worker-thread",
        "codex-tree",
        &json!({"subagent":{"thread_spawn":{"parent_thread_id":"codex-tree","depth":1}}}),
    );
    codex_rollout(
        &guardian_rollout,
        "guardian-thread",
        "codex-tree",
        &json!({"subagent":{"other":"guardian"}}),
    );
    f.codex_prompt("codex-tree", &repo, &root_rollout);
    let call = |rollout: &Path, id: &str| {
        json!({"session_id":"codex-tree","turn_id":"turn-1","cwd":repo,
            "hook_event_name":"PostToolUse","model":"gpt-test","permission_mode":"default",
            "transcript_path":rollout,"tool_name":"Bash","tool_use_id":id,
            "tool_input":{"command":"cargo test"},"tool_response":"synthetic output"})
    };
    codex_finished(&worker_rollout, "exec-worker", &repo, 0);
    let mut worker = call(&worker_rollout, "exec-worker");
    worker["agent_id"] = json!("worker-thread");
    worker["agent_type"] = json!("worker");
    f.codex_call(&worker);
    codex_finished(&guardian_rollout, "exec-guardian", &repo, 0);
    assert_eq!(f.codex_call(&call(&guardian_rollout, "exec-guardian")), "");
    codex_finished(&worker_rollout, "exec-misnamed", &repo, 0);
    f.codex_call(&call(&worker_rollout, "exec-misnamed"));
    assert_eq!(f.evidence(), [("test_result".to_owned(), 0)]);
    assert_eq!(
        f.ledger(2),
        [("recordable".to_owned(), 0), ("unreported".to_owned(), 0)]
    );
    assert_eq!(
        f.rows("SELECT session_id, count(*) FROM tasks GROUP BY session_id"),
        [("codex-tree".to_owned(), 1)],
        "the worker's run belongs to the root session's episode"
    );
}

/// Episodes know their checkouts from the moment they open, so a later
/// verification in another repository marks only the episode it belongs to:
/// repository one's earlier pass still closes it as success.
#[test]
fn a_verification_in_another_repository_leaves_an_earlier_pass_standing() {
    let f = Fixture::new(&["one", "two", "three"]);
    let (one, two) = (f.repo("one"), f.repo("two"));
    f.prompt("roots", &one);
    f.bash(
        "roots",
        &one,
        "t1",
        "cargo test",
        &Run::Pass("test result: ok"),
    );
    // The agent edits repository two, then verifies there with a directory
    // change the shell may not apply: unmodeled, but inside two's checkout.
    f.edit("roots", &two, "e1", "lib.rs");
    f.bash(
        "roots",
        &two,
        "t2",
        "cd src; cargo test",
        &Run::Pass("test result: ok"),
    );
    // The first touch of repository three is a verification there.
    f.bash(
        "roots",
        &one,
        "t3",
        "cd ../three && cargo test",
        &Run::Pass("test result: ok"),
    );
    f.end("roots", &one);
    assert_eq!(f.outcome_in("roots", "one"), "success");
    assert_eq!(
        f.outcome_in("roots", "two"),
        "abandoned",
        "its own run's result stayed unknown"
    );
    assert_eq!(f.outcome_in("roots", "three"), "success");
}

/// A directory that is unmodeled and bounded by no known checkout may be
/// any repository's: every episode's result becomes unknown.
#[test]
fn a_truly_unknown_directory_still_marks_every_episode() {
    let f = Fixture::new(&["one", "two"]);
    let (one, two) = (f.repo("one"), f.repo("two"));
    f.prompt("unknown", &one);
    f.bash(
        "unknown",
        &one,
        "t1",
        "cargo test",
        &Run::Pass("test result: ok"),
    );
    f.bash(
        "unknown",
        &two,
        "t2",
        "cargo test",
        &Run::Pass("test result: ok"),
    );
    f.bash(
        "unknown",
        &one,
        "t3",
        "cd \"$REPO\" && cargo test",
        &Run::Pass("test result: ok"),
    );
    f.end("unknown", &one);
    assert_eq!(f.outcome_in("unknown", "one"), "abandoned");
    assert_eq!(
        f.outcome_in("unknown", "two"),
        "abandoned",
        "the run may have been in repository two"
    );
}

/// Episodes from an earlier build knew no checkout before their first
/// resolved verification. The next prompt gives them the project's
/// registered checkouts, so the write-ahead targets precisely again.
#[test]
fn episodes_from_an_earlier_build_learn_their_checkouts() {
    let f = Fixture::new(&["one", "two"]);
    let one = f.repo("one");
    f.prompt("legacy", &one);
    f.bash(
        "legacy",
        &one,
        "t1",
        "cargo test",
        &Run::Pass("test result: ok"),
    );
    f.forget_checkouts();
    assert!(
        f.sidecars()
            .iter()
            .all(|(_, sidecar)| sidecar["root"].is_null())
    );
    f.prompt("legacy", &one);
    let canonical = one.canonicalize().unwrap();
    assert!(
        f.sidecars()
            .iter()
            .all(|(_, sidecar)| sidecar["root"] == json!(canonical)),
        "{:?}",
        f.sidecars()
    );
    f.bash(
        "legacy",
        &one,
        "t2",
        "cd ../two && cargo test",
        &Run::Pass("test result: ok"),
    );
    f.end("legacy", &one);
    assert_eq!(f.outcome_in("legacy", "one"), "success");
    assert_eq!(f.outcome_in("legacy", "two"), "success");
}
