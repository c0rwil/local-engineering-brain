//! Origin refresh isolation and evidence-state-preserving drift recovery.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use engram::daemon::{DaemonClient, MemoryCreateRequest};
use engram::ids::{MemoryId, NodeId, ProjectId};
use engram::memory::MemoryDraft;
use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    daemon: Child,
}
impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let config = temp.path().join("config.toml");
        fs::write(
            &config,
            toml::to_string(&serde_json::json!({
                "storage":{"db_path":temp.path().join("brain.sqlite")},
                "daemon":{"socket_path":temp.path().join("d.sock")},
                "web":{"enabled":false},"watch":{"enabled":false},
                "jobs":{"enabled":false},"vault":{"enabled":false},
                "architect":{"auto":"off"}
            }))
            .unwrap(),
        )
        .unwrap();
        let mut daemon = Command::new(assert_cmd::cargo::cargo_bin!("engramd"))
            .arg("--config")
            .arg(config)
            .current_dir(temp.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::os::unix::net::UnixStream::connect(temp.path().join("d.sock")).is_err() {
            assert!(daemon.try_wait().unwrap().is_none(), "daemon exited");
            assert!(Instant::now() < deadline, "daemon readiness timeout");
            std::thread::sleep(Duration::from_millis(20));
        }
        Self { temp, daemon }
    }
    fn root(&self) -> &Path {
        self.temp.path()
    }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(assert_cmd::cargo::cargo_bin!("engram"))
            .current_dir(self.root().join("repo"))
            .arg("--db")
            .arg(self.root().join("brain.sqlite"))
            .arg("--socket")
            .arg(self.root().join("d.sock"))
            .args(args)
            .output()
            .unwrap()
    }
    fn json(&self, args: &[&str]) -> Value {
        let o = self.cli(args);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_slice(&o.stdout).expect("stdout must contain only JSON")
    }
    fn connection(&self) -> Connection {
        Connection::open_with_flags(
            self.root().join("brain.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap()
    }
    fn memory(&self, repo: &Path) -> MemoryId {
        let status = self.json(&["status", repo.to_str().unwrap(), "--json"]);
        let project_id = ProjectId::parse(status["project"]["id"].as_str().unwrap()).unwrap();
        let c = self.connection();
        let nodes=c.prepare("SELECT id FROM nodes WHERE project_id=?1 AND file_path='lib.rs' AND tombstoned_at_ms IS NULL").unwrap().query_map([project_id.as_str()],|r|r.get::<_,String>(0)).unwrap().map(|r|NodeId::parse(&r.unwrap()).unwrap()).collect::<Vec<_>>();
        assert!(!nodes.is_empty());
        let client = DaemonClient::new(self.root().join("d.sock"), Duration::from_secs(10));
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(client.create_memory(&MemoryCreateRequest {
                project_id: project_id.clone(),
                memory: MemoryDraft {
                    scope_type: "project".into(),
                    scope_id: project_id.to_string(),
                    kind: "lesson".into(),
                    claim: "Keep the compute contract stable".into(),
                    rationale: "Fixture source anchors".into(),
                    confidence: 0.5,
                    importance: 0.5,
                    tags: vec![],
                    valid_from_commit: None,
                    valid_to_commit: None,
                    trust_source: "agent_inference".into(),
                    evidence_ids: vec![],
                    source_task_ids: vec![],
                    node_ids: nodes,
                },
            }))
            .unwrap()
            .id
    }
    fn index(&self, repo: &Path) {
        self.json(&["index", repo.to_str().unwrap(), "--json"]);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}
fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "fixture@example.invalid"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    fs::write(repo.join(".gitignore"), ".engram/\n").unwrap();
    fs::write(repo.join("lib.rs"), "pub fn compute() -> u32 { 1 }\n").unwrap();
    git(&repo, &["add", ".gitignore", "lib.rs"]);
    git(&repo, &["commit", "-qm", "base"]);
    repo
}

#[test]
fn cli_origin_refresh_keeps_local_graph_and_candidates_separate() {
    let f = Fixture::new();
    let repo = repo(f.root());
    let remote = f.root().join("origin.git");
    git(
        f.root(),
        &[
            "clone",
            "-q",
            "--bare",
            repo.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    fs::write(repo.join("lib.rs"), "pub fn compute() -> u32 { 2 }\n").unwrap();
    git(&repo, &["commit", "-qam", "local change"]);
    f.index(&repo);
    let memory = f.memory(&repo);
    let c = f.connection();
    let before:String=c.query_row("SELECT current_digest FROM nodes WHERE id IN (SELECT node_id FROM memory_nodes WHERE memory_id=?1) ORDER BY id LIMIT 1",[memory.as_str()],|r|r.get(0)).unwrap();
    let result = f.json(&["index", repo.to_str().unwrap(), "--origin", "--json"]);
    assert_eq!(result["reference"], "origin/main");
    assert_eq!(
        f.json(&["index", "--origin", "--json"])["reference"],
        "origin/main"
    );
    let after:String=c.query_row("SELECT current_digest FROM nodes WHERE id IN (SELECT node_id FROM memory_nodes WHERE memory_id=?1) ORDER BY id LIMIT 1",[memory.as_str()],|r|r.get(0)).unwrap();
    assert_eq!(before, after);
    let m = f.json(&["memory", "show", memory.as_str(), "--json"]);
    assert_eq!(m["status"], "active");
    assert_eq!(m["epistemic_state"], "candidate");
    let shadow_count: i64 = c
        .query_row(
            "SELECT count(*) FROM projects WHERE name LIKE '%@origin'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(shadow_count, 1);
    assert_eq!(
        fs::read_to_string(repo.join("lib.rs")).unwrap(),
        "pub fn compute() -> u32 { 2 }\n"
    );
    assert!(
        !f.cli(&[
            "index",
            repo.to_str().unwrap(),
            "--origin",
            "--changed",
            "lib.rs"
        ])
        .status
        .success()
    );
}

#[test]
fn cli_drift_restore_requires_exact_event_and_recovered_source_without_promoting() {
    for validated in [false, true] {
        let f = Fixture::new();
        let repo = repo(f.root());
        f.index(&repo);
        let memory = f.memory(&repo);
        if validated {
            f.json(&[
                "memory",
                "promote",
                memory.as_str(),
                "--to",
                "validated",
                "--human-override",
                "--json",
            ]);
        }
        let original = fs::read(repo.join("lib.rs")).unwrap();
        fs::write(repo.join("lib.rs"), "pub fn compute() -> u32 { 9 }\n").unwrap();
        f.index(&repo);
        let c = f.connection();
        let event: i64=c.query_row("SELECT max(sequence) FROM events WHERE event_type='MEMORY_STATE_CHANGED' AND json_extract(payload_json,'$.data.memory_id')=?1",[memory.as_str()],|r|r.get(0)).unwrap();
        assert!(
            !f.cli(&[
                "memory",
                "restore-drift",
                memory.as_str(),
                "--event",
                &event.to_string(),
                "--json"
            ])
            .status
            .success()
        );
        fs::write(repo.join("lib.rs"), original).unwrap();
        f.index(&repo);
        assert!(
            !f.cli(&[
                "memory",
                "restore-drift",
                memory.as_str(),
                "--event",
                &(event + 1).to_string(),
                "--json"
            ])
            .status
            .success()
        );
        let restored = f.json(&[
            "memory",
            "restore-drift",
            memory.as_str(),
            "--event",
            &event.to_string(),
            "--json",
        ]);
        assert_eq!(restored["status"], "active");
        assert_eq!(
            restored["epistemic_state"],
            if validated { "validated" } else { "candidate" }
        );
        assert_eq!(restored["confidence"], 0.5);
        assert!(
            !f.cli(&[
                "memory",
                "restore-drift",
                memory.as_str(),
                "--event",
                &event.to_string()
            ])
            .status
            .success()
        );
        f.json(&["memory", "forget", memory.as_str(), "--json"]);
        assert!(
            !f.cli(&[
                "memory",
                "restore-drift",
                memory.as_str(),
                "--event",
                &event.to_string()
            ])
            .status
            .success()
        );
    }
}
