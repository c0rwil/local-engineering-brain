//! Deterministic hybrid retrieval with hard scope and validity filters.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ids::{MemoryId, ProjectId};
use crate::memory::{MemoryRecord, MemoryStore};
use crate::storage::Database;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RetrievalWhy {
    pub lexical: f64,
    pub exact: f64,
    pub structural: f64,
    pub confidence: f64,
    pub utility: f64,
    pub importance: f64,
    pub validity: f64,
    pub semantic: f64,
    pub note: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RetrievedMemory {
    pub memory: MemoryRecord,
    pub score: f64,
    pub why: RetrievalWhy,
}

#[derive(Clone, Debug)]
pub struct RetrievalRequest<'a> {
    pub project_id: &'a ProjectId,
    pub task_text: &'a str,
    pub current_paths: &'a [String],
    pub repository_root: Option<&'a Path>,
    pub head: Option<&'a str>,
    pub limit: usize,
}

#[derive(Clone, Debug)]
pub struct Retriever {
    memories: MemoryStore,
}

impl Retriever {
    #[must_use]
    pub fn new(database: Database) -> Self {
        Self {
            memories: MemoryStore::new(database),
        }
    }

    /// Retrieve validated, active, structurally current memories.
    ///
    /// Candidate generation combines FTS5, structural overlap, and an
    /// importance/utility fallback. Semantic score is explicitly zero when no
    /// optional vector accelerator is configured.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid limits, storage corruption, or Git
    /// ancestry failures needed by bounded validity ranges.
    pub fn retrieve(&self, request: &RetrievalRequest<'_>) -> Result<Vec<RetrievedMemory>> {
        if request.limit == 0 || request.limit > 100 {
            return Err(Error::Config(
                "retrieval limit must be between 1 and 100".to_owned(),
            ));
        }
        let query = fts_query(request.task_text);
        let mut candidates = BTreeMap::<MemoryId, (MemoryRecord, f64)>::new();
        for (memory, lexical) in
            self.memories
                .retrieval_candidates(request.project_id, query.as_deref(), 250)?
        {
            candidates.insert(memory.id.clone(), (memory, lexical));
        }
        for memory in
            self.memories
                .structural_candidates(request.project_id, request.current_paths, 250)?
        {
            candidates.entry(memory.id.clone()).or_insert((memory, 0.0));
        }
        for (memory, _) in self
            .memories
            .retrieval_candidates(request.project_id, None, 50)?
        {
            candidates.entry(memory.id.clone()).or_insert((memory, 0.0));
        }

        let task_lower = request.task_text.to_lowercase();
        let mut ranked = Vec::new();
        for (_, (memory, lexical)) in candidates {
            if !commit_applicable(&memory, request.repository_root, request.head)? {
                continue;
            }
            let exact = exact_score(&memory, &task_lower);
            let structural = self
                .memories
                .structural_overlap(&memory.id, request.current_paths)?;
            let utility = utility_score(&memory);
            let why = RetrievalWhy {
                lexical,
                exact,
                structural,
                confidence: memory.confidence,
                utility,
                importance: memory.importance,
                validity: 1.0,
                semantic: 0.0,
                note: "active validated memory; current symbol digests and commit bounds match"
                    .to_owned(),
            };
            let score = 0.18 * lexical
                + 0.08 * exact
                + 0.18 * structural
                + 0.10 * memory.confidence
                + 0.10 * utility
                + 0.05 * memory.importance
                + 0.24 * why.semantic
                + 0.07;
            ranked.push(RetrievedMemory { memory, score, why });
        }
        ranked.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.memory.id.cmp(&right.memory.id))
        });
        ranked.truncate(request.limit);
        Ok(ranked)
    }
}

fn fts_query(task: &str) -> Option<String> {
    let mut terms = task
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .map(str::to_lowercase)
        .filter(|term| term.len() >= 2)
        .collect::<Vec<_>>();
    terms.sort();
    terms.dedup();
    terms.truncate(24);
    if terms.is_empty() {
        None
    } else {
        Some(
            terms
                .into_iter()
                .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" OR "),
        )
    }
}

