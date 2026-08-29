//! Token-bounded deterministic context compilation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::ids::{ContextRunId, ProjectId, TaskId};
use crate::retrieval::{RetrievalRequest, RetrievalWhy, Retriever};
use crate::storage::Database;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextMode {
    Brain,
    Off,
}

impl ContextMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Brain => "brain",
            Self::Off => "off",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ContextCompileRequest {
    pub request_id: String,
    pub project_id: ProjectId,
    pub task_text: String,
    #[serde(default)]
    pub current_paths: Vec<String>,
    pub token_budget: u32,
    pub mode: ContextMode,
    pub repository_root: Option<PathBuf>,
    pub head: Option<String>,
    /// Open task this compilation serves, when the caller knows it. Declaring
    /// it records truthful usage linkage: injected memories are attributed to
    /// this task, and the task's eventual outcome closes those usage records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
}

#[derive(Serialize)]
struct RequestFingerprint<'a> {
    project_id: &'a str,
    task_text_digest: String,
    current_paths: Vec<&'a str>,
    token_budget: u32,
    mode: &'static str,
    repository_root: Option<&'a str>,
    head: Option<&'a str>,
    /// Skipped when absent so fingerprints of task-less requests stay
    /// byte-identical to those computed before this field existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<&'a str>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ContextItem {
    pub ordinal: u32,
    pub source_type: String,
    pub source_id: String,
    pub score: f64,
    pub tokens: u32,
    pub why: RetrievalWhy,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ContextMetrics {
    pub compile_ms: u64,
    pub candidate_count: u32,
    pub selected_tokens: u32,
    pub token_budget: u32,
    pub stable_prefix_hash: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CompiledContext {
    pub id: ContextRunId,
    pub request_id: String,
    pub mode: ContextMode,
    pub stable_prefix: String,
    pub dynamic_context: String,
    pub items: Vec<ContextItem>,
    pub metrics: ContextMetrics,
}

#[derive(Clone, Debug)]
pub struct PreparedContext {
    request: ContextCompileRequest,
    context: CompiledContext,
    existing: bool,
}

#[derive(Clone, Debug)]
pub struct ContextCompiler {
    database: Database,
    retriever: Retriever,
    max_tokens: u32,
    stable_prefix_tokens: u32,
}

impl ContextCompiler {
    #[must_use]
    pub fn new(database: Database, max_tokens: u32, stable_prefix_tokens: u32) -> Self {
        Self {
            retriever: Retriever::new(database.clone()),
            database,
            max_tokens,
            stable_prefix_tokens,
        }
    }

    /// Compile with the optional deterministic semantic retrieval stage
    /// enabled. [`Self::new`] keeps semantic scoring at exactly `0.0` so
    /// ablation controls stay valid.
    #[must_use]
    pub fn with_semantic(
        database: Database,
        max_tokens: u32,
        stable_prefix_tokens: u32,
        options: crate::semantic::SemanticOptions,
    ) -> Self {
        Self {
            retriever: Retriever::with_semantic(database.clone(), options),
            database,
            max_tokens,
            stable_prefix_tokens,
        }
    }

    /// Compile deterministic project context within a hard token budget.
    ///
    /// `Off` mode records the paired control request but performs no retrieval
    /// and injects exactly zero content.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid budgets/IDs, retrieval or validity failures,
    /// duplicate request corruption, or failed atomic metrics persistence.
    pub fn compile(&self, request: &ContextCompileRequest) -> Result<CompiledContext> {
        self.apply_prepared(self.prepare(request)?)
    }

    /// Perform retrieval, validity checks, ranking, and formatting without
    /// entering the canonical writer.
    ///
    /// # Errors
    ///
    /// Returns validation, retrieval, Git, or storage-read errors.
    pub fn prepare(&self, request: &ContextCompileRequest) -> Result<PreparedContext> {
        validate_request(request, self.max_tokens)?;
        if let Some(existing) = self.get_for_request(request)? {
            return Ok(PreparedContext {
                request: request.clone(),
                context: existing,
                existing: true,
            });
        }
        let started = Instant::now();
        let mut context = if request.mode == ContextMode::Off {
            CompiledContext {
                id: ContextRunId::new(),
                request_id: request.request_id.clone(),
                mode: ContextMode::Off,
                stable_prefix: String::new(),
                dynamic_context: String::new(),
                items: Vec::new(),
                metrics: ContextMetrics {
                    compile_ms: 0,
                    candidate_count: 0,
                    selected_tokens: 0,
                    token_budget: request.token_budget,
                    stable_prefix_hash: sha256(b""),
                },
            }
        } else {
            self.compile_brain(request)?
        };
        context.metrics.compile_ms =
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(PreparedContext {
            request: request.clone(),
            context,
            existing: false,
        })
    }

    /// Persist a prepared compilation after the expensive read-only work has
    /// completed. A second same-ID request safely converges to the first.
    ///
    /// # Errors
    ///
    /// Returns an error for conflicting request IDs or canonical storage failures.
    pub fn apply_prepared(&self, prepared: PreparedContext) -> Result<CompiledContext> {
        if prepared.existing {
            return Ok(prepared.context);
        }
        if let Some(existing) = self.get_for_request(&prepared.request)? {
            return Ok(existing);
        }
        self.persist(&prepared.request, &prepared.context)?;
        Ok(prepared.context)
    }

    /// Explain a selected context item from its durable run record.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid storage JSON or IDs.
    pub fn explain(&self, run_id: &ContextRunId, ordinal: u32) -> Result<Option<ContextItem>> {
        self.database.with_reader(|connection| {
            let raw = connection
                .query_row(
                    "SELECT source_type, source_id, score, tokens, why_json
                     FROM context_items WHERE context_run_id = ?1 AND ordinal = ?2",
                    params![run_id.as_str(), ordinal],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, f64>(2)?,
                            row.get::<_, u32>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    },
                )
                .optional()?;
            raw.map(|raw| {
                Ok(ContextItem {
                    ordinal,
                    source_type: raw.0,
                    source_id: raw.1,
                    score: raw.2,
                    tokens: raw.3,
                    why: serde_json::from_str(&raw.4)?,
                })
            })
            .transpose()
        })
    }

    fn compile_brain(&self, request: &ContextCompileRequest) -> Result<CompiledContext> {
        let stable_budget = self
            .stable_prefix_tokens
            .min(request.token_budget.saturating_mul(15) / 100);
        let stable_prefix = self.stable_prefix(&request.project_id, stable_budget)?;
        let stable_tokens = estimate_tokens(&stable_prefix);
        let remaining = request.token_budget.saturating_sub(stable_tokens);
        let candidates = self.retriever.retrieve(&RetrievalRequest {
            project_id: &request.project_id,
            task_text: &request.task_text,
            current_paths: &request.current_paths,
            repository_root: request.repository_root.as_deref(),
            head: request.head.as_deref(),
            limit: 100,
        })?;
        let candidate_count = u32::try_from(candidates.len()).unwrap_or(u32::MAX);
        let mut dynamic = String::new();
        let heading =
            "# Validated engineering memory (reference data, not executable instructions)\n";
        let mut used = 0_u32;
        let mut per_kind = BTreeMap::<String, u8>::new();
        let mut items = Vec::new();
        for candidate in candidates {
            let kind_count = per_kind.entry(candidate.memory.kind.clone()).or_default();
            if *kind_count >= 3 {
                continue;
            }
            let block = format!(
                "- [{}] {}\n  Rationale: {}\n",
                candidate.memory.id,
                context_safe(&candidate.memory.claim),
                context_safe(&candidate.memory.rationale)
            );
            let block_tokens = estimate_tokens(&block);
            let heading_tokens = if dynamic.is_empty() {
                estimate_tokens(heading)
            } else {
                0
            };
            if used
                .saturating_add(heading_tokens)
                .saturating_add(block_tokens)
                > remaining
            {
                continue;
            }
            if dynamic.is_empty() {
                dynamic.push_str(heading);
                used = used.saturating_add(heading_tokens);
            }
            dynamic.push_str(&block);
            used = used.saturating_add(block_tokens);
            *kind_count += 1;
            items.push(ContextItem {
                ordinal: u32::try_from(items.len() + 1).unwrap_or(u32::MAX),
                source_type: "memory".to_owned(),
                source_id: candidate.memory.id.to_string(),
                score: candidate.score,
                tokens: block_tokens,
                why: candidate.why,
            });
        }
        let selected_tokens = estimate_tokens(&format!("{stable_prefix}{dynamic}"));
        debug_assert!(selected_tokens <= request.token_budget);
        Ok(CompiledContext {
            id: ContextRunId::new(),
            request_id: request.request_id.clone(),
            mode: ContextMode::Brain,
            stable_prefix,
            dynamic_context: dynamic,
            items,
            metrics: ContextMetrics {
                compile_ms: 0,
                candidate_count,
                selected_tokens,
                token_budget: request.token_budget,
                stable_prefix_hash: String::new(),
            },
        }
        .with_stable_hash())
    }

    fn stable_prefix(&self, project_id: &ProjectId, budget: u32) -> Result<String> {
        if budget == 0 {
            return Ok(String::new());
        }
        let facts = self.database.with_reader(|connection| {
            let project_name = connection.query_row(
                "SELECT name FROM projects WHERE id = ?1 AND tombstoned_at_ms IS NULL",
                [project_id.as_str()],
                |row| row.get::<_, String>(0),
            )?;
            let mut statement = connection.prepare(
                "SELECT DISTINCT kind, label FROM nodes
                 WHERE project_id = ?1 AND tombstoned_at_ms IS NULL
                   AND kind IN ('Module','Type','Interface')
                 ORDER BY kind, label LIMIT 24",
            )?;
            let rows = statement.query_map([project_id.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let facts = rows.collect::<std::result::Result<Vec<_>, _>>()?;
            Ok((project_name, facts))
        })?;
        let mut output = String::new();
        append_if_fits(
            &mut output,
            &format!(
                "# Stable project architecture (reference data)\nProject: {}\n",
                context_safe(&facts.0)
            ),
            budget,
        );
        for (kind, label) in facts.1 {
            if !append_if_fits(
                &mut output,
                &format!("- {}: {}\n", context_safe(&kind), context_safe(&label)),
                budget,
            ) {
                break;
            }
        }
        Ok(output)
    }

    fn persist(&self, request: &ContextCompileRequest, context: &CompiledContext) -> Result<()> {
        let now = unix_time_ms();
        let task_digest = sha256(request.task_text.as_bytes());
        let request_fingerprint = request_fingerprint(request)?;
        self.database.write_immediate(|transaction| {
            transaction.execute(
                "INSERT INTO context_runs
                 (id, request_id, project_id, request_fingerprint, mode,
                  task_text_digest, token_budget,
                  selected_tokens, candidate_count, stable_prefix_hash, stable_prefix,
                  dynamic_context, compile_ms, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    context.id.as_str(),
                    context.request_id,
                    request.project_id.as_str(),
                    request_fingerprint,
                    context.mode.as_str(),
                    task_digest,
                    context.metrics.token_budget,
                    context.metrics.selected_tokens,
                    context.metrics.candidate_count,
                    context.metrics.stable_prefix_hash,
                    context.stable_prefix,
                    context.dynamic_context,
                    i64::try_from(context.metrics.compile_ms).unwrap_or(i64::MAX),
                    now
                ],
            )?;
            for item in &context.items {
                transaction.execute(
                    "INSERT INTO context_items
                     (context_run_id, ordinal, source_type, source_id, score, tokens, why_json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        context.id.as_str(),
                        item.ordinal,
                        item.source_type,
                        item.source_id,
                        item.score,
                        item.tokens,
                        serde_json::to_string(&item.why)?
                    ],
                )?;
                if item.source_type == "memory" {
                    transaction.execute(
                        "UPDATE memories SET injected_count = injected_count + 1,
                             retrieved_count = retrieved_count + 1, updated_at_ms = ?2
                         WHERE id = ?1",
                        params![item.source_id, now],
                    )?;
                    // Truthful usage provenance: this memory was injected into
                    // exactly this compiled run. Outcome linkage is recorded
                    // separately, only when the caller declared a served task.
                    transaction.execute(
                        "INSERT INTO usage_events
                         (id, source_type, source_id, context_run_id, usage_kind,
                          outcome, created_at_ms)
                         VALUES (?1, 'memory', ?2, ?3, 'injected', NULL, ?4)",
                        params![
                            format!("use_{}", uuid::Uuid::new_v4()),
                            item.source_id,
                            context.id.as_str(),
                            now
                        ],
                    )?;
                }
            }
            if context.mode == ContextMode::Brain
                && let Some(task_id) = &request.task_id
            {
                let in_project: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1 AND project_id = ?2)",
                    params![task_id.as_str(), request.project_id.as_str()],
                    |row| row.get(0),
                )?;
                if !in_project {
                    return Err(Error::Config(format!(
                        "context task `{task_id}` does not belong to the requested project"
                    )));
                }
                transaction.execute(
                    "INSERT INTO usage_events
                     (id, source_type, source_id, context_run_id, usage_kind,
                      outcome, created_at_ms)
                     VALUES (?1, 'task', ?2, ?3, 'context_served', NULL, ?4)",
                    params![
                        format!("use_{}", uuid::Uuid::new_v4()),
                        task_id.as_str(),
                        context.id.as_str(),
                        now
                    ],
                )?;
            }
            Ok(())
        })
    }

    fn get_for_request(&self, request: &ContextCompileRequest) -> Result<Option<CompiledContext>> {
        let expected = request_fingerprint(request)?;
        let actual = self.database.with_reader(|connection| {
            connection
                .query_row(
                    "SELECT request_fingerprint FROM context_runs
                     WHERE project_id = ?1 AND request_id = ?2",
                    params![request.project_id.as_str(), request.request_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(Error::from)
        })?;
        let Some(actual) = actual else {
            return Ok(None);
        };
        if actual != expected {
            return Err(Error::Config(format!(
                "context request ID `{}` was reused with different request content",
                request.request_id
            )));
        }
        self.get_by_request_id(&request.project_id, &request.request_id)
    }

    fn get_by_request_id(
        &self,
        project_id: &ProjectId,
        request_id: &str,
    ) -> Result<Option<CompiledContext>> {
        let header = self.database.with_reader(|connection| {
            connection
                .query_row(
                    "SELECT id, mode, token_budget, selected_tokens, candidate_count,
                            stable_prefix_hash, stable_prefix, dynamic_context, compile_ms
                     FROM context_runs WHERE project_id = ?1 AND request_id = ?2",
                    params![project_id.as_str(), request_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, u32>(2)?,
                            row.get::<_, u32>(3)?,
                            row.get::<_, u32>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                            row.get::<_, String>(7)?,
                            row.get::<_, i64>(8)?,
                        ))
                    },
                )
                .optional()
                .map_err(Error::from)
        })?;
        let Some(header) = header else {
            return Ok(None);
        };
        let id = ContextRunId::parse(&header.0)?;
        let mode = match header.1.as_str() {
            "brain" => ContextMode::Brain,
            "off" => ContextMode::Off,
            other => return Err(Error::Config(format!("invalid context mode `{other}`"))),
        };
        let items = self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT ordinal, source_type, source_id, score, tokens, why_json
                 FROM context_items WHERE context_run_id = ?1 ORDER BY ordinal",
            )?;
            let rows = statement.query_map([id.as_str()], |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, u32>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?;
            let mut items = Vec::new();
            for row in rows {
                let row = row?;
                items.push(ContextItem {
                    ordinal: row.0,
                    source_type: row.1,
                    source_id: row.2,
                    score: row.3,
                    tokens: row.4,
                    why: serde_json::from_str(&row.5)?,
                });
            }
            Ok(items)
        })?;
        Ok(Some(CompiledContext {
            id,
            request_id: request_id.to_owned(),
            mode,
            stable_prefix: header.6,
            dynamic_context: header.7,
            items,
            metrics: ContextMetrics {
                compile_ms: u64::try_from(header.8)
                    .map_err(|_| Error::Config("negative context compile time".to_owned()))?,
                candidate_count: header.4,
                selected_tokens: header.3,
                token_budget: header.2,
                stable_prefix_hash: header.5,
            },
        }))
    }
}

