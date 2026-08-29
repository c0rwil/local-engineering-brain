//! Property-based test layer required by `LOCAL_ENGINEERING_BRAIN_SPEC.md` §32.3.
//!
//! Determinism: every property runs under an explicit [`ProptestConfig`] with a
//! fixed case count and proptest's file-based failure persistence, so a failing
//! case is written to `proptest-regressions/` as a seed that reproduces exactly.
//! No wall-clock or thread-local state feeds any generator.

use std::collections::BTreeMap;
use std::path::{Component, PathBuf};

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use tempfile::TempDir;
use uuid::Uuid;

use leb::cache::{CacheAdmission, PartitionedCache};
use leb::context::{ContextCompileRequest, ContextCompiler, ContextMode};
use leb::events::{
    AppendOutcome, DomainEvent, EventStore, ProjectProjection, ProjectProjectionEntry,
};
use leb::ids::{
    AttemptId, ContextRunId, EdgeId, EvaluationRunId, EvidenceId, MemoryId, NodeId, ProjectId,
    TaskId,
};
use leb::memory::{EpistemicState, MemoryDraft, MemoryStatus, MemoryStore};
use leb::projects::{parse_changed_paths_for_tests, parse_name_status_paths_for_tests};
use leb::retrieval::{RetrievalRequest, Retriever};
use leb::security::redact_text;
use leb::storage::Database;

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

fn config(cases: u32) -> ProptestConfig {
    ProptestConfig {
        cases,
        ..ProptestConfig::default()
    }
}

fn open_database(temp: &TempDir) -> Database {
    Database::open(temp.path().join("brain.sqlite")).expect("open database")
}

/// Deterministic project ID pool so shrunk failures print stable identifiers.
fn pool_project(index: u8) -> ProjectId {
    let uuid = Uuid::from_u128(0xF00D_0000_0000_0000_0000_0000_0000_0000 + u128::from(index));
    ProjectId::parse(&format!("prj_{uuid}")).expect("pool project id")
}

fn register_project(database: &Database, project: &ProjectId) {
    EventStore::new(database.clone())
        .append(&DomainEvent::ProjectRegistered {
            project_id: project.clone(),
            name: "prop-fixture".to_owned(),
            canonical_remote: None,
            root_commit: None,
            parent_project_id: None,
        })
        .expect("register fixture project");
}

