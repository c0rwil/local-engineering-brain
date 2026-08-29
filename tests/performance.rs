use std::time::Instant;

use leb::config::Config;
use leb::context::{ContextCompileRequest, ContextCompiler, ContextMode};
use leb::daemon::DaemonClient;
use leb::memory::{EpistemicState, MemoryDraft, MemoryStore};
use leb::projects::ProjectRegistry;
use leb::storage::Database;
use tempfile::TempDir;
use tokio::sync::oneshot;

#[test]
fn warmed_context_compile_stays_under_hard_v1_ceiling() {
    let temp = TempDir::new().expect("temp");
    let root = temp.path().join("medium-project");
    std::fs::create_dir(&root).expect("project");
    let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
    let project = ProjectRegistry::new(database.clone())
        .resolve_or_register(&root)
        .expect("project")
        .project
        .id;
    let memories = MemoryStore::new(database.clone());
    for index in 0..100 {
        let memory = memories
            .create_candidate(
                &project,
                &MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: project.to_string(),
                    kind: "lesson".to_owned(),
                    claim: format!(
                        "Scheduler cleanup rule {index} requires deterministic resource release"
                    ),
                    rationale: "Synthetic warmed-repository performance fixture".to_owned(),
                    confidence: 0.5,
                    importance: 0.5,
                    tags: vec!["scheduler".to_owned(), "cleanup".to_owned()],
                    valid_from_commit: None,
                    valid_to_commit: None,
                    trust_source: "human_instruction".to_owned(),
                    evidence_ids: Vec::new(),
                    source_task_ids: Vec::new(),
                    node_ids: Vec::new(),
                },
            )
            .expect("candidate");
        memories
            .promote(&memory.id, EpistemicState::Validated, true)
            .expect("validated fixture");
    }
    let compiler = ContextCompiler::new(database, 6_000, 1_800);
    let mut elapsed = Vec::new();
    for index in 0..30 {
        let started = Instant::now();
        let context = compiler
            .compile(&ContextCompileRequest {
                request_id: format!("perf-{index}"),
                project_id: project.clone(),
                task_text: "fix scheduler cleanup and deterministic resource release".to_owned(),
                current_paths: Vec::new(),
                token_budget: 2_000,
                mode: ContextMode::Brain,
                repository_root: Some(root.clone()),
                head: None,
                task_id: None,
            })
            .expect("compile");
        assert!(context.metrics.selected_tokens <= 2_000);
        elapsed.push(started.elapsed());
    }
    elapsed.sort_unstable();
    let p95 = elapsed[(elapsed.len() * 95).div_ceil(100) - 1];
    assert!(
        p95.as_millis() < 300,
        "warmed context p95 {} ms exceeded V1 hard ceiling",
        p95.as_millis()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn medium_daemon_concurrency_stays_within_latency_rss_and_token_bounds() {
    let temp = TempDir::new().expect("temp");
    let root = temp.path().join("medium-repository");
    std::fs::create_dir(&root).expect("root");
    for index in 0..500 {
        std::fs::write(
            root.join(format!("module_{index}.rs")),
            format!("pub fn function_{index}() -> usize {{ {index} }}\n"),
        )
        .expect("fixture source");
    }
    let mut config = Config::default();
    config.storage.db_path = temp.path().join("brain.sqlite");
    config.daemon.socket_path = temp.path().join("lebd.sock");
    config.daemon.request_timeout_ms = 5_000;
    let socket = config.daemon.socket_path.clone();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(leb::daemon::serve_until(config, async move {
        let _ = stop_rx.await;
    }));
    let client = DaemonClient::new(&socket, std::time::Duration::from_secs(5));
    for _ in 0..100 {
        if client.health().await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let index = client
        .index_project(&root, None)
        .await
        .expect("medium index");
    assert_eq!(index.index.parsed_files, 500);

    let mut requests = Vec::new();
    for ordinal in 0..32_u32 {
        let client = client.clone();
        let project_id = index.resolved.project.id.clone();
        let root = root.clone();
        requests.push(tokio::spawn(async move {
            let started = Instant::now();
            let context = client
                .compile_context(&ContextCompileRequest {
                    request_id: format!("perf-daemon-{ordinal}"),
                    project_id,
                    task_text: format!("inspect function {ordinal}"),
                    current_paths: vec![format!("module_{ordinal}.rs")],
                    token_budget: 512,
                    mode: ContextMode::Brain,
                    repository_root: Some(root),
                    head: None,
                    task_id: None,
                })
                .await
                .expect("context");
            assert!(context.metrics.selected_tokens <= 512);
            started.elapsed()
        }));
    }
    let mut elapsed = Vec::new();
    for request in requests {
        elapsed.push(request.await.expect("request join"));
    }
    elapsed.sort_unstable();
    let percentile = |value: usize| elapsed[(elapsed.len() * value).div_ceil(100) - 1];
    let p50 = percentile(50);
    let p95 = percentile(95);
    let p99 = percentile(99);
    assert!(
        p95.as_millis() < 300,
        "daemon context p50/p95/p99 = {}/{}/{} ms",
        p50.as_millis(),
        p95.as_millis(),
        p99.as_millis()
    );
    let health = client.health().await.expect("health");
    if let Some(rss) = health.rss_bytes {
        assert!(rss < health.rss_hard_limit_bytes);
    }
    let _ = stop_tx.send(());
    server.await.expect("server join").expect("server result");
}
