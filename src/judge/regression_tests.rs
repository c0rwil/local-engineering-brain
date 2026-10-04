//! Regressions measured in a live brain on 2026-10-04: path containment,
//! unavailable sources, re-judging churn, malformed output and contradictions.

use std::path::{Path, PathBuf};

use super::*;
use crate::experience::{EvidenceStrength, TaskOutcome};
use crate::ids::ProjectId;
use crate::memory::{MemoryDraft, MemoryStatus, MemoryStore};
use crate::product::lessons::{Completion, LessonDraft, Verification};
use crate::projects::ProjectRegistry;
use tempfile::TempDir;

const SOURCE: &[u8] = b"fn authorize(token: Option<&str>) -> bool { token.is_some() }\n";
const ARTIFACT: &str = "test rejects_missing_token ... ok\n1 passed; 0 failed";
const CLAIM: &str = "The authorization check rejects a request with a missing token.";
const CONFLICT: &str = "The authorization check accepts a request with a missing token.";

struct Lab {
    temp: TempDir,
    root: PathBuf,
    database: Database,
    project: ProjectId,
    store: JudgeStore,
    config: JudgeConfig,
}

fn git(root: &Path, arguments: &[&str]) -> String {
    let output = std::process::Command::new("git")
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
        .args(arguments)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn draft(project: &ProjectId, claim: &str) -> MemoryDraft {
    MemoryDraft {
        scope_type: "project".to_owned(),
        scope_id: project.to_string(),
        kind: "lesson".to_owned(),
        claim: claim.to_owned(),
        rationale: "fixture".to_owned(),
        confidence: 0.5,
        importance: 0.5,
        tags: Vec::new(),
        valid_from_commit: None,
        valid_to_commit: None,
        trust_source: "agent_claim".to_owned(),
        evidence_ids: Vec::new(),
        source_task_ids: Vec::new(),
        node_ids: Vec::new(),
    }
}

impl Lab {
    fn new(with_git: bool) -> Self {
        let temp = TempDir::new().expect("temp");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).expect("repo");
        std::fs::write(repo.join("src/auth.rs"), SOURCE).expect("source");
        if with_git {
            std::fs::write(repo.join(".gitignore"), ".engram/\n").expect("ignore");
            git(&repo, &["init", "--quiet"]);
            git(&repo, &["add", "."]);
            git(&repo, &["commit", "--quiet", "-m", "fixture"]);
        }
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&repo)
            .expect("project")
            .project
            .id;
        let lab = Self {
            root: repo.canonicalize().expect("canonical root"),
            temp,
            store: JudgeStore::new(database.clone()),
            database,
            project,
            config: JudgeConfig {
                enabled: true,
                model: "fixture-model".to_owned(),
                ..JudgeConfig::default()
            },
        };
        lab.scan("scan-a", "revision-a", 0);
        lab.index(&lab.project, "src/auth.rs", SOURCE, "scan-a", 0);
        lab
    }

    fn scan(&self, id: &str, head: &str, completed_at_ms: i64) {
        self.database.write_immediate(|tx| {
            tx.execute("INSERT INTO structural_scans(id,project_id,provider,provider_version,head,diff_fingerprint,started_at_ms,completed_at_ms,file_count,node_count,edge_count,incremental) VALUES(?1,?2,'native','test',?3,?4,0,?5,1,0,0,0)",
                params![id, self.project.as_str(), head, format!("fingerprint-{id}"), completed_at_ms])?;
            Ok(())
        }).expect("scan");
    }

    fn index(&self, project: &ProjectId, path: &str, bytes: &[u8], scan: &str, at_ms: i64) {
        self.database.write_immediate(|tx| {
            tx.execute("INSERT INTO indexed_files(project_id,path,language,content_digest,size_bytes,scan_id,indexed_at_ms) VALUES(?1,?2,'rust',?3,?4,?5,?6)
                ON CONFLICT(project_id,path) DO UPDATE SET content_digest=excluded.content_digest,scan_id=excluded.scan_id,indexed_at_ms=excluded.indexed_at_ms",
                params![project.as_str(), path, crate::experience::sha256(bytes), i64::try_from(bytes.len()).expect("size"), scan, at_ms])?;
            Ok(())
        }).expect("index");
    }

    fn register(&self, path: &Path) -> ProjectId {
        ProjectRegistry::new(self.database.clone())
            .resolve_or_register(path)
            .expect("register")
            .project
            .id
    }

    /// Complete one successful task and return its proposal ID.
    fn complete(
        &self,
        files: Vec<String>,
        revision: Option<&str>,
        evidence: bool,
        claim: &str,
    ) -> String {
        let task = ExperienceStore::new(self.database.clone())
            .start_task(
                &self.project,
                "Verify authorization handles absent tokens",
                None,
            )
            .expect("task");
        let completion = ProductStore::new(self.database.clone(), Redactor::default())
            .complete(&Completion {
                task_id: task.id,
                outcome: TaskOutcome::Success,
                accepted_by_user: None,
                final_commit: revision.map(str::to_owned),
                summary: "Verified absent token rejection".to_owned(),
                files,
                verifications: if evidence {
                    vec![Verification {
                        kind: "test_result".to_owned(),
                        strength: EvidenceStrength::Deterministic,
                        command: Some("cargo test rejects_missing_token".to_owned()),
                        exit_code: Some(0),
                        artifact: ARTIFACT.to_owned(),
                        revision: revision.map(str::to_owned),
                    }]
                } else {
                    Vec::new()
                },
                lessons: vec![LessonDraft {
                    claim: claim.to_owned(),
                    rationale: "Observed regression test".to_owned(),
                    evidence_ids: Vec::new(),
                    expires_at_ms: None,
                }],
            })
            .expect("complete");
        completion.reviews[0].id.clone()
    }

    fn packet(&self, review: &str) -> JudgePacket {
        let review = ProductStore::new(self.database.clone(), Redactor::default())
            .review_by_id(review)
            .expect("lookup")
            .expect("review");
        self.store.prepare(&self.config, review).expect("packet")
    }

    fn pending(&self) -> Vec<JudgePacket> {
        self.store.prepare_pending(&self.config).expect("pending")
    }

    fn judgments(&self) -> i64 {
        self.database
            .with_reader(|c| {
                Ok(c.query_row("SELECT COUNT(*) FROM lesson_judgments", [], |r| r.get(0))?)
            })
            .expect("count")
    }

    fn review_status(&self, review: &str) -> String {
        ProductStore::new(self.database.clone(), Redactor::default())
            .review_by_id(review)
            .expect("lookup")
            .expect("review")
            .status
    }
}

