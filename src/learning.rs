//! Conservative episode-to-memory distillation and exact deduplication.

use rusqlite::params;
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
        let memories = MemoryStore::with_redactor(self.database.clone(), self.redactor.clone());
        if let Some(existing) = memories.find_exact(
            &request.project_id,
            &request.memory.scope_type,
            &request.memory.scope_id,
            &request.memory.kind,
            &request.memory.claim,
        )? {
            let memory =
                memories.link_distillation(&existing.id, &request.project_id, &request.memory)?;
            return Ok(DistillationResult {
                memory,
                reused_existing: true,
            });
        }
        let memory = memories.create_candidate(&request.project_id, &request.memory)?;
        EventStore::new(self.database.clone()).append(&DomainEvent::LearningDistilled {
            project_id: request.project_id.clone(),
            memory_id: memory.id.clone(),
            source_task_ids: request.memory.source_task_ids.clone(),
            reused_existing: false,
        })?;
        Ok(DistillationResult {
            memory,
            reused_existing: false,
        })
    }
}

fn validate_distillation(database: &Database, request: &DistillationRequest) -> Result<()> {
    if request.memory.source_task_ids.is_empty() {
        return Err(Error::Config(
            "distillation requires at least one source task".to_owned(),
        ));
    }
    if request.memory.evidence_ids.is_empty() {
        return Err(Error::Config(
            "distillation requires independent evidence".to_owned(),
        ));
    }
    database.with_reader(|connection| {
        for task_id in &request.memory.source_task_ids {
            let valid: i64 = connection.query_row(
                "SELECT COUNT(*) FROM tasks
                 WHERE id = ?1 AND project_id = ?2 AND status = 'success'",
                params![task_id.as_str(), request.project_id.as_str()],
                |row| row.get(0),
            )?;
            if valid != 1 {
                return Err(Error::Config(format!(
                    "source task `{task_id}` is not a successful task in this project"
                )));
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
                    "evidence `{evidence_id}` is not Tier A/B evidence from a source task"
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
