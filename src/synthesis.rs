//! Conservative episode-to-procedure synthesis (spec §16.3–§16.5).
//!
//! A procedure is a typed memory of kind `procedure` whose claim is the ordered
//! normalized strategy shared by several successful, evidence-backed episodes.
//! Procedures live purely as typed memories (no dedicated skills table exists in
//! the schema): they flow through the same evidence-gated promotion state
//! machine as every other memory, and `leb_get_skill` already reads them by
//! kind. Promotion to the `skill` kind additionally requires recorded
//! successful uses.
//!
//! ## Strategy signature
//!
//! Each successful episode contributes an ordered list of *steps*: the
//! `strategy_summary` of each of its successful attempts, in ordinal order,
//! normalized exactly like memory claims (whitespace collapsed to single
//! spaces, lowercased). The signature is the SHA-256 of the steps joined with
//! `\n`. Two episodes match only when their normalized step sequences are
//! identical, and a group synthesizes only when every supporting episode's
//! touched-file sets share at least one common file (the overlapping
//! topology becomes the procedure's precondition scope). This is deliberately
//! exact and deterministic: near-miss clustering is out of scope for V1.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::events::{DomainEvent, EventStore};
use crate::ids::{AttemptId, EvidenceId, MemoryId, ProjectId, TaskId};
use crate::memory::{EpistemicState, MemoryDraft, MemoryRecord, MemoryStatus, MemoryStore};
use crate::security::Redactor;
use crate::storage::Database;

/// Confidence cap for synthesized drafts: identical to any agent inference.
const SYNTHESIZED_CONFIDENCE: f64 = 0.5;
/// Cross-project hypotheses start strictly below locally observed drafts.
const HYPOTHESIS_CONFIDENCE: f64 = 0.3;
/// Bounded scan window over the most recent successful episodes.
const MAX_SCANNED_EPISODES: usize = 256;
/// Bound on evidence links carried by one synthesized procedure.
const MAX_EVIDENCE_LINKS: usize = 64;
/// Bound on retained failed-variant references (spec §16.3 negative examples).
const MAX_NEGATIVE_EXAMPLES: usize = 32;

#[derive(Clone, Copy, Debug)]
pub struct SynthesisOptions {
    /// Distinct matching successful episodes required for a local draft.
    pub min_supporting_episodes: u32,
    /// Whether validated procedures from other projects may enter this project
    /// as explicitly marked hypotheses (spec §16.5).
    pub cross_project_hypotheses: bool,
}

impl From<&crate::config::SynthesisConfig> for SynthesisOptions {
    fn from(config: &crate::config::SynthesisConfig) -> Self {
        Self {
            min_supporting_episodes: config.min_supporting_episodes,
            cross_project_hypotheses: config.cross_project_hypotheses,
        }
    }
}

