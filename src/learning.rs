//! Conservative episode-to-memory distillation and exact deduplication.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::events::{DomainEvent, EventStore};
use crate::ids::{ProjectId, TaskId};
use crate::memory::{MemoryDraft, MemoryRecord, MemoryStore};
use crate::security::Redactor;
use crate::storage::Database;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DistillationRequest {
    pub project_id: ProjectId,
    pub memory: MemoryDraft,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DistillationResult {
    pub memory: MemoryRecord,
    pub reused_existing: bool,
    /// The candidate still needs review; learning does not enable injection.
    pub next_step: String,
}

#[derive(Clone, Debug)]
pub struct LearningStore {
    database: Database,
    redactor: Redactor,
}

impl LearningStore {
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

    /// Distill an evidence-linked completed episode into a candidate memory.
    ///
    /// This operation never promotes the result. Exact normalized duplicates
    /// in the same scope are linked to the new provenance instead of copied.
    ///
    /// # Errors
    ///
    /// Returns an error if no successful source task/Tier A-B evidence exists,
    /// provenance crosses projects, or canonical storage fails.
    pub fn distill(&self, request: &DistillationRequest) -> Result<DistillationResult> {
        validate_distillation(&self.database, request)?;
        let mut draft = request.memory.clone();
        if draft.node_ids.is_empty() {
            // Without symbol links, drift invalidation can never quarantine
            // this lesson when the code it describes changes (spec 15.1), so
            // anchor it to the indexed symbols of the episode's touched files.
            draft.node_ids = derive_node_links(
                &self.database,
                &request.project_id,
                &draft.source_task_ids,
                MAX_DERIVED_NODE_LINKS,
            )?;
        }
        let memories = MemoryStore::with_redactor(self.database.clone(), self.redactor.clone());
        if let Some(existing) = memories.find_exact(
            &request.project_id,
            &draft.scope_type,
            &draft.scope_id,
            &draft.kind,
            &draft.claim,
        )? {
            let memory = memories.link_distillation(&existing.id, &request.project_id, &draft)?;
            return Ok(DistillationResult {
                memory,
                reused_existing: true,
                next_step: "Review the memory in the web UI; Promote validates it with a recorded human override. Existing validated memories retain their state.".to_owned(),
            });
        }
        let memory = memories.create_candidate(&request.project_id, &draft)?;
        EventStore::new(self.database.clone()).append(&DomainEvent::LearningDistilled {
            project_id: request.project_id.clone(),
            memory_id: memory.id.clone(),
            source_task_ids: request.memory.source_task_ids.clone(),
            reused_existing: false,
        })?;
        Ok(DistillationResult {
            memory,
            reused_existing: false,
            next_step: "Review this candidate in the web UI and click Promote to validate it. It will not inject until validated. Automation can use the stepwise evidence-gated memory promote command.".to_owned(),
        })
    }
}

/// Hard bound on symbol links derived per memory: enough to anchor drift
/// invalidation without turning one broad episode into a full-graph link fan.
const MAX_DERIVED_NODE_LINKS: usize = 32;

/// Resolve the active indexed symbols in the source episodes' touched files,
/// deterministically ordered and hard-bounded, as drift-invalidation anchors.
///
/// # Errors
///
/// Returns an error when canonical storage cannot be read.
pub fn derive_node_links(
    database: &Database,
    project_id: &ProjectId,
    source_task_ids: &[TaskId],
    limit: usize,
) -> Result<Vec<crate::ids::NodeId>> {
    if source_task_ids.is_empty() {
        return Ok(Vec::new());
    }
    let task_ids = serde_json::to_string(
        &source_task_ids
            .iter()
            .map(TaskId::as_str)
            .collect::<Vec<_>>(),
    )?;
    database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT DISTINCT n.id FROM nodes n
             WHERE n.project_id = ?1 AND n.tombstoned_at_ms IS NULL
               AND n.file_path IN (
                 SELECT DISTINCT files.value
                 FROM attempts a
                 JOIN tasks t ON t.id = a.task_id
                 JOIN json_each(a.files_json) AS files
                 WHERE t.project_id = ?1
                   AND a.task_id IN (SELECT value FROM json_each(?2))
               )
             ORDER BY n.id LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![
                project_id.as_str(),
                task_ids,
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            |row| row.get::<_, String>(0),
        )?;
        let mut nodes = Vec::new();
        for row in rows {
            nodes.push(crate::ids::NodeId::parse(&row?)?);
        }
        Ok(nodes)
    })
}