fn request_fingerprint(request: &ContextCompileRequest) -> Result<String> {
    let mut current_paths = request
        .current_paths
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    current_paths.sort_unstable();
    current_paths.dedup();
    let repository_root = request
        .repository_root
        .as_deref()
        .map(Path::to_string_lossy);
    let fingerprint = RequestFingerprint {
        project_id: request.project_id.as_str(),
        task_text_digest: sha256(request.task_text.as_bytes()),
        current_paths,
        token_budget: request.token_budget,
        mode: request.mode.as_str(),
        repository_root: repository_root.as_deref(),
        head: request.head.as_deref(),
        task_id: request.task_id.as_ref().map(TaskId::as_str),
    };
    Ok(sha256(&serde_json::to_vec(&fingerprint)?))
}

impl CompiledContext {
    fn with_stable_hash(mut self) -> Self {
        let hash = sha256(self.stable_prefix.as_bytes());
        self.metrics.stable_prefix_hash.clone_from(&hash);
        self
    }
}

fn validate_request(request: &ContextCompileRequest, max_tokens: u32) -> Result<()> {
    if request.request_id.trim().is_empty() || request.request_id.len() > 256 {
        return Err(Error::Config(
            "context request_id must contain 1..=256 bytes".to_owned(),
        ));
    }
    if request.task_text.trim().is_empty() {
        return Err(Error::Config(
            "context task text cannot be empty".to_owned(),
        ));
    }
    if request.token_budget == 0 || request.token_budget > max_tokens {
        return Err(Error::Config(format!(
            "context token budget must be between 1 and configured maximum {max_tokens}"
        )));
    }
    Ok(())
}

