//! Evidence-gated, version-aware durable memory.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::blobs::BlobStore;
use crate::error::{Error, Result};
use crate::events::{DomainEvent, append_generated_in_transaction};
use crate::ids::{EvidenceId, MemoryId, NodeId, ProjectId, TaskId};
use crate::security::Redactor;
use crate::storage::Database;

#[derive(
    clap::ValueEnum, Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EpistemicState {
    Candidate,
    Observed,
    Supported,
    Validated,
    Rejected,
}

impl EpistemicState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Observed => "observed",
            Self::Supported => "supported",
            Self::Validated => "validated",
            Self::Rejected => "rejected",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "candidate" => Ok(Self::Candidate),
            "observed" => Ok(Self::Observed),
            "supported" => Ok(Self::Supported),
            "validated" => Ok(Self::Validated),
            "rejected" => Ok(Self::Rejected),
            _ => Err(Error::Config(format!(
                "invalid persisted epistemic state `{value}`"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Active,
    Suspect,
    Stale,
    Superseded,
    Contradicted,
    Archived,
    Purged,
}

impl MemoryStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspect => "suspect",
            Self::Stale => "stale",
            Self::Superseded => "superseded",
            Self::Contradicted => "contradicted",
            Self::Archived => "archived",
            Self::Purged => "purged",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "active" => Ok(Self::Active),
            "suspect" => Ok(Self::Suspect),
            "stale" => Ok(Self::Stale),
            "superseded" => Ok(Self::Superseded),
            "contradicted" => Ok(Self::Contradicted),
            "archived" => Ok(Self::Archived),
            "purged" => Ok(Self::Purged),
            _ => Err(Error::Config(format!(
                "invalid persisted memory status `{value}`"
            ))),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MemoryDraft {
    pub scope_type: String,
    pub scope_id: String,
    pub kind: String,
    pub claim: String,
    pub rationale: String,
    pub confidence: f64,
    pub importance: f64,
    pub tags: Vec<String>,
    pub valid_from_commit: Option<String>,
    pub valid_to_commit: Option<String>,
    pub trust_source: String,
    #[serde(default)]
    pub evidence_ids: Vec<EvidenceId>,
    #[serde(default)]
    pub source_task_ids: Vec<TaskId>,
    #[serde(default)]
    pub node_ids: Vec<NodeId>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MemoryRecord {
    pub id: MemoryId,
    pub project_id: ProjectId,
    pub scope_type: String,
    pub scope_id: String,
    pub kind: String,
    pub epistemic_state: EpistemicState,
    pub status: MemoryStatus,
    pub claim: String,
    pub rationale: String,
    pub confidence: f64,
    pub importance: f64,
    pub tags: Vec<String>,
    pub token_estimate: u32,
    pub valid_from_commit: Option<String>,
    pub valid_to_commit: Option<String>,
    pub known_from_ms: i64,
    pub known_to_ms: Option<i64>,
    pub trust_source: String,
    pub retrieved_count: u64,
    pub injected_count: u64,
    pub acknowledged_count: u64,
    pub successful_count: u64,
    pub harmful_count: u64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug)]
pub struct MemoryStore {
    database: Database,
    redactor: Redactor,
}

impl MemoryStore {
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

    /// Create a candidate memory and atomically link supplied provenance.
    ///
    /// Agent inference is confidence-capped at 0.50. Creation never bypasses
    /// the promotion state machine.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid fields, cross-project provenance, missing
    /// objects, or a failed canonical transaction.
    pub fn create_candidate(
        &self,
        project_id: &ProjectId,
        draft: &MemoryDraft,
    ) -> Result<MemoryRecord> {
        validate_draft(project_id, draft)?;
        let id = MemoryId::new();
        let now = unix_time_ms();
        let claim = self.redactor.redact_text(&draft.claim);
        let claim = claim.trim();
        let normalized_claim = normalize_claim(claim);
        let rationale = self.redactor.redact_text(&draft.rationale);
        let rationale = rationale.trim();
        let mut tags = draft
            .tags
            .iter()
            .map(|tag| self.redactor.redact_text(tag).trim().to_lowercase())
            .filter(|tag| !tag.is_empty())
            .collect::<Vec<_>>();
        tags.sort();
        tags.dedup();
        let tags_json = serde_json::to_string(&tags)?;
        let tags_text = tags.join(" ");
        let token_estimate = estimate_tokens(claim) + estimate_tokens(rationale);
        let event = DomainEvent::MemoryCreated {
            project_id: project_id.clone(),
            memory_id: id.clone(),
            kind: draft.kind.clone(),
            claim_digest: sha256(claim.as_bytes()),
            epistemic_state: EpistemicState::Candidate.as_str().to_owned(),
        };
        self.database.write_immediate(|transaction| {
            validate_memory_links(transaction, project_id, draft)?;
            transaction.execute(
                "INSERT INTO memories
                 (id, project_id, scope_type, scope_id, kind, epistemic_state, status,
                  claim, normalized_claim, rationale, confidence, importance, tags_json, token_estimate,
                  valid_from_commit, valid_to_commit, known_from_ms, trust_source, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'candidate', 'active', ?6, ?7, ?8, ?9,
                         ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?15)",
                params![
                    id.as_str(),
                    project_id.as_str(),
                    draft.scope_type,
                    draft.scope_id,
                    draft.kind,
                    claim,
                    normalized_claim,
                    rationale,
                    draft.confidence,
                    draft.importance,
                    tags_json,
                    token_estimate,
                    draft.valid_from_commit,
                    draft.valid_to_commit,
                    now,
                    draft.trust_source,
                ],
            )?;
            transaction.execute(
                "INSERT INTO memories_fts(memory_id, project_id, claim, rationale, tags)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id.as_str(),
                    project_id.as_str(),
                    claim,
                    rationale,
                    tags_text
                ],
            )?;
            crate::semantic::upsert_memory_vector(
                transaction,
                &id,
                project_id,
                claim,
                rationale,
                &tags,
                now,
            )?;
            link_draft_provenance(transaction, &id, draft)?;
            append_generated_in_transaction(transaction, &event, now)
        })?;
        self.get(&id)?.ok_or_else(|| {
            Error::Config("memory projection missing after successful commit".to_owned())
        })
    }

    /// Promote a memory by exactly one epistemic state.
    ///
    /// Technical validation requires deterministic/observational evidence.
    /// A human override may skip states but is permanently labeled.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid transitions, insufficient evidence, or
    /// failed storage.
    pub fn promote(
        &self,
        id: &MemoryId,
        target: EpistemicState,
        human_override: bool,
    ) -> Result<MemoryRecord> {
        if target == EpistemicState::Rejected {
            return Err(Error::Config(
                "use reject for the rejected transition".to_owned(),
            ));
        }
        let now = unix_time_ms();
        self.database.write_immediate(|transaction| {
            let (project_id, current, status) = memory_state(transaction, id)?;
            if status != MemoryStatus::Active {
                return Err(Error::Config(format!(
                    "memory `{id}` is not active and cannot be promoted"
                )));
            }
            let next_is_valid = target > current
                && (human_override || state_rank(target) == state_rank(current) + 1);
            if !next_is_valid {
                return Err(Error::Config(format!(
                    "invalid memory promotion from {} to {}",
                    current.as_str(),
                    target.as_str()
                )));
            }
            if target >= EpistemicState::Supported && !human_override {
                let independent: i64 = transaction.query_row(
                    "SELECT COUNT(DISTINCT verified.evidence_id)
                     FROM memory_evidence me
                     JOIN verified_supporting_evidence verified
                       ON verified.evidence_id = me.evidence_id
                     JOIN memory_sources source
                       ON source.memory_id = me.memory_id
                      AND source.task_id = verified.task_id
                     WHERE me.memory_id = ?1 AND me.relation = 'supports'
                       AND verified.project_id = ?2",
                    params![id.as_str(), project_id.as_str()],
                    |row| row.get(0),
                )?;
                if independent == 0 {
                    return Err(Error::Config(
                        "supported/validated technical memory requires Tier A/B evidence"
                            .to_owned(),
                    ));
                }
            }
            transaction.execute(
                "UPDATE memories
                 SET epistemic_state = ?2,
                     trust_source = CASE WHEN ?3 = 1 THEN 'human_override' ELSE trust_source END,
                     updated_at_ms = ?4
                 WHERE id = ?1",
                params![id.as_str(), target.as_str(), i64::from(human_override), now],
            )?;
            let event = DomainEvent::MemoryStateChanged {
                project_id,
                memory_id: id.clone(),
                epistemic_state: target.as_str().to_owned(),
                status: MemoryStatus::Active.as_str().to_owned(),
                reason: if human_override {
                    "human_override".to_owned()
                } else {
                    "evidence_promotion".to_owned()
                },
            };
            append_generated_in_transaction(transaction, &event, now)
        })?;
        self.get(id)?
            .ok_or_else(|| Error::Config("memory projection missing after promotion".to_owned()))
    }

    /// Mark an active memory rejected; rejected candidates never auto-inject.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown/purged memory or storage failure.
    pub fn reject(&self, id: &MemoryId, reason: &str) -> Result<MemoryRecord> {
        self.change_state(id, EpistemicState::Rejected, MemoryStatus::Archived, reason)
    }

    /// Change validity status while preserving historical queryability.
    ///
    /// # Errors
    ///
    /// Returns an error when status is `Purged`, the memory is unknown, or the
    /// transaction fails. Use [`Self::purge`] for irreversible removal.
    pub fn mark_status(
        &self,
        id: &MemoryId,
        status: MemoryStatus,
        reason: &str,
    ) -> Result<MemoryRecord> {
        if status == MemoryStatus::Purged {
            return Err(Error::Config(
                "use purge for irreversible removal".to_owned(),
            ));
        }
        let current = self
            .get(id)?
            .ok_or_else(|| Error::Config(format!("unknown memory `{id}`")))?;
        if current.status == status {
            return Ok(current);
        }
        self.change_state(id, current.epistemic_state, status, reason)
    }

    /// Detect symbol digest drift and quarantine affected active memories.
    ///
    /// # Errors
    ///
    /// Returns an error for storage or event failures.
    pub fn mark_symbol_drift_suspect(&self, project_id: &ProjectId) -> Result<u64> {
        const BATCH_SIZE: i64 = 256;

        let mut changed = 0_u64;
        loop {
            let now = unix_time_ms();
            let batch_count = self.database.write_immediate(|transaction| {
                let candidates = {
                    let mut statement = transaction.prepare(
                        "SELECT DISTINCT m.id, m.epistemic_state
                         FROM memories m
                         JOIN memory_nodes mn ON mn.memory_id = m.id
                         LEFT JOIN nodes n
                           ON n.id = mn.node_id AND n.tombstoned_at_ms IS NULL
                         WHERE m.project_id = ?1 AND m.status = 'active'
                           AND (n.id IS NULL OR n.current_digest <> mn.observed_digest)
                         ORDER BY m.id LIMIT ?2",
                    )?;
                    let rows = statement
                        .query_map(params![project_id.as_str(), BATCH_SIZE], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })?;
                    rows.collect::<std::result::Result<Vec<_>, _>>()?
                };
                for (raw_id, raw_state) in &candidates {
                    let id = MemoryId::parse(raw_id)?;
                    let epistemic_state = EpistemicState::parse(raw_state)?;
                    transaction.execute(
                        "UPDATE memories SET status = 'suspect', known_to_ms = ?2,
                             updated_at_ms = ?2 WHERE id = ?1 AND status = 'active'",
                        params![id.as_str(), now],
                    )?;
                    crate::semantic::delete_memory_vector(transaction, &id)?;
                    append_generated_in_transaction(
                        transaction,
                        &DomainEvent::MemoryStateChanged {
                            project_id: project_id.clone(),
                            memory_id: id,
                            epistemic_state: epistemic_state.as_str().to_owned(),
                            status: MemoryStatus::Suspect.as_str().to_owned(),
                            reason: "referenced_symbol_changed".to_owned(),
                        },
                        now,
                    )?;
                }
                u64::try_from(candidates.len())
                    .map_err(|_| Error::Config("drift batch size exceeds u64".to_owned()))
            })?;
            changed = changed.saturating_add(batch_count);
            if batch_count == 0 {
                return Ok(changed);
            }
        }
    }

    /// Irreversibly remove memory content and all retrieval/link projections.
    /// The immutable purge audit event contains only typed IDs, never the claim.
    ///
    /// Evidence supported only by this memory also loses its stored artifact:
    /// the blob reference is released inside the transaction and the file is
    /// unlinked after it commits. Retries stay idempotent.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown memory, a failed canonical transaction, or
    /// an unlink failure other than an already-missing file.
    pub fn purge(&self, id: &MemoryId) -> Result<()> {
        let now = unix_time_ms();
        let orphaned = self.database.write_immediate(|transaction| {
            let existing_project = transaction
                .query_row(
                    "SELECT project_id FROM memories WHERE id = ?1",
                    [id.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let Some(raw_project_id) = existing_project else {
                let already_purged: bool = transaction.query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM events
                         WHERE event_type = 'MEMORY_PURGED'
                           AND json_extract(payload_json, '$.data.memory_id') = ?1
                     )",
                    [id.as_str()],
                    |row| row.get(0),
                )?;
                if already_purged {
                    return Ok(Vec::new());
                }
                return Err(Error::Config(format!("unknown memory `{id}`")));
            };
            let project_id = ProjectId::parse(&raw_project_id)?;
            transaction.execute(
                "DELETE FROM memories_fts WHERE memory_id = ?1",
                [id.as_str()],
            )?;
            // Purge MUST remove embeddings (spec §26.5). The foreign key
            // cascade would also remove this row, but the derived sidecar is
            // purged explicitly to mirror the FTS projection.
            crate::semantic::delete_memory_vector(transaction, id)?;
            // Context runs duplicate rendered claim/rationale text for exact
            // retry and explanation. Purge the complete derived run before its
            // provenance rows disappear so no canonical copy survives.
            transaction.execute(
                "DELETE FROM context_runs
                 WHERE id IN (
                     SELECT context_run_id FROM context_items
                     WHERE source_type = 'memory' AND source_id = ?1
                 )",
                [id.as_str()],
            )?;
            // Evidence supported only by this memory loses its stored artifact
            // with it: purge deletes canonical content and blobs, while the
            // provenance row itself stays auditable.
            let owned = {
                let mut statement = transaction.prepare(
                    "SELECT e.id, e.artifact_blob_digest
                     FROM evidence e
                     JOIN memory_evidence link ON link.evidence_id = e.id
                     WHERE link.memory_id = ?1
                       AND NOT EXISTS (
                           SELECT 1 FROM memory_evidence other
                           WHERE other.evidence_id = e.id AND other.memory_id <> ?1
                       )",
                )?;
                let rows = statement.query_map([id.as_str()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?;
                rows.collect::<std::result::Result<Vec<_>, _>>()?
            };
            let mut orphaned = Vec::new();
            for (evidence_id, blob_digest) in owned {
                transaction.execute(
                    "UPDATE evidence
                     SET artifact_text = NULL, artifact_excerpt = NULL,
                         artifact_blob_digest = NULL, artifact_size_bytes = NULL
                     WHERE id = ?1",
                    [evidence_id.as_str()],
                )?;
                if let Some(digest) = blob_digest
                    && BlobStore::release_in_transaction(transaction, &digest)?.removes_file()
                {
                    orphaned.push(digest);
                }
            }
            let deleted =
                transaction.execute("DELETE FROM memories WHERE id = ?1", [id.as_str()])?;
            if deleted != 1 {
                return Err(Error::Config(format!("unknown memory `{id}`")));
            }
            append_generated_in_transaction(
                transaction,
                &DomainEvent::MemoryPurged {
                    project_id,
                    memory_id: id.clone(),
                },
                now,
            )?;
            Ok(orphaned)
        })?;
        // Files are unlinked only after the canonical transaction commits, so a
        // rollback can never leave a metadata row without its bytes.
        let blobs = BlobStore::new(self.database.clone());
        for digest in orphaned {
            blobs.remove_file(&digest)?;
        }
        Ok(())
    }

    /// Fetch one non-purged memory.
    ///
    /// # Errors
    ///
    /// Returns an error for corrupt storage or invalid typed IDs.
    pub fn get(&self, id: &MemoryId) -> Result<Option<MemoryRecord>> {
        self.database.with_reader(|connection| {
            let raw = connection
                .query_row(
                    "SELECT id, project_id, scope_type, scope_id, kind, epistemic_state,
                            status, claim, rationale, confidence, importance, tags_json,
                            token_estimate, valid_from_commit, valid_to_commit, known_from_ms,
                            known_to_ms, trust_source, retrieved_count, injected_count,
                            acknowledged_count, successful_count, harmful_count, updated_at_ms
                     FROM memories WHERE id = ?1",
                    [id.as_str()],
                    raw_memory_from_row,
                )
                .optional()?;
            raw.map(RawMemory::decode).transpose()
        })
    }

    /// Find an exact normalized claim in the same narrow scope.
    ///
    /// # Errors
    ///
    /// Returns an error for storage or corrupt typed identifiers.
    pub fn find_exact(
        &self,
        project_id: &ProjectId,
        scope_type: &str,
        scope_id: &str,
        kind: &str,
        claim: &str,
    ) -> Result<Option<MemoryRecord>> {
        let normalized = normalize_claim(claim);
        let id = self.database.with_reader(|connection| {
            connection
                .query_row(
                    "SELECT id FROM memories
                     WHERE project_id = ?1 AND scope_type = ?2 AND scope_id = ?3
                       AND kind = ?4 AND normalized_claim = ?5
                       AND status <> 'purged'
                     ORDER BY known_from_ms LIMIT 1",
                    params![project_id.as_str(), scope_type, scope_id, kind, normalized],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(Error::from)
        })?;
        id.map(|value| MemoryId::parse(&value))
            .transpose()?
            .map(|id| self.get(&id))
            .transpose()
            .map(Option::flatten)
    }

    /// List bounded memories for inspection/export without affecting utility counters.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid limit or corrupt/storage state.
    pub fn list(
        &self,
        project_id: &ProjectId,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>> {
        if limit == 0 || limit > 10_000 {
            return Err(Error::Config(
                "memory list limit must be between 1 and 10000".to_owned(),
            ));
        }
        let ids = self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT id FROM memories
                 WHERE project_id = ?1 AND (?2 IS NULL OR kind = ?2)
                 ORDER BY updated_at_ms DESC, id LIMIT ?3",
            )?;
            let rows = statement.query_map(
                params![
                    project_id.as_str(),
                    kind,
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                |row| row.get::<_, String>(0),
            )?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Error::from)
        })?;
        ids.into_iter()
            .map(|id| {
                let id = MemoryId::parse(&id)?;
                self.get(&id)?.ok_or_else(|| {
                    Error::Config("memory disappeared during bounded list".to_owned())
                })
            })
            .collect()
    }

    /// Attach newly observed evidence/episodes to a deduplicated memory.
    ///
    /// # Errors
    ///
    /// Returns an error for cross-project links, missing objects, or storage failure.
    pub fn link_distillation(
        &self,
        id: &MemoryId,
        project_id: &ProjectId,
        draft: &MemoryDraft,
    ) -> Result<MemoryRecord> {
        validate_draft(project_id, draft)?;
        let now = unix_time_ms();
        self.database.write_immediate(|transaction| {
            let (stored_project, _, status) = memory_state(transaction, id)?;
            if &stored_project != project_id || status == MemoryStatus::Purged {
                return Err(Error::Config(
                    "cannot link distillation outside the active memory scope".to_owned(),
                ));
            }
            validate_memory_links(transaction, project_id, draft)?;
            link_draft_provenance(transaction, id, draft)?;
            append_generated_in_transaction(
                transaction,
                &DomainEvent::LearningDistilled {
                    project_id: project_id.clone(),
                    memory_id: id.clone(),
                    source_task_ids: draft.source_task_ids.clone(),
                    reused_existing: true,
                },
                now,
            )
        })?;
        self.get(id)?
            .ok_or_else(|| Error::Config("memory disappeared after provenance linking".to_owned()))
    }

    pub(crate) fn retrieval_candidates(
        &self,
        project_id: &ProjectId,
        fts_query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(MemoryRecord, f64)>> {
        let ids = self.database.with_reader(|connection| {
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);
            if let Some(query) = fts_query {
                let mut statement = connection.prepare(
                    "SELECT m.id, bm25(memories_fts)
                     FROM memories_fts
                     JOIN memories m ON m.id = memories_fts.memory_id
                     WHERE memories_fts MATCH ?1 AND m.project_id = ?2
                       AND m.status = 'active' AND m.epistemic_state = 'validated'
                       AND NOT EXISTS (
                         SELECT 1 FROM memory_nodes mn
                         LEFT JOIN nodes n ON n.id = mn.node_id AND n.tombstoned_at_ms IS NULL
                         WHERE mn.memory_id = m.id
                           AND (n.id IS NULL OR n.current_digest <> mn.observed_digest)
                       )
                     ORDER BY bm25(memories_fts), m.importance DESC, m.id
                     LIMIT ?3",
                )?;
                let rows = statement
                    .query_map(params![query, project_id.as_str(), limit], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
                    })?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(Error::from)
            } else {
                let mut statement = connection.prepare(
                    "SELECT id, 0.0 FROM memories m
                     WHERE project_id = ?1 AND status = 'active'
                       AND epistemic_state = 'validated'
                       AND NOT EXISTS (
                         SELECT 1 FROM memory_nodes mn
                         LEFT JOIN nodes n ON n.id = mn.node_id AND n.tombstoned_at_ms IS NULL
                         WHERE mn.memory_id = m.id
                           AND (n.id IS NULL OR n.current_digest <> mn.observed_digest)
                       )
                     ORDER BY importance DESC, id LIMIT ?2",
                )?;
                let rows = statement.query_map(params![project_id.as_str(), limit], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
                })?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(Error::from)
            }
        })?;
        let mut memories = Vec::with_capacity(ids.len());
        for (id, bm25) in ids {
            let id = MemoryId::parse(&id)?;
            if let Some(memory) = self.get(&id)? {
                memories.push((memory, 1.0 / (1.0 + bm25.abs())));
            }
        }
        Ok(memories)
    }

    pub(crate) fn structural_overlap(
        &self,
        memory_id: &MemoryId,
        current_paths: &[String],
    ) -> Result<f64> {
        if current_paths.is_empty() {
            return Ok(0.0);
        }
        self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT n.file_path FROM memory_nodes mn
                 JOIN nodes n ON n.id = mn.node_id
                 WHERE mn.memory_id = ?1 AND n.tombstoned_at_ms IS NULL",
            )?;
            let rows =
                statement.query_map([memory_id.as_str()], |row| row.get::<_, Option<String>>(0))?;
            let mut linked = Vec::new();
            for row in rows {
                if let Some(path) = row? {
                    linked.push(path);
                }
            }
            if linked.is_empty() {
                return Ok(0.0);
            }
            let matches = linked
                .iter()
                .filter(|linked_path| current_paths.iter().any(|path| path == *linked_path))
                .count();
            let matches = u32::try_from(matches).unwrap_or(u32::MAX);
            let linked = u32::try_from(linked.len()).unwrap_or(u32::MAX);
            Ok(f64::from(matches) / f64::from(linked))
        })
    }

    pub(crate) fn structural_candidates(
        &self,
        project_id: &ProjectId,
        current_paths: &[String],
        limit: usize,
    ) -> Result<Vec<MemoryRecord>> {
        if current_paths.is_empty() {
            return Ok(Vec::new());
        }
        let paths_json = serde_json::to_string(current_paths)?;
        let ids = self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT DISTINCT m.id
                 FROM memories m
                 JOIN memory_nodes mn ON mn.memory_id = m.id
                 JOIN nodes n ON n.id = mn.node_id AND n.tombstoned_at_ms IS NULL
                 WHERE m.project_id = ?1 AND m.status = 'active'
                   AND m.epistemic_state = 'validated'
                   AND n.current_digest = mn.observed_digest
                   AND n.file_path IN (SELECT value FROM json_each(?2))
                 ORDER BY m.importance DESC, m.id LIMIT ?3",
            )?;
            let rows = statement.query_map(
                params![
                    project_id.as_str(),
                    paths_json,
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                |row| row.get::<_, String>(0),
            )?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Error::from)
        })?;
        let mut records = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(record) = self.get(&MemoryId::parse(&id)?)? {
                records.push(record);
            }
        }
        Ok(records)
    }

    fn change_state(
        &self,
        id: &MemoryId,
        epistemic_state: EpistemicState,
        status: MemoryStatus,
        reason: &str,
    ) -> Result<MemoryRecord> {
        if reason.trim().is_empty() {
            return Err(Error::Config(
                "memory state reason cannot be empty".to_owned(),
            ));
        }
        let now = unix_time_ms();
        self.database.write_immediate(|transaction| {
            let (project_id, _, _) = memory_state(transaction, id)?;
            transaction.execute(
                "UPDATE memories SET epistemic_state = ?2, status = ?3,
                     known_to_ms = CASE WHEN ?3 = 'active' THEN NULL ELSE ?4 END,
                     updated_at_ms = ?4 WHERE id = ?1",
                params![id.as_str(), epistemic_state.as_str(), status.as_str(), now],
            )?;
            // Forgotten/archived/invalidated memories drop their derived
            // vector immediately; reactivation re-derives it deterministically.
            crate::semantic::sync_memory_vector(
                transaction,
                id,
                status == MemoryStatus::Active,
                now,
            )?;
            append_generated_in_transaction(
                transaction,
                &DomainEvent::MemoryStateChanged {
                    project_id,
                    memory_id: id.clone(),
                    epistemic_state: epistemic_state.as_str().to_owned(),
                    status: status.as_str().to_owned(),
                    reason: reason.to_owned(),
                },
                now,
            )
        })?;
        self.get(id)?
            .ok_or_else(|| Error::Config(format!("unknown memory `{id}`")))
    }
}

