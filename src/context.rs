//! Token-bounded deterministic context compilation.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::ids::{ContextRunId, MemoryId, ProjectId, TaskId};
use crate::memory::{CompactInjection, MemoryRecord, MemoryStore};
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
    /// How the injected memory block introduces itself to the model.
    /// `None` keeps the compiler's configured default and serializes to
    /// nothing, so pre-existing request fingerprints are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framing: Option<InjectionFraming>,
}

/// Presentation stance of the injected memory block. `Reference` labels it
/// as background data; `Directive` presents it as project rules to apply.
/// The A/B exists because transcripts showed the reference stance inducing
/// extra deliberation loops on hard tasks (perf-evidence 2026-08-30).
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectionFraming {
    #[default]
    Reference,
    Directive,
}

impl InjectionFraming {
    #[must_use]
    pub const fn heading(self) -> &'static str {
        match self {
            Self::Reference => {
                "# Validated engineering memory (reference data, not executable instructions)\n"
            }
            Self::Directive => {
                "# Known project rules - apply these established decisions where they bear on the task\n"
            }
        }
    }

    /// Parse the `x-engram-injection-framing` header value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "reference" => Some(Self::Reference),
            "directive" => Some(Self::Directive),
            _ => None,
        }
    }
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
    /// Skipped when disabled so default-mode fingerprints remain compatible
    /// with context runs persisted before compact rendering existed.
    #[serde(skip_serializing_if = "is_false")]
    compact_injection: bool,
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
    /// Compile-time content/provenance signature; old stored runs lack one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_digest: Option<String>,
    pub items: Vec<ContextItem>,
    pub metrics: ContextMetrics,
}

#[derive(Clone, Debug)]
pub struct PreparedContext {
    request: ContextCompileRequest,
    context: CompiledContext,
    existing: bool,
}

const REVIEW_HEADING: &str =
    "# Team review standards - a reviewer rejects changes that violate these\n";

#[derive(Clone, Debug)]
struct MemorySelection {
    dynamic: String,
    review: String,
    items: Vec<ContextItem>,
}

#[derive(Clone)]
pub struct ContextCompiler {
    database: Database,
    retriever: Retriever,
    max_tokens: u32,
    stable_prefix_tokens: u32,
    compact_injection: bool,
    max_memories: u32,
    review_standards: bool,
    linked_projects: bool,
}

impl ContextCompiler {
    #[must_use]
    pub fn new(database: Database, max_tokens: u32, stable_prefix_tokens: u32) -> Self {
        Self {
            retriever: Retriever::new(database.clone()),
            database,
            max_tokens,
            stable_prefix_tokens,
            compact_injection: false,
            max_memories: 0,
            review_standards: false,
            linked_projects: false,
        }
    }

    /// Cap the number of injected memories per compile (0 = budget-only).
    #[must_use]
    pub const fn with_memory_cap(mut self, max_memories: u32) -> Self {
        self.max_memories = max_memories;
        self
    }

