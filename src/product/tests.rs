use super::*;
use crate::experience::{EvidenceStrength, ExperienceStore, TaskOutcome};
use crate::memory::MemoryStore;
use crate::projects::{ProjectRegistry, ResolvedProject};
use crate::retrieval::{RetrievalRequest, Retriever};
use crate::world::{TreeSitterProvider, WorldModel};
use lessons::{Completion, Decision, LessonDraft, ReviewRequest, Verification};
use serde_json::{Value, json};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    database: Database,
    project: ResolvedProject,
    store: ProductStore,
}
impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("lib.rs"), "pub fn run() -> u32 { 1 }\n").unwrap();
        std::fs::write(root.join("other.rs"), "pub fn unrelated() {}\n").unwrap();
        let database = Database::open(temp.path().join("brain.sqlite")).unwrap();
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .unwrap();
        let store = ProductStore::new(database.clone(), Redactor::default());
        let result = Self {
            _temp: temp,
            database,
            project,
            store,
        };
        result.index();
        result
    }
    fn index(&self) {
        WorldModel::new(self.database.clone())
            .index(
                &TreeSitterProvider::new(100, 1_000_000, 10_000, 20_000, 16_000_000),
                &self.project,
                None,
            )
            .unwrap();
    }
    #[allow(clippy::needless_pass_by_value)]
    fn save(&self, id: &str, version: u64, item: Value) -> Result<Record> {
        self.store.mutate(
            serde_json::from_value(
                json!({"operation":"save","id":id,"expected_version":version,"item":item}),
            )
            .unwrap(),
        )
    }
    fn entity(&self, id: &str, kind: &str) -> Record {
        self.save(
            id,
            0,
            json!({"type":"entity","value":{"kind":kind,"name":id}}),
        )
        .unwrap()
    }
    fn path(&self, path: &str) -> Value {
        json!({"project_id":self.project.project.id,"path":path})
    }
    fn freshness(&self, id: &str) -> Freshness {
        self.store
            .snapshot()
            .unwrap()
            .records
            .into_iter()
            .find(|r| r.id == id)
            .unwrap()
            .freshness
    }
}

#[test]
fn catalog_versions_redaction_cycles_and_parallel_contracts() {
    let f = Fixture::new();
    f.entity("product", "product");
    let service=f.save("service",0,json!({"type":"entity","value":{"kind":"service","name":"billing sk_test_fakeKey123","parent_id":"product"}})).unwrap();
    assert!(
        !serde_json::to_string(&service)
            .unwrap()
            .contains("sk_test_fakeKey123")
    );
    assert!(
        f.save(
            "service",
            0,
            json!({"type":"entity","value":{"kind":"service","name":"lost update"}})
        )
        .is_err()
    );
    assert!(f.save("product",1,json!({"type":"entity","value":{"kind":"product","name":"product","parent_id":"service"}})).is_err());
    f.entity("api", "api");
    for (id, contract) in [("read", "GET /quota"), ("write", "POST /quota")] {
        f.save(id,0,json!({"type":"dependency","value":{"source_id":"service","target_id":"api","relation":"calls","contract":contract}})).unwrap();
    }
    assert_eq!(
        f.store
            .snapshot()
            .unwrap()
            .records
            .iter()
            .filter(|r| matches!(r.item, Item::Dependency(_)))
            .count(),
        2
    );
    let mut item = service.item.clone();
    if let Item::Entity(e) = &mut item {
        e.owner = "platform team".to_owned();
    }
    f.store
        .mutate(Mutation::Save {
            id: Some(service.id.clone()),
            expected_version: 1,
            item: Box::new(item),
        })
        .unwrap();
    let history = f.store.history(&service.id).unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].version, 2);
}