fn validate_distillation(database: &Database, request: &DistillationRequest) -> Result<()> {
    if request.memory.source_task_ids.is_empty() {
        return Err(Error::Config(
            "distillation requires at least one source task: call engram_task_start, record evidence on its task_id, then engram_mark_outcome(success) before engram_learn".to_owned(),
        ));
    }
    if request.memory.evidence_ids.is_empty() {
        return Err(Error::Config(
            "distillation requires independent evidence: call engram_record_evidence with this project_id and source task_id; use deterministic test_result/build_result/lint_result/typecheck_result with command and exit_code=0, or observational runtime_observation/trace/metric with the observed output; then mark the task success and pass the returned evidence_id to engram_learn".to_owned(),
        ));
    }
    database.with_reader(|connection| {
        for task_id in &request.memory.source_task_ids {
            let status: Option<String> = connection.query_row(
                "SELECT status FROM tasks WHERE id = ?1 AND project_id = ?2",
                params![task_id.as_str(), request.project_id.as_str()],
                |row| row.get(0),
            ).optional()?;
            match status.as_deref() {
                Some("success") => {},
                Some("open") => return Err(Error::Config(format!(
                    "source task `{task_id}` is still open: record evidence on this task, then call engram_mark_outcome with task_id={task_id} and outcome=success before retrying engram_learn; accepted_by_user may remain unknown"
                ))),
                Some(status) => return Err(Error::Config(format!(
                    "source task `{task_id}` ended as {status}; only successful episodes can be distilled. Use a successful task with its own supporting evidence; do not relabel a failed episode"
                ))),
                None => return Err(Error::Config(format!(
                    "source task `{task_id}` does not belong to this project: resolve the repository being edited with engram_project_status and use its project_id and task_id"
                ))),
            }
        }
        let task_ids = serde_json::to_string(
            &request
                .memory
                .source_task_ids
                .iter()
                .map(TaskId::as_str)
                .collect::<Vec<_>>(),
        )?;
        for evidence_id in &request.memory.evidence_ids {
            let valid: i64 = connection.query_row(
                "SELECT COUNT(*) FROM verified_supporting_evidence
                 WHERE evidence_id = ?1 AND project_id = ?2
                   AND task_id IN (SELECT value FROM json_each(?3))",
                params![evidence_id.as_str(), request.project_id.as_str(), task_ids],
                |row| row.get(0),
            )?;
            if valid != 1 {
                return Err(Error::Config(format!(
                    "evidence `{evidence_id}` is not qualifying evidence from these source tasks: use the same project_id and task_id, a successful attempt (or omit attempt_id), and matching commit. Deterministic kinds require command and exit_code=0; ops observations use kind=runtime_observation|trace|metric, strength=observational, and exit_code=0 or omitted. Record the actual output, mark the source task success, then retry engram_learn"
                )));
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::experience::{EvidenceStrength, ExperienceStore, TaskOutcome};
    use crate::projects::ProjectRegistry;

    #[test]
    fn ops_observation_requires_correct_scope_then_learns_and_validates() {
        let temp = TempDir::new().expect("temp");
        let root = temp.path().join("project");
        std::fs::create_dir(&root).expect("root");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("project")
            .project
            .id;
        let experience = ExperienceStore::new(database.clone());
        let task = experience
            .start_task(&project, "verify Cloud Run traffic", None)
            .expect("task");
        let evidence = experience.record_evidence(&project, Some(&task.id), None,
            "runtime_observation", EvidenceStrength::Observational, None,
            Some("gcloud run services describe fixture-service --format=json"), Some(0),
            br#"{"traffic":[{"revisionName":"fixture-old","percent":100}],"latestReadyRevisionName":"fixture-new"}"#,
            &json!({"fixture":true})).expect("observation");
        let mut request = DistillationRequest {
            project_id: project.clone(),
            memory: MemoryDraft {
                scope_type: "project".to_owned(),
                scope_id: project.to_string(),
                kind: "lesson".to_owned(),
                claim: "Cloud Run traffic is pinned per revision; verify traffic after deploying."
                    .to_owned(),
                rationale: "Observed traffic stays on the old revision.".to_owned(),
                confidence: 0.5,
                importance: 0.8,
                tags: vec!["traffic".to_owned()],
                valid_from_commit: None,
                valid_to_commit: None,
                trust_source: "agent_inference".to_owned(),
                evidence_ids: vec![evidence.id],
                source_task_ids: vec![task.id.clone()],
                node_ids: vec![],
            },
        };
        let learning = LearningStore::new(database.clone());
        assert!(
            learning
                .distill(&request)
                .expect_err("still open")
                .to_string()
                .contains("engram_mark_outcome")
        );
        experience
            .finish_task(&task.id, TaskOutcome::Success, None, None)
            .expect("success without acceptance");
        let result = learning.distill(&request).expect("learn observation");
        assert_eq!(
            result.memory.epistemic_state,
            crate::memory::EpistemicState::Candidate
        );
        assert!((result.memory.confidence - 0.5).abs() < f64::EPSILON);
        assert!(result.next_step.contains("Promote"));
        let memories = MemoryStore::new(database.clone());
        for state in [
            crate::memory::EpistemicState::Observed,
            crate::memory::EpistemicState::Supported,
            crate::memory::EpistemicState::Validated,
        ] {
            memories
                .promote(&result.memory.id, state, false)
                .expect("evidence-gated promotion");
        }
        let other = experience
            .start_task(&project, "unrelated success", None)
            .expect("other");
        experience
            .finish_task(&other.id, TaskOutcome::Success, None, None)
            .expect("finish");
        request.memory.source_task_ids = vec![other.id];
        assert!(
            learning
                .distill(&request)
                .expect_err("wrong task")
                .to_string()
                .contains("same project_id and task_id")
        );
    }

    #[test]
    fn distilled_lessons_are_anchored_to_touched_symbols_and_drift_invalidated() {
        let temp = TempDir::new().expect("temp");
        let root = temp.path().join("project");
        std::fs::create_dir(&root).expect("project");
        std::fs::write(
            root.join("lib.rs"),
            "pub struct Reactor;\nimpl Reactor { pub fn cleanup(&self) -> bool { true } }\n",
        )
        .expect("source");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("project");
        let world = crate::world::WorldModel::new(database.clone());
        let provider =
            crate::world::TreeSitterProvider::new(100, 1_000_000, 10_000, 20_000, 16_000_000);
        world.index(&provider, &project, None).expect("index");

        let experience = ExperienceStore::new(database.clone());
        let task = experience
            .start_task(&project.project.id, "fix reactor cleanup", None)
            .expect("task");
        experience
            .record_attempt(
                &task.id,
                "move cleanup into the child lifecycle",
                crate::experience::AttemptOutcome::Success,
                &["lib.rs".to_owned()],
            )
            .expect("attempt");
        let evidence = experience
            .record_evidence(
                &project.project.id,
                Some(&task.id),
                None,
                "test_result",
                EvidenceStrength::Deterministic,
                None,
                Some("cargo test"),
                Some(0),
                b"ok",
                &json!({}),
            )
            .expect("evidence");
        experience
            .finish_task(&task.id, TaskOutcome::Success, Some(true), None)
            .expect("finish");

        let request = DistillationRequest {
            project_id: project.project.id.clone(),
            memory: MemoryDraft {
                scope_type: "project".to_owned(),
                scope_id: project.project.id.to_string(),
                kind: "lesson".to_owned(),
                claim: "Cleanup runs in the child lifecycle".to_owned(),
                rationale: "Regression test passed".to_owned(),
                confidence: 0.5,
                importance: 0.8,
                tags: Vec::new(),
                valid_from_commit: None,
                valid_to_commit: None,
                trust_source: "agent_inference".to_owned(),
                evidence_ids: vec![evidence.id],
                source_task_ids: vec![task.id],
                node_ids: Vec::new(),
            },
        };
        let learning = LearningStore::new(database.clone());
        let distilled = learning.distill(&request).expect("distill");
        let links: i64 = database
            .with_reader(|connection| {
                connection
                    .query_row(
                        "SELECT COUNT(*) FROM memory_nodes WHERE memory_id = ?1",
                        [distilled.memory.id.as_str()],
                        |row| row.get(0),
                    )
                    .map_err(Error::from)
            })
            .expect("links");
        assert!(
            links > 0,
            "a distilled lesson with no explicit links must anchor to touched-file symbols"
        );

        // The whole point of the anchors: an implementation change must
        // quarantine the lesson without anyone naming it explicitly.
        std::fs::write(
            root.join("lib.rs"),
            "pub struct Reactor;\nimpl Reactor { pub fn cleanup(&self) -> bool { false } }\n",
        )
        .expect("drift");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("re-resolve");
        world.index(&provider, &project, None).expect("reindex");
        let memories = MemoryStore::new(database.clone());
        let drift = memories
            .mark_symbol_drift_suspect(&project.project.id)
            .expect("drift sweep");
        assert!(
            drift.invalidated >= 1,
            "drift must quarantine the lesson: {drift:?}"
        );
    }

    #[test]
    fn successful_evidence_linked_episode_distills_once() {
        let temp = TempDir::new().expect("temp");
        let root = temp.path().join("project");
        std::fs::create_dir(&root).expect("project");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("project");
        let experience = ExperienceStore::new(database.clone());
        let task = experience
            .start_task(&project.project.id, "fix cleanup", None)
            .expect("task");
        let evidence = experience
            .record_evidence(
                &project.project.id,
                Some(&task.id),
                None,
                "test_result",
                EvidenceStrength::Deterministic,
                None,
                Some("cargo test"),
                Some(0),
                b"ok",
                &json!({}),
            )
            .expect("evidence");
        experience
            .finish_task(&task.id, TaskOutcome::Success, Some(true), None)
            .expect("finish");
        let request = DistillationRequest {
            project_id: project.project.id.clone(),
            memory: MemoryDraft {
                scope_type: "project".to_owned(),
                scope_id: project.project.id.to_string(),
                kind: "lesson".to_owned(),
                claim: "Cleanup must run after abort".to_owned(),
                rationale: "Regression test passed".to_owned(),
                confidence: 0.5,
                importance: 0.8,
                tags: vec!["cleanup".to_owned()],
                valid_from_commit: None,
                valid_to_commit: None,
                trust_source: "agent_inference".to_owned(),
                evidence_ids: vec![evidence.id],
                source_task_ids: vec![task.id],
                node_ids: Vec::new(),
            },
        };
        let learning = LearningStore::new(database);
        let first = learning.distill(&request).expect("distill");
        assert!(!first.reused_existing);
        let mut duplicate = request;
        duplicate.memory.claim = " cleanup   MUST run AFTER abort ".to_owned();
        let second = learning.distill(&duplicate).expect("dedupe");
        assert!(second.reused_existing);
        assert_eq!(first.memory.id, second.memory.id);
    }
}
