use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::Command as AssertCommand;
use predicates::prelude::*;
use tempfile::TempDir;

use engram::storage::Database;

#[test]
fn doctor_json_reports_healthy_database() {
    let temp = TempDir::new().expect("temp dir");
    let db = temp.path().join("brain.sqlite");
    let mut command = AssertCommand::cargo_bin("engram").expect("engram binary");
    command
        .args(["--db", db.to_str().unwrap(), "doctor", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"ok\": true"))
        .stdout(predicate::str::contains("sqlite_integrity"));
}

#[test]
fn status_registers_plain_project_and_is_stable() {
    let temp = TempDir::new().expect("temp dir");
    let project = temp.path().join("project");
    fs::create_dir_all(&project).expect("project");
    let db = temp.path().join("brain.sqlite");
    let socket = temp.path().join("engramd.sock");
    let mut daemon = Command::new(assert_cmd::cargo::cargo_bin!("engramd"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon");
    let ready_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            break;
        }
        if let Some(status) = daemon.try_wait().expect("poll daemon process") {
            panic!("daemon exited before readiness: {status}");
        }
        assert!(Instant::now() < ready_deadline, "daemon readiness timeout");
        thread::sleep(Duration::from_millis(20));
    }

    let first = Command::new(assert_cmd::cargo::cargo_bin!("engram"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "status",
            project.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("first status");
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = Command::new(assert_cmd::cargo::cargo_bin!("engram"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "status",
            project.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("second status");
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let first_json: serde_json::Value = serde_json::from_slice(&first.stdout).expect("first json");
    let second_json: serde_json::Value =
        serde_json::from_slice(&second.stdout).expect("second json");
    assert_eq!(first_json["project"]["id"], second_json["project"]["id"]);
    assert_eq!(second_json["project_event_count"], 2);

    let stopped = Command::new(assert_cmd::cargo::cargo_bin!("engram"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "daemon",
            "stop",
        ])
        .output()
        .expect("stop daemon");
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let daemon_output = daemon.wait_with_output().expect("daemon exits gracefully");
    assert_process_success(&daemon_output);
}

#[test]
fn metrics_and_dashboard_render_from_a_running_daemon() {
    let temp = TempDir::new().expect("temp dir");
    let project = temp.path().join("project");
    fs::create_dir_all(&project).expect("project");
    let db = temp.path().join("brain.sqlite");
    let socket = temp.path().join("engramd.sock");
    let mut daemon = Command::new(assert_cmd::cargo::cargo_bin!("engramd"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon");
    let ready_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            break;
        }
        if let Some(status) = daemon.try_wait().expect("poll daemon process") {
            panic!("daemon exited before readiness: {status}");
        }
        assert!(Instant::now() < ready_deadline, "daemon readiness timeout");
        thread::sleep(Duration::from_millis(20));
    }

    let engram = |args: &[&str]| {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("engram"));
        command.args([
            "--db",
            db.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
        ]);
        command.args(args);
        let output = command.output().expect("run engram");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    // Register a project so the dashboard has canonical state to report.
    engram(&["status", project.to_str().unwrap()]);

    let metrics_json = engram(&["metrics", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&metrics_json).expect("metrics json");
    assert!(parsed["metrics"]["counters"]["context_compiles_brain"].is_u64());
    assert!(parsed["metrics"]["requests_ms"]["projects"]["count"].is_u64());

    let metrics_text = engram(&["metrics"]);
    assert!(metrics_text.contains("Counters"), "{metrics_text}");
    assert!(metrics_text.contains("Latency (ms)"), "{metrics_text}");

    let dashboard = engram(&["dashboard"]);
    for section in [
        "== Daemon ==",
        "== Cache ==",
        "== Latency ==",
        "== Context compiles ==",
        "== Memory ==",
        "== Learning ==",
    ] {
        assert!(dashboard.contains(section), "{dashboard}");
    }

    engram(&["daemon", "stop"]);
    let daemon_output = daemon.wait_with_output().expect("daemon exits gracefully");
    assert_process_success(&daemon_output);
}

fn assert_process_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn status_autostarts_daemon_without_direct_writes() {
    let temp = TempDir::new().expect("temp dir");
    let project = temp.path().join("project");
    fs::create_dir_all(&project).expect("project");
    let db = temp.path().join("brain.sqlite");
    let socket = temp.path().join("missing.sock");
    let mut command = AssertCommand::cargo_bin("engram").expect("engram binary");
    command
        .args([
            "--db",
            db.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "status",
            project.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Project:"));
    assert!(db.exists(), "the canonical daemon must create storage");
    assert!(socket.exists(), "the canonical daemon must own the socket");

    AssertCommand::cargo_bin("engram")
        .expect("engram binary")
        .args([
            "--db",
            db.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "daemon",
            "stop",
        ])
        .assert()
        .success();
    let shutdown_deadline = Instant::now() + Duration::from_secs(5);
    while socket.exists() && Instant::now() < shutdown_deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !socket.exists(),
        "auto-started daemon must shut down cleanly"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn jobs_cli_enqueues_and_lists_durable_background_jobs() {
    let temp = TempDir::new().expect("temp dir");
    let db = temp.path().join("brain.sqlite");
    let socket = temp.path().join("engramd.sock");
    let mut daemon = Command::new(assert_cmd::cargo::cargo_bin!("engramd"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon");
    let ready_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            break;
        }
        if let Some(status) = daemon.try_wait().expect("poll daemon process") {
            panic!("daemon exited before readiness: {status}");
        }
        assert!(Instant::now() < ready_deadline, "daemon readiness timeout");
        thread::sleep(Duration::from_millis(20));
    }

    let enqueued = Command::new(assert_cmd::cargo::cargo_bin!("engram"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "jobs",
            "enqueue",
            "fts_integrity",
            "--dedupe-key",
            "cli-test",
            "--json",
        ])
        .output()
        .expect("enqueue job");
    assert_process_success(&enqueued);
    let job: serde_json::Value = serde_json::from_slice(&enqueued.stdout).expect("job json");
    let job_id = job["id"].as_str().expect("job id").to_owned();
    assert_eq!(job["job_type"], "fts_integrity");

    // Job types outside the allowlist never reach the daemon.
    AssertCommand::cargo_bin("engram")
        .expect("engram binary")
        .args([
            "--db",
            db.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "jobs",
            "enqueue",
            "shell_exec",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unsupported job type"));

    let listed = Command::new(assert_cmd::cargo::cargo_bin!("engram"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "jobs",
            "list",
        ])
        .output()
        .expect("list jobs");
    assert_process_success(&listed);
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(stdout.contains(&job_id), "list shows the job: {stdout}");
    assert!(stdout.contains("fts_integrity"), "{stdout}");

    let shown = Command::new(assert_cmd::cargo::cargo_bin!("engram"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "jobs",
            "status",
            &job_id,
        ])
        .output()
        .expect("job status");
    assert_process_success(&shown);
    assert!(String::from_utf8_lossy(&shown.stdout).contains(&job_id));

    let stopped = Command::new(assert_cmd::cargo::cargo_bin!("engram"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "daemon",
            "stop",
        ])
        .output()
        .expect("stop daemon");
    assert_process_success(&stopped);
    let daemon_output = daemon.wait_with_output().expect("daemon exits gracefully");
    assert_process_success(&daemon_output);
}

#[test]
fn process_kill_rolls_back_event_and_projection_together() {
    let temp = TempDir::new().expect("temp dir");
    let db = temp.path().join("brain.sqlite");
    let ready = temp.path().join("ready");
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("engram"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "internal-crash-transaction",
            "--ready",
            ready.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn crash helper");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() && Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll child") {
            panic!("crash helper exited early: {status}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(ready.exists(), "crash helper did not enter its transaction");
    child.kill().expect("kill child");
    child.wait().expect("reap child");

    let database = Database::open(&db).expect("reopen database");
    let connection = database.connect().expect("connection");
    let projects: i64 = connection
        .query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))
        .expect("projects count");
    let events: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .expect("events count");
    assert_eq!((projects, events), (0, 0));
    assert_eq!(
        database.integrity_report().expect("integrity").integrity,
        "ok"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn synthesize_cli_creates_a_procedure_from_three_matching_episodes() {
    let temp = TempDir::new().expect("temp dir");
    let project = temp.path().join("project");
    fs::create_dir_all(&project).expect("project");
    let db = temp.path().join("brain.sqlite");
    let socket = temp.path().join("engramd.sock");
    let mut daemon = Command::new(assert_cmd::cargo::cargo_bin!("engramd"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon");
    let ready_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            break;
        }
        if let Some(status) = daemon.try_wait().expect("poll daemon process") {
            panic!("daemon exited before readiness: {status}");
        }
        assert!(Instant::now() < ready_deadline, "daemon readiness timeout");
        thread::sleep(Duration::from_millis(20));
    }

    let engram = |args: &[&str]| {
        let mut full = vec![
            "--db",
            db.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
        ];
        full.extend_from_slice(args);
        let output = Command::new(assert_cmd::cargo::cargo_bin!("engram"))
            .args(&full)
            .output()
            .expect("run engram");
        assert!(
            output.status.success(),
            "engram {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    for _ in 0..3 {
        let started: serde_json::Value = serde_json::from_slice(&engram(&[
            "task",
            "start",
            "fix worker lifecycle timeout",
            "--path",
            project.to_str().unwrap(),
        ]))
        .expect("task json");
        let task_id = started["id"].as_str().expect("task id").to_owned();
        engram(&[
            "task",
            "attempt",
            &task_id,
            "Inspect signal recipient",
            "--outcome",
            "success",
            "--file",
            "worker.rs",
        ]);
        engram(&[
            "task",
            "evidence",
            "test_result",
            "--path",
            project.to_str().unwrap(),
            "--task-id",
            &task_id,
            "--strength",
            "deterministic",
            "--command",
            "cargo test",
            "--exit-code",
            "0",
        ]);
        engram(&[
            "task",
            "finish",
            &task_id,
            "--outcome",
            "success",
            "--accepted-by-user",
            "true",
        ]);
    }

    let report: serde_json::Value = serde_json::from_slice(&engram(&[
        "synthesize",
        project.to_str().unwrap(),
        "--json",
    ]))
    .expect("synthesis json");
    assert_eq!(report["examined_episodes"], 3);
    let procedures = report["procedures"].as_array().expect("procedures");
    assert_eq!(procedures.len(), 1);
    assert_eq!(
        procedures[0]["memory"]["kind"],
        serde_json::json!("procedure")
    );
    assert_eq!(
        procedures[0]["memory"]["claim"],
        serde_json::json!("Procedure: inspect signal recipient")
    );
    assert_eq!(procedures[0]["reused_existing"], serde_json::json!(false));

    let stopped = Command::new(assert_cmd::cargo::cargo_bin!("engram"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "daemon",
            "stop",
        ])
        .output()
        .expect("stop daemon");
    assert_process_success(&stopped);
    let daemon_output = daemon.wait_with_output().expect("daemon exits gracefully");
    assert_process_success(&daemon_output);
}