fn validate_draft(project_id: &ProjectId, draft: &MemoryDraft) -> Result<()> {
    if draft.claim.trim().is_empty() || draft.claim.len() > 16_384 {
        return Err(Error::Config(
            "memory claim must contain 1..=16384 bytes".to_owned(),
        ));
    }
    if draft.rationale.len() > 32_768
        || draft.scope_type.trim().is_empty()
        || draft.scope_id.trim().is_empty()
        || draft.kind.trim().is_empty()
        || draft.trust_source.trim().is_empty()
    {
        return Err(Error::Config(
            "memory draft contains invalid fields".to_owned(),
        ));
    }
    if !(0.0..=1.0).contains(&draft.confidence)
        || !(0.0..=1.0).contains(&draft.importance)
        || !draft.confidence.is_finite()
        || !draft.importance.is_finite()
    {
        return Err(Error::Config(
            "memory confidence and importance must be finite values in [0,1]".to_owned(),
        ));
    }
    if draft.trust_source == "agent_inference" && draft.confidence > 0.5 {
        return Err(Error::Config(
            "agent-inferred candidates are capped at confidence 0.50".to_owned(),
        ));
    }
    if draft.scope_type == "project" && draft.scope_id != project_id.as_str() {
        return Err(Error::Config(
            "project-scoped memory scope_id must equal project_id".to_owned(),
        ));
    }
    if draft.tags.len() > 64 {
        return Err(Error::Config("memory supports at most 64 tags".to_owned()));
    }
    Ok(())
}

