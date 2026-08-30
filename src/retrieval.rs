//! Deterministic hybrid retrieval with hard scope and validity filters.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ids::{MemoryId, ProjectId};
use crate::memory::{MemoryRecord, MemoryStore};
use crate::semantic::{
    EmbeddingProvider, HashedNGramEmbedder, SemanticOptions, SqliteVectorIndex, VectorIndex,
};
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
struct SemanticStage {
    embedder: HashedNGramEmbedder,
    index: SqliteVectorIndex,
    max_candidates: usize,
}

#[derive(Clone, Debug)]
pub struct Retriever {
    memories: MemoryStore,
    semantic: Option<SemanticStage>,
}

impl Retriever {
    #[must_use]
    pub fn new(database: Database) -> Self {
        Self {
            memories: MemoryStore::new(database),
            semantic: None,
        }
    }

    /// Enable the optional deterministic semantic candidate stage.
    ///
    /// When disabled ([`Self::new`]) behavior is byte-identical to the pure
    /// lexical/structural retriever and `semantic` stays exactly `0.0`, which
    /// keeps the brain-off/ablation control valid (spec Appendix E/G).
    #[must_use]
    pub fn with_semantic(database: Database, options: SemanticOptions) -> Self {
        Self {
            memories: MemoryStore::new(database.clone()),
            semantic: Some(SemanticStage {
                embedder: HashedNGramEmbedder::new(),
                index: SqliteVectorIndex::new(database, options.max_scanned),
                max_candidates: options.max_candidates,
            }),
        }
    }

    /// Retrieve validated, active, structurally current memories.
    ///
    /// Candidate generation combines FTS5, structural overlap, an
    /// importance/utility fallback, and — only when explicitly enabled —
    /// bounded semantic vector search. Semantic score is explicitly zero when
    /// no vector accelerator is configured.
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
        let semantic_scores = self.semantic_scores(request, &mut candidates)?;