#[test]
fn sections_track_only_their_sources_preserve_curated_text_and_archive_after_deletion() {
    let f = Fixture::new();
    f.entity("service", "service");
    for (id, path) in [("behavior", "lib.rs"), ("unrelated", "other.rs")] {
        f.save(id,0,json!({"type":"section","value":{"entity_id":"service","title":id,"curated":"Human explanation","reviewed":true,"anchors":[f.path(path)]}})).unwrap();
    }
    assert_eq!(f.freshness("behavior"), Freshness::Verified);
    std::fs::write(f.project.root.join("lib.rs"), "pub fn run() -> u32 { 2 }\n").unwrap();
    f.index();
    assert_eq!(f.freshness("behavior"), Freshness::NeedsReview);
    assert_eq!(f.freshness("unrelated"), Freshness::Verified);
    let refreshed = f
        .store
        .mutate(Mutation::RefreshSection {
            id: "behavior".to_owned(),
            expected_version: 1,
        })
        .unwrap();
    let Item::Section(section) = refreshed.item else {
        panic!("section")
    };
    assert_eq!(section.curated, "Human explanation");
    assert!(!section.reviewed);
    assert!(section.generated.contains("lib.rs"));
    std::fs::remove_file(f.project.root.join("other.rs")).unwrap();
    f.index();
    assert_eq!(f.freshness("unrelated"), Freshness::NeedsReview);
    f.store
        .mutate(Mutation::Archive {
            id: "unrelated".to_owned(),
            expected_version: 1,
        })
        .unwrap();
    assert!(
        !f.store
            .snapshot()
            .unwrap()
            .records
            .iter()
            .any(|r| r.id == "unrelated")
    );
    assert!(f.store.history("unrelated").unwrap()[0].archived);
}

#[test]
fn known_drift_wins_over_an_unknown_anchor() {
    let f = Fixture::new();
    f.entity("service", "service");
    f.save("section",0,json!({"type":"section","value":{"entity_id":"service","title":"Mixed sources","reviewed":true,"anchors":[f.path("missing.rs"),f.path("lib.rs")]}})).unwrap();
    f.save("inferred",0,json!({"type":"section","value":{"entity_id":"service","title":"Inferred explanation","basis":"inferred","anchors":[f.path("lib.rs")]}})).unwrap();
    assert_eq!(f.freshness("inferred"), Freshness::Inferred);
    assert_eq!(f.freshness("section"), Freshness::Unknown);
    std::fs::write(f.project.root.join("lib.rs"), "pub fn changed() {}\n").unwrap();
    f.index();
    assert_eq!(f.freshness("section"), Freshness::NeedsReview);
    assert_eq!(f.freshness("inferred"), Freshness::NeedsReview);
}

#[test]
fn observed_claims_require_independent_same_project_evidence_and_deployments_exact_revision() {
    let f = Fixture::new();
    let experience = ExperienceStore::new(f.database.clone());
    let task = experience
        .start_task(&f.project.project.id, "observe deployment", None)
        .unwrap();
    let inferred = experience
        .record_evidence(
            &task.project_id,
            Some(&task.id),
            None,
            "runtime_observation",
            EvidenceStrength::Inferred,
            Some("abc"),
            None,
            None,
            b"model said it deployed",
            &json!({}),
        )
        .unwrap();
    let observed = experience
        .record_evidence(
            &task.project_id,
            Some(&task.id),
            None,
            "runtime_observation",
            EvidenceStrength::Observational,
            Some("abc"),
            Some("describe service"),
            Some(0),
            b"revision abc",
            &json!({}),
        )
        .unwrap();
    experience
        .finish_task(&task.id, TaskOutcome::Success, None, Some("abc"))
        .unwrap();
    let item = |evidence: &EvidenceId| json!({"type":"entity","value":{"kind":"service","name":"observed","basis":"observed","anchors":[{"project_id":task.project_id,"evidence_id":evidence}]}});
    assert!(f.save("untrusted", 0, item(&inferred.id)).is_err());
    assert_eq!(
        f.save("service", 0, item(&observed.id)).unwrap().freshness,
        Freshness::Verified
    );
    let deployment = |revision: &str| json!({"type":"deployment","value":{"entity_id":"service","project_id":task.project_id,"environment":"production","revision":revision,"evidence_id":observed.id}});
    assert!(f.save("deploy", 0, deployment("wrong")).is_err());
    f.save("deploy", 0, deployment("abc")).unwrap();
    let other_root = f.project.root.parent().unwrap().join("other-repo");
    std::fs::create_dir(&other_root).unwrap();
    let other = ProjectRegistry::new(f.database.clone())
        .resolve_or_register(&other_root)
        .unwrap();
    f.save("other-service", 0, json!({"type":"entity","value":{"kind":"service","name":"Other service","project_id":other.project.id}})).unwrap();
    for target in [
        format!("repo:{}", other.project.id),
        "other-service".to_owned(),
    ] {
        let mut wrong = deployment("abc");
        wrong["value"]["entity_id"] = json!(target);
        assert!(f.save("wrong-project", 0, wrong).is_err());
    }
}