fn validate_memory_links(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    draft: &MemoryDraft,
) -> Result<()> {
    for evidence_id in &draft.evidence_ids {
        require_link_project(transaction, "evidence", evidence_id.as_str(), project_id)?;
    }
    for task_id in &draft.source_task_ids {
        require_link_project(transaction, "tasks", task_id.as_str(), project_id)?;
    }
    for node_id in &draft.node_ids {
        require_link_project(transaction, "nodes", node_id.as_str(), project_id)?;
    }
    Ok(())
}

fn require_link_project(
    transaction: &Transaction<'_>,
    table: &str,
    id: &str,
    project_id: &ProjectId,
) -> Result<()> {
    let sql = format!("SELECT project_id FROM {table} WHERE id = ?1");
    let stored = transaction
        .query_row(&sql, [id], |row| row.get::<_, String>(0))
        .optional()?
        .ok_or_else(|| Error::Config(format!("unknown linked {table} object `{id}`")))?;
    if stored != project_id.as_str() {
        return Err(Error::Config(format!(
            "linked {table} object belongs to another project"
        )));
    }
    Ok(())
}

fn link_draft_provenance(
    transaction: &Transaction<'_>,
    memory_id: &MemoryId,
    draft: &MemoryDraft,
) -> Result<()> {
    for task_id in &draft.source_task_ids {
        transaction.execute(
            "INSERT OR IGNORE INTO memory_sources(memory_id, task_id) VALUES (?1, ?2)",
            params![memory_id.as_str(), task_id.as_str()],
        )?;
    }
    for evidence_id in &draft.evidence_ids {
        let contradicts: bool = transaction.query_row(
            "SELECT
                 COALESCE(exit_code <> 0, 0)
                 OR EXISTS (
                     SELECT 1 FROM attempts a
                     WHERE a.id = evidence.attempt_id
                       AND a.outcome IN ('failure','partial')
                 )
             FROM evidence WHERE id = ?1",
            [evidence_id.as_str()],
            |row| row.get(0),
        )?;
        let relation = if contradicts {
            "contradicts"
        } else {
            "supports"
        };
        transaction.execute(
            "INSERT OR IGNORE INTO memory_evidence(memory_id, evidence_id, relation)
             VALUES (?1, ?2, ?3)",
            params![memory_id.as_str(), evidence_id.as_str(), relation],
        )?;
    }
    for node_id in &draft.node_ids {
        let digest = transaction.query_row(
            "SELECT current_digest FROM nodes WHERE id = ?1 AND tombstoned_at_ms IS NULL",
            [node_id.as_str()],
            |row| row.get::<_, String>(0),
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO memory_nodes(memory_id, node_id, observed_digest) VALUES (?1, ?2, ?3)",
            params![memory_id.as_str(), node_id.as_str(), digest],
        )?;
    }
    Ok(())
}