fn verdict(packet: &JudgePacket, decision: VerdictDecision) -> JudgeEvaluation {
    JudgeEvaluation {
        verdict: Some(JudgeVerdict {
            decision,
            claim: CLAIM.to_owned(),
            reason: "The named test checked rejection of missing tokens and passed.".to_owned(),
            citations: packet
                .evidence
                .iter()
                .take(1)
                .map(|evidence| Citation {
                    evidence_id: evidence.id.clone(),
                    quote: "test rejects_missing_token ... ok".to_owned(),
                })
                .collect(),
            contradicts_memory_ids: Vec::new(),
        }),
        error: None,
        usage: JudgeUsage {
            input_tokens: Some(100),
            output_tokens: Some(40),
            reported_cost_usd: None,
        },
        duration_ms: 12,
    }
}

#[cfg(unix)]
#[test]
fn absolute_paths_inside_a_registered_checkout_are_verified() {
    // Live cause of 171 deferrals: agents recorded absolute paths, and every
    // absolute path failed the relative-only containment check.
    let lab = Lab::new(false);
    let link = lab.temp.path().join("checkout-link");
    std::os::unix::fs::symlink(&lab.root, &link).expect("symlink");
    for recorded in [
        lab.root.join("src/auth.rs"),
        // On macOS the temporary directory is /var/..., canonically /private/var/...
        lab.temp.path().join("repo/src/auth.rs"),
        link.join("src/auth.rs"),
        lab.root.join("src/../src/auth.rs"),
    ] {
        let review = lab.complete(vec![recorded.display().to_string()], None, true, CLAIM);
        let packet = lab.packet(&review);
        assert!(
            packet.blocked_reason.is_none(),
            "{recorded:?}: {:?}",
            packet.blocked_reason
        );
        assert_eq!(
            packet.source_anchors["live"]["files"][0]["path"],
            "src/auth.rs"
        );
        assert_eq!(
            packet.source_anchors["files"][0]["path"], "src/auth.rs",
            "absolute paths must reach their index anchor"
        );
    }
    let review = lab.complete(
        vec![lab.root.join("src/auth.rs").display().to_string()],
        None,
        true,
        "Missing tokens are rejected by the authorization check.",
    );
    let packet = lab.packet(&review);
    let audit = lab
        .store
        .apply(
            &lab.config,
            &packet,
            &verdict(&packet, VerdictDecision::Accept),
        )
        .expect("apply");
    assert_eq!(audit.outcome, "accepted");
}

#[cfg(unix)]
#[test]
fn paths_outside_every_registered_checkout_stay_blocked() {
    let lab = Lab::new(false);
    let outside = lab.temp.path().join("outside");
    std::fs::create_dir_all(&outside).expect("outside");
    std::fs::write(outside.join("secret.rs"), b"secret").expect("secret");
    std::os::unix::fs::symlink(outside.join("secret.rs"), lab.root.join("src/link.rs"))
        .expect("escaping link");
    let unlinked = lab.temp.path().join("unlinked");
    std::fs::create_dir_all(&unlinked).expect("unlinked");
    std::fs::write(unlinked.join("lib.rs"), b"pub fn other() {}").expect("other source");
    lab.register(&unlinked);
    for (recorded, reason) in [
        (
            outside.join("secret.rs").display().to_string(),
            sources::OUTSIDE_CHECKOUTS,
        ),
        (
            unlinked.join("lib.rs").display().to_string(),
            sources::OUTSIDE_CHECKOUTS,
        ),
        ("../outside/secret.rs".to_owned(), sources::INVALID_PATH),
        ("src/link.rs".to_owned(), sources::RESOLVES_OUTSIDE),
    ] {
        let review = lab.complete(vec![recorded.clone()], None, true, CLAIM);
        assert_eq!(
            lab.packet(&review).blocked_reason.as_deref(),
            Some(reason),
            "{recorded}"
        );
    }
}