fn memory_draft(project: &ProjectId, claim: &str, confidence: f64, agent: bool) -> MemoryDraft {
    MemoryDraft {
        scope_type: "project".to_owned(),
        scope_id: project.to_string(),
        kind: "lesson".to_owned(),
        claim: claim.to_owned(),
        rationale: "property fixture rationale".to_owned(),
        confidence,
        importance: 0.5,
        tags: Vec::new(),
        valid_from_commit: None,
        valid_to_commit: None,
        trust_source: if agent {
            "agent_inference"
        } else {
            "human_stated"
        }
        .to_owned(),
        evidence_ids: Vec::new(),
        source_task_ids: Vec::new(),
        node_ids: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// 1. Event store
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum EventOp {
    Register(u8),
    Alias(u8, u8),
    Tombstone(u8),
}

fn event_op() -> impl Strategy<Value = EventOp> {
    prop_oneof![
        (0_u8..4).prop_map(EventOp::Register),
        (0_u8..4, 0_u8..6).prop_map(|(project, alias)| EventOp::Alias(project, alias)),
        (0_u8..4).prop_map(EventOp::Tombstone),
    ]
}

fn to_domain_event(op: &EventOp) -> DomainEvent {
    match op {
        EventOp::Register(project) => DomainEvent::ProjectRegistered {
            project_id: pool_project(*project),
            name: format!("project-{project}"),
            canonical_remote: None,
            root_commit: None,
            parent_project_id: None,
        },
        EventOp::Alias(project, alias) => DomainEvent::ProjectAliasAdded {
            project_id: pool_project(*project),
            path: format!("/alias/{alias}"),
        },
        EventOp::Tombstone(project) => DomainEvent::ProjectTombstoned {
            project_id: pool_project(*project),
        },
    }
}

/// Incremental in-memory application with the same semantics the replay fold
/// documents, applied only to events whose append succeeded.
fn apply_incremental(model: &mut BTreeMap<ProjectId, ProjectProjectionEntry>, event: &DomainEvent) {
    match event {
        DomainEvent::ProjectRegistered {
            project_id, name, ..
        } => {
            model.insert(
                project_id.clone(),
                ProjectProjectionEntry {
                    name: name.clone(),
                    canonical_remote: None,
                    root_commit: None,
                    parent_project_id: None,
                    aliases: Vec::new(),
                    tombstoned: false,
                },
            );
        }
        DomainEvent::ProjectAliasAdded { project_id, path } => {
            if let Some(entry) = model.get_mut(project_id)
                && !entry.aliases.contains(path)
            {
                entry.aliases.push(path.clone());
                entry.aliases.sort();
            }
        }
        DomainEvent::ProjectTombstoned { project_id } => {
            if let Some(entry) = model.get_mut(project_id) {
                entry.tombstoned = true;
            }
        }
        _ => {}
    }
}

proptest! {
    #![proptest_config(config(24))]

    /// Appending an arbitrary event sequence never decreases the persisted event
    /// count, sequences are strictly monotonic, and replaying the project
    /// projection equals incremental application of exactly the successful events.
    #[test]
    fn event_log_is_append_only_and_replay_matches_incremental(
        ops in prop::collection::vec(event_op(), 1..16),
    ) {
        let temp = TempDir::new().expect("temp dir");
        let store = EventStore::new(open_database(&temp));
        let mut model = BTreeMap::new();
        let mut successful = 0_usize;
        let mut last_sequence = 0_i64;
        let mut last_count = 0_usize;
        for op in &ops {
            let event = to_domain_event(op);
            if let Ok(envelope) = store.append(&event) {
                successful += 1;
                prop_assert!(envelope.sequence > last_sequence, "sequence must be monotonic");
                last_sequence = envelope.sequence;
                apply_incremental(&mut model, &event);
            }
            let count = store.replay().expect("replay").len();
            prop_assert!(count >= last_count, "event count must never decrease");
            prop_assert_eq!(count, successful, "log holds exactly the successful appends");
            last_count = count;
        }
        let replayed = store.replay_project_projection().expect("replay projection");
        prop_assert_eq!(replayed, ProjectProjection { projects: model });
    }

    /// Idempotent append with the same request ID never duplicates the event,
    /// and reusing the ID for different content is rejected.
    #[test]
    fn event_append_with_id_is_idempotent(project in 0_u8..4, other in 4_u8..8) {
        let temp = TempDir::new().expect("temp dir");
        let store = EventStore::new(open_database(&temp));
        let event = to_domain_event(&EventOp::Register(project));
        let first = store.append_with_id("evt_prop_idem", 1, &event).expect("first append");
        let AppendOutcome::Appended { sequence } = first else {
            return Err(TestCaseError::fail("first append must insert"));
        };
        let retry = store.append_with_id("evt_prop_idem", 1, &event).expect("idempotent retry");
        prop_assert_eq!(retry, AppendOutcome::AlreadyPresent { sequence });
        prop_assert_eq!(store.replay().expect("replay").len(), 1);
        let conflicting = to_domain_event(&EventOp::Register(other));
        prop_assert!(store.append_with_id("evt_prop_idem", 1, &conflicting).is_err());
        prop_assert_eq!(store.replay().expect("replay").len(), 1);
    }
}

// ---------------------------------------------------------------------------
// 2. Memory state machine
// ---------------------------------------------------------------------------

const EPISTEMIC_STATES: [EpistemicState; 5] = [
    EpistemicState::Candidate,
    EpistemicState::Observed,
    EpistemicState::Supported,
    EpistemicState::Validated,
    EpistemicState::Rejected,
];

const MEMORY_STATUSES: [MemoryStatus; 7] = [
    MemoryStatus::Active,
    MemoryStatus::Suspect,
    MemoryStatus::Stale,
    MemoryStatus::Superseded,
    MemoryStatus::Contradicted,
    MemoryStatus::Archived,
    MemoryStatus::Purged,
];

#[derive(Clone, Debug)]
enum MemOp {
    Create {
        slot: usize,
        pct: u8,
        agent: bool,
    },
    Promote {
        slot: usize,
        target: EpistemicState,
        human: bool,
    },
    Reject {
        slot: usize,
    },
    Mark {
        slot: usize,
        status: MemoryStatus,
    },
    Purge {
        slot: usize,
    },
}

fn mem_op() -> impl Strategy<Value = MemOp> {
    let slot = 0_usize..3;
    prop_oneof![
        (slot.clone(), 0_u8..=100, any::<bool>()).prop_map(|(slot, pct, agent)| MemOp::Create {
            slot,
            pct,
            agent
        }),
        (
            slot.clone(),
            prop::sample::select(&EPISTEMIC_STATES[..]),
            any::<bool>()
        )
            .prop_map(|(slot, target, human)| MemOp::Promote {
                slot,
                target,
                human
            }),
        slot.clone().prop_map(|slot| MemOp::Reject { slot }),
        (slot.clone(), prop::sample::select(&MEMORY_STATUSES[..]))
            .prop_map(|(slot, status)| MemOp::Mark { slot, status }),
        slot.prop_map(|slot| MemOp::Purge { slot }),
    ]
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum SlotModel {
    Live(EpistemicState, MemoryStatus),
    Purged,
}

const fn state_rank(state: EpistemicState) -> u8 {
    match state {
        EpistemicState::Candidate => 0,
        EpistemicState::Observed => 1,
        EpistemicState::Supported => 2,
        EpistemicState::Validated => 3,
        EpistemicState::Rejected => 4,
    }
}

/// Expected success of `promote`, mirroring the documented state machine: only
/// active memories move, only forward, one state at a time unless a human
/// override, and supported/validated require evidence (absent in this fixture).
fn promote_should_succeed(model: SlotModel, target: EpistemicState, human: bool) -> bool {
    let SlotModel::Live(state, status) = model else {
        return false;
    };
    target != EpistemicState::Rejected
        && status == MemoryStatus::Active
        && target > state
        && (human || state_rank(target) == state_rank(state) + 1)
        && (target < EpistemicState::Supported || human)
}

proptest! {
    #![proptest_config(config(24))]

    /// For arbitrary lifecycle sequences, invalid transitions always error,
    /// purge is idempotent, and purged content never resurrects.
    #[test]
    fn memory_lifecycle_respects_state_machine(
        ops in prop::collection::vec(mem_op(), 1..14),
    ) {
        let temp = TempDir::new().expect("temp dir");
        let database = open_database(&temp);
        let project = pool_project(0);
        register_project(&database, &project);
        let memories = MemoryStore::new(database);

        let mut ids: [Option<MemoryId>; 3] = [None, None, None];
        let mut claims: [String; 3] = [String::new(), String::new(), String::new()];
        let mut model: [Option<SlotModel>; 3] = [None, None, None];
        let mut nonce = 0_u32;

        for op in &ops {
            match *op {
                MemOp::Create { slot, pct, agent } => {
                    nonce += 1;
                    let claim = format!("distinct property claim {slot} nonce {nonce}");
                    let confidence = f64::from(pct) / 100.0;
                    let draft = memory_draft(&project, &claim, confidence, agent);
                    let result = memories.create_candidate(&project, &draft);
                    if agent && confidence > 0.5 {
                        prop_assert!(result.is_err(), "agent confidence above cap must error");
                    } else {
                        let record = result.expect("valid candidate creation");
                        prop_assert_eq!(record.epistemic_state, EpistemicState::Candidate);
                        prop_assert_eq!(record.status, MemoryStatus::Active);
                        prop_assert!(record.confidence <= 0.5 || !agent);
                        ids[slot] = Some(record.id);
                        claims[slot] = claim;
                        model[slot] = Some(SlotModel::Live(
                            EpistemicState::Candidate,
                            MemoryStatus::Active,
                        ));
                    }
                }
                MemOp::Promote { slot, target, human } => {
                    let (Some(id), Some(state)) = (&ids[slot], model[slot]) else { continue };
                    let result = memories.promote(id, target, human);
                    if promote_should_succeed(state, target, human) {
                        let record = result.expect("valid promotion");
                        prop_assert_eq!(record.epistemic_state, target);
                        model[slot] = Some(SlotModel::Live(target, MemoryStatus::Active));
                    } else {
                        prop_assert!(result.is_err(), "invalid promotion must error");
                    }
                }
                MemOp::Reject { slot } => {
                    let (Some(id), Some(state)) = (&ids[slot], model[slot]) else { continue };
                    let result = memories.reject(id, "property_reject");
                    if state == SlotModel::Purged {
                        prop_assert!(result.is_err(), "rejecting purged memory must error");
                    } else {
                        let record = result.expect("reject succeeds on live memory");
                        prop_assert_eq!(record.epistemic_state, EpistemicState::Rejected);
                        prop_assert_eq!(record.status, MemoryStatus::Archived);
                        model[slot] = Some(SlotModel::Live(
                            EpistemicState::Rejected,
                            MemoryStatus::Archived,
                        ));
                    }
                }
                MemOp::Mark { slot, status } => {
                    let (Some(id), Some(state)) = (&ids[slot], model[slot]) else { continue };
                    let result = memories.mark_status(id, status, "property_mark");
                    match state {
                        _ if status == MemoryStatus::Purged => {
                            prop_assert!(result.is_err(), "mark_status(Purged) must be refused");
                        }
                        SlotModel::Purged => {
                            prop_assert!(result.is_err(), "marking purged memory must error");
                        }
                        SlotModel::Live(epistemic, _) => {
                            let record = result.expect("status change on live memory");
                            prop_assert_eq!(record.status, status);
                            prop_assert_eq!(record.epistemic_state, epistemic);
                            model[slot] = Some(SlotModel::Live(epistemic, status));
                        }
                    }
                }
                MemOp::Purge { slot } => {
                    let (Some(id), Some(_)) = (&ids[slot], model[slot]) else { continue };
                    memories.purge(id).expect("purge is idempotent for known memory");
                    model[slot] = Some(SlotModel::Purged);
                    prop_assert!(
                        memories.get(id).expect("get after purge").is_none(),
                        "purged memory must not be readable"
                    );
                    let resurrected = memories
                        .find_exact(&project, "project", project.as_str(), "lesson", &claims[slot])
                        .expect("find_exact after purge");
                    prop_assert!(
                        resurrected.is_none(),
                        "purged claim content must not be findable"
                    );
                }
            }
        }

        for slot in 0..3 {
            let Some(id) = &ids[slot] else { continue };
            match model[slot].expect("tracked slot has model") {
                SlotModel::Purged => {
                    prop_assert!(memories.get(id).expect("final get").is_none());
                }
                SlotModel::Live(state, status) => {
                    let record = memories
                        .get(id)
                        .expect("final get")
                        .expect("live memory still present");
                    prop_assert_eq!(record.epistemic_state, state);
                    prop_assert_eq!(record.status, status);
                    if record.trust_source == "agent_inference" {
                        prop_assert!(record.confidence <= 0.5);
                    }
                }
            }
        }
    }
}

proptest! {
    #![proptest_config(config(32))]

    /// Candidate creation accepts exactly finite confidences in [0,1] and caps
    /// agent inference at 0.50 regardless of the requested confidence.
    #[test]
    fn agent_inference_confidence_is_capped(confidence in any::<f64>(), agent in any::<bool>()) {
        let temp = TempDir::new().expect("temp dir");
        let database = open_database(&temp);
        let project = pool_project(0);
        register_project(&database, &project);
        let memories = MemoryStore::new(database);
        let draft = memory_draft(&project, "confidence cap probe", confidence, agent);
        let result = memories.create_candidate(&project, &draft);
        let valid = confidence.is_finite()
            && (0.0..=1.0).contains(&confidence)
            && !(agent && confidence > 0.5);
        if valid {
            let record = result.expect("valid draft accepted");
            prop_assert!(record.confidence.is_finite());
            prop_assert!((0.0..=1.0).contains(&record.confidence));
            if record.trust_source == "agent_inference" {
                prop_assert!(record.confidence <= 0.5, "agent inference exceeds its cap");
            }
        } else {
            prop_assert!(result.is_err(), "invalid confidence must be rejected");
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Git porcelain / name-status parsing
// ---------------------------------------------------------------------------

/// A parsed path is safe when it is non-empty, relative, contains no parent or
/// root components, and does not resolve into `.git` or `.leb` even behind a
/// leading `./` component.
fn assert_paths_safe(paths: &[PathBuf]) -> Result<(), TestCaseError> {
    for path in paths {
        prop_assert!(!path.as_os_str().is_empty(), "empty path emitted");
        prop_assert!(!path.is_absolute(), "absolute path emitted: {path:?}");
        prop_assert!(
            path.components().all(|component| !matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )),
            "traversal component emitted: {path:?}"
        );
        let first = path
            .components()
            .find(|component| !matches!(component, Component::CurDir));
        if let Some(Component::Normal(name)) = first {
            prop_assert!(
                name != ".git" && name != ".leb",
                "path resolves into a reserved directory: {path:?}"
            );
        }
    }
    Ok(())
}

fn nul_joined_fields() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(prop::collection::vec(any::<u8>(), 0..12), 0..8).prop_map(|fields| {
        let mut bytes = Vec::new();
        for field in fields {
            bytes.extend(field);
            bytes.push(0);
        }
        bytes
    })
}

/// Well-formed porcelain-z records around adversarial path shapes.
fn planted_porcelain() -> impl Strategy<Value = Vec<u8>> {
    let path = prop_oneof![
        "[a-z]{1,8}(/[a-z]{1,8}){0,2}",
        Just("/etc/passwd".to_owned()),
        Just("../escape".to_owned()),
        Just("a/../../b".to_owned()),
        Just(".git/config".to_owned()),
        Just("./.git/config".to_owned()),
        Just(".leb/project.toml".to_owned()),
        Just("./.leb/project.toml".to_owned()),
        Just(".gitignore".to_owned()),
    ];
    let status = prop::sample::select(&["M ", "A ", "??", " D"][..]);
    prop::collection::vec((status, path), 0..5).prop_map(|records| {
        let mut bytes = Vec::new();
        for (status, path) in records {
            bytes.extend(status.as_bytes());
            bytes.push(b' ');
            bytes.extend(path.as_bytes());
            bytes.push(0);
        }
        bytes
    })
}

proptest! {
    #![proptest_config(config(192))]

    /// Arbitrary porcelain-z byte streams either error or parse to safe
    /// repository-relative paths; they never panic and never emit unsafe paths.
    #[test]
    fn porcelain_status_parser_never_emits_unsafe_paths(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
        if let Ok(paths) = parse_changed_paths_for_tests(&bytes) {
            assert_paths_safe(&paths)?;
        }
    }

    /// NUL-joined fuzz fields through the porcelain parser obey the same
    /// error-or-safe-paths contract.
    #[test]
    fn porcelain_status_parser_handles_nul_separated_fuzz(bytes in nul_joined_fields()) {
        if let Ok(paths) = parse_changed_paths_for_tests(&bytes) {
            assert_paths_safe(&paths)?;
        }
    }

    /// Well-formed records around planted hostile paths (absolute, `..`,
    /// `.git`, `.leb`, `./`-prefixed) never yield an unsafe path.
    #[test]
    fn porcelain_status_parser_rejects_planted_hostile_paths(bytes in planted_porcelain()) {
        if let Ok(paths) = parse_changed_paths_for_tests(&bytes) {
            assert_paths_safe(&paths)?;
        }
    }

    /// Arbitrary `--name-status -z` byte streams either error or parse to safe
    /// repository-relative paths; they never panic and never emit unsafe paths.
    #[test]
    fn name_status_parser_never_emits_unsafe_paths(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
        if let Ok(paths) = parse_name_status_paths_for_tests(&bytes) {
            assert_paths_safe(&paths)?;
        }
    }

    /// NUL-joined fuzz fields through the name-status parser obey the same
    /// error-or-safe-paths contract.
    #[test]
    fn name_status_parser_handles_nul_separated_fuzz(bytes in nul_joined_fields()) {
        if let Ok(paths) = parse_name_status_paths_for_tests(&bytes) {
            assert_paths_safe(&paths)?;
        }
    }
}

// ---------------------------------------------------------------------------
// 4 & 5. Context compiler budgeting and retrieval scoring
// ---------------------------------------------------------------------------

const WORDS: [&str; 8] = [
    "alpha", "bravo", "cactus", "delta", "ember", "falcon", "granite", "harbor",
];

fn phrase(range: std::ops::Range<usize>) -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(&WORDS[..]), range).prop_map(|words| words.join(" "))
}

/// Register a project and create validated memories for the given claims.
fn validated_memory_fixture(database: &Database, project: &ProjectId, claims: &[String]) {
    register_project(database, project);
    let memories = MemoryStore::new(database.clone());
    for (index, claim) in claims.iter().enumerate() {
        let claim = format!("{claim} item {index}");
        let record = memories
            .create_candidate(project, &memory_draft(project, &claim, 0.8, false))
            .expect("create fixture memory");
        memories
            .promote(&record.id, EpistemicState::Validated, true)
            .expect("human-override validation");
    }
}

proptest! {
    #![proptest_config(config(12))]

    /// Compiled context never exceeds the token budget, compilation is
    /// deterministic for identical input, and every selected item retains a
    /// durable why-explanation.
    #[test]
    fn context_compiler_is_budgeted_deterministic_and_explainable(
        claims in prop::collection::vec(phrase(2..5), 1..5),
        task_text in phrase(1..4),
        token_budget in 1_u32..=600,
    ) {
        let temp = TempDir::new().expect("temp dir");
        let database = open_database(&temp);
        let project = pool_project(0);
        validated_memory_fixture(&database, &project, &claims);
        let compiler = ContextCompiler::new(database, 6_000, 1_800);
        let request = |request_id: &str| ContextCompileRequest {
            request_id: request_id.to_owned(),
            project_id: project.clone(),
            task_text: task_text.clone(),
            current_paths: Vec::new(),
            token_budget,
            mode: ContextMode::Brain,
            repository_root: None,
            head: None,
        };
        let first = compiler.compile(&request("prop-req-a")).expect("first compile");
        prop_assert!(
            first.metrics.selected_tokens <= token_budget,
            "selected tokens {} exceed budget {}",
            first.metrics.selected_tokens,
            token_budget
        );
        for item in &first.items {
            prop_assert!(!item.why.note.is_empty(), "selected item lost its why note");
            let explained = compiler
                .explain(&first.id, item.ordinal)
                .expect("explain query")
                .expect("selected item is explainable");
            prop_assert_eq!(&explained.source_id, &item.source_id);
            // serde_json's default reader can lose the final ulp when parsing
            // floats back out of `why_json` (exact parsing requires its
            // `float_roundtrip` feature), so persisted why-scores are compared
            // with a one-ulp-scale tolerance instead of bitwise equality.
            prop_assert_eq!(&explained.why.note, &item.why.note);
            let float_pairs = [
                (explained.why.lexical, item.why.lexical),
                (explained.why.exact, item.why.exact),
                (explained.why.structural, item.why.structural),
                (explained.why.confidence, item.why.confidence),
                (explained.why.utility, item.why.utility),
                (explained.why.importance, item.why.importance),
                (explained.why.validity, item.why.validity),
                (explained.why.semantic, item.why.semantic),
                (explained.score, item.score),
            ];
            for (persisted, computed) in float_pairs {
                prop_assert!(
                    (persisted - computed).abs() <= 1e-12_f64.max(computed.abs() * 1e-12),
                    "persisted why-score {} drifted from computed {}",
                    persisted,
                    computed
                );
            }
        }
        let second = compiler.compile(&request("prop-req-b")).expect("second compile");
        prop_assert_eq!(&first.stable_prefix, &second.stable_prefix);
        prop_assert_eq!(&first.dynamic_context, &second.dynamic_context);
        prop_assert_eq!(&first.items, &second.items);
        prop_assert_eq!(first.metrics.selected_tokens, second.metrics.selected_tokens);
        prop_assert_eq!(&first.metrics.stable_prefix_hash, &second.metrics.stable_prefix_hash);
    }

    /// Retrieval scores are always finite and within [0,1], and ranking is
    /// deterministic with a stable score-then-ID tie-break.
    #[test]
    fn retrieval_scores_are_bounded_and_ordering_is_deterministic(
        claims in prop::collection::vec(phrase(2..5), 1..5),
        task_text in phrase(1..4),
        limit in 1_usize..=100,
    ) {
        let temp = TempDir::new().expect("temp dir");
        let database = open_database(&temp);
        let project = pool_project(0);
        validated_memory_fixture(&database, &project, &claims);
        let retriever = Retriever::new(database);
        let request = RetrievalRequest {
            project_id: &project,
            task_text: &task_text,
            current_paths: &[],
            repository_root: None,
            head: None,
            limit,
        };
        let first = retriever.retrieve(&request).expect("first retrieval");
        prop_assert!(first.len() <= limit);
        for entry in &first {
            prop_assert!(entry.score.is_finite(), "score must be finite");
            prop_assert!(
                (0.0..=1.0).contains(&entry.score),
                "score {} outside [0,1]",
                entry.score
            );
        }
        for pair in first.windows(2) {
            let ordered = match pair[0].score.total_cmp(&pair[1].score) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Equal => pair[0].memory.id < pair[1].memory.id,
                std::cmp::Ordering::Less => false,
            };
            prop_assert!(ordered, "ranking must be sorted by score desc, id asc");
        }
        let second = retriever.retrieve(&request).expect("second retrieval");
        prop_assert_eq!(&first, &second, "identical input must rank identically");
    }
}

// ---------------------------------------------------------------------------
// 6. Byte-weighted cache
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum CacheOp {
    Insert {
        project: u8,
        generation: u8,
        key: u8,
        weight: u32,
    },
    Get {
        project: u8,
        generation: u8,
        key: u8,
    },
}

fn cache_op() -> impl Strategy<Value = CacheOp> {
    prop_oneof![
        (0_u8..4, 0_u8..3, 0_u8..8, 0_u32..=8_192).prop_map(
            |(project, generation, key, weight)| {
                CacheOp::Insert {
                    project,
                    generation,
                    key,
                    weight,
                }
            }
        ),
        (0_u8..4, 0_u8..3, 0_u8..8).prop_map(|(project, generation, key)| CacheOp::Get {
            project,
            generation,
            key,
        }),
    ]
}

proptest! {
    #![proptest_config(config(64))]

    /// For arbitrary insert/get sequences the weighted size never exceeds the
    /// budget, oversize items are always rejected, and a read never returns a
    /// value from another project, generation, or key.
    #[test]
    fn cache_is_bounded_rejects_oversize_and_never_leaks(
        total_budget in 64_u64..=4_096,
        max_projects in 1_usize..=4,
        ops in prop::collection::vec(cache_op(), 1..40),
    ) {
        let per_project = total_budget / u64::try_from(max_projects).expect("usize fits u64");
        let max_item = u32::try_from(per_project).expect("per-project budget fits u32");
        let cache: PartitionedCache<u8, (u8, u8, u8)> =
            PartitionedCache::new(total_budget, max_projects, max_item);
        for op in &ops {
            match *op {
                CacheOp::Insert { project, generation, key, weight } => {
                    let admission = cache.insert(
                        &pool_project(project),
                        u64::from(generation),
                        key,
                        (project, generation, key),
                        weight,
                    );
                    if weight > max_item {
                        prop_assert_eq!(
                            admission,
                            CacheAdmission::RejectedOversized,
                            "oversize item must be rejected"
                        );
                    } else {
                        prop_assert_eq!(admission, CacheAdmission::Admitted);
                    }
                }
                CacheOp::Get { project, generation, key } => {
                    if let Some(value) =
                        cache.get(&pool_project(project), u64::from(generation), &key)
                    {
                        prop_assert_eq!(
                            *value,
                            (project, generation, key),
                            "cache returned a value from another scope"
                        );
                    }
                }
            }
            let stats = cache.stats();
            prop_assert!(stats.active_projects <= max_projects);
            prop_assert!(
                stats.weighted_bytes <= total_budget,
                "weighted bytes {} exceed total budget {}",
                stats.weighted_bytes,
                total_budget
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 7. Redaction
// ---------------------------------------------------------------------------

/// (secret text as planted, distinctive core that must disappear).
fn planted_secret() -> impl Strategy<Value = (String, String)> {
    let core = "[a-z0-9]{12,24}";
    let long_core = "[a-z0-9]{16,24}";
    let upper_core = "[A-Z0-9]{16}";
    let key = prop::sample::select(
        &[
            "api_key",
            "password",
            "token",
            "secret",
            "passwd",
            "client_secret",
            "access_token",
            "auth_token",
            "aws_secret_access_key",
        ][..],
    );
    prop_oneof![
        core.prop_map(|core| (format!("ghp_{core}"), core)),
        core.prop_map(|core| (format!("github_pat_{core}"), core)),
        core.prop_map(|core| (format!("xoxb-{core}"), core)),
        long_core.prop_map(|core| (format!("sk-{core}"), core)),
        upper_core.prop_map(|core| (format!("AKIA{core}"), core)),
        (key, core).prop_map(|(key, core)| (format!("{key}={core}"), core)),
        core.prop_map(|core| (format!("Bearer {core}"), core)),
        core.prop_map(|core| {
            (
                format!("-----BEGIN PRIVATE KEY-----\n{core}\n-----END PRIVATE KEY-----"),
                core,
            )
        }),
    ]
}

proptest! {
    #![proptest_config(config(64))]

    /// Redacted output never contains a planted secret matching the built-in
    /// patterns, and redaction is idempotent.
    #[test]
    fn redaction_removes_planted_secrets_and_is_idempotent(
        prefix in prop::collection::vec(prop::sample::select(&WORDS[..]), 0..5),
        (secret, core) in planted_secret(),
        suffix in prop::collection::vec(prop::sample::select(&WORDS[..]), 0..5),
    ) {
        let mut text = prefix.join(" ");
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&secret);
        if !suffix.is_empty() {
            text.push(' ');
            text.push_str(&suffix.join(" "));
        }
        let redacted = redact_text(&text);
        prop_assert!(
            !redacted.contains(&core),
            "secret core `{}` survived redaction: `{}`",
            core,
            redacted
        );
        let again = redact_text(&redacted);
        prop_assert_eq!(&again, &redacted, "redaction must be idempotent");
    }
}

// ---------------------------------------------------------------------------
// 8. Typed IDs
// ---------------------------------------------------------------------------

macro_rules! check_id_type {
    ($ty:ty, $prefix:literal, $wrong:literal, $uuid:expr) => {{
        let canonical = format!(concat!($prefix, "_{}"), $uuid);
        let parsed = <$ty>::parse(&canonical).expect("canonical form parses");
        prop_assert_eq!(parsed.as_str(), canonical.as_str());
        prop_assert_eq!(&parsed.to_string(), &canonical);
        let reparsed: $ty = parsed.to_string().parse().expect("display round-trips");
        prop_assert_eq!(&reparsed, &parsed);
        prop_assert!(
            <$ty>::parse(&format!(concat!($wrong, "_{}"), $uuid)).is_err(),
            "wrong prefix must be rejected"
        );
        prop_assert!(
            <$ty>::parse(&$uuid.to_string()).is_err(),
            "bare uuid must be rejected"
        );
        prop_assert!(<$ty>::parse(concat!($prefix, "_not-a-uuid")).is_err());
        prop_assert!(<$ty>::parse(concat!($prefix, "_")).is_err());
        prop_assert!(<$ty>::parse("").is_err());
    }};
}

proptest! {
    #![proptest_config(config(64))]

    /// Every typed ID round-trips through parse/display, and wrong prefixes,
    /// bare UUIDs, and malformed suffixes are always rejected.
    #[test]
    fn typed_ids_round_trip_and_reject_invalid_prefixes(raw in any::<u128>()) {
        let uuid = Uuid::from_u128(raw);
        check_id_type!(TaskId, "tsk", "mem", uuid);
        check_id_type!(AttemptId, "att", "tsk", uuid);
        check_id_type!(EvidenceId, "evd", "att", uuid);
        check_id_type!(MemoryId, "mem", "evd", uuid);
        check_id_type!(NodeId, "nod", "edg", uuid);
        check_id_type!(EdgeId, "edg", "nod", uuid);
        check_id_type!(ContextRunId, "ctx", "eval", uuid);
        check_id_type!(EvaluationRunId, "eval", "ctx", uuid);
    }

    /// `ProjectId` canonicalizes bare and prefixed UUIDs to the same identity
    /// and rejects anything that is not a UUID.
    #[test]
    fn project_id_canonicalizes_and_rejects_garbage(raw in any::<u128>(), garbage in "[a-z_/-]{1,12}") {
        let uuid = Uuid::from_u128(raw);
        let prefixed = ProjectId::parse(&format!("prj_{uuid}")).expect("prefixed parses");
        let bare = ProjectId::parse(&uuid.to_string()).expect("bare uuid parses");
        prop_assert_eq!(&prefixed, &bare);
        let canonical = format!("prj_{uuid}");
        prop_assert_eq!(prefixed.as_str(), canonical.as_str());
        let reparsed = ProjectId::parse(&prefixed.to_string()).expect("display round-trips");
        prop_assert_eq!(&reparsed, &prefixed);
        let garbage_prefixed = format!("prj_{garbage}");
        prop_assert!(ProjectId::parse(&garbage_prefixed).is_err());
        prop_assert!(Uuid::parse_str(&garbage).is_ok() || ProjectId::parse(&garbage).is_err());
        prop_assert!(ProjectId::parse("").is_err());
    }
}