fn memory_state(
    transaction: &Transaction<'_>,
    id: &MemoryId,
) -> Result<(ProjectId, EpistemicState, MemoryStatus)> {
    let raw = transaction
        .query_row(
            "SELECT project_id, epistemic_state, status FROM memories WHERE id = ?1",
            [id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| Error::Config(format!("unknown memory `{id}`")))?;
    Ok((
        ProjectId::parse(&raw.0)?,
        EpistemicState::parse(&raw.1)?,
        MemoryStatus::parse(&raw.2)?,
    ))
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

struct RawMemory {
    id: String,
    project_id: String,
    scope_type: String,
    scope_id: String,
    kind: String,
    epistemic_state: String,
    status: String,
    claim: String,
    rationale: String,
    confidence: f64,
    importance: f64,
    tags_json: String,
    token_estimate: i64,
    valid_from_commit: Option<String>,
    valid_to_commit: Option<String>,
    known_from_ms: i64,
    known_to_ms: Option<i64>,
    trust_source: String,
    retrieved_count: i64,
    injected_count: i64,
    acknowledged_count: i64,
    successful_count: i64,
    harmful_count: i64,
    updated_at_ms: i64,
}

impl RawMemory {
    fn decode(self) -> Result<MemoryRecord> {
        Ok(MemoryRecord {
            id: MemoryId::parse(&self.id)?,
            project_id: ProjectId::parse(&self.project_id)?,
            scope_type: self.scope_type,
            scope_id: self.scope_id,
            kind: self.kind,
            epistemic_state: EpistemicState::parse(&self.epistemic_state)?,
            status: MemoryStatus::parse(&self.status)?,
            claim: self.claim,
            rationale: self.rationale,
            confidence: self.confidence,
            importance: self.importance,
            tags: serde_json::from_str(&self.tags_json)?,
            token_estimate: u32::try_from(self.token_estimate)
                .map_err(|_| Error::Config("invalid stored token estimate".to_owned()))?,
            valid_from_commit: self.valid_from_commit,
            valid_to_commit: self.valid_to_commit,
            known_from_ms: self.known_from_ms,
            known_to_ms: self.known_to_ms,
            trust_source: self.trust_source,
            retrieved_count: nonnegative_u64(self.retrieved_count)?,
            injected_count: nonnegative_u64(self.injected_count)?,
            acknowledged_count: nonnegative_u64(self.acknowledged_count)?,
            successful_count: nonnegative_u64(self.successful_count)?,
            harmful_count: nonnegative_u64(self.harmful_count)?,
            updated_at_ms: self.updated_at_ms,
        })
    }
}

fn raw_memory_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawMemory> {
    Ok(RawMemory {
        id: row.get(0)?,
        project_id: row.get(1)?,
        scope_type: row.get(2)?,
        scope_id: row.get(3)?,
        kind: row.get(4)?,
        epistemic_state: row.get(5)?,
        status: row.get(6)?,
        claim: row.get(7)?,
        rationale: row.get(8)?,
        confidence: row.get(9)?,
        importance: row.get(10)?,
        tags_json: row.get(11)?,
        token_estimate: row.get(12)?,
        valid_from_commit: row.get(13)?,
        valid_to_commit: row.get(14)?,
        known_from_ms: row.get(15)?,
        known_to_ms: row.get(16)?,
        trust_source: row.get(17)?,
        retrieved_count: row.get(18)?,
        injected_count: row.get(19)?,
        acknowledged_count: row.get(20)?,
        successful_count: row.get(21)?,
        harmful_count: row.get(22)?,
        updated_at_ms: row.get(23)?,
    })
}

fn estimate_tokens(value: &str) -> u32 {
    let bytes = u32::try_from(value.len()).unwrap_or(u32::MAX);
    bytes.saturating_add(3) / 4
}

fn nonnegative_u64(value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::Config("negative memory usage counter".to_owned()))
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

pub(crate) fn normalize_claim(claim: &str) -> String {
    claim
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
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
    use crate::experience::{EvidenceStrength, ExperienceStore};
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

    fn draft(project: &ProjectId, evidence_ids: Vec<EvidenceId>) -> MemoryDraft {
        MemoryDraft {
            scope_type: "project".to_owned(),
            scope_id: project.to_string(),
            kind: "lesson".to_owned(),
            claim: "Always release the reservation after abort.".to_owned(),
            rationale: "A regression test demonstrates the cleanup path.".to_owned(),
            confidence: 0.5,
            importance: 0.9,
            tags: vec!["cleanup".to_owned(), "abort".to_owned()],
            valid_from_commit: None,
            valid_to_commit: None,
            trust_source: "agent_inference".to_owned(),
            evidence_ids,
            source_task_ids: Vec::new(),
            node_ids: Vec::new(),
        }
    }

    #[test]
    fn technical_memory_requires_independent_evidence_for_support_and_validation() {
        let (_temp, database, project) = fixture();
        let memories = MemoryStore::new(database.clone());
        let memory = memories
            .create_candidate(&project, &draft(&project, Vec::new()))
            .expect("candidate");
        memories
            .promote(&memory.id, EpistemicState::Observed, false)
            .expect("observed");
        let error = memories
            .promote(&memory.id, EpistemicState::Supported, false)
            .expect_err("unsupported");
        assert!(error.to_string().contains("Tier A/B"));

        let experience = ExperienceStore::new(database);
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
            .finish_task(
                &task.id,
                crate::experience::TaskOutcome::Success,
                Some(true),
                None,
            )
            .expect("finish task");
        let mut supported_draft = draft(&project, vec![evidence.id]);
        supported_draft.source_task_ids = vec![task.id];
        let supported = memories
            .create_candidate(&project, &supported_draft)
            .expect("candidate with evidence");
        memories
            .promote(&supported.id, EpistemicState::Observed, false)
            .expect("observed");
        memories
            .promote(&supported.id, EpistemicState::Supported, false)
            .expect("supported");
        let validated = memories
            .promote(&supported.id, EpistemicState::Validated, false)
            .expect("validated");
        assert_eq!(validated.epistemic_state, EpistemicState::Validated);
    }

    #[test]
    fn inference_confidence_cap_and_purge_are_enforced() {
        let (_temp, database, project) = fixture();
        let memories = MemoryStore::new(database.clone());
        assert_eq!(
            database
                .integrity_report()
                .expect("integrity before insert")
                .integrity,
            "ok"
        );
        let mut invalid = draft(&project, Vec::new());
        invalid.confidence = 0.7;
        assert!(memories.create_candidate(&project, &invalid).is_err());

        let memory = memories
            .create_candidate(&project, &draft(&project, Vec::new()))
            .expect("memory");
        assert_eq!(
            database
                .integrity_report()
                .expect("integrity after insert")
                .integrity,
            "ok"
        );
        memories.purge(&memory.id).expect("purge");
        memories.purge(&memory.id).expect("idempotent purge");
        assert!(memories.get(&memory.id).expect("get").is_none());
        let counts = database
            .with_reader(|connection| {
                let fts = connection.query_row(
                    "SELECT COUNT(*) FROM memories_fts WHERE memory_id = ?1",
                    [memory.id.as_str()],
                    |row| row.get::<_, i64>(0),
                )?;
                let links = connection.query_row(
                    "SELECT COUNT(*) FROM memory_evidence WHERE memory_id = ?1",
                    [memory.id.as_str()],
                    |row| row.get::<_, i64>(0),
                )?;
                let purge_events = connection.query_row(
                    "SELECT COUNT(*) FROM events
                     WHERE event_type = 'MEMORY_PURGED'
                       AND json_extract(payload_json, '$.data.memory_id') = ?1",
                    [memory.id.as_str()],
                    |row| row.get::<_, i64>(0),
                )?;
                Ok((fts, links, purge_events))
            })
            .expect("counts");
        assert_eq!(counts, (0, 0, 1));
    }

    #[test]
    fn purging_a_memory_deletes_the_blob_backing_its_own_evidence() {
        let (_temp, database, project) = fixture();
        let memories = MemoryStore::new(database.clone());
        let experience = ExperienceStore::new(database.clone()).with_blob_threshold(64);
        let artifact = "supporting artifact line\n".repeat(64);
        let evidence = experience
            .record_evidence(
                &project,
                None,
                None,
                "log",
                EvidenceStrength::Observational,
                None,
                None,
                None,
                artifact.as_bytes(),
                &serde_json::json!({}),
            )
            .expect("evidence");
        let digest = evidence
            .artifact_blob_digest
            .clone()
            .expect("artifact must be blob backed");
        let path = experience.blobs().path_for(&digest).expect("path");
        assert!(path.exists());

        let memory = memories
            .create_candidate(&project, &draft(&project, vec![evidence.id.clone()]))
            .expect("memory");
        memories.purge(&memory.id).expect("purge");
        assert!(!path.exists(), "purge must delete the owned blob file");
        assert_eq!(
            experience.blobs().refcount(&digest).expect("refcount"),
            None
        );
        let resolved = experience
            .get_evidence(&evidence.id)
            .expect("fetch")
            .expect("provenance row survives");
        assert!(
            resolved.artifact_text.is_empty(),
            "purged artifact content must be gone"
        );
        assert!(resolved.artifact_blob_digest.is_none());

        memories.purge(&memory.id).expect("idempotent purge");
    }

    #[test]
    fn memory_lifecycle_maintains_the_derived_semantic_vector() {
        fn vector_rows(database: &Database, id: &MemoryId) -> i64 {
            database
                .with_reader(|connection| {
                    connection
                        .query_row(
                            "SELECT COUNT(*) FROM memory_vectors WHERE memory_id = ?1",
                            [id.as_str()],
                            |row| row.get(0),
                        )
                        .map_err(Error::from)
                })
                .expect("vector count")
        }

        let (_temp, database, project) = fixture();
        let memories = MemoryStore::new(database.clone());
        let created = memories
            .create_candidate(&project, &draft(&project, Vec::new()))
            .expect("memory");
        assert_eq!(
            vector_rows(&database, &created.id),
            1,
            "creation must upsert the derived embedding"
        );

        memories
            .mark_status(&created.id, MemoryStatus::Archived, "user_forget")
            .expect("forget");
        assert_eq!(
            vector_rows(&database, &created.id),
            0,
            "forget must drop the derived embedding"
        );
        memories
            .mark_status(&created.id, MemoryStatus::Active, "user_restore")
            .expect("restore");
        assert_eq!(
            vector_rows(&database, &created.id),
            1,
            "reactivation must re-derive the embedding"
        );

        memories.purge(&created.id).expect("purge");
        assert_eq!(
            vector_rows(&database, &created.id),
            0,
            "purge must remove embeddings (spec 26.5)"
        );
    }

    #[test]
    fn repeated_logical_forget_is_side_effect_free() {
        let (_temp, database, project) = fixture();
        let memories = MemoryStore::new(database.clone());
        let memory = memories
            .create_candidate(&project, &draft(&project, Vec::new()))
            .expect("memory");

        let first = memories
            .mark_status(&memory.id, MemoryStatus::Archived, "user_forget")
            .expect("first forget");
        let second = memories
            .mark_status(&memory.id, MemoryStatus::Archived, "user_forget")
            .expect("idempotent forget");
        assert_eq!(first.status, MemoryStatus::Archived);
        assert_eq!(first.updated_at_ms, second.updated_at_ms);

        let state_events = database
            .with_reader(|connection| {
                connection
                    .query_row(
                        "SELECT COUNT(*) FROM events
                         WHERE event_type = 'MEMORY_STATE_CHANGED'
                           AND json_extract(payload_json, '$.data.memory_id') = ?1
                           AND json_extract(payload_json, '$.data.status') = 'archived'",
                        [memory.id.as_str()],
                        |row| row.get::<_, i64>(0),
                    )
                    .map_err(Error::from)
            })
            .expect("state event count");
        assert_eq!(state_events, 1);
    }

    #[test]
    fn failed_and_unrelated_evidence_cannot_support_a_memory() {
        let (_temp, database, project) = fixture();
        let experience = ExperienceStore::new(database.clone());
        let failed_task = experience
            .start_task(&project, "failed verification", None)
            .expect("failed task");
        let failed_attempt = experience
            .record_attempt(
                &failed_task.id,
                "run regression suite",
                crate::experience::AttemptOutcome::Failure,
                &[],
            )
            .expect("failed attempt");
        let failed_evidence = experience
            .record_evidence(
                &project,
                Some(&failed_task.id),
                Some(&failed_attempt.id),
                "test_result",
                EvidenceStrength::Deterministic,
                None,
                Some("cargo test"),
                Some(1),
                b"FAILED",
                &serde_json::json!({"failed": 1}),
            )
            .expect("failed evidence is valid contradictory provenance");
        experience
            .finish_task(
                &failed_task.id,
                crate::experience::TaskOutcome::Success,
                Some(true),
                None,
            )
            .expect("finish failed-evidence fixture task");

        let memories = MemoryStore::new(database.clone());
        let mut failed_draft = draft(&project, vec![failed_evidence.id]);
        failed_draft.source_task_ids = vec![failed_task.id];
        let failed_memory = memories
            .create_candidate(&project, &failed_draft)
            .expect("failed-evidence memory");
        memories
            .promote(&failed_memory.id, EpistemicState::Observed, false)
            .expect("observed");
        let error = memories
            .promote(&failed_memory.id, EpistemicState::Supported, false)
            .expect_err("failing evidence cannot support");
        assert!(error.to_string().contains("Tier A/B"));

        let evidence_task = experience
            .start_task(&project, "evidence task", None)
            .expect("evidence task");
        let passing = experience
            .record_evidence(
                &project,
                Some(&evidence_task.id),
                None,
                "test_result",
                EvidenceStrength::Deterministic,
                None,
                Some("cargo test"),
                Some(0),
                b"ok",
                &serde_json::json!({}),
            )
            .expect("passing evidence");
        experience
            .finish_task(
                &evidence_task.id,
                crate::experience::TaskOutcome::Success,
                Some(true),
                None,
            )
            .expect("finish evidence task");
        let unrelated_task = experience
            .start_task(&project, "unrelated source", None)
            .expect("unrelated task");
        experience
            .finish_task(
                &unrelated_task.id,
                crate::experience::TaskOutcome::Success,
                Some(true),
                None,
            )
            .expect("finish unrelated task");
        let mut unrelated_draft = draft(&project, vec![passing.id]);
        unrelated_draft.source_task_ids = vec![unrelated_task.id];
        let unrelated_memory = memories
            .create_candidate(&project, &unrelated_draft)
            .expect("unrelated-evidence memory");
        memories
            .promote(&unrelated_memory.id, EpistemicState::Observed, false)
            .expect("observed");
        memories
            .promote(&unrelated_memory.id, EpistemicState::Supported, false)
            .expect_err("unrelated evidence cannot support");
    }
}
