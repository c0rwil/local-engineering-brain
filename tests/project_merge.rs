//! `engram project merge` end to end: a real daemon, real Git checkouts of
//! one repository registered as two projects, and the CLI. Fixtures are
//! synthetic.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use engram::daemon::{DaemonClient, TaskStartRequest};
use engram::ids::ProjectId;
use tempfile::TempDir;

struct Workspace {
    temp: TempDir,
}

impl Workspace {
    fn new() -> Self {
        let temp = TempDir::new().expect("temp dir");
        let config = temp.path().join(".config/engram");
        fs::create_dir_all(&config).expect("config dir");
        fs::write(
            config.join("config.toml"),
            "[web]\nenabled = false\n\n[watch]\nenabled = false\n\n[vault]\nenabled = false\n\n\
             [jobs]\nenabled = false\n",
        )
        .expect("config");
        Self { temp }
    }

    fn root(&self) -> &Path {
        self.temp.path()
    }

    fn database(&self) -> PathBuf {
        self.root().join("brain.sqlite")
    }

    fn socket(&self) -> PathBuf {
        self.root().join("e.sock")
    }

    fn engram(&self, args: &[&str]) -> Output {
        Command::new(assert_cmd::cargo::cargo_bin!("engram"))
            .current_dir(self.root())
            .env("HOME", self.root())
            .env_remove("ENGRAM_WORKSPACE")
            .args(["--db", self.database().to_str().unwrap()])
            .args(["--socket", self.socket().to_str().unwrap()])
            .args(args)
            .output()
            .expect("run engram")
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let output = self.engram(args);
        assert!(
            output.status.success(),
            "engram {args:?} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("json output")
    }

    fn project_of(&self, checkout: &Path) -> String {
        self.json(&["status", checkout.to_str().unwrap(), "--json"])["project"]["id"]
            .as_str()
            .expect("project id")
            .to_owned()
    }

    fn client(&self) -> DaemonClient {
        DaemonClient::new(self.socket(), Duration::from_secs(30))
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if self.socket().exists() {
            let _ = self.engram(&["daemon", "stop", "--wait-secs", "10"]);
        }
    }
}

fn git(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit(repository: &Path, content: &str) {
    fs::create_dir_all(repository).expect("repository");
    git(repository, &["init", "-q"]);
    git(
        repository,
        &["config", "user.email", "engram@example.invalid"],
    );
    git(repository, &["config", "user.name", "Engram Tests"]);
    fs::write(repository.join("lib.rs"), content).expect("source");
    git(repository, &["add", "lib.rs"]);
    git(
        repository,
        &["-c", "commit.gpgsign=false", "commit", "-qm", "initial"],
    );
}

/// Register `checkout` while it is not yet a Git repository (so it gets its
/// own pinned id, as when Git was unreadable), then make it a clone of
/// `upstream` with the shared origin remote.
fn duplicate_checkout(workspace: &Workspace, upstream: &Path, name: &str) -> (PathBuf, String) {
    let checkout = workspace.root().join(name);
    fs::create_dir_all(&checkout).expect("checkout");
    let project = workspace.project_of(&checkout);
    git(&checkout, &["init", "-q"]);
    git(
        &checkout,
        &["fetch", "-q", upstream.to_str().unwrap(), "HEAD"],
    );
    git(&checkout, &["reset", "-q", "--hard", "FETCH_HEAD"]);
    git(
        &checkout,
        &["remote", "add", "origin", "git@example.com:org/shared.git"],
    );
    (checkout, project)
}

fn pin_of(checkout: &Path) -> String {
    fs::read_to_string(checkout.join(".engram/project.toml")).expect("pin")
}

#[test]
#[allow(clippy::too_many_lines)]
fn merging_a_duplicate_registration_end_to_end() {
    let workspace = Workspace::new();
    let upstream = workspace.root().join("upstream");
    commit(&upstream, "pub fn run() {}\npub fn helper() {}\n");
    let (first, survivor) = duplicate_checkout(&workspace, &upstream, "first");
    let (second, retired) = duplicate_checkout(&workspace, &upstream, "second");
    assert_ne!(survivor, retired);
    // Knowledge recorded under the duplicate id.
    let started = workspace.json(&[
        "task",
        "start",
        "synthetic task in the copy",
        "--path",
        second.to_str().unwrap(),
        "--json",
    ]);
    let task = started["id"].as_str().expect("task id").to_owned();

    // A dry run plans and changes nothing.
    let plan = workspace.json(&[
        "project",
        "merge",
        "--into",
        first.to_str().unwrap(),
        "--from",
        second.to_str().unwrap(),
        "--dry-run",
        "--json",
    ]);
    assert_eq!(plan["status"], "planned", "{plan}");
    assert_eq!(plan["plan"]["identity"]["same_repository"], true, "{plan}");
    assert_eq!(plan["plan"]["retired"]["id"], retired.as_str());
    assert_eq!(plan["pins"][0]["state"], "would_rewrite", "{plan}");
    let tasks_row = plan["plan"]["tables"]
        .as_array()
        .expect("tables")
        .iter()
        .find(|table| table["table"] == "tasks")
        .expect("tasks row")
        .clone();
    assert_eq!(tasks_row["rows"], 1, "{tasks_row}");

    // Without --yes or --dry-run: the plan, a refusal, nothing changed.
    let refused = workspace.engram(&[
        "project",
        "merge",
        "--into",
        first.to_str().unwrap(),
        "--from",
        second.to_str().unwrap(),
    ]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stdout).contains("Merge plan:"));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--yes"));
    assert!(pin_of(&second).contains(&retired));

    // Merge.
    let merged = workspace.json(&[
        "project",
        "merge",
        "--into",
        first.to_str().unwrap(),
        "--from",
        second.to_str().unwrap(),
        "--yes",
        "--json",
    ]);
    assert_eq!(merged["status"], "merged", "{merged}");
    assert_eq!(merged["follow_up"]["complete"], true, "{merged}");
    let backup = merged["outcome"]["audit"]["backup_path"]
        .as_str()
        .expect("backup path");
    assert!(Path::new(backup).is_file(), "backup at {backup}");
    assert_eq!(merged["follow_up"]["pins"][0]["state"], "rewritten");
    assert!(pin_of(&second).contains(&survivor), "{}", pin_of(&second));
    let reindexed = merged["follow_up"]["reindex"]
        .as_array()
        .expect("reindex")
        .iter()
        .map(|result| {
            (
                result["path"].as_str().unwrap().to_owned(),
                result["state"].clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(reindexed.len(), 2, "{reindexed:?}");
    assert!(
        reindexed.iter().all(|(_, state)| state == "indexed"),
        "{reindexed:?}"
    );
    assert!(
        reindexed[0].0.ends_with("second"),
        "retired checkouts first"
    );

    // The copy now resolves to the survivor and its task moved.
    assert_eq!(workspace.project_of(&second), survivor);
    let brief = workspace.json(&[
        "task",
        "start",
        "follow-up",
        "--path",
        second.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(brief["project_id"], survivor.as_str(), "{brief}");
    let database = engram::storage::Database::open(workspace.database()).expect("database");
    let task_project: String = database
        .with_reader(|connection| {
            connection
                .query_row(
                    "SELECT project_id FROM tasks WHERE id = ?1",
                    [&task],
                    |row| row.get(0),
                )
                .map_err(engram::Error::from)
        })
        .expect("task");
    assert_eq!(task_project, survivor);

    // A client that still sends the retired id (a hook sidecar written before
    // the merge, an MCP client) is redirected to the survivor.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let stale = runtime
        .block_on(workspace.client().start_task(&TaskStartRequest {
            context_request_id: None,
            project_id: ProjectId::parse(&retired).expect("retired id"),
            session_id: None,
            prompt: "sent with a stale project id".to_owned(),
        }))
        .expect("redirected task");
    assert_eq!(stale.project_id.as_str(), survivor);

    // A repeated merge reports the earlier one and has nothing left to do.
    let again = workspace.json(&[
        "project", "merge", "--into", "first", "--from", "second", "--yes", "--json",
    ]);
    assert_eq!(again["status"], "already_merged", "{again}");
    assert_eq!(again["follow_up"]["complete"], true, "{again}");
    assert_eq!(
        again["outcome"]["merge_event_id"],
        merged["outcome"]["merge_event_id"]
    );
    let doctor = workspace.json(&["doctor", "--json"]);
    let identity = doctor["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|check| check["name"] == "project_identity")
        .expect("identity check")
        .clone();
    assert_eq!(identity["status"], "pass", "{identity}");
}

#[test]
fn checkouts_of_different_repositories_are_never_merged() {
    let workspace = Workspace::new();
    let upstream = workspace.root().join("upstream");
    commit(&upstream, "pub fn run() {}\n");
    let (first, _) = duplicate_checkout(&workspace, &upstream, "first");
    let unrelated = workspace.root().join("unrelated");
    commit(&unrelated, "pub fn other() {}\n");
    git(
        &unrelated,
        &["remote", "add", "origin", "git@example.com:org/other.git"],
    );
    let other = workspace.project_of(&unrelated);

    let plan = workspace.engram(&[
        "project",
        "merge",
        "--into",
        first.to_str().unwrap(),
        "--from",
        &other,
        "--dry-run",
    ]);
    assert!(!plan.status.success(), "a refused plan fails");
    let text = String::from_utf8_lossy(&plan.stdout);
    assert!(
        text.contains("Refused: the checkouts are not one repository"),
        "{text}"
    );

    let executed = workspace.engram(&[
        "project",
        "merge",
        "--into",
        first.to_str().unwrap(),
        "--from",
        &other,
        "--yes",
    ]);
    assert!(!executed.status.success());
    let error = String::from_utf8_lossy(&executed.stderr);
    assert!(error.contains("merge refused"), "{error}");
    assert_eq!(workspace.project_of(&unrelated), other, "nothing changed");
    let backups = workspace.root().join("backups");
    assert!(
        !backups.exists() || fs::read_dir(&backups).expect("listing").next().is_none(),
        "no backup without a merge"
    );
}