#[test]
fn explicitly_linked_repository_paths_use_that_repository_index() {
    let lab = Lab::new(false);
    let other = lab.temp.path().join("client");
    std::fs::create_dir_all(&other).expect("linked repository");
    let library = b"pub fn call_auth() {}\n";
    std::fs::write(other.join("lib.rs"), library).expect("linked source");
    let linked = lab.register(&other);
    lab.index(&linked, "lib.rs", library, "scan-a", 0);
    lab.database
        .write_immediate(|tx| {
            tx.execute("INSERT INTO project_links(project_id,linked_project_id,relation,note,created_at_ms) VALUES(?1,?2,'uses',NULL,0)",
                params![lab.project.as_str(), linked.as_str()])?;
            Ok(())
        })
        .expect("link");
    let recorded = other.canonicalize().expect("canonical").join("lib.rs");
    let review = lab.complete(vec![recorded.display().to_string()], None, true, CLAIM);
    let packet = lab.packet(&review);
    assert!(
        packet.blocked_reason.is_none(),
        "{:?}",
        packet.blocked_reason
    );
    let file = &packet.source_anchors["live"]["files"][0];
    assert_eq!(file["path"], "lib.rs");
    assert_eq!(file["project_id"], linked.as_str());
    std::fs::write(other.join("lib.rs"), b"pub fn changed() {}\n").expect("edit");
    assert_eq!(
        lab.packet(&review).blocked_reason.as_deref(),
        Some(sources::DIFFERS_FROM_INDEX)
    );
}

#[test]
fn the_linked_worktree_at_the_task_revision_is_the_live_checkout() {
    let lab = Lab::new(true);
    let primary_head = git(&lab.root, &["rev-parse", "HEAD"]);
    let worktree = lab.temp.path().join("feature");
    git(
        &lab.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "feature",
            worktree.to_str().expect("utf-8 path"),
        ],
    );
    git(
        &worktree,
        &["commit", "--quiet", "--allow-empty", "-m", "feature"],
    );
    let feature_head = git(&worktree, &["rev-parse", "HEAD"]);
    assert_eq!(lab.register(&worktree), lab.project);
    // Agents often record abbreviated hashes; this one names the worktree HEAD.
    // The index row comes from an older scan of identical bytes, which used
    // to block every such packet as "Indexed source revision differs".
    let short = &feature_head[..7];
    let review = lab.complete(vec!["src/auth.rs".to_owned()], Some(short), true, CLAIM);
    let packet = lab.packet(&review);
    assert!(
        packet.blocked_reason.is_none(),
        "{:?}",
        packet.blocked_reason
    );
    assert_eq!(packet.source_anchors["live"]["head"], feature_head.as_str());
    assert_eq!(
        packet.source_anchors["live"]["checkout"],
        worktree
            .canonicalize()
            .expect("worktree")
            .display()
            .to_string()
    );
    let review = lab.complete(
        vec!["src/auth.rs".to_owned()],
        Some(&primary_head),
        true,
        CLAIM,
    );
    let packet = lab.packet(&review);
    assert!(packet.blocked_reason.is_none());
    assert_eq!(packet.source_anchors["live"]["head"], primary_head.as_str());
    // No checkout sits at the recorded revision: compare the primary checkout.
    let missing = "f".repeat(40);
    let review = lab.complete(vec!["src/auth.rs".to_owned()], Some(&missing), true, CLAIM);
    let packet = lab.packet(&review);
    assert_eq!(packet.source_anchors["live"]["head"], primary_head.as_str());
    assert!(
        packet
            .blocked_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("revision differs")),
        "{:?}",
        packet.blocked_reason
    );
}

#[test]
fn deleted_and_unreadable_sources_have_distinct_reasons() {
    let lab = Lab::new(false);
    let review = lab.complete(vec!["src/removed.rs".to_owned()], None, true, CLAIM);
    assert_eq!(
        lab.packet(&review).blocked_reason.as_deref(),
        Some(sources::MISSING_FILE)
    );
    let review = lab.complete(
        vec!["src/auth.rs (session scratchpad)".to_owned()],
        None,
        true,
        CLAIM,
    );
    assert_eq!(
        lab.packet(&review).blocked_reason.as_deref(),
        Some(sources::MISSING_FILE),
        "annotated text is not a readable path"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let path = lab.root.join("src/auth.rs");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        // A superuser can still read the file; the reason is then not applicable.
        let readable = std::fs::read(&path).is_ok();
        let review = lab.complete(vec!["src/auth.rs".to_owned()], None, true, CLAIM);
        let reason = lab.packet(&review).blocked_reason;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("restore");
        if !readable {
            assert_eq!(reason.as_deref(), Some(sources::PERMISSION_DENIED));
        }
    }
}