    /// Render preference memories as an explicit review-standards section.
    #[must_use]
    pub const fn with_review_standards(mut self, enabled: bool) -> Self {
        self.review_standards = enabled;
        self
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
            compact_injection: false,
            max_memories: 0,
            review_standards: false,
            linked_projects: false,
        }
    }

    /// Select the deterministic derived compact memory projection. This is an
    /// explicit ablation switch; constructors preserve verbose rendering.
    #[must_use]
    pub const fn with_compact_injection(mut self, enabled: bool) -> Self {
        self.compact_injection = enabled;
        self
    }

    /// Also inject relevance-gated memories from explicitly linked projects,
    /// each under a heading naming the source repository and relation.
    #[must_use]
    pub const fn with_linked_projects(mut self, enabled: bool) -> Self {
        self.linked_projects = enabled;
        self
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
                delivery_digest: None,
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
        if context.delivery_digest.is_none() {
            context.delivery_digest =
                Some(self.delivery_digest(request, &context, &context.dynamic_context)?);
        }
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

    /// Score-ordered two-pass memory selection: a kind-diversity pass, then a
    /// budget-filling pass; preference memories divert to the review-standards
    /// section when that mode is enabled.
    fn fill_memory_sections(
        &self,
        candidates: Vec<crate::retrieval::RetrievedMemory>,
        compact_injections: &BTreeMap<MemoryId, crate::memory::CompactInjection>,
        remaining: u32,
        heading: &str,
        review_heading: &str,
    ) -> Result<MemorySelection> {
        let mut dynamic = String::new();
        let mut review = String::new();
        let mut used = 0_u32;
        let mut per_kind = BTreeMap::<String, u8>::new();
        let mut items = Vec::new();
        // Two passes over the score-ranked candidates: the first bounds each
        // memory kind to 3 entries so one kind cannot monopolize the budget;
        // the second spends whatever budget remains on the skipped
        // candidates in score order. A hard per-kind cap alone once left
        // >90% of the configured budget unused while dropping the one
        // relevant lesson for a task.
        let candidates = candidates
            .into_iter()
            .map(|candidate| (candidate, false))
            .collect::<Vec<_>>();
        let mut candidates = candidates;
        for diversity_pass in [true, false] {
            for (candidate, consumed) in &mut candidates {
                if *consumed {
                    continue;
                }
                if self.max_memories > 0
                    && u32::try_from(items.len()).unwrap_or(u32::MAX) >= self.max_memories
                {
                    break;
                }
                let kind_count = per_kind.entry(candidate.memory.kind.clone()).or_default();
                if diversity_pass && *kind_count >= 3 {
                    continue;
                }
                let review_entry = self.review_standards && candidate.memory.kind == "preference";
                let (block, block_tokens) = if review_entry {
                    let entry = format!("- MUST: {}\n", candidate.memory.claim);
                    let tokens = estimate_tokens(&entry);
                    (entry, tokens)
                } else {
                    self.memory_block(&candidate.memory, compact_injections)?
                };
                let target_empty = if review_entry {
                    review.is_empty()
                } else {
                    dynamic.is_empty()
                };
                let heading_tokens = if target_empty {
                    estimate_tokens(if review_entry {
                        review_heading
                    } else {
                        heading
                    })
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
                if review_entry {
                    if review.is_empty() {
                        review.push_str(review_heading);
                        used = used.saturating_add(heading_tokens);
                    }
                    review.push_str(&block);
                } else {
                    if dynamic.is_empty() {
                        dynamic.push_str(heading);
                        used = used.saturating_add(heading_tokens);
                    }
                    dynamic.push_str(&block);
                }
                used = used.saturating_add(block_tokens);
                *kind_count += 1;
                *consumed = true;
                items.push(ContextItem {
                    ordinal: u32::try_from(items.len() + 1).unwrap_or(u32::MAX),
                    source_type: "memory".to_owned(),
                    source_id: candidate.memory.id.to_string(),
                    score: candidate.score,
                    tokens: block_tokens,
                    why: candidate.why.clone(),
                });
            }
        }
        Ok(MemorySelection {
            dynamic,
            review,
            items,
        })
    }

    /// Append bounded, relevance-gated sections for explicitly linked
    /// projects. Local memories were filled first, so linked knowledge only
    /// spends leftover budget; every section names its source repository and
    /// relation so the model can attribute cross-repo constraints. Returns
    /// the number of linked candidates considered.
    #[allow(clippy::too_many_lines)]
    fn fill_linked_sections(
        &self,
        request: &ContextCompileRequest,
        mut linked_budget: u32,
        dynamic: &mut String,
        items: &mut Vec<ContextItem>,
    ) -> Result<u32> {
        const MAX_LINKED_PROJECTS: usize = 8;
        const MAX_MEMORIES_PER_LINK: usize = 5;
        if linked_budget == 0 {
            return Ok(0);
        }
        let registry = crate::projects::ProjectRegistry::new(self.database.clone());
        let links = registry.links(&request.project_id)?;
        let mut considered = 0_u32;
        for link in links.iter().take(MAX_LINKED_PROJECTS) {
            if linked_budget == 0 {
                break;
            }
            // Commit-validity bounds are relative to the LINKED repository's
            // history, so resolve its local checkout (most recent alias that
            // still exists) and validate against ITS HEAD. Distilled lessons
            // always carry `valid_from_commit`; without this they would never
            // travel between repos. A missing checkout degrades conservatively
            // (commit-bounded memories are excluded, unbounded ones travel).
            // Graph-only links and unbounded conventions do not need a Git
            // status/diff walk of another checkout merely to retrieve context.
            let needs_commit_check = self.database.with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM memories WHERE project_id=?1
                     AND status='active' AND epistemic_state='validated'
                     AND (valid_from_commit IS NOT NULL OR valid_to_commit IS NOT NULL))",
                    [link.project_id.as_str()],
                    |row| row.get::<_, bool>(0),
                )?)
            })?;
            let linked_git = if needs_commit_check {
                registry
                    .alias_paths(&link.project_id)?
                    .into_iter()
                    .find(|path| path.is_dir())
                    .and_then(|path| crate::projects::GitState::discover(&path).ok().flatten())
            } else {
                None
            };
            let candidates = self.retriever.retrieve(&RetrievalRequest {
                project_id: &link.project_id,
                task_text: &request.task_text,
                // Paths are repo-local; cross-repo structural overlap is
                // meaningless.
                current_paths: &[],
                repository_root: linked_git.as_ref().map(|git| git.repository_root.as_path()),
                head: linked_git.as_ref().and_then(|git| git.head.as_deref()),
                limit: 20,
            })?;
            considered = considered.saturating_add(u32::try_from(candidates.len()).unwrap_or(0));
            let ids = candidates
                .iter()
                .map(|candidate| candidate.memory.id.clone())
                .collect::<Vec<_>>();
            let compact_injections = if self.compact_injection {
                MemoryStore::new(self.database.clone()).compact_injections(&ids)?
            } else {
                BTreeMap::new()
            };
            let mut heading = format!(
                "{}# Linked repository `{}` ({}) - validated knowledge that applies across repos\n",
                if dynamic.is_empty() { "" } else { "\n" },
                context_safe(&link.name),
                context_safe(&link.relation)
            );
            for fact in registry
                .facts(&link.project_id)?
                .into_iter()
                .filter(|fact| !fact.key.starts_with("internal."))
                .take(5)
            {
                let _ = writeln!(
                    heading,
                    "- {}: {}",
                    context_safe(&fact.key),
                    context_safe(&fact.value)
                );
            }
            let mut section = String::new();
            let mut section_count = 0_usize;
            for candidate in candidates {
                if section_count >= MAX_MEMORIES_PER_LINK {
                    break;
                }
                // Cross-repo injection is relevance-gated: a memory must
                // actually match the task text to travel between repos, so
                // another repo's high-importance fallback noise never leaks.
                let relevant = candidate.why.lexical > 0.0
                    || candidate.why.exact > 0.0
                    || candidate.why.semantic > 0.0;
                if !relevant {
                    continue;
                }
                let (block, block_tokens) =
                    self.memory_block(&candidate.memory, &compact_injections)?;
                let heading_tokens = if section.is_empty() {
                    estimate_tokens(&heading)
                } else {
                    0
                };
                if heading_tokens.saturating_add(block_tokens) > linked_budget {
                    continue;
                }
                if section.is_empty() {
                    section.push_str(&heading);
                    linked_budget = linked_budget.saturating_sub(heading_tokens);
                }
                section.push_str(&block);
                linked_budget = linked_budget.saturating_sub(block_tokens);
                section_count += 1;
                let mut why = candidate.why.clone();
                why.note = format!(
                    "linked from `{}` ({}); {}",
                    link.name, link.relation, why.note
                );
                items.push(ContextItem {
                    ordinal: u32::try_from(items.len() + 1).unwrap_or(u32::MAX),
                    source_type: "linked_memory".to_owned(),
                    source_id: candidate.memory.id.to_string(),
                    score: candidate.score,
                    tokens: block_tokens,
                    why,
                });
            }
            if !section.is_empty() {
                dynamic.push_str(&section);
            }
        }
        Ok(considered)
    }

    #[allow(clippy::too_many_lines)] // Keep the budgeted assembly and its signature together.
    fn compile_brain(&self, request: &ContextCompileRequest) -> Result<CompiledContext> {
        let stable_budget = self
            .stable_prefix_tokens
            .min(request.token_budget.saturating_mul(15) / 100);
        let stable_prefix = self.stable_prefix(
            &request.project_id,
            stable_budget,
            request.repository_root.as_deref(),
        )?;
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
        // Confidence and past utility are not evidence of relevance to this
        // task. Standing project preferences remain available, but unrelated
        // lessons must not fill the prompt merely because there is room.
        let candidates = candidates
            .into_iter()
            .filter(|candidate| {
                (candidate.memory.kind == "preference" && candidate.memory.scope_type == "project")
                    || candidate.why.lexical > 0.0
                    || candidate.why.exact > 0.0
                    || candidate.why.structural > 0.0
                    || candidate.why.semantic > 0.0
            })
            .collect::<Vec<_>>();
        // Reserve only the space actually occupied by relevant source pointers.
        // A full memory candidate set must not crowd out the code for this task.
        let (source_text, source_items) = crate::repository_context::build(
            &self.database,
            request,
            remaining.min(request.token_budget / 3),
            self.linked_projects,
        )?;
        let memory_budget = remaining.saturating_sub(estimate_tokens(&source_text));
        let compact_injections = if self.compact_injection {
            let ids = candidates
                .iter()
                .map(|candidate| candidate.memory.id.clone())
                .collect::<Vec<_>>();
            MemoryStore::new(self.database.clone()).compact_injections(&ids)?
        } else {
            BTreeMap::new()
        };
        let heading = request.framing.unwrap_or_default().heading();
        let selection = self.fill_memory_sections(
            candidates,
            &compact_injections,
            memory_budget,
            heading,
            REVIEW_HEADING,
        )?;
        let mut dynamic = selection.dynamic;
        let review = selection.review;
        let mut items = selection.items;
        let mut candidate_count = candidate_count;
        let source_start = dynamic.len();
        dynamic.push_str(&source_text);
        for mut item in source_items {
            item.ordinal = u32::try_from(items.len() + 1).unwrap_or(u32::MAX);
            items.push(item);
        }
        if self.linked_projects {
            let local_tokens = estimate_tokens(&dynamic).saturating_add(estimate_tokens(&review));
            let linked_budget = remaining.saturating_sub(local_tokens);
            candidate_count = candidate_count.saturating_add(self.fill_linked_sections(
                request,
                linked_budget,
                &mut dynamic,
                &mut items,
            )?);
        }
        // Review standards render LAST: adjacency to the conversation is the
        // salience this section exists for.
        if !review.is_empty() {
            // Each preceding block already ends in a newline. An unbudgeted
            // separator here can exceed the hard cap by one token.
            dynamic.push_str(&review);
        }
        let selected_tokens = estimate_tokens(&format!("{stable_prefix}{dynamic}"));
        debug_assert!(selected_tokens <= request.token_budget);
        // Canonicalize the known source block at its actual assembly boundary.
        // Memory text can contain arbitrary Markdown, including identical
        // headings, so searching the complete rendered output is unsafe.
        let canonical_dynamic = format!(
            "{}{}{}",
            &dynamic[..source_start],
            canonical_source_map(&source_text),
            &dynamic[source_start + source_text.len()..],
        );
        let mut context = CompiledContext {
            id: ContextRunId::new(),
            request_id: request.request_id.clone(),
            mode: ContextMode::Brain,
            stable_prefix,
            dynamic_context: dynamic,
            delivery_digest: None,
            items,
            metrics: ContextMetrics {
                compile_ms: 0,
                candidate_count,
                selected_tokens,
                token_budget: request.token_budget,
                stable_prefix_hash: String::new(),
            },
        }
        .with_stable_hash();
        context.delivery_digest =
            Some(self.delivery_digest(request, &context, &canonical_dynamic)?);
        Ok(context)
    }

    fn memory_block(
        &self,
        memory: &MemoryRecord,
        compact_injections: &BTreeMap<MemoryId, CompactInjection>,
    ) -> Result<(String, u32)> {
        if !self.compact_injection {
            let block = format!(
                "- [{}] {}\n  Rationale: {}\n",
                memory.id,
                context_safe(&memory.claim),
                context_safe(&memory.rationale)
            );
            let tokens = estimate_tokens(&block);
            return Ok((block, tokens));
        }
        // A candidate that does not fit may be visited again during the second
        // selection pass. Looking at its rendering must not consume it.
        let projection = compact_injections.get(&memory.id).ok_or_else(|| {
            Error::Config(format!(
                "compact injection projection is missing for memory `{}`; rebuild required",
                memory.id
            ))
        })?;
        if !projection.matches(&memory.kind, &memory.claim) {
            return Err(Error::Config(format!(
                "compact injection projection is stale for memory `{}`; rebuild required",
                memory.id
            )));
        }
        Ok((projection.text.clone(), projection.token_estimate))
    }

    fn stable_prefix(
        &self,
        project_id: &ProjectId,
        budget: u32,
        repository_root: Option<&Path>,
    ) -> Result<String> {
        if budget == 0 {
            return Ok(String::new());
        }
        let facts = self.database.with_reader(|connection| {
            let project_name = connection.query_row(
                "SELECT name FROM projects WHERE id = ?1 AND tombstoned_at_ms IS NULL",
                [project_id.as_str()],
                |row| row.get::<_, String>(0),
            )?;
            let mut fact_statement = connection.prepare(
                "SELECT key, value FROM project_facts
                 WHERE project_id = ?1 AND key NOT LIKE 'internal.%'
                 ORDER BY key LIMIT 24",
            )?;
            let fact_rows = fact_statement.query_map([project_id.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let curated = fact_rows.collect::<std::result::Result<Vec<_>, _>>()?;
            Ok((project_name, curated))
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
        // Curated architecture facts (deployment hosts, hosting, storage)
        // lead: they are the highest-signal lines an engineer needs first.
        for (key, value) in facts.1 {
            if !append_if_fits(
                &mut output,
                &format!("- {}: {}\n", context_safe(&key), context_safe(&value)),
                budget,
            ) {
                break;
            }
        }
        // The detected stack is the single highest-signal orientation line
        // for an agent: it stops re-derivation of "what is this built with".
        if let Some(root) = repository_root {
            let stack = crate::techstack::detect(root)
                .into_iter()
                .map(|tech| tech.name)
                .collect::<Vec<_>>()
                .join(", ");
            if !stack.is_empty() {
                append_if_fits(
                    &mut output,
                    &format!("- built-with: {}\n", context_safe(&stack)),
                    budget,
                );
            }
        }
        Ok(output)
    }

    fn delivery_digest(
        &self,
        request: &ContextCompileRequest,
        context: &CompiledContext,
        canonical_dynamic: &str,
    ) -> Result<String> {
        let mut sources = self.database.with_reader(|connection| {
            context.items.iter().map(|item| {
                // Observation times and memory usage counters change during
                // harmless reindexing/compilation; they are not freshness.
                let query = match item.source_type.as_str() {
                    "code" | "code_dependency" => Some(
                        "SELECT json_array(project_id,current_digest,source_ref,extractor_version,tombstoned_at_ms)
                         FROM nodes WHERE id=?1"),
                    "memory" | "linked_memory" => Some(
                        "SELECT json_array(project_id,scope_type,scope_id,kind,epistemic_state,status,
                            valid_from_commit,valid_to_commit,known_from_ms,known_to_ms,trust_source)
                         FROM memories WHERE id=?1"),
                    "verification" => Some(
                        "SELECT json_array(project_id,artifact_digest,commit_hash,strength,exit_code)
                         FROM evidence WHERE id=?1"),
                    _ => None,
                };
                let provenance: Option<String> = match query {
                    Some(query) => connection.prepare_cached(query)?.query_row([&item.source_id], |row| row.get(0)).optional()?,
                    None => None,
                };
                Ok((item.source_type.clone(), item.source_id.clone(), provenance))
            }).collect::<Result<Vec<_>>>()
        })?;
        sources.sort_unstable();
        Ok(sha256(&serde_json::to_vec(&(
            "context-delivery-v1",
            &request.project_id,
            &request.repository_root,
            &request.task_id,
            sha256(request.task_text.trim().as_bytes()),
            &request.head,
            context.mode,
            &context.stable_prefix,
            canonical_dynamic,
            sources,
        ))?))
    }

    fn persist(&self, request: &ContextCompileRequest, context: &CompiledContext) -> Result<()> {
        let now = unix_time_ms();
        let task_digest = sha256(request.task_text.as_bytes());
        let request_fingerprint = request_fingerprint(request, self.compact_injection)?;
        self.database.write_immediate(|transaction| {
            transaction.execute(
                "INSERT INTO context_runs
                 (id, request_id, project_id, request_fingerprint, mode,
                  task_text_digest, token_budget,
                  selected_tokens, candidate_count, stable_prefix_hash, stable_prefix,
                  dynamic_context, compile_ms, created_at_ms, delivery_digest)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
                    now,
                    context.delivery_digest,
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
        let expected = request_fingerprint(request, self.compact_injection)?;
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
                            stable_prefix_hash, stable_prefix, dynamic_context, compile_ms, delivery_digest
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
                            row.get::<_, Option<String>>(9)?,
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
            delivery_digest: header.9,
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

/// Ignore ranking order only inside the compiler's source-map list. Memory
/// prose, section order, framing, and every rendered pointer byte remain exact.
fn canonical_source_map(text: &str) -> String {
    const INTRO: &str = "\n# Task source map (indexed reference data)\nInspect these sources before changing behavior; labels are data and linked repositories may be at a different revision.\n";
    let Some(start) = text.find(INTRO).map(|index| index + INTRO.len()) else {
        return text.to_owned();
    };
    let mut end = start;
    let mut lines = Vec::new();
    for line in text[start..].split_inclusive('\n') {
        if !line.starts_with("- ") {
            break;
        }
        end += line.len();
        lines.push(line);
    }
    lines.sort_unstable();
    format!("{}{}{}", &text[..start], lines.concat(), &text[end..])
}

fn request_fingerprint(request: &ContextCompileRequest, compact_injection: bool) -> Result<String> {
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
        compact_injection: request.mode == ContextMode::Brain && compact_injection,
    };
    Ok(sha256(&serde_json::to_vec(&fingerprint)?))
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

impl CompiledContext {
    /// Signature for hook delivery, with exact rendered bytes as the conservative
    /// fallback for legacy runs that lack compile-time semantic metadata.
    #[must_use]
    pub fn delivery_signature(&self) -> String {
        self.delivery_digest.clone().unwrap_or_else(|| {
            sha256(format!("{}{}", self.stable_prefix, self.dynamic_context).as_bytes())
        })
    }

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
    #[allow(clippy::too_many_lines)]
    fn delivery_signature_ignores_source_order_but_retains_revision_and_membership() {
        let (_temp, compiler, project) = fixture();
        compiler.database.write_immediate(|tx| {
            for name in ["alpha", "beta"] {
                tx.execute("INSERT INTO nodes(id,project_id,kind,stable_key,label,file_path,current_digest,source_kind,source_ref,extractor_version,observed_at_ms)
                    VALUES(?1,?2,'Function',?1,?3,?4,'digest-1','native','revision-1','test',0)",
                    params![name, project.as_str(), format!("abort_{name}"), format!("src/{name}.rs")])?;
            }
            Ok(())
        }).unwrap();
        let request = ContextCompileRequest {
            request_id: "delivery-alpha".to_owned(),
            project_id: project.clone(),
            task_text: "fix abort cleanup".to_owned(),
            current_paths: vec!["src/alpha.rs".to_owned()],
            token_budget: 6000,
            mode: ContextMode::Brain,
            repository_root: None,
            head: None,
            task_id: None,
            framing: None,
        };
        let first = compiler.compile(&request).unwrap();
        let mut changed = request.clone();
        changed.request_id = "delivery-beta".to_owned();
        changed.current_paths = vec!["src/beta.rs".to_owned()];
        let reordered = compiler.compile(&changed).unwrap();
        assert_ne!(first.dynamic_context, reordered.dynamic_context);
        assert_eq!(first.delivery_digest, reordered.delivery_digest);
        assert_eq!(
            first
                .items
                .iter()
                .filter(|item| item.source_type == "code")
                .count(),
            2
        );

        for (column, value) in [("current_digest", "digest-2"), ("source_ref", "revision-2")] {
            compiler
                .database
                .write_immediate(|tx| {
                    tx.execute(
                        &format!("UPDATE nodes SET {column}=?1 WHERE id='alpha'"),
                        [value],
                    )?;
                    Ok(())
                })
                .unwrap();
            changed.request_id = format!("delivery-{column}");
            let revised = compiler.compile(&changed).unwrap();
            assert_eq!(revised.dynamic_context, reordered.dynamic_context);
            assert_ne!(revised.delivery_digest, reordered.delivery_digest);
            // Replays remain tied to the original compile-time provenance.
            let replay = compiler.compile(&request).unwrap();
            assert_eq!(replay.id, first.id);
            assert_eq!(replay.delivery_digest, first.delivery_digest);
            assert_eq!(replay.stable_prefix, first.stable_prefix);
            assert_eq!(replay.dynamic_context, first.dynamic_context);
            assert_eq!(
                replay
                    .items
                    .iter()
                    .map(|item| (&item.source_type, &item.source_id))
                    .collect::<Vec<_>>(),
                first
                    .items
                    .iter()
                    .map(|item| (&item.source_type, &item.source_id))
                    .collect::<Vec<_>>()
            );
        }
        compiler.database.write_immediate(|tx| {
            tx.execute("UPDATE nodes SET current_digest='digest-1',source_ref='revision-1',tombstoned_at_ms=1 WHERE id='alpha'", [])?;
            Ok(())
        }).unwrap();
        changed.request_id = "delivery-removed".to_owned();
        let removed = compiler.compile(&changed).unwrap();
        assert_ne!(removed.delivery_digest, first.delivery_digest);
        assert!(!removed.items.iter().any(|item| item.source_id == "alpha"));
        compiler
            .database
            .write_immediate(|tx| {
                tx.execute(
                    "UPDATE nodes SET tombstoned_at_ms=NULL,observed_at_ms=999 WHERE id='alpha'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        changed.request_id = "delivery-restored".to_owned();
        assert_eq!(
            compiler.compile(&changed).unwrap().delivery_digest,
            first.delivery_digest
        );
        compiler.database.write_immediate(|tx| {
            tx.execute("INSERT INTO nodes(id,project_id,kind,stable_key,label,file_path,current_digest,source_kind,source_ref,extractor_version,observed_at_ms)
                VALUES('gamma',?1,'Function','gamma','abort_gamma','src/gamma.rs','digest-1','native','revision-1','test',0)", [project.as_str()])?;
            Ok(())
        }).unwrap();
        changed.request_id = "delivery-added".to_owned();
        let added = compiler.compile(&changed).unwrap();
        assert!(added.items.iter().any(|item| item.source_id == "gamma"));
        assert_ne!(added.delivery_digest, first.delivery_digest);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn delivery_signature_tracks_facts_lessons_and_task_workspace_scope() {
        let (_temp, compiler, project) = fixture();
        let mut request = ContextCompileRequest {
            request_id: "delivery-facts-base".to_owned(),
            project_id: project.clone(),
            task_text: "fix abort cleanup".to_owned(),
            current_paths: vec![],
            token_budget: 6000,
            mode: ContextMode::Brain,
            repository_root: None,
            head: None,
            task_id: None,
            framing: None,
        };
        let initial = compiler.compile(&request).unwrap();
        assert!(
            initial
                .items
                .iter()
                .any(|item| item.source_type == "memory")
        );
        ProjectRegistry::new(compiler.database.clone())
            .set_fact(&project, "storage", "SQLite", None)
            .unwrap();
        request.request_id = "delivery-facts-new".to_owned();
        let fact = compiler.compile(&request).unwrap();
        assert!(fact.stable_prefix.contains("storage: SQLite"));
        assert_ne!(initial.delivery_digest, fact.delivery_digest);
        compiler.database.write_immediate(|tx| {
            tx.execute("UPDATE memories SET rationale='Abort regression now covers cancellation' WHERE project_id=?1", [project.as_str()])?;
            Ok(())
        }).unwrap();
        request.request_id = "delivery-lesson-edit".to_owned();
        let lesson = compiler.compile(&request).unwrap();
        assert_ne!(lesson.delivery_digest, fact.delivery_digest);
        assert!(lesson.dynamic_context.contains("covers cancellation"));
        compiler
            .database
            .write_immediate(|tx| {
                tx.execute(
                    "UPDATE memories SET known_from_ms=known_from_ms+1 WHERE project_id=?1",
                    [project.as_str()],
                )?;
                Ok(())
            })
            .unwrap();
        request.request_id = "delivery-lesson-freshness".to_owned();
        let freshness = compiler.compile(&request).unwrap();
        assert_eq!(freshness.dynamic_context, lesson.dynamic_context);
        assert_ne!(freshness.delivery_digest, lesson.delivery_digest);
        compiler
            .database
            .write_immediate(|tx| {
                tx.execute(
                    "UPDATE memories SET status='suspect' WHERE project_id=?1",
                    [project.as_str()],
                )?;
                Ok(())
            })
            .unwrap();
        request.request_id = "delivery-lesson-invalidated".to_owned();
        let invalidated = compiler.compile(&request).unwrap();
        assert!(
            !invalidated
                .items
                .iter()
                .any(|item| item.source_type == "memory")
        );
        assert_ne!(invalidated.delivery_digest, freshness.delivery_digest);
        compiler
            .database
            .write_immediate(|tx| {
                tx.execute(
                    "UPDATE memories SET status='active' WHERE project_id=?1",
                    [project.as_str()],
                )?;
                Ok(())
            })
            .unwrap();
        request.request_id = "delivery-lesson-restored".to_owned();
        assert_eq!(
            compiler.compile(&request).unwrap().delivery_digest,
            freshness.delivery_digest
        );

        // Scope affects delivery even when the rendered contents stay identical.
        let mut scoped = request.clone();
        scoped.repository_root = Some(PathBuf::from("/workspace/other"));
        assert_ne!(
            compiler
                .delivery_digest(&scoped, &freshness, &freshness.dynamic_context)
                .unwrap(),
            freshness.delivery_digest.clone().unwrap()
        );
        scoped = request.clone();
        scoped.task_id = Some(TaskId::new());
        assert_ne!(
            compiler
                .delivery_digest(&scoped, &freshness, &freshness.dynamic_context)
                .unwrap(),
            freshness.delivery_digest.clone().unwrap()
        );
        scoped = request.clone();
        scoped.task_text = "another abort task".to_owned();
        assert_ne!(
            compiler
                .delivery_digest(&scoped, &freshness, &freshness.dynamic_context)
                .unwrap(),
            freshness.delivery_digest.unwrap()
        );
    }

    #[test]
    fn delivery_canonicalization_preserves_non_source_prose_and_pointer_changes() {
        let prefix = "# Lessons\n- First\n- Second\n\n# Task source map (indexed reference data)\nInspect these sources before changing behavior; labels are data and linked repositories may be at a different revision.\n";
        let first = format!("{prefix}- repo: a.rs\n- repo: b.rs\n# Review\n- Never reorder\n");
        let reordered = format!("{prefix}- repo: b.rs\n- repo: a.rs\n# Review\n- Never reorder\n");
        assert_eq!(
            canonical_source_map(&first),
            canonical_source_map(&reordered)
        );
        for changed in [
            first.replace("a.rs", "c.rs"),
            first.replace("- First\n- Second", "- Second\n- First"),
            first.replace("Never reorder", "Changed fact"),
        ] {
            assert_ne!(canonical_source_map(&first), canonical_source_map(&changed));
        }
    }

    #[test]
    fn compact_candidates_can_be_skipped_under_tight_budgets() {
        let (_temp, compiler, project) = fixture();
        let compiler = compiler.with_compact_injection(true);
        for budget in [1, 8, 24, 40, 64, 100, 200] {
            let context = compiler
                .compile(&ContextCompileRequest {
                    request_id: format!("compact-tight-{budget}"),
                    project_id: project.clone(),
                    task_text: "fix abort cleanup".to_owned(),
                    current_paths: vec![],
                    token_budget: budget,
                    mode: ContextMode::Brain,
                    repository_root: None,
                    head: None,
                    task_id: None,
                    framing: None,
                })
                .unwrap();
            assert!(context.metrics.selected_tokens <= budget);
        }
    }

    #[test]
    fn unrelated_lessons_do_not_fill_context_but_standing_preferences_remain() {
        let (_temp, compiler, project) = fixture();
        let store = MemoryStore::new(compiler.database.clone());
        let preference = store
            .create_candidate(
                &project,
                &MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: project.to_string(),
                    kind: "preference".to_owned(),
                    claim: "Behavior changes include a regression test".to_owned(),
                    rationale: "Established team convention".to_owned(),
                    confidence: 0.5,
                    importance: 1.0,
                    tags: vec![],
                    valid_from_commit: None,
                    valid_to_commit: None,
                    trust_source: "human_instruction".to_owned(),
                    evidence_ids: vec![],
                    source_task_ids: vec![],
                    node_ids: vec![],
                },
            )
            .unwrap();
        store
            .promote(&preference.id, EpistemicState::Validated, true)
            .unwrap();
        let scoped = store
            .create_candidate(
                &project,
                &MemoryDraft {
                    scope_type: "file".to_owned(),
                    scope_id: "src/invoice.rs".to_owned(),
                    kind: "preference".to_owned(),
                    claim: "Store invoice amounts as integers".to_owned(),
                    rationale: "A convention for billing files only".to_owned(),
                    confidence: 0.5,
                    importance: 1.0,
                    tags: vec![],
                    valid_from_commit: None,
                    valid_to_commit: None,
                    trust_source: "human_instruction".to_owned(),
                    evidence_ids: vec![],
                    source_task_ids: vec![],
                    node_ids: vec![],
                },
            )
            .unwrap();
        store
            .promote(&scoped.id, EpistemicState::Validated, true)
            .unwrap();
        let context = compiler
            .compile(&ContextCompileRequest {
                request_id: "relevance-gate".to_owned(),
                project_id: project,
                task_text: "Improve authentication".to_owned(),
                current_paths: vec![],
                token_budget: 2000,
                mode: ContextMode::Brain,
                repository_root: None,
                head: None,
                task_id: None,
                framing: None,
            })
            .unwrap();
        assert!(!context.dynamic_context.contains("Release reservation"));
        assert!(!context.dynamic_context.contains("Store invoice amounts"));
        assert!(
            context
                .dynamic_context
                .contains("Behavior changes include a regression test")
        );
        assert_eq!(
            context
                .items
                .iter()
                .filter(|i| i.source_type == "memory")
                .count(),
            1
        );
    }

    #[test]
    fn relevant_source_keeps_space_when_memory_budget_is_full() {
        let (_temp, compiler, project) = fixture();
        compiler.database.write_immediate(|tx| {
            tx.execute("INSERT INTO nodes(id,project_id,kind,stable_key,label,file_path,current_digest,source_kind,source_ref,extractor_version,observed_at_ms) VALUES(?1,?2,'Function','abort_handler','abort_handler','src/abort.rs','digest','native','head','test',0)",
                params![crate::ids::NodeId::new().to_string(), project.as_str()])?;
            Ok(())
        }).unwrap();
        let store = MemoryStore::new(compiler.database.clone());
        for index in 0..8 {
            let memory = store
                .create_candidate(
                    &project,
                    &MemoryDraft {
                        scope_type: "project".to_owned(),
                        scope_id: project.to_string(),
                        kind: "lesson".to_owned(),
                        claim: format!("Abort cleanup rule {index}"),
                        rationale: "Verified cleanup behavior. ".repeat(24),
                        confidence: 0.5,
                        importance: 1.0,
                        tags: vec!["abort".to_owned()],
                        valid_from_commit: None,
                        valid_to_commit: None,
                        trust_source: "human_instruction".to_owned(),
                        evidence_ids: vec![],
                        source_task_ids: vec![],
                        node_ids: vec![],
                    },
                )
                .unwrap();
            store
                .promote(&memory.id, EpistemicState::Validated, true)
                .unwrap();
        }
        let context = compiler
            .compile(&ContextCompileRequest {
                request_id: "source-reservation".to_owned(),
                project_id: project,
                task_text: "Fix abort cleanup".to_owned(),
                current_paths: vec!["src/abort.rs".to_owned()],
                token_budget: 800,
                mode: ContextMode::Brain,
                repository_root: None,
                head: None,
                task_id: None,
                framing: None,
            })
            .unwrap();
        assert!(context.items.iter().any(|i| i.source_type == "memory"));
        assert!(context.items.iter().any(|i| i.source_type == "code"));
        assert!(context.dynamic_context.contains("src/abort.rs"));
        assert!(context.metrics.selected_tokens <= 800);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn review_standards_section_renders_preferences_imperatively_and_last() {
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
            .start_task(&project, "respect review standards", None)
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
            .expect("finish");
        let memories = MemoryStore::new(database.clone());
        for (kind, claim) in [
            ("lesson", "Cleanup runs during review standards checks"),
            (
                "preference",
                "Review standards require terminal counters bumped",
            ),
        ] {
            let memory = memories
                .create_candidate(
                    &project,
                    &MemoryDraft {
                        scope_type: "project".to_owned(),
                        scope_id: project.to_string(),
                        kind: kind.to_owned(),
                        claim: claim.to_owned(),
                        rationale: "review standards fixture".to_owned(),
                        confidence: 0.5,
                        importance: 0.9,
                        tags: Vec::new(),
                        valid_from_commit: None,
                        valid_to_commit: None,
                        trust_source: "agent_inference".to_owned(),
                        evidence_ids: vec![evidence.id.clone()],
                        source_task_ids: vec![task.id.clone()],
                        node_ids: Vec::new(),
                    },
                )
                .expect("memory");
            for state in [
                EpistemicState::Observed,
                EpistemicState::Supported,
                EpistemicState::Validated,
            ] {
                memories.promote(&memory.id, state, false).expect("promote");
            }
        }
        let request = ContextCompileRequest {
            request_id: "req-review".to_owned(),
            project_id: project,
            task_text: "respect review standards for terminal counters".to_owned(),
            current_paths: Vec::new(),
            token_budget: 4_000,
            mode: ContextMode::Brain,
            repository_root: None,
            head: None,
            task_id: None,
            framing: None,
        };
        let plain = ContextCompiler::new(database.clone(), 6_000, 1_800)
            .compile(&request)
            .expect("plain");
        assert!(!plain.dynamic_context.contains("# Team review standards"));
        let mut request2 = request;
        request2.request_id = "req-review-on".to_owned();
        let sectioned = ContextCompiler::new(database, 6_000, 1_800)
            .with_review_standards(true)
            .compile(&request2)
            .expect("sectioned");
        let text = &sectioned.dynamic_context;
        assert!(text.contains("# Team review standards"));
        assert!(text.contains("- MUST: Review standards require terminal counters bumped"));
        let review_at = text
            .find("# Team review standards")
            .expect("review section");
        let memory_at = text
            .find("# Validated engineering memory")
            .expect("memory section");
        assert!(review_at > memory_at, "review section must render last");
        assert_eq!(
            sectioned
                .items
                .iter()
                .filter(|item| item.source_type == "memory")
                .count(),
            2
        );
    }

    #[test]
    fn memory_cap_limits_injection_below_budget() {
        let (_temp, compiler, project) = fixture();
        let capped = compiler.clone().with_memory_cap(0); // 0 = uncapped baseline
        let base = ContextCompileRequest {
            request_id: "req-cap-base".to_owned(),
            project_id: project.clone(),
            task_text: "fix abort cleanup".to_owned(),
            current_paths: Vec::new(),
            token_budget: 2_000,
            mode: ContextMode::Brain,
            repository_root: None,
            head: None,
            task_id: None,
            framing: None,
        };
        let uncapped = capped.compile(&base).expect("uncapped");
        assert!(!uncapped.items.is_empty());
        let one = compiler.with_memory_cap(1);
        let mut request = base;
        request.request_id = "req-cap-one".to_owned();
        let limited = one.compile(&request).expect("capped");
        assert_eq!(
            limited
                .items
                .iter()
                .filter(|item| item.source_type == "memory")
                .count(),
            1
        );
    }

    #[test]
    fn directive_framing_changes_only_the_heading() {
        let (_temp, compiler, project) = fixture();
        let base = ContextCompileRequest {
            request_id: "req-framing-ref".to_owned(),
            project_id: project.clone(),
            task_text: "fix abort cleanup".to_owned(),
            current_paths: Vec::new(),
            token_budget: 2_000,
            mode: ContextMode::Brain,
            repository_root: None,
            head: None,
            task_id: None,
            framing: None,
        };
        let reference = compiler.compile(&base).expect("reference compile");
        let mut directive_request = base;
        directive_request.request_id = "req-framing-dir".to_owned();
        directive_request.framing = Some(InjectionFraming::Directive);
        let directive = compiler
            .compile(&directive_request)
            .expect("directive compile");
        assert!(
            reference
                .dynamic_context
                .starts_with("# Validated engineering memory")
        );
        assert!(
            directive
                .dynamic_context
                .starts_with("# Known project rules")
        );
        // Same memories, same order — only the stance differs.
        let ids = |context: &CompiledContext| {
            context
                .items
                .iter()
                .map(|item| item.source_id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&reference), ids(&directive));
        assert_eq!(
            InjectionFraming::parse("directive"),
            Some(InjectionFraming::Directive)
        );
        assert_eq!(InjectionFraming::parse("bogus"), None);
    }

    #[test]
    fn budget_headroom_admits_more_than_three_memories_of_one_kind() {
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
            .start_task(&project, "fix scheduler retry budget", None)
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
        for ordinal in 0..6 {
            let memory = memories
                .create_candidate(
                    &project,
                    &MemoryDraft {
                        scope_type: "project".to_owned(),
                        scope_id: project.to_string(),
                        kind: "lesson".to_owned(),
                        claim: format!("Scheduler retry budget rule number {ordinal}"),
                        rationale: "Regression test passes".to_owned(),
                        confidence: 0.5,
                        importance: 0.9,
                        tags: Vec::new(),
                        valid_from_commit: None,
                        valid_to_commit: None,
                        trust_source: "agent_inference".to_owned(),
                        evidence_ids: vec![evidence.id.clone()],
                        source_task_ids: vec![task.id.clone()],
                        node_ids: Vec::new(),
                    },
                )
                .expect("memory");
            for state in [
                EpistemicState::Observed,
                EpistemicState::Supported,
                EpistemicState::Validated,
            ] {
                memories.promote(&memory.id, state, false).expect("promote");
            }
        }
        let compiler = ContextCompiler::new(database, 6_000, 1_800);
        let context = compiler
            .compile(&ContextCompileRequest {
                request_id: "req-headroom".to_owned(),
                project_id: project,
                task_text: "enforce the scheduler retry budget".to_owned(),
                current_paths: Vec::new(),
                token_budget: 4_000,
                mode: ContextMode::Brain,
                repository_root: None,
                head: None,
                task_id: None,
                framing: None,
            })
            .expect("compile");
        let memory_items = context
            .items
            .iter()
            .filter(|item| item.source_type == "memory")
            .count();
        assert_eq!(
            memory_items, 6,
            "with budget headroom every validated same-kind memory must inject: {context:?}"
        );
        assert!(context.metrics.selected_tokens <= 4_000);
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
                framing: None,
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
            framing: None,
        };
        let first = compiler.compile(&request).expect("first");
        let second = compiler
            .clone()
            .with_compact_injection(true)
            .compile(&request)
            .expect("compact flag cannot affect an off-mode retry");
        assert_eq!(first, second);
        assert!(first.stable_prefix.is_empty());
        assert!(first.dynamic_context.is_empty());
        assert_eq!(first.metrics.selected_tokens, 0);
        assert!(first.items.is_empty());
    }

    #[test]
    fn compact_rendering_reduces_tokens_but_explain_retains_canonical_provenance() {
        let (_temp, verbose_compiler, project) = fixture();
        let verbose = verbose_compiler
            .compile(&ContextCompileRequest {
                request_id: "verbose-render".to_owned(),
                project_id: project.clone(),
                task_text: "fix abort cleanup".to_owned(),
                current_paths: Vec::new(),
                token_budget: 400,
                mode: ContextMode::Brain,
                repository_root: None,
                head: None,
                task_id: None,
                framing: None,
            })
            .expect("verbose context");
        let compact_compiler = verbose_compiler.clone().with_compact_injection(true);
        let compact = compact_compiler
            .compile(&ContextCompileRequest {
                request_id: "compact-render".to_owned(),
                project_id: project,
                task_text: "fix abort cleanup".to_owned(),
                current_paths: Vec::new(),
                token_budget: 400,
                mode: ContextMode::Brain,
                repository_root: None,
                head: None,
                task_id: None,
                framing: None,
            })
            .expect("compact context");

        assert_eq!(verbose.items.len(), 1);
        assert_eq!(compact.items.len(), 1);
        assert_eq!(verbose.items[0].source_id, compact.items[0].source_id);
        assert!(
            verbose
                .dynamic_context
                .contains("Rationale: Regression test passes")
        );
        assert!(
            verbose
                .dynamic_context
                .contains(&compact.items[0].source_id)
        );
        assert!(
            compact
                .dynamic_context
                .contains("- RULE: Release reservation")
        );
        assert!(!compact.dynamic_context.contains("Rationale:"));
        assert!(
            !compact
                .dynamic_context
                .contains(&compact.items[0].source_id)
        );
        assert!(
            compact.metrics.selected_tokens < verbose.metrics.selected_tokens,
            "compact={} verbose={}",
            compact.metrics.selected_tokens,
            verbose.metrics.selected_tokens
        );

        let explained = compact_compiler
            .explain(&compact.id, compact.items[0].ordinal)
            .expect("explain query")
            .expect("compact item remains explainable");
        assert_eq!(explained.source_id, compact.items[0].source_id);
        // Persisted why-scores round-trip through serde_json's default float
        // parsing, which can be 1 ulp off (see tests/properties.rs); compare
        // with the same tolerance instead of bit equality.
        assert!(
            (explained.why.lexical - compact.items[0].why.lexical).abs() < 1e-9,
            "lexical drifted: {} vs {}",
            explained.why.lexical,
            compact.items[0].why.lexical
        );
        assert!((explained.why.exact - compact.items[0].why.exact).abs() < 1e-9);
        assert_eq!(explained.why.note, compact.items[0].why.note);
        let memory_id = MemoryId::parse(&explained.source_id).expect("canonical memory ID");
        let canonical = MemoryStore::new(compact_compiler.database.clone())
            .get(&memory_id)
            .expect("canonical query")
            .expect("canonical memory");
        assert_eq!(canonical.claim, "Release reservation during abort cleanup");
        assert_eq!(canonical.rationale, "Regression test passes");
    }

    #[test]
    fn compact_injected_bytes_and_digest_are_stable_across_turn_ids() {
        let (_temp, compiler, project) = fixture();
        let compiler = compiler.with_compact_injection(true);
        let duplicate = MemoryStore::new(compiler.database.clone())
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
                    trust_source: "human_instruction".to_owned(),
                    evidence_ids: Vec::new(),
                    source_task_ids: Vec::new(),
                    node_ids: Vec::new(),
                },
            )
            .expect("equal-score memory");
        MemoryStore::new(compiler.database.clone())
            .promote(&duplicate.id, EpistemicState::Validated, true)
            .expect("validated equal-score memory");
        let compile_turn = |request_id: &str| {
            compiler
                .compile(&ContextCompileRequest {
                    request_id: request_id.to_owned(),
                    project_id: project.clone(),
                    task_text: "fix abort cleanup".to_owned(),
                    current_paths: vec!["src/cleanup.rs".to_owned()],
                    token_budget: 400,
                    mode: ContextMode::Brain,
                    repository_root: None,
                    head: None,
                    task_id: None,
                    framing: None,
                })
                .expect("compile turn")
        };
        let first = compile_turn("turn-request-1");
        let second = compile_turn("turn-request-2");
        let first_bytes = format!("{}{}", first.stable_prefix, first.dynamic_context);
        let second_bytes = format!("{}{}", second.stable_prefix, second.dynamic_context);

        assert_eq!(first_bytes.as_bytes(), second_bytes.as_bytes());
        assert_eq!(
            sha256(first_bytes.as_bytes()),
            sha256(second_bytes.as_bytes())
        );
        assert_eq!(first.items, second.items);
        assert_eq!(first.items.len(), 2);
        assert!(first.items[0].source_id < first.items[1].source_id);
        assert_eq!(
            first.metrics.selected_tokens,
            second.metrics.selected_tokens
        );
        assert!(!first_bytes.contains("turn-request-1"));
        assert!(!second_bytes.contains("turn-request-2"));
        assert!(!first_bytes.contains("ctx_"));
        assert!(!first_bytes.contains("mem_"));
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
            framing: None,
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
                framing: None,
            })
            .expect("same caller id in another project is independent");
        assert!(!second.dynamic_context.contains("reservation"));
        assert_ne!(first.id, second.id);

        let mut conflicting_retry = first_request.clone();
        conflicting_retry.task_text = "different task".to_owned();
        let error = compiler
            .compile(&conflicting_retry)
            .expect_err("same project retry fingerprint must match");
        assert!(error.to_string().contains("different request content"));

        let compact_error = compiler
            .with_compact_injection(true)
            .compile(&first_request)
            .expect_err("rendering ablation is part of a brain request fingerprint");
        assert!(
            compact_error
                .to_string()
                .contains("different request content")
        );
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
            framing: None,
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

    #[test]
    #[allow(clippy::too_many_lines)]
    fn linked_projects_inject_labeled_relevant_memories_only_when_enabled() {
        let temp = TempDir::new().expect("temp");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let registry = ProjectRegistry::new(database.clone());
        let root_a = temp.path().join("service-api");
        let root_b = temp.path().join("platform");
        std::fs::create_dir(&root_a).expect("a");
        std::fs::create_dir(&root_b).expect("b");
        let project_a = registry.resolve_or_register(&root_a).expect("a").project.id;
        let project_b = registry.resolve_or_register(&root_b).expect("b").project.id;

        let experience = ExperienceStore::new(database.clone());
        let memories = MemoryStore::new(database.clone());
        let validated = |project: &ProjectId, claim: &str, tags: Vec<String>| {
            let task = experience.start_task(project, claim, None).expect("task");
            let evidence = experience
                .record_evidence(
                    project,
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
                .expect("finish");
            let memory = memories
                .create_candidate(
                    project,
                    &MemoryDraft {
                        scope_type: "project".to_owned(),
                        scope_id: project.to_string(),
                        kind: "lesson".to_owned(),
                        claim: claim.to_owned(),
                        rationale: "Validated across services".to_owned(),
                        confidence: 0.5,
                        importance: 0.9,
                        tags,
                        valid_from_commit: None,
                        valid_to_commit: None,
                        trust_source: "agent_inference".to_owned(),
                        evidence_ids: vec![evidence.id],
                        source_task_ids: vec![task.id],
                        node_ids: Vec::new(),
                    },
                )
                .expect("memory");
            for state in [
                EpistemicState::Observed,
                EpistemicState::Supported,
                EpistemicState::Validated,
            ] {
                memories.promote(&memory.id, state, false).expect("promote");
            }
        };
        // The relevant cross-repo fact lives in the linked platform repo; a
        // second, unrelated platform memory must NOT travel (relevance gate).
        validated(
            &project_b,
            "Pagination cursors from the platform api are opaque - never parse them",
            vec!["pagination".to_owned(), "cursor".to_owned()],
        );
        validated(
            &project_b,
            "Platform deploys read the blue-green flag from the orchestrator",
            vec!["deploy".to_owned()],
        );
        registry
            .link(&project_a, &project_b, "interface", None)
            .expect("link");

        let request = ContextCompileRequest {
            request_id: "req-linked".to_owned(),
            project_id: project_a.clone(),
            task_text: "add pagination cursor handling to the client".to_owned(),
            current_paths: Vec::new(),
            token_budget: 4_000,
            mode: ContextMode::Brain,
            repository_root: None,
            head: None,
            task_id: None,
            framing: None,
        };
        let with_links = ContextCompiler::new(database.clone(), 6_000, 1_800)
            .with_linked_projects(true)
            .compile(&request)
            .expect("linked compile");
        let text = &with_links.dynamic_context;
        assert!(
            text.contains("# Linked repository `platform` (interface)"),
            "linked section must name the source repo: {text}"
        );
        assert!(text.contains("Pagination cursors from the platform api"));
        assert!(
            !text.contains("blue-green flag"),
            "irrelevant linked memories must not travel: {text}"
        );
        assert!(
            with_links
                .items
                .iter()
                .any(|item| item.source_type == "linked_memory"
                    && item.why.note.contains("linked from `platform`"))
        );

        let mut request2 = request;
        request2.request_id = "req-unlinked".to_owned();
        let without = ContextCompiler::new(database, 6_000, 1_800)
            .compile(&request2)
            .expect("default compile");
        assert!(
            !without.dynamic_context.contains("Linked repository"),
            "linked injection must be off unless enabled"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn commit_bounded_linked_memories_validate_against_the_linked_repos_head() {
        // Regression: distilled lessons always carry `valid_from_commit`
        // relative to THEIR repository. The linked pass must resolve the
        // linked repo's own checkout and HEAD; validating against the
        // requesting repo (or nothing) silently drops every real lesson.
        let run_git = |path: &std::path::Path, args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .output()
                .expect("git");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        let temp = TempDir::new().expect("temp");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let registry = ProjectRegistry::new(database.clone());
        let root_a = temp.path().join("consumer");
        let root_b = temp.path().join("provider");
        std::fs::create_dir(&root_a).expect("a");
        std::fs::create_dir(&root_b).expect("b");
        run_git(&root_b, &["init", "-q"]);
        run_git(&root_b, &["config", "user.email", "engram@example.invalid"]);
        run_git(&root_b, &["config", "user.name", "Engram Tests"]);
        std::fs::write(root_b.join("README.md"), "provider\n").expect("file");
        run_git(&root_b, &["add", "README.md"]);
        run_git(&root_b, &["commit", "-qm", "initial"]);
        let head = String::from_utf8(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&root_b)
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("git")
                .stdout,
        )
        .expect("utf8")
        .trim()
        .to_owned();

        let project_a = registry.resolve_or_register(&root_a).expect("a").project.id;
        let project_b = registry.resolve_or_register(&root_b).expect("b").project.id;
        let experience = ExperienceStore::new(database.clone());
        let memories = MemoryStore::new(database.clone());
        let task = experience
            .start_task(&project_b, "queue draining", None)
            .expect("task");
        let evidence = experience
            .record_evidence(
                &project_b,
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
            .expect("finish");
        let memory = memories
            .create_candidate(
                &project_b,
                &MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: project_b.to_string(),
                    kind: "lesson".to_owned(),
                    claim: "Draining the provider queue requires a barrier flush first".to_owned(),
                    rationale: "Regression test".to_owned(),
                    confidence: 0.5,
                    importance: 0.9,
                    tags: vec!["queue".to_owned()],
                    valid_from_commit: Some(head),
                    valid_to_commit: None,
                    trust_source: "agent_inference".to_owned(),
                    evidence_ids: vec![evidence.id],
                    source_task_ids: vec![task.id],
                    node_ids: Vec::new(),
                },
            )
            .expect("memory");
        for state in [
            EpistemicState::Observed,
            EpistemicState::Supported,
            EpistemicState::Validated,
        ] {
            memories.promote(&memory.id, state, false).expect("promote");
        }
        registry
            .link(&project_a, &project_b, "uses", None)
            .expect("link");
        let compiled = ContextCompiler::new(database, 6_000, 1_800)
            .with_linked_projects(true)
            .compile(&ContextCompileRequest {
                request_id: "req-linked-commit-bounded".to_owned(),
                project_id: project_a,
                task_text: "drain the provider queue safely".to_owned(),
                current_paths: Vec::new(),
                token_budget: 4_000,
                mode: ContextMode::Brain,
                repository_root: None,
                head: None,
                task_id: None,
                framing: None,
            })
            .expect("compile");
        assert!(
            compiled
                .dynamic_context
                .contains("Draining the provider queue requires a barrier flush"),
            "commit-bounded linked memory must validate against the linked repo's HEAD: {}",
            compiled.dynamic_context
        );
    }
}
