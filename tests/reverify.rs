//! Operator-initiated re-verification, end to end through the CLI and a
//! temporary daemon, on a synthetic repository: the dry-run plan, a `--yes`
//! run in a disposable worktree, refusals, a failing run, worktree cleanup,
//! the provenance label and the judge's eligibility.
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

/// Tests spawn CLIs that auto-start daemons; one at a time keeps every
/// child's pipes private (see `agent_integrations.rs`).
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

const NO_EVIDENCE: &str =
    "No readable, independently verified evidence from the successful source task";
const MAKEFILE: &str = "test:\n\t@echo ran > ran.marker\n\t@echo '1 passed; 0 failed'\n\
                        lint:\n\t@echo 'warning: unused import'\n\t@exit 3\n";

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
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
        std::fs::write(repo.join("Makefile"), MAKEFILE).unwrap();
        std::fs::write(repo.join(".gitignore"), ".engram/\nran.marker\n").unwrap();
        git(&repo, &["init", "--quiet"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "--quiet", "-m", "fixture"]);
        fixture.cli(&["index", repo.to_str().unwrap(), "--json"]);
        fixture
    }

    fn root(&self) -> &Path {
        self.temp.path()
    }

    fn repo(&self) -> PathBuf {
        self.root().join("repo")
    }

    fn cli_with(&self, args: &[&str], input: Option<&Value>) -> String {
        let mut child = Command::new(assert_cmd::cargo::cargo_bin!("engram"))
            .env("HOME", self.root())
            .env_remove("ENGRAM_MODE")
            .args([
                "--db",
                self.root().join("brain.sqlite").to_str().unwrap(),
                "--socket",
                self.root().join("engramd.sock").to_str().unwrap(),
            ])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("cli");
        let mut stdin = child.stdin.take().expect("stdin");
        if let Some(input) = input {
            stdin.write_all(input.to_string().as_bytes()).unwrap();
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

    fn cli(&self, args: &[&str]) -> String {
        self.cli_with(args, None)
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_str(&self.cli(args)).expect("json")
    }

    /// Complete one successful task that touched `lib.rs` and recorded these
    /// verification commands (all reported passing); return its review id.
    fn proposal(&self, prompt: &str, commands: &[&str]) -> String {
        let repo = self.repo();
        let task = self.json(&[
            "task",
            "start",
            prompt,
            "--path",
            repo.to_str().unwrap(),
            "--json",
        ]);
        let verifications: Vec<Value> = commands
            .iter()
            .map(|command| {
                json!({"kind":"test_result","strength":"deterministic","command":command,
                    "exit_code":0,"artifact":"1 passed; 0 failed"})
            })
            .collect();
        let completion = self
            .root()
            .join(format!("{}.json", task["id"].as_str().unwrap()));
        std::fs::write(
            &completion,
            json!({"operation":"complete","task_id":task["id"],"outcome":"success",
                "summary":prompt,"files":["lib.rs"],"verifications":verifications,
                "lessons":[{"claim":format!("{prompt}: compute returns 2"),
                    "rationale":"Observed test run"}]})
            .to_string(),
        )
        .unwrap();
        let completed = self.json(&["product", "lesson", completion.to_str().unwrap()]);
        completed["reviews"][0]["id"].as_str().unwrap().to_owned()
    }

    /// Change the touched file and re-index it, so the recorded runs no
    /// longer observed the indexed source, as on the live backlog.
    fn move_on(&self) -> String {
        let repo = self.repo();
        std::fs::write(repo.join("lib.rs"), "pub fn compute() -> u32 { 2 }\n").unwrap();
        git(&repo, &["commit", "--quiet", "-am", "compute returns 2"]);
        self.cli(&["index", repo.to_str().unwrap(), "--json"]);
        git(&repo, &["rev-parse", "HEAD"])
    }

    fn db(&self) -> Connection {
        Connection::open(self.root().join("brain.sqlite")).unwrap()
    }

    fn evidence(&self) -> Vec<(String, Option<i64>, Option<String>, Value)> {
        self.db()
            .prepare(
                "SELECT command,exit_code,commit_hash,metadata_json FROM evidence
                 WHERE json_extract(metadata_json,'$.source')='engram-reverify'
                 ORDER BY observed_at_ms,rowid",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    serde_json::from_str::<Value>(&row.get::<_, String>(3)?).unwrap(),
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
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

fn review<'a>(plan: &'a Value, id: &str) -> &'a Value {
    plan["reviews"]
        .as_array()
        .unwrap()
        .iter()
        .find(|review| review["review_id"] == id)
        .unwrap_or_else(|| panic!("{id} in {plan}"))
}

fn command<'a>(review: &'a Value, text: &str) -> &'a Value {
    review["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|command| command["command"] == text)
        .unwrap_or_else(|| panic!("{text} in {review}"))
}

#[test]
#[allow(clippy::too_many_lines)]
fn reverification_plans_runs_in_a_disposable_worktree_and_unblocks_the_judge() {
    let f = Fixture::new();
    let repo = f.repo();
    let passing = f.proposal(
        "Make compute return 2",
        &[
            "make test",
            "make test 2>&1 | tail -3",
            "curl -fsS https://example.com/health && make test",
        ],
    );
    let failing = f.proposal("Keep the build clean", &["make lint"]);
    let refused = f.proposal("Document compute", &["make test > /tmp/out.txt"]);
    let head = f.move_on();

    // The plan reads only: nothing runs and nothing is recorded.
    let plan = f.json(&[
        "review",
        "reverify",
        "--all",
        "--project",
        repo.to_str().unwrap(),
        "--dry-run",
        "--json",
    ]);
    assert!(!repo.join("ran.marker").exists());
    assert!(f.evidence().is_empty());
    let planned = review(&plan, &passing);
    assert_eq!(planned["status"], "runnable", "{planned}");
    assert_eq!(planned["head"], head);
    assert_eq!(planned["source_files"], json!(["lib.rs"]));
    assert_eq!(command(planned, "make test")["runnable"], true);
    for (text, kind) in [
        ("make test 2>&1 | tail -3", "status_hidden"),
        (
            "curl -fsS https://example.com/health && make test",
            "network",
        ),
    ] {
        let refusal = &command(planned, text)["refusal"];
        assert_eq!(refusal["kind"], kind, "{text}");
        assert!(refusal["detail"].as_str().is_some_and(|d| !d.is_empty()));
    }
    assert_eq!(planned["gate"]["current"], NO_EVIDENCE);
    assert_eq!(planned["gate"]["previewed"], true);
    assert!(
        planned["gate"]["if_passing"].is_null(),
        "a passing run would make it judge-eligible: {planned}"
    );
    assert_eq!(review(&plan, &refused)["status"], "refused");
    assert_eq!(
        command(review(&plan, &refused), "make test > /tmp/out.txt")["refusal"]["kind"],
        "redirection"
    );
    let summary = &plan["summary"];
    assert_eq!(
        (
            summary["reviews"].as_u64(),
            summary["runnable"].as_u64(),
            summary["refused"].as_u64(),
            summary["runs"].as_u64(),
            summary["judge_eligible_now"].as_u64(),
            summary["judge_eligible_if_passing"].as_u64(),
        ),
        (Some(3), Some(2), Some(1), Some(2), Some(0), Some(2))
    );
    assert_eq!(
        summary["refusals"],
        json!({"network":1,"redirection":1,"status_hidden":1})
    );
    // Without --yes or --dry-run the plan is shown and nothing runs either.
    let shown = f.cli(&["review", "reverify", &passing]);
    assert!(shown.contains("Nothing ran"), "{shown}");
    assert!(f.evidence().is_empty());

    let ran = f.json(&["review", "reverify", "--all", "--yes", "--json"]);
    let execution = &ran["execution"];
    assert!(
        !repo.join("ran.marker").exists(),
        "commands never run in the operator's checkout"
    );
    let runs = execution["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2, "{execution}");
    let test = runs
        .iter()
        .find(|run| run["command"] == "make test")
        .unwrap();
    assert_eq!(test["outcome"]["exit_code"], 0);
    let lint = runs
        .iter()
        .find(|run| run["command"] == "make lint")
        .unwrap();
    assert_eq!(
        lint["outcome"]["exit_code"], 2,
        "make reports the failed recipe"
    );
    let worktrees = execution["worktrees"].as_array().unwrap();
    assert_eq!(worktrees.len(), 1);
    assert_eq!(worktrees[0]["removed"], true);
    assert!(!Path::new(worktrees[0]["path"].as_str().unwrap()).exists());
    assert_eq!(
        git(&repo, &["worktree", "list", "--porcelain"])
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count(),
        1,
        "the disposable worktree is gone"
    );
    assert_eq!(execution["attached"]["judge_enabled"], false);

    // Hook-grade provenance on the source tasks, at the worktree's commit.
    let digest = {
        use sha2::Digest as _;
        let bytes = std::fs::read(repo.join("lib.rs")).unwrap();
        sha2::Sha256::digest(bytes)
            .iter()
            .fold(String::new(), |mut hex, byte| {
                use std::fmt::Write as _;
                let _ = write!(hex, "{byte:02x}");
                hex
            })
    };
    let recorded = f.evidence();
    assert_eq!(
        recorded
            .iter()
            .map(|(command, exit_code, _, _)| (command.as_str(), *exit_code))
            .collect::<Vec<_>>(),
        vec![("make test", Some(0)), ("make lint", Some(2))]
    );
    for (_, _, commit_hash, metadata) in &recorded {
        assert_eq!(commit_hash, &None);
        assert_eq!(metadata["head"], head);
        assert_eq!(metadata["source_digests"]["lib.rs"], digest);
    }
    let attached = |id: &str| {
        execution["attached"]["reviews"]
            .as_array()
            .unwrap()
            .iter()
            .find(|review| review["id"] == id)
            .unwrap()["evidence_ids"]
            .as_array()
            .unwrap()
            .len()
    };
    assert_eq!(attached(&passing), 4, "three recorded runs and the re-run");
    assert_eq!(attached(&failing), 2, "a failing re-run is attached too");

    // The judge now sees hook-grade evidence: the passing review would go to
    // the model, the failing one stays deferred.
    let after = f.json(&["review", "reverify", "--all", "--dry-run", "--json"]);
    assert!(
        review(&after, &passing)["gate"]["current"].is_null(),
        "{}",
        review(&after, &passing)
    );
    assert_eq!(review(&after, &failing)["gate"]["current"], NO_EVIDENCE);
    assert_eq!(after["summary"]["judge_eligible_now"], 1);
}