fn append_if_fits(output: &mut String, candidate: &str, budget: u32) -> bool {
    if estimate_tokens(output).saturating_add(estimate_tokens(candidate)) > budget {
        false
    } else {
        output.push_str(candidate);
        true
    }
}

fn context_safe(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() && !matches!(character, '\n' | '\t') {
                '�'
            } else {
                character
            }
        })
        .collect()
}

#[must_use]
pub fn estimate_tokens(value: &str) -> u32 {
    let bytes = u32::try_from(value.len()).unwrap_or(u32::MAX);
    bytes.saturating_add(3) / 4
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::experience::{EvidenceStrength, ExperienceStore, TaskOutcome};
    use crate::ids::MemoryId;
    use crate::memory::{EpistemicState, MemoryDraft, MemoryStore};
    use crate::projects::ProjectRegistry;

    fn fixture() -> (TempDir, ContextCompiler, ProjectId) {
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
        let memories = MemoryStore::new(database.clone());
        let memory = memories
            .create_candidate(
                &project,
                &MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: project.to_string(),
                    kind: "lesson".to_owned(),
                    claim: "Release reservation during abort cleanup".to_owned(),
                    rationale: "Regression test passes".to_owned(),
                    confidence: 0.5,
                    importance: 0.9,
                    tags: vec!["abort".to_owned()],
                    valid_from_commit: None,
                    valid_to_commit: None,
                    trust_source: "agent_inference".to_owned(),
                    evidence_ids: vec![evidence.id],
                    source_task_ids: vec![task.id],
                    node_ids: Vec::new(),
                },
            )
            .expect("memory");
        memories
            .promote(&memory.id, EpistemicState::Observed, false)
            .expect("observed");
        memories
            .promote(&memory.id, EpistemicState::Supported, false)
            .expect("supported");
        memories
            .promote(&memory.id, EpistemicState::Validated, false)
            .expect("validated");
        (temp, ContextCompiler::new(database, 6_000, 1_800), project)
    }

    #[test]
    fn compiler_never_exceeds_budget_and_every_item_is_explainable() {
        let (_temp, compiler, project) = fixture();
        let context = compiler
            .compile(&ContextCompileRequest {
                request_id: "req-1".to_owned(),
                project_id: project,
                task_text: "fix abort cleanup".to_owned(),
                current_paths: Vec::new(),
                token_budget: 80,
                mode: ContextMode::Brain,
                repository_root: None,
                head: None,
                task_id: None,
            })
            .expect("compile");
        assert!(context.metrics.selected_tokens <= 80);
        assert_eq!(
            estimate_tokens(&context.stable_prefix) + estimate_tokens(&context.dynamic_context),
            context.metrics.selected_tokens
        );
        for item in &context.items {
            assert_eq!(
                compiler
                    .explain(&context.id, item.ordinal)
                    .expect("explain")
                    .expect("item")
                    .source_id,
                item.source_id
            );
        }
    }

    #[test]
    fn off_mode_injects_nothing_and_idempotent_retry_does_not_double_count() {
        let (_temp, compiler, project) = fixture();
        let request = ContextCompileRequest {
            request_id: "off-1".to_owned(),
            project_id: project,
            task_text: "control".to_owned(),
            current_paths: Vec::new(),
            token_budget: 100,
            mode: ContextMode::Off,
            repository_root: None,
            head: None,
            task_id: None,
        };
        let first = compiler.compile(&request).expect("first");
        let second = compiler.compile(&request).expect("retry");
        assert_eq!(first, second);
        assert!(first.stable_prefix.is_empty());
        assert!(first.dynamic_context.is_empty());
        assert_eq!(first.metrics.selected_tokens, 0);
        assert!(first.items.is_empty());
    }

    #[test]
    fn request_idempotency_is_project_scoped_and_fingerprint_checked() {
        let (temp, compiler, first_project) = fixture();
        let other_root = temp.path().join("other-project");
        std::fs::create_dir(&other_root).expect("other project");
        let other_project = ProjectRegistry::new(compiler.database.clone())
            .resolve_or_register(&other_root)
            .expect("register other")
            .project
            .id;
        let first_request = ContextCompileRequest {
            request_id: "shared-caller-id".to_owned(),
            project_id: first_project,
            task_text: "private abort cleanup".to_owned(),
            current_paths: vec!["src/private.rs".to_owned()],
            token_budget: 400,
            mode: ContextMode::Brain,
            repository_root: None,
            head: Some("first-head".to_owned()),
            task_id: None,
        };
        let first = compiler.compile(&first_request).expect("first context");
        assert!(first.dynamic_context.contains("reservation"));

        let second = compiler
            .compile(&ContextCompileRequest {
                request_id: "shared-caller-id".to_owned(),
                project_id: other_project,
                task_text: "unrelated public task".to_owned(),
                current_paths: Vec::new(),
                token_budget: 400,
                mode: ContextMode::Brain,
                repository_root: None,
                head: None,
                task_id: None,
            })
            .expect("same caller id in another project is independent");
        assert!(!second.dynamic_context.contains("reservation"));
        assert_ne!(first.id, second.id);

        let mut conflicting_retry = first_request;
        conflicting_retry.task_text = "different task".to_owned();
        let error = compiler
            .compile(&conflicting_retry)
            .expect_err("same project retry fingerprint must match");
        assert!(error.to_string().contains("different request content"));
    }

    #[test]
    fn purge_invalidates_runs_containing_rendered_memory_text() {
        let (_temp, compiler, project) = fixture();
        let request = ContextCompileRequest {
            request_id: "purge-context".to_owned(),
            project_id: project.clone(),
            task_text: "fix abort cleanup".to_owned(),
            current_paths: Vec::new(),
            token_budget: 400,
            mode: ContextMode::Brain,
            repository_root: None,
            head: None,
            task_id: None,
        };
        let context = compiler.compile(&request).expect("compile");
        assert!(context.dynamic_context.contains("reservation"));
        let memory_id = MemoryId::parse(&context.items[0].source_id).expect("memory id");
        MemoryStore::new(compiler.database.clone())
            .purge(&memory_id)
            .expect("purge");
        let persisted = compiler
            .get_by_request_id(&project, &request.request_id)
            .expect("lookup");
        assert!(persisted.is_none());
        let leaked: i64 = compiler
            .database
            .with_reader(|connection| {
                connection
                    .query_row(
                        "SELECT COUNT(*) FROM context_runs
                     WHERE dynamic_context LIKE '%reservation%'",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(Error::from)
            })
            .expect("leak query");
        assert_eq!(leaked, 0);
    }
}