#[cfg(unix)]
#[test]
fn an_unreadable_checkout_is_reported_as_an_access_problem() {
    // Live cause of "Source checkout unavailable": the daemon could not read
    // existing checkouts. A missing checkout and a refused one need different fixes.
    use std::os::unix::fs::PermissionsExt as _;
    let lab = Lab::new(false);
    let locked = lab.temp.path().join("locked");
    let checkout = locked.join("checkout");
    std::fs::create_dir_all(checkout.join("src")).expect("checkout");
    std::fs::write(checkout.join("src/auth.rs"), SOURCE).expect("source");
    let alias = checkout.canonicalize().expect("canonical");
    lab.database
        .write_immediate(|tx| {
            tx.execute(
                "UPDATE project_aliases SET path=?2 WHERE project_id=?1",
                params![lab.project.as_str(), alias.to_string_lossy()],
            )?;
            Ok(())
        })
        .expect("alias");
    let review = lab.complete(vec!["src/auth.rs".to_owned()], None, true, CLAIM);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("lock");
    // A superuser can still search the directory; the reason is then not applicable.
    let refused = checkout.canonicalize().is_err();
    let reason = lab.packet(&review).blocked_reason;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("unlock");
    if refused {
        assert_eq!(reason.as_deref(), Some(sources::CHECKOUT_ACCESS_DENIED));
    }
    assert!(
        lab.packet(&review).blocked_reason.is_none(),
        "the same review verifies once the checkout is readable"
    );
}

#[test]
fn unchanged_reviews_are_not_judged_again_after_volatile_changes() {
    // Live churn: one review was recorded 129 times because every pass saw a
    // new scan, re-written index rows, memory recency or a moved HEAD.
    let lab = Lab::new(true);
    let memories = MemoryStore::new(lab.database.clone());
    let first = memories
        .create_candidate(&lab.project, &draft(&lab.project, CONFLICT))
        .expect("memory")
        .id;
    let second = memories
        .create_candidate(
            &lab.project,
            &draft(
                &lab.project,
                "An authorization check rejects a request with a missing token header.",
            ),
        )
        .expect("memory")
        .id;
    let gated = lab.complete(vec!["src/auth.rs".to_owned()], None, false, CLAIM);
    let modelled = lab.complete(
        vec!["src/auth.rs".to_owned()],
        None,
        true,
        "Requests without a token fail the authorization check.",
    );
    let packets = lab.pending();
    assert_eq!(packets.len(), 2);
    for packet in &packets {
        let evaluation = if packet.review.id == gated {
            assert!(packet.blocked_reason.is_some());
            deferred_evaluation(packet, "gate")
        } else {
            assert_eq!(packet.review.id, modelled);
            assert!(
                packet.blocked_reason.is_none(),
                "{:?}",
                packet.blocked_reason
            );
            verdict(packet, VerdictDecision::Defer)
        };
        assert_eq!(
            lab.store
                .apply(&lab.config, packet, &evaluation)
                .expect("apply")
                .outcome,
            "deferred"
        );
    }
    assert_eq!(lab.judgments(), 2);
    for round in 1..=3_i64 {
        lab.database
            .write_immediate(|tx| {
                // Recency flips the related-memory order on every round.
                let bumped = if round % 2 == 0 { &first } else { &second };
                tx.execute(
                    "UPDATE memories SET updated_at_ms=updated_at_ms+?2 WHERE id=?1",
                    params![bumped.as_str(), 1_000 * round],
                )?;
                Ok(())
            })
            .expect("recency");
        let scan = format!("scan-{round}");
        lab.scan(&scan, &format!("head-{round}"), round);
        lab.index(&lab.project, "src/auth.rs", SOURCE, &scan, 0);
        git(
            &lab.root,
            &["commit", "--quiet", "--allow-empty", "-m", "unrelated"],
        );
        let worktree = lab.temp.path().join(format!("wt-{round}"));
        git(
            &lab.root,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                worktree.to_str().expect("utf-8 path"),
            ],
        );
        lab.register(&worktree);
        assert!(
            lab.pending().is_empty(),
            "round {round} re-judged an unchanged review"
        );
    }
    assert_eq!(lab.judgments(), 2);
    lab.database
        .write_immediate(|tx| {
            tx.execute(
                "UPDATE lesson_reviews SET rationale='Revised rationale',updated_at_ms=updated_at_ms+1 WHERE id=?1",
                [&gated],
            )?;
            Ok(())
        })
        .expect("revise proposal");
    let revised = lab.pending();
    assert_eq!(revised.len(), 1, "a revised proposal is reviewed again");
    assert_eq!(revised[0].review.id, gated);
}

#[test]
fn alternating_gate_results_are_each_recorded_once() {
    let lab = Lab::new(false);
    lab.complete(vec!["src/auth.rs".to_owned()], None, true, CLAIM);
    let path = lab.root.join("src/auth.rs");
    for _ in 0..3 {
        for changed in [false, true] {
            if changed {
                std::fs::write(&path, b"fn authorize(_: Option<&str>) -> bool { true }")
                    .expect("edit");
            } else {
                std::fs::remove_file(&path).expect("delete");
            }
            for packet in lab.pending() {
                lab.store
                    .apply(&lab.config, &packet, &deferred_evaluation(&packet, "gate"))
                    .expect("defer");
            }
        }
    }
    assert_eq!(
        lab.judgments(),
        2,
        "one missing-file and one changed-file row"
    );
}

#[test]
fn verdict_schema_lists_only_presented_identifiers() {
    let lab = Lab::new(false);
    let review = lab.complete(vec!["src/auth.rs".to_owned()], None, true, CLAIM);
    let isolated = schema::verdict_schema(&lab.packet(&review)).expect("schema");
    assert_eq!(
        isolated["properties"]["contradicts_memory_ids"],
        json!({"type": "array", "maxItems": 0})
    );
    let memory = MemoryStore::new(lab.database.clone())
        .create_candidate(&lab.project, &draft(&lab.project, CONFLICT))
        .expect("memory")
        .id;
    let packet = lab.packet(&review);
    let schema = schema::verdict_schema(&packet).expect("schema");
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["properties"]["claim"]["minLength"], 1);
    let citation = &schema["properties"]["citations"]["items"];
    assert_eq!(citation["additionalProperties"], false);
    assert_eq!(
        citation["properties"]["evidence_id"]["enum"],
        json!([packet.evidence[0].id])
    );
    assert_eq!(
        schema["properties"]["contradicts_memory_ids"]["items"]["enum"],
        json!([memory])
    );
}