#[test]
fn impact_traverses_queue_consumers_and_environment_filter_precedes_expansion() {
    let f = Fixture::new();
    f.save("producer",0,json!({"type":"entity","value":{"kind":"service","name":"producer","project_id":f.project.project.id,"anchors":[f.path("lib.rs")]}})).unwrap();
    f.entity("queue", "queue");
    f.entity("consumer", "service");
    f.entity("unanchored-consumer", "service");
    f.save("unanchored-consume",0,json!({"type":"dependency","value":{"source_id":"unanchored-consumer","target_id":"queue","relation":"consumes","contract":"event.created","environment":"production"}})).unwrap();
    f.entity("staging-only", "service");
    for (id, source, target, relation, env) in [
        ("publish", "producer", "queue", "publishes", "production"),
        ("consume", "consumer", "queue", "consumes", "production"),
        ("stage", "producer", "staging-only", "calls", "staging"),
    ] {
        f.save(id,0,json!({"type":"dependency","value":{"source_id":source,"target_id":target,"relation":relation,"contract":"event.created","environment":env,"anchors":[f.path("lib.rs")]}})).unwrap();
    }
    let impact = f
        .store
        .impact(&ImpactRequest {
            project_id: f.project.project.id.clone(),
            paths: vec![],
            diff: "--- a/lib.rs\n+++ b/lib.rs\n".to_owned(),
            environment: Some("production".to_owned()),
        })
        .unwrap();
    assert!(impact.direct.iter().any(|i| i.id == "producer"));
    assert!(impact.possible.iter().any(|i| i.id == "consumer"));
    assert!(
        impact
            .possible
            .iter()
            .any(|i| i.id == "unanchored-consumer")
    );
    assert!(!impact.possible.iter().any(|i| i.id == "staging-only"));
    let bundle = f
        .store
        .bundle(&BundleRequest {
            entity_id: "producer".to_owned(),
            task: "change event".to_owned(),
            token_budget: 4000,
            environment: Some("production".to_owned()),
        })
        .unwrap();
    assert!(bundle.text.contains("consumer"));
    assert!(!bundle.text.contains("staging-only"));
}

#[test]
fn every_bundle_budget_holds_after_redaction_and_is_deterministic() {
    let f = Fixture::new();
    f.save("ghp_a",0,json!({"type":"entity","value":{"kind":"service","name":"X".repeat(260),"description":"ghp_expands sk_test_secret123"}})).unwrap();
    for budget in 128..300 {
        let request = BundleRequest {
            entity_id: "ghp_a".to_owned(),
            task: "🧪".repeat(128),
            token_budget: budget,
            environment: None,
        };
        let result = f.store.bundle(&request).unwrap();
        assert!(result.tokens <= budget, "{} > {budget}", result.tokens);
        assert_eq!(result.text, f.store.bundle(&request).unwrap().text);
        assert!(!result.text.contains("sk_test_secret123"));
    }
}

