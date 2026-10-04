#![cfg(unix)]

//! A graceful daemon stop must preserve an in-flight judge call's audit and cost.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::{Json, Router, routing::post};
use engram::config::Config;
use engram::daemon::{DaemonClient, serve_until};
use engram::experience::{EvidenceStrength, ExperienceStore, TaskOutcome};
use engram::judge::JudgeStore;
use engram::product::ProductStore;
use engram::product::lessons::{Completion, LessonDraft, Verification};
use engram::projects::ProjectRegistry;
use engram::security::Redactor;
use engram::storage::Database;
use engram::world::{TreeSitterProvider, WorldModel};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::Notify;

fn git(root: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "-c",
            "user.name=Judge fixture",
            "-c",
            "user.email=judge-fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(arguments)
        .output()
        .expect("run fixture git");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn seed_pending_lessons(database_path: &Path, repository: &Path) {
    std::fs::create_dir_all(repository.join("src")).expect("repository");
    std::fs::write(repository.join(".gitignore"), ".engram/\n").expect("gitignore");
    std::fs::write(
        repository.join("src/auth.rs"),
        "pub fn authorized(token: Option<&str>) -> bool { token.is_some() }\n",
    )
    .expect("source");
    git(repository, &["init", "--quiet"]);
    git(repository, &["add", ".gitignore", "src/auth.rs"]);
    git(
        repository,
        &["commit", "--quiet", "-m", "Fixture authorization"],
    );

    let database = Database::open(database_path).expect("database");
    let project = ProjectRegistry::new(database.clone())
        .resolve_or_register(repository)
        .expect("register project");
    WorldModel::new(database.clone())
        .index(
            &TreeSitterProvider::new(10, 1_000_000, 100, 100, 2_000_000),
            &project,
            None,
        )
        .expect("index source anchors");
    let task = ExperienceStore::new(database.clone())
        .start_task(&project.project.id, "Verify authorization handling", None)
        .expect("task");
    let revision = project.git.and_then(|state| state.head);
    let completed = ProductStore::new(database, Redactor::default())
        .complete(&Completion {
            task_id: task.id,
            outcome: TaskOutcome::Success,
            accepted_by_user: None,
            final_commit: revision.clone(),
            summary: "Verified absent-token rejection".to_owned(),
            files: vec!["src/auth.rs".to_owned()],
            verifications: vec![Verification {
                kind: "test_result".to_owned(),
                strength: EvidenceStrength::Deterministic,
                command: Some("cargo test rejects_missing_token".to_owned()),
                exit_code: Some(0),
                artifact: "test rejects_missing_token ... ok\n1 passed; 0 failed".to_owned(),
                revision,
            }],
            lessons: [
                "Authorization rejects requests without a token.",
                "Missing credentials require an explicit authorization failure.",
            ]
            .into_iter()
            .map(|claim| LessonDraft {
                claim: claim.to_owned(),
                rationale: "Observed regression test".to_owned(),
                evidence_ids: Vec::new(),
                expires_at_ms: None,
            })
            .collect(),
        })
        .expect("complete source task");
    assert_eq!(completed.reviews.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_waits_for_judge_audit_and_usage_without_starting_next_review() {
    tokio::time::timeout(Duration::from_secs(15), shutdown_scenario())
        .await
        .expect("bounded judge shutdown scenario");
}

#[allow(clippy::too_many_lines)]
async fn shutdown_scenario() {
    let temp = TempDir::new().expect("temp");
    let database_path = temp.path().join("brain.sqlite");
    seed_pending_lessons(&database_path, &temp.path().join("repo"));

    let received = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let requests = Arc::new(AtomicUsize::new(0));
    let handler_received = Arc::clone(&received);
    let handler_release = Arc::clone(&release);
    let handler_requests = Arc::clone(&requests);
    let router = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(request): Json<Value>| {
            let received = Arc::clone(&handler_received);
            let release = Arc::clone(&handler_release);
            let requests = Arc::clone(&handler_requests);
            async move {
                requests.fetch_add(1, Ordering::SeqCst);
                received.notify_one();
                release.notified().await;
                let packet: Value = serde_json::from_str(
                    request["messages"][1]["content"].as_str().expect("packet"),
                )
                .expect("packet JSON");
                let verdict = json!({
                    "decision":"defer", "claim":packet["review"]["claim"],
                    "reason":"Further independent observations are required.",
                    "citations":[], "contradicts_memory_ids":[]
                });
                Json(json!({
                    "choices":[{"message":{"content":verdict.to_string()},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":321,"completion_tokens":87,"cost_usd":0.001}
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("model listener");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("model address")
    );
    let model = tokio::spawn(async move { axum::serve(listener, router).await });

    let mut config = Config::default();
    config.storage.db_path.clone_from(&database_path);
    config.daemon.socket_path = temp.path().join("engramd.sock");
    config.web.enabled = false;
    config.watch.enabled = false;
    config.jobs.enabled = false;
    config.vault.enabled = false;
    config.vault.root = temp.path().join("vault");
    config.judge.enabled = true;
    "shutdown-fixture".clone_into(&mut config.judge.model);
    config.judge.endpoint = endpoint;
    config.judge.timeout_seconds = 5;
    config.judge.batch_size = 2;
    let client = DaemonClient::new(&config.daemon.socket_path, Duration::from_secs(1));
    let mut daemon = tokio::spawn(serve_until(config, std::future::pending()));

    // The immediate scheduled pass must have dispatched a real HTTP request.
    tokio::time::timeout(Duration::from_secs(5), received.notified())
        .await
        .expect("judge inference started");
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    client.shutdown().await.expect("request graceful shutdown");
    let exited_early = tokio::time::timeout(Duration::from_millis(100), &mut daemon)
        .await
        .is_ok();
    release.notify_one();
    assert!(
        !exited_early,
        "daemon exited before its in-flight judge result was audited"
    );
    tokio::time::timeout(Duration::from_secs(5), daemon)
        .await
        .expect("daemon finished after inference")
        .expect("daemon task")
        .expect("graceful shutdown");
    model.abort();
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "shutdown dispatched another review"
    );

    let database = Database::open(database_path).expect("reopen database");
    let audits = JudgeStore::new(database.clone())
        .audits(10)
        .expect("audits");
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].outcome, "deferred");
    assert_eq!(audits[0].evaluation.usage.input_tokens, Some(321));
    assert_eq!(audits[0].evaluation.usage.output_tokens, Some(87));
    let ledger = database
        .with_reader(|connection| {
            Ok(connection.query_row(
                "SELECT count(*),sum(input_tokens),sum(output_tokens),sum(cost_usd)
                 FROM resource_usage WHERE source='internal_judge' AND sample_id=?1",
                [&audits[0].id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, f64>(3)?,
                    ))
                },
            )?)
        })
        .expect("usage ledger");
    assert_eq!((ledger.0, ledger.1, ledger.2), (1, 321, 87));
    assert!((ledger.3 - 0.001).abs() < f64::EPSILON);
}