#[tokio::test]
async fn local_requests_carry_the_strict_schema_unless_configured_otherwise() {
    use axum::{Json, Router, routing::post};
    use std::sync::{Arc, Mutex};
    let lab = Lab::new(false);
    let review = lab.complete(vec!["src/auth.rs".to_owned()], None, true, CLAIM);
    let packet = lab.packet(&review);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let deferral = json!({"decision":"defer","claim":CLAIM,"reason":"Needs more observations.",
        "citations":[],"contradicts_memory_ids":[]});
    let router = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(request): Json<Value>| {
            recorder
                .lock()
                .expect("lock")
                .push(request["response_format"].clone());
            let body = json!({"choices":[{"message":{"content":deferral.to_string()},"finish_reason":"stop"}]});
            async move { Json(body) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let mut config = lab.config.clone();
    config.endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("address")
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    for mode in [StructuredOutput::JsonSchema, StructuredOutput::JsonObject] {
        config.structured_output = mode;
        let evaluation = evaluate(&config, &packet).await;
        assert!(evaluation.error.is_none(), "{:?}", evaluation.error);
    }
    server.abort();
    let seen = seen.lock().expect("lock");
    assert_eq!(seen[0]["type"], "json_schema");
    assert_eq!(seen[0]["json_schema"]["strict"], true);
    assert_eq!(
        seen[0]["json_schema"]["schema"]["properties"]["citations"]["items"]["properties"]["evidence_id"]
            ["enum"],
        json!([packet.evidence[0].id])
    );
    assert_eq!(seen[1], json!({"type": "json_object"}));
}

#[test]
fn malformed_model_output_is_a_bounded_runtime_failure() {
    let lab = Lab::new(false);
    lab.complete(vec!["src/auth.rs".to_owned()], None, true, CLAIM);
    // Live sample: the model nested a top-level field inside a citation.
    let nested = json!({"decision":"reject","claim":CLAIM,"reason":"unsupported",
        "citations":[{"evidence_id":"evd_x","quote":"12345678","contradicts_memory_ids":[]}]});
    let response = json!({"choices":[{"message":{"content":nested.to_string()},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":10,"completion_tokens":5}});
    let evaluation =
        parse_response(&serde_json::to_vec(&response).expect("json")).expect("response");
    let error = evaluation.error.clone().expect("malformed");
    assert!(error.starts_with("Malformed judge output:"), "{error}");
    assert!(!error.contains("configuration error"), "{error}");
    let mut packet = lab.pending().remove(0);
    for attempt in 1..=lab.config.max_retries {
        assert_eq!(packet.attempt, attempt);
        let mut audit = lab
            .store
            .apply(&lab.config, &packet, &evaluation)
            .expect("apply");
        assert_eq!(audit.outcome, "runtime_error");
        assert!(audit.reason.starts_with("Malformed judge output:"));
        assert_eq!(
            audit.retry_after_ms.is_some(),
            attempt < lab.config.max_retries
        );
        assert!(lab.pending().is_empty(), "retry waits for its backoff");
        if attempt < lab.config.max_retries {
            audit.retry_after_ms = Some(0);
            lab.database
                .write_immediate(|tx| {
                    tx.execute(
                        "UPDATE lesson_judgments SET retry_after_ms=0,result_json=?1 WHERE id=?2",
                        params![serde_json::to_string(&audit)?, audit.id],
                    )?;
                    Ok(())
                })
                .expect("elapse backoff");
            packet = lab.pending().remove(0);
        }
    }
    // A schema-violating empty claim is a model failure, not a deferral.
    let review = lab.complete(
        vec!["src/auth.rs".to_owned()],
        None,
        true,
        "Requests without a token fail the authorization check.",
    );
    let packet = lab.packet(&review);
    let mut empty = verdict(&packet, VerdictDecision::Reject);
    empty.verdict.as_mut().expect("verdict").claim = String::new();
    let audit = lab
        .store
        .apply(&lab.config, &packet, &empty)
        .expect("apply");
    assert_eq!(audit.outcome, "runtime_error");
    assert_eq!(
        audit.reason,
        "Malformed judge output: verdict has missing or oversized fields"
    );
}

#[test]
fn conflicting_verdict_quarantines_memories_until_a_person_decides() {
    let lab = Lab::new(false);
    let memories = MemoryStore::new(lab.database.clone());
    let conflict = memories
        .create_candidate(&lab.project, &draft(&lab.project, CONFLICT))
        .expect("memory")
        .id;
    let review = lab.complete(vec!["src/auth.rs".to_owned()], None, true, CLAIM);
    let packet = lab.pending().remove(0);
    assert!(
        packet
            .review
            .related_memories
            .iter()
            .any(|memory| memory.id == conflict && !memory.exact)
    );
    let mut evaluation = verdict(&packet, VerdictDecision::Accept);
    evaluation
        .verdict
        .as_mut()
        .expect("verdict")
        .contradicts_memory_ids = vec![conflict.clone()];
    let audit = lab
        .store
        .apply(&lab.config, &packet, &evaluation)
        .expect("apply");
    assert_eq!(audit.outcome, "deferred");
    assert!(
        audit.reason.starts_with(CONFLICTS_AWAIT_REVIEW),
        "{}",
        audit.reason
    );
    assert_eq!(
        memories
            .get(&conflict)
            .expect("read")
            .expect("memory")
            .status,
        MemoryStatus::Suspect
    );
    assert_eq!(lab.review_status(&review), "pending");
    for _ in 0..2 {
        for packet in lab.pending() {
            assert_eq!(
                packet.blocked_reason.as_deref(),
                Some(CONFLICTS_AWAIT_REVIEW)
            );
            lab.store
                .apply(&lab.config, &packet, &deferred_evaluation(&packet, "gate"))
                .expect("defer");
        }
    }
    assert_eq!(lab.judgments(), 2, "the hand-off is recorded once");
    let product = ProductStore::new(lab.database.clone(), Redactor::default());
    let current = product
        .review_by_id(&review)
        .expect("lookup")
        .expect("review");
    product
        .review(&ReviewRequest {
            id: review.clone(),
            expected_updated_at_ms: current.updated_at_ms,
            decision: Decision::Reject,
            claim: None,
            rationale: None,
            contradicts: Vec::new(),
            reviewed_related: true,
            acknowledge_security_flags: false,
        })
        .expect("person rejects");
    assert_eq!(
        memories
            .get(&conflict)
            .expect("read")
            .expect("memory")
            .status,
        MemoryStatus::Active,
        "rejecting the proposal restores the quarantined memory"
    );
}

#[test]
fn a_person_can_confirm_a_quarantined_contradiction() {
    let lab = Lab::new(false);
    let memories = MemoryStore::new(lab.database.clone());
    let conflict = memories
        .create_candidate(&lab.project, &draft(&lab.project, CONFLICT))
        .expect("memory")
        .id;
    let review = lab.complete(vec!["src/auth.rs".to_owned()], None, true, CLAIM);
    let packet = lab.pending().remove(0);
    let mut evaluation = verdict(&packet, VerdictDecision::Defer);
    evaluation
        .verdict
        .as_mut()
        .expect("verdict")
        .contradicts_memory_ids = vec![conflict.clone()];
    lab.store
        .apply(&lab.config, &packet, &evaluation)
        .expect("apply");
    assert_eq!(
        memories
            .get(&conflict)
            .expect("read")
            .expect("memory")
            .status,
        MemoryStatus::Suspect
    );
    let product = ProductStore::new(lab.database.clone(), Redactor::default());
    let current = product
        .review_by_id(&review)
        .expect("lookup")
        .expect("review");
    let accepted = product
        .review(&ReviewRequest {
            id: review,
            expected_updated_at_ms: current.updated_at_ms,
            decision: Decision::Accept,
            claim: None,
            rationale: None,
            contradicts: vec![conflict.clone()],
            reviewed_related: true,
            acknowledge_security_flags: false,
        })
        .expect("person accepts");
    assert_eq!(accepted.status, "accepted");
    assert_eq!(
        memories
            .get(&conflict)
            .expect("read")
            .expect("memory")
            .status,
        MemoryStatus::Contradicted
    );
}

#[test]
fn conflicts_must_be_presented_and_rejections_quarantine_nothing() {
    let lab = Lab::new(false);
    let memories = MemoryStore::new(lab.database.clone());
    let conflict = memories
        .create_candidate(&lab.project, &draft(&lab.project, CONFLICT))
        .expect("memory")
        .id;
    let review = lab.complete(vec!["src/auth.rs".to_owned()], None, true, CLAIM);
    let packet = lab.pending().remove(0);
    let mut invented = verdict(&packet, VerdictDecision::Defer);
    invented
        .verdict
        .as_mut()
        .expect("verdict")
        .contradicts_memory_ids = vec![MemoryId::new()];
    let audit = lab
        .store
        .apply(&lab.config, &packet, &invented)
        .expect("apply");
    assert_eq!(audit.outcome, "runtime_error");
    assert!(audit.reason.starts_with("Malformed judge output:"));
    let mut rejection = verdict(&packet, VerdictDecision::Reject);
    rejection
        .verdict
        .as_mut()
        .expect("verdict")
        .contradicts_memory_ids = vec![conflict.clone()];
    let mut retry = packet.clone();
    retry.attempt = 2;
    let audit = lab
        .store
        .apply(&lab.config, &retry, &rejection)
        .expect("apply");
    assert_eq!(audit.outcome, "rejected");
    assert_eq!(lab.review_status(&review), "rejected");
    let stored = ProductStore::new(lab.database.clone(), Redactor::default())
        .review_by_id(&review)
        .expect("lookup")
        .expect("review");
    assert_eq!(
        stored.claim, CLAIM,
        "a rejection keeps the proposal wording"
    );
    assert_eq!(
        memories
            .get(&conflict)
            .expect("read")
            .expect("memory")
            .status,
        MemoryStatus::Active
    );
}

// Touched files the indexer does not cover, and recorded revisions that the
// live HEAD has moved past. Measured after integration: 3 reviews blocked on
// unindexed files and 28 on revision differences.

const CONFIG: &[u8] = b"retries: 3\n";

impl Lab {
    /// Replace a review's evidence with one hook-observed verification whose
    /// hook-only provenance recorded these checkout-relative source digests.
    fn hook_evidence(&self, review: &str, revision: Option<&str>, digests: &[(&str, &[u8])]) {
        let task: String = self
            .database
            .with_reader(|c| {
                Ok(c.query_row(
                    "SELECT task_id FROM lesson_reviews WHERE id=?1",
                    [review],
                    |r| r.get(0),
                )?)
            })
            .expect("review task");
        let digests: serde_json::Map<String, Value> = digests
            .iter()
            .map(|(path, bytes)| ((*path).to_owned(), json!(crate::experience::sha256(bytes))))
            .collect();
        let metadata = json!({"source": crate::agenthooks::CLAUDE_HOOK_SOURCE,
            "exit_status": "success_event", "source_digests": digests});
        let record = ExperienceStore::new(self.database.clone())
            .record_evidence(
                &self.project,
                Some(&crate::ids::TaskId::parse(&task).expect("task")),
                None,
                "test_result",
                EvidenceStrength::Deterministic,
                revision,
                Some("cargo test rejects_missing_token"),
                Some(0),
                ARTIFACT.as_bytes(),
                &metadata,
            )
            .expect("hook evidence");
        self.database
            .write_immediate(|tx| {
                tx.execute(
                    "UPDATE lesson_reviews SET evidence_ids_json=json_array(?1),
                     updated_at_ms=updated_at_ms+1 WHERE id=?2",
                    params![record.id.as_str(), review],
                )?;
                Ok(())
            })
            .expect("review evidence");
    }

    fn commit(&self, path: &str, bytes: &[u8], message: &str) {
        std::fs::write(self.root.join(path), bytes).expect("write");
        git(&self.root, &["add", path]);
        git(&self.root, &["commit", "--quiet", "-m", message]);
    }
}

fn live_file<'a>(packet: &'a JudgePacket, path: &str) -> &'a Value {
    packet.source_anchors["live"]["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|file| file["path"] == path)
        .expect("touched file")
}

#[test]
fn hook_digests_of_the_live_content_certify_unindexed_files() {
    let lab = Lab::new(false);
    std::fs::create_dir_all(lab.root.join("deploy")).expect("deploy");
    std::fs::write(lab.root.join("deploy/app.yml"), CONFIG).expect("config");
    let review = lab.complete(
        vec!["src/auth.rs".to_owned(), "deploy/app.yml".to_owned()],
        None,
        false,
        CLAIM,
    );
    lab.hook_evidence(
        &review,
        None,
        &[("deploy/app.yml", CONFIG), ("src/auth.rs", SOURCE)],
    );
    let packet = lab.packet(&review);
    assert!(
        packet.blocked_reason.is_none(),
        "{:?}",
        packet.blocked_reason
    );
    let config = live_file(&packet, "deploy/app.yml");
    assert!(config["indexed_digest"].is_null(), "the indexer skips YAML");
    assert_eq!(config["hook_digest_matches_live"], true);
    assert!(live_file(&packet, "src/auth.rs")["hook_digest_matches_live"].is_null());
    let audit = lab
        .store
        .apply(
            &lab.config,
            &packet,
            &verdict(&packet, VerdictDecision::Accept),
        )
        .expect("apply");
    assert_eq!(audit.outcome, "accepted");
}

#[test]
fn unindexed_files_without_matching_hook_digests_stay_deferred() {
    let lab = Lab::new(false);
    std::fs::create_dir_all(lab.root.join("deploy")).expect("deploy");
    std::fs::write(lab.root.join("deploy/app.yml"), CONFIG).expect("config");
    let touched = || vec!["deploy/app.yml".to_owned()];
    // Agent-reported evidence carries no hook digests and gets no new path.
    let agent = lab.complete(touched(), None, true, CLAIM);
    assert_eq!(
        lab.packet(&agent).blocked_reason.as_deref(),
        Some(sources::UNANCHORED_UNOBSERVED)
    );
    // A hook run that did not record this file.
    let unrecorded = lab.complete(touched(), None, false, CLAIM);
    lab.hook_evidence(&unrecorded, None, &[("src/auth.rs", SOURCE)]);
    assert_eq!(
        lab.packet(&unrecorded).blocked_reason.as_deref(),
        Some(sources::UNANCHORED_UNOBSERVED)
    );
    // The file changed after the hook-observed run: the digests disagree.
    let changed = lab.complete(touched(), None, false, CLAIM);
    lab.hook_evidence(&changed, None, &[("deploy/app.yml", b"retries: 1\n")]);
    assert_eq!(
        lab.packet(&changed).blocked_reason.as_deref(),
        Some(sources::UNANCHORED_CHANGED)
    );
    // Editing the file after a matching run blocks it the same way.
    let matched = lab.complete(touched(), None, false, CLAIM);
    lab.hook_evidence(&matched, None, &[("deploy/app.yml", CONFIG)]);
    assert!(lab.packet(&matched).blocked_reason.is_none());
    std::fs::write(lab.root.join("deploy/app.yml"), b"retries: 9\n").expect("edit");
    assert_eq!(
        lab.packet(&matched).blocked_reason.as_deref(),
        Some(sources::UNANCHORED_CHANGED)
    );
    // Files in an explicitly linked repository keep the index requirement.
    let other = lab.temp.path().join("client");
    std::fs::create_dir_all(&other).expect("linked repository");
    std::fs::write(other.join("client.yml"), CONFIG).expect("linked config");
    let linked = lab.register(&other);
    lab.database
        .write_immediate(|tx| {
            tx.execute("INSERT INTO project_links(project_id,linked_project_id,relation,note,created_at_ms) VALUES(?1,?2,'uses',NULL,0)",
                params![lab.project.as_str(), linked.as_str()])?;
            Ok(())
        })
        .expect("link");
    let recorded = other.canonicalize().expect("canonical").join("client.yml");
    let review = lab.complete(vec![recorded.display().to_string()], None, false, CLAIM);
    lab.hook_evidence(&review, None, &[("client.yml", CONFIG)]);
    assert_eq!(
        lab.packet(&review).blocked_reason.as_deref(),
        Some(sources::NOT_INDEXED)
    );
}

#[test]
fn a_newer_head_is_immaterial_when_the_touched_files_are_unchanged() {
    let lab = Lab::new(true);
    let recorded = git(&lab.root, &["rev-parse", "HEAD"]);
    // Agents often record abbreviated hashes (migration 0019 accepts 7+ digits).
    let short = recorded[..7].to_owned();
    let review = lab.complete(vec!["src/auth.rs".to_owned()], Some(&short), true, CLAIM);
    lab.commit("README.md", b"notes\n", "unrelated");
    let packet = lab.packet(&review);
    assert!(
        packet.blocked_reason.is_none(),
        "{:?}",
        packet.blocked_reason
    );
    assert_ne!(packet.source_anchors["live"]["head"], recorded.as_str());
    assert_eq!(
        packet.source_anchors["live"]["touched_files_unchanged_since"],
        json!([short])
    );
    let packets = lab.pending();
    assert_eq!(packets.len(), 1);
    lab.store
        .apply(
            &lab.config,
            &packets[0],
            &verdict(&packets[0], VerdictDecision::Defer),
        )
        .expect("defer");
    for round in 0..2 {
        lab.commit("README.md", format!("notes {round}\n").as_bytes(), "more");
        assert!(lab.pending().is_empty(), "unrelated commits re-judged it");
    }
    assert_eq!(lab.judgments(), 1);
    // Changing the touched file changes the gate result: recorded once.
    let changed = b"fn authorize(_: Option<&str>) -> bool { true }\n";
    lab.commit("src/auth.rs", changed, "change the touched file");
    lab.index(&lab.project, "src/auth.rs", changed, "scan-a", 0);
    for packet in lab.pending() {
        assert!(
            packet
                .blocked_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("revision differs")),
            "{:?}",
            packet.blocked_reason
        );
        lab.store
            .apply(&lab.config, &packet, &deferred_evaluation(&packet, "gate"))
            .expect("defer");
    }
    lab.commit("README.md", b"notes again\n", "unrelated again");
    assert!(lab.pending().is_empty());
    assert_eq!(lab.judgments(), 2);
}

#[test]
fn changed_files_and_unknown_revisions_keep_the_revision_gate() {
    let lab = Lab::new(true);
    let recorded = git(&lab.root, &["rev-parse", "HEAD"]);
    let reason = |review: &str| lab.packet(review).blocked_reason.unwrap_or_default();
    // The touched file changed between the recorded revision and HEAD; the
    // index already holds the new bytes, so only the revision check differs.
    let changed_review = lab.complete(vec!["src/auth.rs".to_owned()], Some(&recorded), true, CLAIM);
    let changed = b"fn authorize(_: Option<&str>) -> bool { true }\n";
    lab.commit("src/auth.rs", changed, "change the touched file");
    lab.index(&lab.project, "src/auth.rs", changed, "scan-a", 0);
    assert!(reason(&changed_review).contains("revision differs"));
    assert!(
        lab.packet(&changed_review).source_anchors["live"]
            .get("touched_files_unchanged_since")
            .is_none()
    );
    // A revision the repository does not contain.
    let unknown = "0123456789abcdef0123456789abcdef01234567";
    let missing = lab.complete(vec!["src/auth.rs".to_owned()], Some(unknown), true, CLAIM);
    assert!(reason(&missing).contains("revision differs"));
    // Fewer than seven hexadecimal digits is never passed to Git.
    let current = git(&lab.root, &["rev-parse", "HEAD"]);
    lab.commit("README.md", b"notes\n", "unrelated");
    let too_short = lab.complete(
        vec!["src/auth.rs".to_owned()],
        Some(&current[..6]),
        true,
        CLAIM,
    );
    assert!(reason(&too_short).contains("revision differs"));
    // The same revision at seven digits passes: the touched file is unchanged.
    let seven = lab.complete(
        vec!["src/auth.rs".to_owned()],
        Some(&current[..7]),
        true,
        CLAIM,
    );
    assert_eq!(lab.packet(&seven).blocked_reason, None);
    // Without touched files nothing shows that the difference is immaterial.
    let untouched = lab.complete(Vec::new(), Some(&current), true, CLAIM);
    assert!(reason(&untouched).contains("revision differs"));
}