        let task_lower = request.task_text.to_lowercase();
        let task_terms = coverage_terms(&task_lower);
        let mut ranked = Vec::new();
        for (_, (memory, _bm25)) in candidates {
            // FTS/bm25 stays a candidate prefilter only. On small memory sets
            // its IDF is nearly flat, so ranking uses claim-term coverage of
            // the task text instead: a short claim either names the concepts
            // the ticket is about or it does not.
            let lexical = claim_coverage(&memory.claim, &memory.tags, &task_terms);
            if !commit_applicable(&memory, request.repository_root, request.head)? {
                continue;
            }
            let exact = exact_score(&memory, &task_lower);
            let structural = self
                .memories
                .structural_overlap(&memory.id, request.current_paths)?;
            let utility = utility_score(&memory);
            let semantic = semantic_scores.get(&memory.id).copied().unwrap_or(0.0);
            let why = RetrievalWhy {
                lexical,
                exact,
                structural,
                confidence: memory.confidence,
                utility,
                importance: memory.importance,
                validity: 1.0,
                semantic,
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

    /// Embed the task text and merge the bounded top-k semantic hits into the
    /// candidate set, returning per-memory cosine scores clamped to `[0, 1]`.
    /// Returns an empty map when the semantic stage is disabled.
    fn semantic_scores(
        &self,
        request: &RetrievalRequest<'_>,
        candidates: &mut BTreeMap<MemoryId, (MemoryRecord, f64)>,
    ) -> Result<BTreeMap<MemoryId, f64>> {
        let mut scores = BTreeMap::new();
        let Some(stage) = &self.semantic else {
            return Ok(scores);
        };
        let query_vector = stage
            .embedder
            .embed(&[request.task_text])?
            .into_iter()
            .next()
            .ok_or_else(|| Error::Semantic("embedder returned no query vector".to_owned()))?;
        for hit in stage
            .index
            .search(request.project_id, &query_vector, stage.max_candidates)?
        {
            let score = hit.score.clamp(0.0, 1.0);
            if score <= 0.0 {
                continue;
            }
            if !candidates.contains_key(&hit.memory_id)
                && let Some(memory) = self.memories.get(&hit.memory_id)?
            {
                candidates.insert(hit.memory_id.clone(), (memory, 0.0));
            }
            scores.insert(hit.memory_id, score);
        }
        Ok(scores)
    }
}

/// English function words that carry no retrieval signal. Discriminative
/// domain terms must never lose their query slot to these.
const FTS_STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "can", "do", "does", "for", "from",
    "has", "have", "if", "in", "into", "is", "it", "its", "like", "make", "must", "not", "of",
    "on", "or", "our", "so", "sure", "than", "that", "the", "their", "them", "then", "they",
    "this", "to", "under", "up", "was", "we", "what", "when", "which", "will", "with", "you",
    "your",
];

fn fts_query(task: &str) -> Option<String> {
    let mut terms = task
        .split(|character: char| !character.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|term| term.len() >= 2)
        .filter(|term| !FTS_STOPWORDS.contains(&term.as_str()))
        .collect::<Vec<_>>();
    terms.sort();
    terms.dedup();
    // Keep the most discriminative terms: longer tokens (identifiers,
    // domain words) over short generic ones. A plain alphabetical truncate
    // here once silently dropped every late-alphabet domain term on
    // realistic prompt lengths, flattening lexical relevance to noise.
    terms.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    terms.truncate(48);
    terms.sort();
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

/// Light deterministic stemmer: trims common English inflections so
/// "retries"/"retrying" meet "retry" without a full stemming dependency.
fn light_stem(term: &str) -> String {
    for suffix in ["ing", "ies", "ied", "es", "ed", "s"] {
        if let Some(stripped) = term.strip_suffix(suffix) {
            if stripped.len() >= 3 {
                return stripped.to_owned();
            }
        }
    }
    term.to_owned()
}

fn coverage_terms(text_lower: &str) -> std::collections::BTreeSet<String> {
    text_lower
        .split(|character: char| !character.is_alphanumeric())
        .filter(|term| term.len() >= 3)
        .filter(|term| !FTS_STOPWORDS.contains(term))
        .map(light_stem)
        .collect()
}

/// Fraction of the claim's specific terms that the task text mentions,
/// weighted by term length so rare identifiers count more than short generic
/// words. Deterministic; in [0, 1].
fn claim_coverage(
    claim: &str,
    tags: &[String],
    task_terms: &std::collections::BTreeSet<String>,
) -> f64 {
    let mut text = claim.to_lowercase();
    for tag in tags {
        text.push(' ');
        text.push_str(&tag.to_lowercase());
    }
    let claim_terms = coverage_terms(&text);
    let mut total = 0_f64;
    let mut matched = 0_f64;
    for term in &claim_terms {
        // Terms are tiny (<=64 bytes); the cast is exact.
        #[allow(clippy::cast_precision_loss)]
        let weight = term.len() as f64;
        total += weight;
        if task_terms.contains(term) {
            matched += weight;
        }
    }
    if total == 0.0 { 0.0 } else { matched / total }
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

    #[test]
    fn claim_coverage_ranks_the_right_family_lesson_for_real_tickets() {
        // The six seeded benchmark lessons, verbatim.
        let lessons = [
            (
                "B",
                "Job cancellation must set JobState::Cancelled and remove from the queue; the legacy Aborted state is non-terminal and abort_job is not a cancellation path",
            ),
            (
                "C",
                "Scheduler drain() only pops jobs that are already due - it is not a full sweep; evacuate() is the stable-order full sweep for snapshots and shutdown",
            ),
            (
                "F",
                "Every dedupe-key lookup, removal, and comparison must normalize through DedupeKey::new first; raw string comparison misses case and whitespace variants",
            ),
            (
                "A",
                "Every new serde config or persistence field must carry serde(default) with the documented default; fielded legacy files without the field must keep parsing byte-identically",
            ),
            (
                "D",
                "The attempts counter already includes the just-failed run; retry backoff and budget checks must use attempts-1 or they are off by one",
            ),
            (
                "E",
                "All millisecond-to-tick conversions must round UP via ticks_for_ms; plain integer division silently schedules work one tick early",
            ),
        ];
        let tickets = [
            (
                "D",
                "Ticket: enforce the retry budget. RetryPolicy max_attempts is documented as the total number of executions a job is allowed, but the scheduler currently retries transient failures forever. Once a job has used its whole execution budget, stop retrying and move it to the dead-letter terminal state reserved for exhausted jobs.",
            ),
            (
                "E",
                "Ticket: deadline handling must round up. A job deadline given in milliseconds is converted to scheduler ticks; partial ticks must give the job breathing room rather than cutting it short.",
            ),
            (
                "C",
                "Ticket: snapshots must include future jobs. Persisted snapshots currently lose jobs scheduled past the capture instant; the snapshot writer must sweep every queued job, not only the currently due ones.",
            ),
            (
                "B",
                "Ticket: cancelling by key must work with messy input. Cancel the pending job registered under a dedupe key even when the caller passes extra whitespace or different casing, and make sure the cancelled job is reaped like other finished work.",
            ),
        ];
        for (ticket_family, ticket) in tickets {
            let task_terms = coverage_terms(&ticket.to_lowercase());
            let mut scored = lessons
                .iter()
                .map(|(family, claim)| (*family, claim_coverage(claim, &[], &task_terms)))
                .collect::<Vec<_>>();
            scored.sort_by(|a, b| b.1.total_cmp(&a.1));
            let top_families: Vec<&str> = scored.iter().take(2).map(|(f, _)| *f).collect();
            assert!(
                top_families.contains(&ticket_family),
                "ticket family {ticket_family} must rank in the top 2, got {scored:?}"
            );
        }
    }

    #[test]
    fn fts_query_keeps_discriminative_terms_on_realistic_prompt_lengths() {
        // A realistic ticket: >24 unique words, with the domain terms late in
        // the alphabet. The old alphabetical truncation dropped all of them.
        let prompt = "Ticket: enforce the retry budget. RetryPolicy max_attempts is \
            documented as the total number of executions a job is allowed, first run \
            included, but the scheduler currently retries transient failures forever \
            and the field does nothing. Enforce it: once a job has used its whole \
            execution budget stop retrying and move it to the dead-letter terminal \
            state reserved for exhausted jobs and keep the reaper behaviour green.";
        let query = fts_query(prompt).expect("query");
        // `_` separates, matching the index tokenizer: max_attempts -> max, attempts.
        for term in ["attempts", "retrying", "exhausted", "transient", "budget"] {
            assert!(query.contains(term), "query must keep `{term}`: {query}");
        }
        for stopword in ["\"the\"", "\"and\"", "\"for\""] {
            assert!(!query.contains(stopword), "stopword slipped in: {query}");
        }
        // Determinism: same prompt, same query.
        assert_eq!(fts_query(prompt), Some(query));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn semantic_stage_is_opt_in_and_only_changes_the_semantic_component() {
        let temp = TempDir::new().expect("temp");
        let root = temp.path().join("project");
        std::fs::create_dir(&root).expect("project");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("project")
            .project
            .id;
        let store = MemoryStore::new(database.clone());
        let mut validated_ids = Vec::new();
        for claim in [
            "Release reservations when abort cleanup runs",
            "Rotate archived log files every week",
        ] {
            let memory = store
                .create_candidate(
                    &project,
                    &MemoryDraft {
                        scope_type: "project".to_owned(),
                        scope_id: project.to_string(),
                        kind: "lesson".to_owned(),
                        claim: claim.to_owned(),
                        rationale: "Fixture".to_owned(),
                        confidence: 0.5,
                        importance: 0.5,
                        tags: Vec::new(),
                        valid_from_commit: None,
                        valid_to_commit: None,
                        trust_source: "human_instruction".to_owned(),
                        evidence_ids: Vec::new(),
                        source_task_ids: Vec::new(),
                        node_ids: Vec::new(),
                    },
                )
                .expect("candidate");
            store
                .promote(&memory.id, EpistemicState::Validated, true)
                .expect("validated");
            validated_ids.push(memory.id);
        }

        let request = RetrievalRequest {
            project_id: &project,
            task_text: "fix abort cleanup and release reservations",
            current_paths: &[],
            repository_root: None,
            head: None,
            limit: 10,
        };
        let disabled = Retriever::new(database.clone())
            .retrieve(&request)
            .expect("disabled retrieve");
        let disabled_again = Retriever::new(database.clone())
            .retrieve(&request)
            .expect("disabled retry");
        assert_eq!(
            disabled, disabled_again,
            "disabled retrieval must stay deterministic"
        );
        assert!(
            disabled
                .iter()
                .all(|result| result.why.semantic.abs() < f64::EPSILON),
            "disabled retrieval must keep semantic at exactly 0.0"
        );

        let enabled = Retriever::with_semantic(
            database,
            SemanticOptions {
                max_candidates: 50,
                max_scanned: 50_000,
            },
        )
        .retrieve(&request)
        .expect("enabled retrieve");
        assert_eq!(enabled.len(), disabled.len());
        let related = enabled
            .iter()
            .find(|result| result.memory.id == validated_ids[0])
            .expect("related memory retrieved");
        let unrelated = enabled
            .iter()
            .find(|result| result.memory.id == validated_ids[1])
            .expect("unrelated memory retrieved");
        assert!(
            related.why.semantic > 0.0,
            "enabled retrieval must populate the semantic score"
        );
        assert!((0.0..=1.0).contains(&related.why.semantic));
        assert!(related.why.semantic > unrelated.why.semantic);
        assert_eq!(enabled[0].memory.id, validated_ids[0]);
        for result in &enabled {
            let baseline = disabled
                .iter()
                .find(|candidate| candidate.memory.id == result.memory.id)
                .expect("same candidate set");
            assert!(
                (result.score - (baseline.score + 0.24 * result.why.semantic)).abs() < 1e-9,
                "semantic must only add the weighted semantic component"
            );
            assert!((result.why.lexical - baseline.why.lexical).abs() < f64::EPSILON);
            assert!((result.why.structural - baseline.why.structural).abs() < f64::EPSILON);
        }
    }
}
