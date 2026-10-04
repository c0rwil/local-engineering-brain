//! `engram project merge` end to end: a real daemon, real Git checkouts of
//! one repository registered as two projects, and the CLI. Fixtures are
//! synthetic.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use engram::daemon::{
    DaemonClient, EvidenceCreateRequest, FactSetRequest, MemoryCreateRequest, TaskStartRequest,
    WorldSearchRequest,
};
use engram::experience::EvidenceStrength;
use engram::ids::ProjectId;
use engram::memory::MemoryDraft;
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

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn scalar(workspace: &Workspace, sql: &str, parameter: &str) -> String {
    let database = engram::storage::Database::open(workspace.database()).expect("database");
    database
        .with_reader(|connection| {
            connection
                .query_row(sql, [parameter], |row| row.get(0))
                .map_err(engram::Error::from)
        })
        .expect("query")
}

/// A survivor memory anchored to the symbol `run` in the survivor's index.
fn anchored_memory(workspace: &Workspace, survivor: &str) -> String {
    let client = workspace.client();
    let project_id = ProjectId::parse(survivor).expect("survivor id");
    let runtime = runtime();
    let nodes = runtime
        .block_on(client.search_world(&WorldSearchRequest {
            project_id: project_id.clone(),
            query: "run".to_owned(),
            limit: 10,
        }))
        .expect("search");
    let run = nodes
        .iter()
        .find(|node| node.label == "run")
        .expect("indexed symbol run");
    runtime
        .block_on(client.create_memory(&MemoryCreateRequest {
            project_id: project_id.clone(),
            memory: MemoryDraft {
                scope_type: "project".to_owned(),
                scope_id: survivor.to_owned(),
                kind: "lesson".to_owned(),
                claim: "run starts the synthetic worker".to_owned(),
                rationale: "synthetic fixture".to_owned(),
                confidence: 0.4,
                importance: 0.5,
                tags: Vec::new(),
                valid_from_commit: None,
                valid_to_commit: None,
                trust_source: "human_stated".to_owned(),
                evidence_ids: Vec::new(),
                source_task_ids: Vec::new(),
                node_ids: vec![run.id.clone()],
            },
        }))
        .expect("memory")
        .id
        .to_string()
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
    // The copy is on another commit where `run` differs. Re-indexing it as
    // part of the merge would quarantine the survivor's memory about `run`.
    fs::write(
        second.join("lib.rs"),
        "pub fn run() { helper(); }\npub fn helper() {}\n",
    )
    .expect("edit");
    git(
        &second,
        &[
            "-c",
            "user.email=engram@example.invalid",
            "-c",
            "user.name=Engram Tests",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qam",
            "change run",
        ],
    );
    let indexed = workspace.json(&["index", first.to_str().unwrap(), "--json"]);
    assert_eq!(
        indexed["resolved"]["project"]["id"],
        survivor.as_str(),
        "{indexed}"
    );
    let memory = anchored_memory(&workspace, &survivor);
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
    // One re-index, of the survivor's primary checkout; the copy's tree is
    // never scanned, so the survivor's memory about `run` stays active.
    let reindex = &merged["follow_up"]["reindex"];
    assert_eq!(reindex["state"], "indexed", "{reindex}");
    assert!(
        reindex["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("first")),
        "{reindex}"
    );
    assert_eq!(reindex["quarantined"], 0, "{reindex}");
    assert_eq!(
        scalar(
            &workspace,
            "SELECT status FROM memories WHERE id = ?1",
            &memory
        ),
        "active"
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
    let runtime = runtime();
    let stale = runtime
        .block_on(workspace.client().start_task(&TaskStartRequest {
            context_request_id: None,
            project_id: ProjectId::parse(&retired).expect("retired id"),
            session_id: None,
            prompt: "sent with a stale project id".to_owned(),
        }))
        .expect("redirected task");
    assert_eq!(stale.project_id.as_str(), survivor);
    // Only typed ids move: text that is, or quotes, the retired id is kept.
    let quoted = format!("see \"{retired}\"");
    let evidence = runtime
        .block_on(workspace.client().record_evidence(&EvidenceCreateRequest {
            project_id: ProjectId::parse(&retired).expect("retired id"),
            task_id: None,
            attempt_id: None,
            kind: "runtime_observation".to_owned(),
            strength: EvidenceStrength::Observational,
            commit_hash: None,
            command: Some(retired.clone()),
            exit_code: Some(0),
            artifact_text: quoted.clone(),
            metadata: serde_json::json!({"label": retired, "quoted": quoted}),
        }))
        .expect("evidence");
    let stored: (String, String, String, String) = {
        let database = engram::storage::Database::open(workspace.database()).expect("database");
        database
            .with_reader(|connection| {
                connection
                    .query_row(
                        "SELECT project_id, command, artifact_text, metadata_json
                         FROM evidence WHERE id = ?1",
                        [evidence.id.as_str()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .map_err(engram::Error::from)
            })
            .expect("evidence row")
    };
    assert_eq!(stored.0, survivor);
    assert_eq!(stored.1, retired, "the command keeps its text");
    assert_eq!(stored.2, quoted, "the artifact keeps its text");
    let metadata: serde_json::Value = serde_json::from_str(&stored.3).expect("metadata");
    assert_eq!(metadata["label"], retired.as_str(), "{metadata}");
    assert_eq!(metadata["quoted"], quoted.as_str(), "{metadata}");
    let prompt = runtime
        .block_on(workspace.client().start_task(&TaskStartRequest {
            context_request_id: None,
            project_id: ProjectId::parse(&retired).expect("retired id"),
            session_id: None,
            prompt: retired.clone(),
        }))
        .expect("task");
    assert_eq!(prompt.project_id.as_str(), survivor);
    assert_eq!(prompt.prompt, retired, "the prompt keeps its text");
    let fact = runtime
        .block_on(workspace.client().set_fact(&FactSetRequest {
            project_id: ProjectId::parse(&retired).expect("retired id"),
            key: "storage.note".to_owned(),
            value: retired.clone(),
            note: Some(quoted.clone()),
        }))
        .expect("fact");
    assert_eq!(
        (fact.value.as_str(), fact.note.as_deref()),
        (retired.as_str(), Some(quoted.as_str()))
    );
    assert_eq!(
        scalar(
            &workspace,
            "SELECT project_id FROM project_facts WHERE key = ?1",
            "storage.note"
        ),
        survivor
    );

    // A repeated merge reports the earlier one and has nothing left to do.
    let again = workspace.json(&[
        "project", "merge", "--into", "first", "--from", "second", "--yes", "--json",
    ]);
    assert_eq!(again["status"], "already_merged", "{again}");
    assert_eq!(again["follow_up"]["complete"], true, "{again}");
    assert_eq!(again["follow_up"]["reindex"]["state"], "skipped", "{again}");
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

/// A client that disconnects while the merge job runs (Ctrl-C during the
/// backup) must not leave the merged id without its redirect.
#[test]
fn redirects_are_installed_when_the_client_disconnects_mid_merge() {
    use std::io::Write as _;
    let workspace = Workspace::new();
    let upstream = workspace.root().join("upstream");
    commit(&upstream, "pub fn run() {}\n");
    let (first, survivor) = duplicate_checkout(&workspace, &upstream, "first");
    let (second, retired) = duplicate_checkout(&workspace, &upstream, "second");
    let body = serde_json::json!({
        "into": first.to_str().unwrap(),
        "from": second.to_str().unwrap(),
        "dry_run": false,
    })
    .to_string();
    let mut stream = std::os::unix::net::UnixStream::connect(workspace.socket()).expect("connect");
    write!(
        stream,
        "POST /api/v1/projects/merge HTTP/1.1\r\nhost: localhost\r\n\
         content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
    .expect("request");
    // Hang up as soon as the merge job starts writing its backup.
    let backups = workspace.root().join("backups");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !backups
        .read_dir()
        .is_ok_and(|mut entries| entries.next().is_some())
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the merge job never started"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(stream);
    let merged = || {
        let database = engram::storage::Database::open(workspace.database()).expect("database");
        database
            .with_reader(|connection| {
                connection
                    .query_row(
                        "SELECT merged_into FROM projects WHERE id = ?1",
                        [retired.as_str()],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .map_err(engram::Error::from)
            })
            .expect("project")
    };
    while merged().is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "the merge never committed"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(merged().as_deref(), Some(survivor.as_str()));
    let stale = runtime()
        .block_on(workspace.client().start_task(&TaskStartRequest {
            context_request_id: None,
            project_id: ProjectId::parse(&retired).expect("retired id"),
            session_id: None,
            prompt: "sent with the retired id after a disconnected merge".to_owned(),
        }))
        .expect("task");
    assert_eq!(
        stale.project_id.as_str(),
        survivor,
        "the redirect is in place"
    );
}