fn exact_score(memory: &MemoryRecord, task_lower: &str) -> f64 {
    if task_lower.contains(&memory.claim.to_lowercase()) {
        return 1.0;
    }
    if memory
        .tags
        .iter()
        .any(|tag| task_lower.contains(&tag.to_lowercase()))
    {
        0.8
    } else {
        0.0
    }
}

fn utility_score(memory: &MemoryRecord) -> f64 {
    let success = f64::from(u32::try_from(memory.successful_count).unwrap_or(u32::MAX));
    let harmful = f64::from(u32::try_from(memory.harmful_count).unwrap_or(u32::MAX));
    (success + 1.0) / (success + harmful + 2.0)
}

fn commit_applicable(
    memory: &MemoryRecord,
    repository_root: Option<&Path>,
    head: Option<&str>,
) -> Result<bool> {
    if memory.valid_from_commit.is_none() && memory.valid_to_commit.is_none() {
        return Ok(true);
    }
    let (Some(root), Some(head)) = (repository_root, head) else {
        return Ok(false);
    };
    if let Some(from) = &memory.valid_from_commit
        && !is_ancestor(root, from, head)?
    {
        return Ok(false);
    }
    if let Some(to) = &memory.valid_to_commit
        && is_ancestor(root, to, head)?
    {
        return Ok(false);
    }
    Ok(true)
}

fn is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let status = Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| Error::Git(format!("could not run merge-base: {error}")))?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        code => Err(Error::Git(format!(
            "git merge-base failed with status {code:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::experience::{EvidenceStrength, ExperienceStore, TaskOutcome};
    use crate::memory::{EpistemicState, MemoryDraft};
    use crate::projects::ProjectRegistry;

    #[test]
    fn retrieval_excludes_candidates_and_ranks_validated_lexical_matches() {
        let temp = TempDir::new().expect("temp");
        let root = temp.path().join("project");
        std::fs::create_dir(&root).expect("project");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("project")
            .project
            .id;
        let experience = ExperienceStore::new(database.clone());
        let task = experience
            .start_task(&project, "fix abort cleanup", None)
            .expect("task");
        let evidence = experience
            .record_evidence(
                &project,
                Some(&task.id),
                None,
                "test_result",
                EvidenceStrength::Deterministic,
                None,
                Some("cargo test"),
                Some(0),
                b"pass",
                &serde_json::json!({}),
            )
            .expect("evidence");
        experience
            .finish_task(&task.id, TaskOutcome::Success, Some(true), None)
            .expect("finish task");
        let store = MemoryStore::new(database.clone());
        let draft = MemoryDraft {
            scope_type: "project".to_owned(),
            scope_id: project.to_string(),
            kind: "lesson".to_owned(),
            claim: "Release reservations when abort cleanup runs".to_owned(),
            rationale: "Validated by regression test".to_owned(),
            confidence: 0.5,
            importance: 0.9,
            tags: vec!["abort".to_owned(), "cleanup".to_owned()],
            valid_from_commit: None,
            valid_to_commit: None,
            trust_source: "agent_inference".to_owned(),
            evidence_ids: vec![evidence.id],
            source_task_ids: vec![task.id],
            node_ids: Vec::new(),
        };
        let validated = store.create_candidate(&project, &draft).expect("candidate");
        store
            .promote(&validated.id, EpistemicState::Observed, false)
            .expect("observed");
        store
            .promote(&validated.id, EpistemicState::Supported, false)
            .expect("supported");
        store
            .promote(&validated.id, EpistemicState::Validated, false)
            .expect("validated");
        let candidate = store.create_candidate(&project, &draft).expect("candidate");

        let results = Retriever::new(database)
            .retrieve(&RetrievalRequest {
                project_id: &project,
                task_text: "fix abort cleanup",
                current_paths: &[],
                repository_root: None,
                head: None,
                limit: 10,
            })
            .expect("retrieve");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memory.id, validated.id);
        assert_ne!(results[0].memory.id, candidate.id);
        assert!(results[0].why.lexical > 0.0);
    }
}
