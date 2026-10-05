//! Operator-run review expiry, end to end through the CLI and a temporary
//! daemon on a synthetic repository: the age threshold, the dry run, a class
//! filter, `--yes`, the inbox status filter, reopening, and reviews that a
//! re-verification could still clear.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

/// Tests spawn CLIs that auto-start daemons; one at a time keeps every
/// child's pipes private (see `agent_integrations.rs`).
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

const NO_EVIDENCE: &str =
    "No readable, independently verified evidence from the successful source task";

fn git(root: &Path, args: &[&str]) {
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
        std::fs::write(
            repo.join("Makefile"),
            "test:\n\t@echo '1 passed; 0 failed'\n",
        )
        .unwrap();
        std::fs::write(repo.join(".gitignore"), ".engram/\n").unwrap();
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

    fn run(&self, args: &[&str]) -> Output {
        Command::new(assert_cmd::cargo::cargo_bin!("engram"))
            .env("HOME", self.root())
            .env_remove("ENGRAM_MODE")
            .args([
                "--db",
                self.root().join("brain.sqlite").to_str().unwrap(),
                "--socket",
                self.root().join("engramd.sock").to_str().unwrap(),
            ])
            .args(args)
            .output()
            .expect("cli")
    }

    fn cli(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("utf8")
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_str(&self.cli(args)).expect("json")
    }

    /// Complete one successful task that touched `lib.rs` and recorded this
    /// verification command (reported passing); return its review id.
    fn proposal(&self, prompt: &str, command: &str) -> String {
        let repo = self.repo();
        let task = self.json(&[
            "task",
            "start",
            prompt,
            "--path",
            repo.to_str().unwrap(),
            "--json",
        ]);
        let completion = self
            .root()
            .join(format!("{}.json", task["id"].as_str().unwrap()));
        std::fs::write(
            &completion,
            json!({"operation":"complete","task_id":task["id"],"outcome":"success",
                "summary":prompt,"files":["lib.rs"],
                "verifications":[{"kind":"test_result","strength":"deterministic",
                    "command":command,"exit_code":0,"artifact":"1 passed; 0 failed"}],
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
    fn move_on(&self) {
        let repo = self.repo();
        std::fs::write(repo.join("lib.rs"), "pub fn compute() -> u32 { 2 }\n").unwrap();
        git(&repo, &["commit", "--quiet", "-am", "compute returns 2"]);
        self.cli(&["index", repo.to_str().unwrap(), "--json"]);
    }

    fn db(&self) -> Connection {
        Connection::open(self.root().join("brain.sqlite")).unwrap()
    }

    fn status(&self, review: &str) -> String {
        self.db()
            .query_row(
                "SELECT status FROM lesson_reviews WHERE id=?1",
                [review],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn count(&self, sql: &str) -> i64 {
        self.db().query_row(sql, [], |row| row.get(0)).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.run(&["daemon", "stop"]);
    }
}

fn ids(reviews: &Value) -> Vec<String> {
    let mut ids: Vec<String> = reviews
        .as_array()
        .unwrap()
        .iter()
        .map(|review| review["id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    ids
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

#[test]
#[allow(clippy::too_many_lines)]
fn expiry_closes_reviews_only_new_work_can_unblock_and_reopens_them() {
    let f = Fixture::new();
    // A plain command a passing re-run would certify: the review stays.
    let runnable = f.proposal("Make compute return 2", "make test");
    // Refused re-runs: only new verification work can unblock these.
    let redirected = f.proposal("Document compute", "make test > /tmp/out.txt");
    let prose = f.proposal("Explain compute", "see the CI run");
    f.move_on();
    let expirable = sorted(vec![redirected.clone(), prose.clone()]);

    // Reviews younger than the default 14 days are not considered.
    let young = f.json(&["review", "expire", "--json"]);
    assert_eq!(
        (&young["pending"], &young["considered"], &young["expirable"]),
        (&json!(3), &json!(0), &json!(0)),
        "{young}"
    );

    // The dry run reports counts and sample ids, without claim text.
    let judgments = f.count("SELECT count(*) FROM lesson_judgments");
    let output = f.cli(&[
        "review",
        "expire",
        "--older-than",
        "0d",
        "--dry-run",
        "--json",
    ]);
    assert!(
        !output.contains("compute returns 2"),
        "no claim text without --verbose: {output}"
    );
    let dry: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(dry["applied"], false);
    assert_eq!(
        (&dry["considered"], &dry["expirable"], &dry["expired"]),
        (&json!(3), &json!(2), &json!(0))
    );
    assert_eq!(dry["by_class"], json!({"no_verified_evidence": 2}));
    assert_eq!(dry["kept"], json!({"reverify_may_clear": 1}));
    assert_eq!(dry["by_gate"], json!({NO_EVIDENCE: 3}));
    let projects = dry["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0]["project_name"], "repo");
    assert_eq!(projects[0]["expirable"], 2);
    let samples: Vec<String> =
        serde_json::from_value(projects[0]["sample_review_ids"].clone()).expect("sample ids");
    assert_eq!(sorted(samples), expirable);
    assert!(dry.get("reviews").is_none());
    for review in [&runnable, &redirected, &prose] {
        assert_eq!(f.status(review), "pending", "the dry run changes nothing");
    }
    // Without --yes the report is the same, and the terminal says so.
    let text = f.cli(&["review", "expire", "--older-than", "0d"]);
    assert!(text.contains("2 can expire"), "{text}");
    assert!(text.contains("Nothing changed"), "{text}");

    // --verbose lists every expirable review with the start of its claim.
    let verbose = f.json(&[
        "review",
        "expire",
        "--older-than",
        "0d",
        "--verbose",
        "--json",
    ]);
    let listed = verbose["reviews"].as_array().unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|review| {
        review["claim"]
            .as_str()
            .unwrap()
            .ends_with("compute returns 2")
            && review["gate_reason"] == NO_EVIDENCE
    }));

    // A class filter keeps the reviews of other classes.
    let filtered = f.json(&[
        "review",
        "expire",
        "--older-than",
        "0d",
        "--reason-class",
        "revision-changed",
        "--json",
    ]);
    assert_eq!(filtered["expirable"], 0);
    assert_eq!(
        filtered["kept"],
        json!({"class_not_selected": 2, "reverify_may_clear": 1})
    );

    // --yes expires them; every judgment audit row stays.
    let applied = f.json(&["review", "expire", "--older-than", "0d", "--yes", "--json"]);
    assert_eq!(
        (&applied["applied"], &applied["expired"]),
        (&json!(true), &json!(2))
    );
    assert_eq!(f.status(&runnable), "pending");
    assert_eq!(f.status(&redirected), "expired");
    assert_eq!(f.status(&prose), "expired");
    assert_eq!(f.count("SELECT count(*) FROM lesson_judgments"), judgments);
    assert_eq!(
        f.count(
            "SELECT count(*) FROM lesson_review_actions WHERE action='expired'
             AND gate_class='no_verified_evidence'"
        ),
        2
    );

    // The inbox status filter: expired reviews carry their expiry, and the
    // pending view no longer lists them.
    let expired = f.json(&["product", "show", "--reviews", "--status", "expired"]);
    assert_eq!(ids(&expired), expirable);
    for review in expired.as_array().unwrap() {
        assert_eq!(review["expiry"]["gate_class"], "no_verified_evidence");
        assert_eq!(review["expiry"]["gate_reason"], NO_EVIDENCE);
    }
    let pending = f.json(&["product", "show", "--reviews", "--status", "pending"]);
    assert_eq!(ids(&pending), std::slice::from_ref(&runnable));
    assert!(pending[0].get("expiry").is_none());
    let everything = f.json(&["product", "show", "--reviews"]);
    assert_eq!(everything.as_array().unwrap().len(), 3);
    let unknown = f.run(&["product", "show", "--reviews", "--status", "closed"]);
    assert!(!unknown.status.success());

    // Nothing is left to expire.
    let again = f.json(&["review", "expire", "--older-than", "0d", "--json"]);
    assert_eq!(
        (&again["pending"], &again["expirable"]),
        (&json!(1), &json!(0))
    );

    // Reopening returns a review to pending and keeps its history.
    let reopened = f.json(&["review", "reopen", &redirected, "--json"]);
    assert_eq!(reopened["status"], "pending");
    assert_eq!(f.status(&redirected), "pending");
    let pending = f.json(&["product", "show", "--reviews", "--status", "pending"]);
    assert_eq!(
        ids(&pending),
        sorted(vec![runnable.clone(), redirected.clone()])
    );
    assert_eq!(
        f.count(&format!(
            "SELECT count(*) FROM lesson_review_actions WHERE review_id='{redirected}'"
        )),
        2
    );
    let refused = f.run(&["review", "reopen", &runnable]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("only an expired review"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
}