#[test]
fn completion_review_retry_expiry_and_purge_preserve_evidence_gates() {
    let f = Fixture::new();
    let experience = ExperienceStore::new(f.database.clone());
    let task = experience
        .start_task(&f.project.project.id, "fix pinned traffic", None)
        .unwrap();
    let completion = Completion {
        task_id: task.id.clone(),
        outcome: TaskOutcome::Success,
        accepted_by_user: None,
        final_commit: None,
        summary: "Traffic remains pinned until explicitly shifted".to_owned(),
        files: vec!["lib.rs".to_owned()],
        verifications: vec![Verification {
            kind: "runtime_observation".to_owned(),
            strength: EvidenceStrength::Observational,
            command: Some("describe traffic".to_owned()),
            exit_code: Some(0),
            artifact: "old revision=100%, latest=0%".to_owned(),
            revision: None,
        }],
        lessons: vec![LessonDraft {
            claim: "Traffic stays pinned to a revision until explicitly shifted".to_owned(),
            rationale: "Observed allocation".to_owned(),
            evidence_ids: vec![],
            expires_at_ms: Some(now_ms() + 86_400_000),
        }],
    };
    let first = f.store.complete(&completion).unwrap();
    let retry = f.store.complete(&completion).unwrap();
    assert_eq!(first.evidence_ids, retry.evidence_ids);
    assert_eq!(first.reviews[0].id, retry.reviews[0].id);
    assert_eq!(first.task.accepted_by_user, None);
    let retrieve = || {
        Retriever::new(f.database.clone())
            .retrieve(&RetrievalRequest {
                project_id: &task.project_id,
                task_text: "traffic revision",
                current_paths: &[],
                repository_root: None,
                head: None,
                limit: 10,
            })
            .unwrap()
    };
    assert!(retrieve().is_empty(), "proposal must not inject");
    let proposal = &first.reviews[0];
    let accepted = f
        .store
        .review(&ReviewRequest {
            id: proposal.id.clone(),
            expected_updated_at_ms: proposal.updated_at_ms,
            decision: Decision::Accept,
            claim: None,
            rationale: None,
            contradicts: vec![],
            reviewed_related: false,
            acknowledge_security_flags: false,
        })
        .unwrap();
    let id = accepted.memory_id.unwrap();
    let memory = MemoryStore::new(f.database.clone())
        .get(&id)
        .unwrap()
        .unwrap();
    assert!((memory.confidence - 0.5).abs() < f64::EPSILON);
    assert_eq!(retrieve().len(), 1);
    assert_eq!(
        experience
            .get_task(&task.id)
            .unwrap()
            .unwrap()
            .accepted_by_user,
        None
    );
    f.database
        .write_immediate(|tx| {
            tx.execute(
                "UPDATE memory_expiry SET expires_at_ms=1 WHERE memory_id=?1",
                [id.as_str()],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(retrieve().is_empty(), "expired lesson must not inject");
    MemoryStore::new(f.database.clone()).purge(&id).unwrap();
    let review = f.store.reviews().unwrap().remove(0);
    assert_eq!(review.claim, "[PURGED]");
    assert!(review.memory_id.is_none());
}

#[test]
fn unsupported_proposals_cannot_be_accepted_and_acceptance_remains_unknown() {
    let f = Fixture::new();
    let experience = ExperienceStore::new(f.database.clone());
    let task = experience
        .start_task(&f.project.project.id, "speculate", None)
        .unwrap();
    let result = f
        .store
        .complete(&Completion {
            task_id: task.id,
            outcome: TaskOutcome::Success,
            accepted_by_user: None,
            final_commit: None,
            summary: "Plausible but unsupported lesson".to_owned(),
            files: vec![],
            verifications: vec![],
            lessons: vec![],
        })
        .unwrap();
    let r = &result.reviews[0];
    assert!(
        f.store
            .review(&ReviewRequest {
                id: r.id.clone(),
                expected_updated_at_ms: r.updated_at_ms,
                decision: Decision::Accept,
                claim: None,
                rationale: None,
                contradicts: vec![],
                reviewed_related: false,
                acknowledge_security_flags: false,
            })
            .is_err()
    );
    assert_eq!(f.store.reviews().unwrap()[0].status, "pending");
}

#[test]
fn purging_shared_memory_evidence_invalidates_product_observations() {
    let f = Fixture::new();
    let experience = ExperienceStore::new(f.database.clone());
    let task = experience
        .start_task(&f.project.project.id, "observe traffic", None)
        .unwrap();
    let completion:Completion=serde_json::from_value(json!({"task_id":task.id,"outcome":"success","summary":"Traffic remains explicitly pinned","verifications":[{"kind":"runtime_observation","strength":"observational","artifact":"old revision=100%"}]})).unwrap();
    let result = f.store.complete(&completion).unwrap();
    let r = &result.reviews[0];
    let accepted = f
        .store
        .review(&ReviewRequest {
            id: r.id.clone(),
            expected_updated_at_ms: r.updated_at_ms,
            decision: Decision::Accept,
            claim: None,
            rationale: None,
            contradicts: vec![],
            reviewed_related: false,
            acknowledge_security_flags: false,
        })
        .unwrap();
    let item = json!({"type":"entity","value":{"kind":"service","name":"traffic","basis":"observed","anchors":[{"project_id":task.project_id,"evidence_id":result.evidence_ids[0]}]}});
    f.save("traffic", 0, item.clone()).unwrap();
    assert_eq!(f.freshness("traffic"), Freshness::Verified);
    let signal = lessons::Signal {
        task_id: task.id.clone(),
        kind: lessons::SignalKind::CommitLanded,
        reference: "observed revision".to_owned(),
        evidence_id: result.evidence_ids[0].clone(),
    };
    f.store.signal(&signal).unwrap();
    let signals = f.store.task_signals(&task.id).unwrap();
    assert_eq!(signals["signals"].as_array().unwrap().len(), 1);
    assert!(signals["accepted_by_user"].is_null());
    let mut incompatible = signal.clone();
    incompatible.kind = lessons::SignalKind::UserAcceptance;
    assert!(f.store.signal(&incompatible).is_err());
    MemoryStore::new(f.database.clone())
        .purge(&accepted.memory_id.unwrap())
        .unwrap();
    assert_eq!(f.freshness("traffic"), Freshness::NeedsReview);
    assert!(f.save("traffic", 1, item).is_err());
    assert!(f.store.signal(&signal).is_err());
}

#[test]
fn failed_then_passing_verifications_do_not_poison_auto_proposals() {
    let f = Fixture::new();
    let experience = ExperienceStore::new(f.database.clone());
    let task = experience
        .start_task(&f.project.project.id, "fix test failure", None)
        .unwrap();
    let completion:Completion=serde_json::from_value(json!({"task_id":task.id,"outcome":"success","summary":"Check lifecycle transition counters","verifications":[
        {"kind":"test_result","strength":"deterministic","command":"cargo test","exit_code":1,"artifact":"counter assertion failed"},
        {"kind":"test_result","strength":"deterministic","command":"cargo test","exit_code":0,"artifact":"all tests pass"}
    ]})).unwrap();
    let result = f.store.complete(&completion).unwrap();
    assert_eq!(result.evidence_ids.len(), 2);
    let r = &result.reviews[0];
    assert_eq!(r.evidence_ids.len(), 1);
    assert_eq!(r.evidence_ids[0], result.evidence_ids[1]);
    assert_eq!(
        f.store
            .review(&ReviewRequest {
                id: r.id.clone(),
                expected_updated_at_ms: r.updated_at_ms,
                decision: Decision::Accept,
                claim: None,
                rationale: None,
                contradicts: vec![],
                reviewed_related: false,
                acknowledge_security_flags: false,
            })
            .unwrap()
            .status,
        "accepted"
    );
}

#[test]
fn rejected_duplicates_stay_rejected_and_reverification_renews_expiry() {
    let f = Fixture::new();
    let experience = ExperienceStore::new(f.database.clone());
    let complete = || {
        let task = experience
            .start_task(&f.project.project.id, "recheck traffic", None)
            .unwrap();
        let c:Completion=serde_json::from_value(json!({"task_id":task.id,"outcome":"success","verifications":[{"kind":"runtime_observation","strength":"observational","artifact":"traffic pinned"}],"lessons":[{"claim":"Traffic stays pinned until explicitly shifted","expires_at_ms":now_ms()+86_400_000}]})).unwrap();
        f.store.complete(&c).unwrap().reviews.remove(0)
    };
    let accept = |r: &lessons::LessonReview| {
        f.store.review(&ReviewRequest {
            id: r.id.clone(),
            expected_updated_at_ms: r.updated_at_ms,
            decision: Decision::Accept,
            claim: None,
            rationale: None,
            contradicts: vec![],
            reviewed_related: false,
            acknowledge_security_flags: false,
        })
    };
    let first = complete();
    let id = accept(&first).unwrap().memory_id.unwrap();
    f.database
        .write_immediate(|tx| {
            tx.execute(
                "UPDATE memory_expiry SET expires_at_ms=1 WHERE memory_id=?1",
                [id.as_str()],
            )?;
            Ok(())
        })
        .unwrap();
    let second = complete();
    assert_eq!(accept(&second).unwrap().memory_id, Some(id.clone()));
    assert!(
        MemoryStore::new(f.database.clone())
            .expired_ids(&f.project.project.id)
            .unwrap()
            .is_empty()
    );
    MemoryStore::new(f.database.clone())
        .reject(&id, "human rejected this claim")
        .unwrap();
    let third = complete();
    assert!(accept(&third).is_err());
    assert_eq!(
        f.store
            .reviews()
            .unwrap()
            .iter()
            .find(|r| r.id == third.id)
            .unwrap()
            .status,
        "pending"
    );
}