/// Structured payload stored in the procedure memory's rationale.
///
/// The canonical provenance is always the `memory_sources`/`memory_evidence`
/// link tables; this JSON is a readable snapshot from the synthesis run that
/// created the memory and is not rewritten when later runs extend provenance.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProcedureBody {
    pub procedure_version: u32,
    /// SHA-256 over the normalized step sequence.
    pub signature: String,
    /// Ordered normalized strategy steps shared by every supporting episode.
    pub steps: Vec<String>,
    /// Precondition scope: files touched by every supporting episode.
    pub precondition_files: Vec<String>,
    /// Successful episodes that support this procedure at creation time.
    pub source_episode_ids: Vec<TaskId>,
    /// Verified deterministic/observational evidence union at creation time.
    pub evidence_ids: Vec<EvidenceId>,
    /// Failed attempts from the source episodes, kept as negative examples.
    pub negative_example_attempt_ids: Vec<AttemptId>,
    /// True only for cross-project transfer hypotheses.
    pub hypothesis: bool,
    /// Present only on hypotheses: where the validated procedure came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer: Option<ProcedureTransfer>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProcedureTransfer {
    pub source_project_id: ProjectId,
    pub source_memory_id: MemoryId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SynthesizedProcedure {
    pub memory: MemoryRecord,
    pub signature: String,
    pub supporting_episodes: Vec<TaskId>,
    pub reused_existing: bool,
    pub cross_project_hypothesis: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SynthesisReport {
    pub project_id: ProjectId,
    /// Successful evidence-backed episodes examined in this bounded pass.
    pub examined_episodes: u32,
    pub procedures: Vec<SynthesizedProcedure>,
}

/// One successful, evidence-backed episode eligible for synthesis.
#[derive(Clone, Debug)]
struct Episode {
    task_id: TaskId,
    /// Ordered normalized strategy steps from successful attempts.
    steps: Vec<String>,
    /// Normalized union of files touched by successful attempts.
    files: BTreeSet<String>,
    /// Verified Tier A/B evidence for this episode.
    evidence: Vec<EvidenceId>,
    /// Failed attempts, retained as negative examples.
    failed_attempts: Vec<AttemptId>,
}

#[derive(Clone, Debug)]
pub struct SynthesisStore {
    database: Database,
    redactor: Redactor,
}

impl SynthesisStore {
    #[must_use]
    pub fn new(database: Database) -> Self {
        Self {
            database,
            redactor: Redactor::default(),
        }
    }

    #[must_use]
    pub const fn with_redactor(database: Database, redactor: Redactor) -> Self {
        Self { database, redactor }
    }

    /// Run one bounded synthesis pass over a project's recent successful
    /// episodes.
    ///
    /// A procedure draft is created only when at least
    /// `min_supporting_episodes` distinct successful episodes share one
    /// normalized strategy signature, every one of them carries verified
    /// deterministic/observational evidence, and their touched files overlap.
    /// Drafts start as unvalidated candidates at the agent-inference
    /// confidence cap and never auto-promote. Re-running with the same
    /// episodes converges on the same memory; a superset extends provenance on
    /// the existing memory instead of duplicating it.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid thresholds or canonical storage failures.
    pub fn synthesize(
        &self,
        project_id: &ProjectId,
        options: &SynthesisOptions,
    ) -> Result<SynthesisReport> {
        if options.min_supporting_episodes < 2 {
            return Err(Error::Config(
                "synthesis requires min_supporting_episodes >= 2".to_owned(),
            ));
        }
        let episodes = self.eligible_episodes(project_id)?;
        let examined = u32::try_from(episodes.len()).unwrap_or(u32::MAX);
        let mut groups = BTreeMap::<String, Vec<Episode>>::new();
        for episode in episodes {
            groups
                .entry(episode.steps.join("\n"))
                .or_default()
                .push(episode);
        }
        let memories = MemoryStore::with_redactor(self.database.clone(), self.redactor.clone());
        let mut procedures = Vec::new();
        for (key, group) in &groups {
            let signature = sha256(key.as_bytes());
            let shared_files = shared_files(group);
            let threshold = usize::try_from(options.min_supporting_episodes).unwrap_or(usize::MAX);
            if group.len() >= threshold {
                if shared_files.is_empty() {
                    // Same wording but disjoint file topology: too weak to
                    // generalize; skip conservatively.
                    continue;
                }
                if let Some(result) =
                    self.materialize(&memories, project_id, group, &signature, &shared_files)?
                {
                    procedures.push(result);
                }
            } else if options.cross_project_hypotheses
                && let Some(result) =
                    self.transfer_hypothesis(&memories, project_id, group, &signature)?
            {
                procedures.push(result);
            }
        }
        Ok(SynthesisReport {
            project_id: project_id.clone(),
            examined_episodes: examined,
            procedures,
        })
    }

    /// Promote a validated procedure memory to the `skill` kind (spec §16.4).
    ///
    /// Requirements enforced here, on top of the evidence-gated epistemic
    /// promotion the memory must already have passed:
    /// - the memory is an active, validated `procedure`;
    /// - it has no recorded harmful uses and no contradicting evidence links;
    /// - at least `min_distinct_uses` distinct tasks finished successfully
    ///   after a compiled context that both injected this memory and was
    ///   explicitly declared as serving that task.
    ///
    /// Usage honesty: uses are counted only from explicit
    /// `ContextCompileRequest.task_id` declarations, so the count
    /// under-reports callers that never declare a task and never
    /// over-attributes.
    ///
    /// # Errors
    ///
    /// Returns an error when any gate fails or canonical storage fails.
    pub fn promote_to_skill(&self, id: &MemoryId, min_distinct_uses: u32) -> Result<MemoryRecord> {
        let memories = MemoryStore::with_redactor(self.database.clone(), self.redactor.clone());
        let memory = memories
            .get(id)?
            .ok_or_else(|| Error::Config(format!("unknown memory `{id}`")))?;
        if memory.kind != "procedure" {
            return Err(Error::Config(format!(
                "memory `{id}` is not a procedure (kind `{}`)",
                memory.kind
            )));
        }
        if memory.status != MemoryStatus::Active
            || memory.epistemic_state != EpistemicState::Validated
        {
            return Err(Error::Config(
                "skill promotion requires an active memory validated through the evidence-gated \
                 promotion path"
                    .to_owned(),
            ));
        }
        if memory.harmful_count > 0 {
            return Err(Error::Config(
                "skill promotion is blocked by recorded harmful uses".to_owned(),
            ));
        }
        let (successful_uses, contradictions) = self.database.with_reader(|connection| {
            let uses: i64 = connection.query_row(
                "SELECT COUNT(DISTINCT served.source_id)
                 FROM usage_events injected
                 JOIN usage_events served
                   ON served.context_run_id = injected.context_run_id
                 WHERE injected.source_type = 'memory' AND injected.source_id = ?1
                   AND injected.usage_kind = 'injected'
                   AND served.source_type = 'task'
                   AND served.usage_kind = 'context_served'
                   AND served.outcome = 'success'",
                [id.as_str()],
                |row| row.get(0),
            )?;
            let contradictions: i64 = connection.query_row(
                "SELECT COUNT(*) FROM memory_evidence
                 WHERE memory_id = ?1 AND relation = 'contradicts'",
                [id.as_str()],
                |row| row.get(0),
            )?;
            Ok((uses, contradictions))
        })?;
        if contradictions > 0 {
            return Err(Error::Config(
                "skill promotion is blocked by unresolved contradicting evidence".to_owned(),
            ));
        }
        if successful_uses < i64::from(min_distinct_uses) {
            return Err(Error::Config(format!(
                "skill promotion requires {min_distinct_uses} distinct successful uses; \
                 {successful_uses} recorded"
            )));
        }
        let now = unix_time_ms();
        let project_id = memory.project_id.clone();
        self.database.write_immediate(|transaction| {
            let updated = transaction.execute(
                "UPDATE memories SET kind = 'skill', updated_at_ms = ?2
                 WHERE id = ?1 AND kind = 'procedure' AND status = 'active'",
                params![id.as_str(), now],
            )?;
            if updated != 1 {
                return Err(Error::Config(format!(
                    "memory `{id}` changed concurrently; skill promotion aborted"
                )));
            }
            crate::events::append_generated_in_transaction(
                transaction,
                &DomainEvent::MemoryStateChanged {
                    project_id,
                    memory_id: id.clone(),
                    epistemic_state: EpistemicState::Validated.as_str().to_owned(),
                    status: MemoryStatus::Active.as_str().to_owned(),
                    reason: "procedure_promoted_to_skill".to_owned(),
                },
                now,
            )
        })?;
        memories
            .get(id)?
            .ok_or_else(|| Error::Config("memory disappeared after skill promotion".to_owned()))
    }

    /// Load the bounded set of successful, evidence-backed episodes.
    fn eligible_episodes(&self, project_id: &ProjectId) -> Result<Vec<Episode>> {
        self.database.with_reader(|connection| {
            let task_ids = {
                let mut statement = connection.prepare(
                    "SELECT t.id FROM tasks t
                     WHERE t.project_id = ?1 AND t.status = 'success'
                       AND EXISTS (
                           SELECT 1 FROM attempts a
                           WHERE a.task_id = t.id AND a.outcome = 'success'
                       )
                       AND EXISTS (
                           SELECT 1 FROM verified_supporting_evidence v
                           WHERE v.task_id = t.id AND v.project_id = ?1
                       )
                     ORDER BY t.completed_at_ms DESC, t.id
                     LIMIT ?2",
                )?;
                let rows = statement.query_map(
                    params![
                        project_id.as_str(),
                        i64::try_from(MAX_SCANNED_EPISODES).unwrap_or(i64::MAX)
                    ],
                    |row| row.get::<_, String>(0),
                )?;
                rows.collect::<std::result::Result<Vec<_>, _>>()?
            };
            let mut episodes = Vec::with_capacity(task_ids.len());
            for raw_task_id in task_ids {
                let task_id = TaskId::parse(&raw_task_id)?;
                let mut steps = Vec::new();
                let mut files = BTreeSet::new();
                let mut failed_attempts = Vec::new();
                {
                    let mut statement = connection.prepare(
                        "SELECT id, strategy_summary, outcome, files_json
                         FROM attempts WHERE task_id = ?1 ORDER BY ordinal",
                    )?;
                    let rows = statement.query_map([raw_task_id.as_str()], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    })?;
                    for row in rows {
                        let (attempt_id, strategy, outcome, files_json) = row?;
                        if outcome == "success" {
                            steps.push(normalize_step(&strategy));
                            let touched: Vec<String> = serde_json::from_str(&files_json)?;
                            files.extend(
                                touched
                                    .iter()
                                    .map(|path| path.trim().to_owned())
                                    .filter(|path| !path.is_empty()),
                            );
                        } else if outcome == "failure" {
                            failed_attempts.push(AttemptId::parse(&attempt_id)?);
                        }
                    }
                }
                let evidence = {
                    let mut statement = connection.prepare(
                        "SELECT DISTINCT evidence_id FROM verified_supporting_evidence
                         WHERE task_id = ?1 AND project_id = ?2 ORDER BY evidence_id",
                    )?;
                    let rows = statement
                        .query_map(params![raw_task_id.as_str(), project_id.as_str()], |row| {
                            row.get::<_, String>(0)
                        })?;
                    rows.map(|row| EvidenceId::parse(&row?))
                        .collect::<Result<Vec<_>>>()?
                };
                if steps.is_empty() || evidence.is_empty() {
                    continue;
                }
                episodes.push(Episode {
                    task_id,
                    steps,
                    files,
                    evidence,
                    failed_attempts,
                });
            }
            Ok(episodes)
        })
    }

    /// Create or extend the canonical procedure memory for one matched group.
    fn materialize(
        &self,
        memories: &MemoryStore,
        project_id: &ProjectId,
        group: &[Episode],
        signature: &str,
        shared_files: &BTreeSet<String>,
    ) -> Result<Option<SynthesizedProcedure>> {
        let steps = group[0].steps.clone();
        let claim = procedure_claim(&steps);
        if claim.len() > 16_000 {
            // A claim this long cannot be stored losslessly; skip rather than
            // truncate into an ambiguous procedure.
            return Ok(None);
        }
        let source_task_ids = group
            .iter()
            .map(|episode| episode.task_id.clone())
            .collect::<Vec<_>>();
        let body = ProcedureBody {
            procedure_version: 1,
            signature: signature.to_owned(),
            steps,
            precondition_files: shared_files.iter().cloned().collect(),
            source_episode_ids: source_task_ids.clone(),
            evidence_ids: bounded_evidence_union(group),
            negative_example_attempt_ids: bounded_negative_examples(group),
            hypothesis: false,
            transfer: None,
        };
        let draft = MemoryDraft {
            scope_type: "project".to_owned(),
            scope_id: project_id.to_string(),
            kind: "procedure".to_owned(),
            claim: claim.clone(),
            rationale: serde_json::to_string(&body)?,
            confidence: SYNTHESIZED_CONFIDENCE,
            importance: 0.7,
            tags: vec!["procedure".to_owned(), "synthesized".to_owned()],
            valid_from_commit: None,
            valid_to_commit: None,
            trust_source: "agent_inference".to_owned(),
            evidence_ids: body.evidence_ids.clone(),
            source_task_ids: source_task_ids.clone(),
            // Anchor the procedure to its precondition files' symbols so
            // drift invalidation can quarantine it when they change.
            node_ids: crate::learning::derive_node_links(
                &self.database,
                project_id,
                &source_task_ids,
                32,
            )?,
        };
        let (memory, reused_existing) = if let Some(existing) =
            memories.find_exact(project_id, "project", &draft.scope_id, "procedure", &claim)?
        {
            // Same or superset episode input: extend provenance on the
            // existing memory instead of duplicating it.
            (
                memories.link_distillation(&existing.id, project_id, &draft)?,
                true,
            )
        } else {
            let memory = memories.create_candidate(project_id, &draft)?;
            EventStore::new(self.database.clone()).append(&DomainEvent::ProcedureSynthesized {
                project_id: project_id.clone(),
                memory_id: memory.id.clone(),
                signature: signature.to_owned(),
                source_task_ids: source_task_ids.clone(),
                cross_project_hypothesis: false,
            })?;
            (memory, false)
        };
        Ok(Some(SynthesizedProcedure {
            memory,
            signature: signature.to_owned(),
            supporting_episodes: source_task_ids,
            reused_existing,
            cross_project_hypothesis: false,
        }))
    }

    /// Spec §16.5: an identical procedure validated in another project may
    /// enter this project as an explicitly marked hypothesis when local
    /// evidence exists but is below the local synthesis threshold.
    fn transfer_hypothesis(
        &self,
        memories: &MemoryStore,
        project_id: &ProjectId,
        group: &[Episode],
        signature: &str,
    ) -> Result<Option<SynthesizedProcedure>> {
        let steps = group[0].steps.clone();
        let claim = procedure_claim(&steps);
        if claim.len() > 16_000 {
            return Ok(None);
        }
        if memories
            .find_exact(
                project_id,
                "project",
                project_id.as_str(),
                "procedure",
                &claim,
            )?
            .is_some()
        {
            // The procedure (or an earlier hypothesis) already exists locally;
            // re-running stays side-effect free.
            return Ok(None);
        }
        let normalized = crate::memory::normalize_claim(&claim);
        let source = self.database.with_reader(|connection| {
            connection
                .query_row(
                    "SELECT id, project_id FROM memories
                     WHERE project_id <> ?1 AND kind = 'procedure'
                       AND epistemic_state = 'validated' AND status = 'active'
                       AND scope_type = 'project' AND normalized_claim = ?2
                     ORDER BY known_from_ms, id LIMIT 1",
                    params![project_id.as_str(), normalized],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(Error::from)
        })?;
        let Some((source_memory, source_project)) = source else {
            return Ok(None);
        };
        let source_task_ids = group
            .iter()
            .map(|episode| episode.task_id.clone())
            .collect::<Vec<_>>();
        let body = ProcedureBody {
            procedure_version: 1,
            signature: signature.to_owned(),
            steps,
            precondition_files: shared_files(group).into_iter().collect(),
            source_episode_ids: source_task_ids.clone(),
            evidence_ids: bounded_evidence_union(group),
            negative_example_attempt_ids: bounded_negative_examples(group),
            hypothesis: true,
            transfer: Some(ProcedureTransfer {
                source_project_id: ProjectId::parse(&source_project)?,
                source_memory_id: MemoryId::parse(&source_memory)?,
            }),
        };
        let draft = MemoryDraft {
            scope_type: "project".to_owned(),
            scope_id: project_id.to_string(),
            kind: "procedure".to_owned(),
            claim,
            rationale: serde_json::to_string(&body)?,
            confidence: HYPOTHESIS_CONFIDENCE,
            importance: 0.5,
            tags: vec![
                "cross_project".to_owned(),
                "hypothesis".to_owned(),
                "procedure".to_owned(),
            ],
            valid_from_commit: None,
            valid_to_commit: None,
            trust_source: "agent_inference".to_owned(),
            evidence_ids: body.evidence_ids.clone(),
            source_task_ids: source_task_ids.clone(),
            node_ids: Vec::new(),
        };
        let memory = memories.create_candidate(project_id, &draft)?;
        EventStore::new(self.database.clone()).append(&DomainEvent::ProcedureSynthesized {
            project_id: project_id.clone(),
            memory_id: memory.id.clone(),
            signature: signature.to_owned(),
            source_task_ids: source_task_ids.clone(),
            cross_project_hypothesis: true,
        })?;
        Ok(Some(SynthesizedProcedure {
            memory,
            signature: signature.to_owned(),
            supporting_episodes: source_task_ids,
            reused_existing: false,
            cross_project_hypothesis: true,
        }))
    }
}

/// Normalize one strategy step exactly like memory claims: collapse
/// whitespace runs to single spaces and lowercase.
fn normalize_step(step: &str) -> String {
    step.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Deterministic ordered claim rendered from normalized steps.
fn procedure_claim(steps: &[String]) -> String {
    format!("Procedure: {}", steps.join(" -> "))
}

/// Files touched by every supporting episode (the shared precondition scope).
fn shared_files(group: &[Episode]) -> BTreeSet<String> {
    let mut iterator = group.iter();
    let Some(first) = iterator.next() else {
        return BTreeSet::new();
    };
    let mut shared = first.files.clone();
    for episode in iterator {
        shared = shared.intersection(&episode.files).cloned().collect();
    }
    shared
}

/// Sorted union of verified evidence across the group, bounded for storage.
fn bounded_evidence_union(group: &[Episode]) -> Vec<EvidenceId> {
    let mut union = BTreeSet::new();
    for episode in group {
        union.extend(episode.evidence.iter().cloned());
    }
    union.into_iter().take(MAX_EVIDENCE_LINKS).collect()
}

/// Sorted bounded failed-variant references across the group.
fn bounded_negative_examples(group: &[Episode]) -> Vec<AttemptId> {
    let mut union = BTreeSet::new();
    for episode in group {
        union.extend(episode.failed_attempts.iter().cloned());
    }
    union.into_iter().take(MAX_NEGATIVE_EXAMPLES).collect()
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn unix_time_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::context::{ContextCompileRequest, ContextCompiler, ContextMode};
    use crate::experience::{AttemptOutcome, EvidenceStrength, ExperienceStore, TaskOutcome};
    use crate::projects::ProjectRegistry;

    fn fixture() -> (TempDir, Database, ProjectId) {
        let temp = TempDir::new().expect("temp");
        let root = temp.path().join("project");
        std::fs::create_dir(&root).expect("project");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("project")
            .project
            .id;
        (temp, database, project)
    }

    fn options(min: u32) -> SynthesisOptions {
        SynthesisOptions {
            min_supporting_episodes: min,
            cross_project_hypotheses: false,
        }
    }

    /// One successful episode with a matching strategy; `verified` controls
    /// whether it carries Tier A/B evidence.
    fn episode(
        database: &Database,
        project: &ProjectId,
        strategy: &str,
        files: &[&str],
        verified: bool,
    ) -> TaskId {
        let experience = ExperienceStore::new(database.clone());
        let task = experience
            .start_task(project, "fix worker lifecycle timeout", None)
            .expect("task");
        let attempt = experience
            .record_attempt(
                &task.id,
                strategy,
                AttemptOutcome::Success,
                &files
                    .iter()
                    .map(|&file| file.to_owned())
                    .collect::<Vec<_>>(),
            )
            .expect("attempt");
        if verified {
            experience
                .record_evidence(
                    project,
                    Some(&task.id),
                    Some(&attempt.id),
                    "test_result",
                    EvidenceStrength::Deterministic,
                    None,
                    Some("cargo test"),
                    Some(0),
                    b"ok",
                    &json!({}),
                )
                .expect("evidence");
        } else {
            experience
                .record_evidence(
                    project,
                    Some(&task.id),
                    Some(&attempt.id),
                    "log",
                    EvidenceStrength::Inferred,
                    None,
                    None,
                    None,
                    b"narrative only",
                    &json!({}),
                )
                .expect("inferred evidence");
        }
        experience
            .finish_task(&task.id, TaskOutcome::Success, Some(true), None)
            .expect("finish");
        task.id
    }

    #[test]
    fn below_threshold_synthesizes_nothing_and_third_episode_creates_one_draft() {
        let (_temp, database, project) = fixture();
        let synthesis = SynthesisStore::new(database.clone());
        episode(
            &database,
            &project,
            "Inspect signal recipient",
            &["a.rs"],
            true,
        );
        episode(
            &database,
            &project,
            "inspect  SIGNAL recipient",
            &["a.rs"],
            true,
        );

        let report = synthesis.synthesize(&project, &options(3)).expect("run");
        assert_eq!(report.examined_episodes, 2);
        assert!(
            report.procedures.is_empty(),
            "two episodes must not synthesize"
        );

        episode(
            &database,
            &project,
            "INSPECT signal   recipient",
            &["a.rs", "b.rs"],
            true,
        );
        let report = synthesis.synthesize(&project, &options(3)).expect("run");
        assert_eq!(report.procedures.len(), 1);
        let procedure = &report.procedures[0];
        assert!(!procedure.reused_existing);
        assert_eq!(procedure.memory.kind, "procedure");
        assert_eq!(procedure.memory.epistemic_state, EpistemicState::Candidate);
        assert!(
            (procedure.memory.confidence - 0.5).abs() < f64::EPSILON,
            "synthesized drafts stay at the agent-inference cap"
        );
        assert_eq!(
            procedure.memory.claim,
            "Procedure: inspect signal recipient"
        );
        let body: ProcedureBody =
            serde_json::from_str(&procedure.memory.rationale).expect("procedure_json");
        assert_eq!(body.steps, vec!["inspect signal recipient".to_owned()]);
        assert_eq!(body.precondition_files, vec!["a.rs".to_owned()]);
        assert_eq!(body.source_episode_ids.len(), 3);
        assert!(!body.hypothesis);
        assert!(!body.evidence_ids.is_empty());
    }

    #[test]
    fn episodes_without_verified_evidence_do_not_count() {
        let (_temp, database, project) = fixture();
        let synthesis = SynthesisStore::new(database.clone());
        episode(&database, &project, "widen retry backoff", &["r.rs"], true);
        episode(&database, &project, "widen retry backoff", &["r.rs"], true);
        episode(&database, &project, "widen retry backoff", &["r.rs"], false);

        let report = synthesis.synthesize(&project, &options(3)).expect("run");
        assert_eq!(
            report.examined_episodes, 2,
            "unverified episodes are not eligible"
        );
        assert!(report.procedures.is_empty());
    }

    #[test]
    fn resynthesis_is_idempotent_and_a_superset_extends_provenance() {
        let (_temp, database, project) = fixture();
        let synthesis = SynthesisStore::new(database.clone());
        for _ in 0..3 {
            episode(&database, &project, "rebuild schema cache", &["s.rs"], true);
        }
        let first = synthesis.synthesize(&project, &options(3)).expect("first");
        let second = synthesis.synthesize(&project, &options(3)).expect("second");
        assert_eq!(first.procedures.len(), 1);
        assert_eq!(second.procedures.len(), 1);
        assert_eq!(
            first.procedures[0].memory.id,
            second.procedures[0].memory.id
        );
        assert!(second.procedures[0].reused_existing);

        episode(&database, &project, "rebuild schema cache", &["s.rs"], true);
        let superset = synthesis
            .synthesize(&project, &options(3))
            .expect("superset");
        assert_eq!(superset.procedures.len(), 1);
        assert_eq!(
            superset.procedures[0].memory.id,
            first.procedures[0].memory.id
        );
        let linked_sources: i64 = database
            .with_reader(|connection| {
                connection
                    .query_row(
                        "SELECT COUNT(*) FROM memory_sources WHERE memory_id = ?1",
                        [first.procedures[0].memory.id.as_str()],
                        |row| row.get(0),
                    )
                    .map_err(Error::from)
            })
            .expect("sources");
        assert_eq!(linked_sources, 4, "superset must extend provenance");
        let procedure_count: i64 = database
            .with_reader(|connection| {
                connection
                    .query_row(
                        "SELECT COUNT(*) FROM memories
                         WHERE project_id = ?1 AND kind = 'procedure'",
                        [project.as_str()],
                        |row| row.get(0),
                    )
                    .map_err(Error::from)
            })
            .expect("count");
        assert_eq!(procedure_count, 1, "no duplicate procedure memories");
    }

    #[test]
    fn cross_project_hypotheses_are_gated_and_explicitly_marked() {
        let (temp, database, project_a) = fixture();
        let other_root = temp.path().join("other");
        std::fs::create_dir(&other_root).expect("other");
        let project_b = ProjectRegistry::new(database.clone())
            .resolve_or_register(&other_root)
            .expect("other project")
            .project
            .id;
        let synthesis = SynthesisStore::new(database.clone());

        // Project B: fully synthesized and validated procedure.
        for _ in 0..3 {
            episode(
                &database,
                &project_b,
                "drain queue before shutdown",
                &["q.rs"],
                true,
            );
        }
        let created = synthesis
            .synthesize(&project_b, &options(3))
            .expect("project B synthesis");
        let procedure_b = &created.procedures[0].memory;
        let memories = MemoryStore::new(database.clone());
        for state in [
            EpistemicState::Observed,
            EpistemicState::Supported,
            EpistemicState::Validated,
        ] {
            memories
                .promote(&procedure_b.id, state, false)
                .expect("evidence-gated promotion");
        }

        // Project A: one matching local episode, below threshold.
        episode(
            &database,
            &project_a,
            "drain queue before shutdown",
            &["q.rs"],
            true,
        );
        let disabled = synthesis
            .synthesize(&project_a, &options(3))
            .expect("disabled run");
        assert!(
            disabled.procedures.is_empty(),
            "cross-project transfer must stay off by default"
        );

        let enabled = SynthesisOptions {
            min_supporting_episodes: 3,
            cross_project_hypotheses: true,
        };
        let transferred = synthesis
            .synthesize(&project_a, &enabled)
            .expect("enabled run");
        assert_eq!(transferred.procedures.len(), 1);
        let hypothesis = &transferred.procedures[0];
        assert!(hypothesis.cross_project_hypothesis);
        assert_eq!(hypothesis.memory.project_id, project_a);
        assert_eq!(hypothesis.memory.kind, "procedure");
        assert_eq!(hypothesis.memory.epistemic_state, EpistemicState::Candidate);
        assert!(hypothesis.memory.confidence <= 0.5);
        assert!(hypothesis.memory.tags.contains(&"hypothesis".to_owned()));
        assert!(hypothesis.memory.tags.contains(&"cross_project".to_owned()));
        let body: ProcedureBody = serde_json::from_str(&hypothesis.memory.rationale).expect("body");
        assert!(body.hypothesis);
        let transfer = body.transfer.expect("transfer provenance");
        assert_eq!(transfer.source_project_id, project_b);
        assert_eq!(transfer.source_memory_id, procedure_b.id);

        let rerun = synthesis
            .synthesize(&project_a, &enabled)
            .expect("idempotent rerun");
        assert!(rerun.procedures.is_empty(), "hypothesis is created once");
    }

    #[test]
    fn skill_promotion_requires_validation_and_recorded_successful_uses() {
        let (_temp, database, project) = fixture();
        let synthesis = SynthesisStore::new(database.clone());
        for _ in 0..3 {
            episode(
                &database,
                &project,
                "restart stalled indexer",
                &["i.rs"],
                true,
            );
        }
        let created = synthesis.synthesize(&project, &options(3)).expect("run");
        let procedure = created.procedures[0].memory.clone();

        let unvalidated = synthesis
            .promote_to_skill(&procedure.id, 1)
            .expect_err("candidate procedures cannot become skills");
        assert!(unvalidated.to_string().contains("validated"));

        let memories = MemoryStore::new(database.clone());
        for state in [
            EpistemicState::Observed,
            EpistemicState::Supported,
            EpistemicState::Validated,
        ] {
            memories
                .promote(&procedure.id, state, false)
                .expect("promotion");
        }
        let unused = synthesis
            .promote_to_skill(&procedure.id, 2)
            .expect_err("validated but unused procedures cannot become skills");
        assert!(unused.to_string().contains("distinct successful uses"));

        // Two declared-task compilations that inject the procedure, each
        // followed by a successful finish, record two distinct truthful uses.
        let experience = ExperienceStore::new(database.clone());
        let compiler = ContextCompiler::new(database.clone(), 6_000, 1_800);
        for ordinal in 0..2 {
            let task = experience
                .start_task(&project, "restart stalled indexer again", None)
                .expect("use task");
            let context = compiler
                .compile(&ContextCompileRequest {
                    request_id: format!("use-{ordinal}"),
                    project_id: project.clone(),
                    task_text: "restart stalled indexer".to_owned(),
                    current_paths: Vec::new(),
                    token_budget: 500,
                    mode: ContextMode::Brain,
                    repository_root: None,
                    head: None,
                    task_id: Some(task.id.clone()),
                })
                .expect("compile");
            assert!(
                context
                    .items
                    .iter()
                    .any(|item| item.source_id == procedure.id.as_str()),
                "validated procedure must be injectable"
            );
            experience
                .finish_task(&task.id, TaskOutcome::Success, Some(true), None)
                .expect("finish use");
        }
        let skill = synthesis
            .promote_to_skill(&procedure.id, 2)
            .expect("promotion after recorded uses");
        assert_eq!(skill.kind, "skill");
        assert_eq!(skill.epistemic_state, EpistemicState::Validated);

        let repeated = synthesis
            .promote_to_skill(&procedure.id, 2)
            .expect_err("skills are not procedures anymore");
        assert!(repeated.to_string().contains("not a procedure"));
    }
}
