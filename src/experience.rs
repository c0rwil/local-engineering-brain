//! Typed engineering episodes and evidence capture.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::blobs::BlobStore;
use crate::error::{Error, Result};
use crate::events::{DomainEvent, append_generated_in_transaction};
use crate::ids::{AttemptId, EvidenceId, ProjectId, TaskId};
use crate::security::Redactor;
use crate::storage::Database;

/// Default artifact size above which evidence text moves to the blob store.
/// Mirrors `storage.blob_threshold_bytes`.
pub const DEFAULT_BLOB_THRESHOLD_BYTES: usize = 16_384;

/// Bounded head of a blob-backed artifact kept inline for cheap inspection.
const ARTIFACT_EXCERPT_BYTES: usize = 1_024;

/// Content type recorded for evidence artifacts in the blob store.
const ARTIFACT_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

#[derive(clap::ValueEnum, Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    Success,
    Partial,
    Failure,
    Abandoned,
}

impl TaskOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Partial => "partial",
            Self::Failure => "failure",
            Self::Abandoned => "abandoned",
        }
    }
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    Unknown,
    Success,
    Failure,
    Partial,
}

impl AttemptOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Partial => "partial",
        }
    }
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStrength {
    Deterministic,
    Observational,
    Inferred,
}

impl EvidenceStrength {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Deterministic => "deterministic",
            Self::Observational => "observational",
            Self::Inferred => "inferred",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TaskRecord {
    pub id: TaskId,
    pub project_id: ProjectId,
    pub session_id: Option<String>,
    pub prompt: String,
    pub status: String,
    pub accepted_by_user: Option<bool>,
    pub started_at_ms: i64,
    pub completed_at_ms: Option<i64>,
    pub final_commit: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AttemptRecord {
    pub id: AttemptId,
    pub task_id: TaskId,
    pub ordinal: u32,
    pub strategy_summary: String,
    pub outcome: AttemptOutcome,
    pub files: Vec<String>,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EvidenceRecord {
    pub id: EvidenceId,
    pub project_id: ProjectId,
    pub task_id: Option<TaskId>,
    pub attempt_id: Option<AttemptId>,
    pub kind: String,
    pub strength: EvidenceStrength,
    pub commit_hash: Option<String>,
    pub command: Option<String>,
    pub exit_code: Option<i32>,
    /// SHA-256 of the artifact bytes exactly as observed. Stable across the
    /// blob store: it addresses the observation, not the stored copy.
    pub artifact_digest: String,
    /// Full redacted artifact text, resolved from the blob store when needed.
    pub artifact_text: String,
    /// Blob address of the redacted artifact when it exceeded the threshold.
    pub artifact_blob_digest: Option<String>,
    pub metadata: Value,
    pub observed_at_ms: i64,
}

/// Where one evidence artifact's redacted bytes are persisted.
enum ArtifactPayload {
    /// Small artifacts stay inline in the canonical row.
    Inline(String),
    /// Large artifacts live in the blob store behind a digest and head excerpt.
    Blob {
        digest: String,
        excerpt: String,
        size_bytes: usize,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PastFailure {
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub problem: String,
    pub strategy_summary: String,
    pub files: Vec<String>,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug)]
pub struct ExperienceStore {
    database: Database,
    redactor: Redactor,
    blobs: BlobStore,
    blob_threshold_bytes: usize,
}

impl ExperienceStore {
    #[must_use]
    pub fn new(database: Database) -> Self {
        Self::with_redactor(database, Redactor::default())
    }

    #[must_use]
    pub fn with_redactor(database: Database, redactor: Redactor) -> Self {
        Self {
            blobs: BlobStore::new(database.clone()),
            database,
            redactor,
            blob_threshold_bytes: DEFAULT_BLOB_THRESHOLD_BYTES,
        }
    }

    /// Route redacted artifacts larger than `bytes` into the blob store.
    #[must_use]
    pub fn with_blob_threshold(mut self, bytes: usize) -> Self {
        self.blob_threshold_bytes = bytes;
        self
    }

    /// Blob store backing this experience store's large artifacts.
    #[must_use]
    pub const fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    /// Start one bounded engineering episode.
    ///
    /// # Errors
    ///
    /// Returns an error if the prompt is empty, the project is unknown, or the
    /// event/projection transaction cannot commit.
    pub fn start_task(
        &self,
        project_id: &ProjectId,
        prompt: &str,
        session_id: Option<&str>,
    ) -> Result<TaskRecord> {
        let prompt = self.redactor.redact_text(prompt);
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err(Error::Config("task prompt cannot be empty".to_owned()));
        }
        let id = TaskId::new();
        let now = unix_time_ms();
        let prompt_digest = sha256(prompt.as_bytes());
        let event = DomainEvent::TaskStarted {
            project_id: project_id.clone(),
            task_id: id.clone(),
            session_id: session_id.map(str::to_owned),
            prompt_digest: prompt_digest.clone(),
        };
        self.database.write_immediate(|transaction| {
            transaction.execute(
                "INSERT INTO tasks
                 (id, project_id, session_id, prompt, prompt_digest, status, started_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6)",
                params![
                    id.as_str(),
                    project_id.as_str(),
                    session_id,
                    prompt,
                    prompt_digest,
                    now
                ],
            )?;
            append_generated_in_transaction(transaction, &event, now)
        })?;
        self.get_task(&id)?.ok_or_else(|| {
            Error::Config("task projection missing after successful commit".to_owned())
        })
    }

    /// Record a coherent attempt within an open task.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty strategy, closed/unknown task, invalid
    /// serialization, or transaction failure.
    pub fn record_attempt(
        &self,
        task_id: &TaskId,
        strategy_summary: &str,
        outcome: AttemptOutcome,
        files: &[String],
    ) -> Result<AttemptRecord> {
        let strategy_summary = self.redactor.redact_text(strategy_summary);
        let strategy_summary = strategy_summary.trim();
        if strategy_summary.is_empty() {
            return Err(Error::Config("attempt strategy cannot be empty".to_owned()));
        }
        let id = AttemptId::new();
        let now = unix_time_ms();
        let files_json = serde_json::to_string(files)?;
        let (project_id, ordinal) = self.database.write_immediate(|transaction| {
            let project_id = open_task_project(transaction, task_id)?;
            let ordinal = transaction.query_row(
                "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM attempts WHERE task_id = ?1",
                [task_id.as_str()],
                |row| row.get::<_, u32>(0),
            )?;
            transaction.execute(
                "INSERT INTO attempts
                 (id, task_id, ordinal, strategy_summary, outcome, files_json, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id.as_str(),
                    task_id.as_str(),
                    ordinal,
                    strategy_summary,
                    outcome.as_str(),
                    files_json,
                    now
                ],
            )?;
            let event = DomainEvent::AttemptRecorded {
                project_id: project_id.clone(),
                task_id: task_id.clone(),
                attempt_id: id.clone(),
                ordinal,
                outcome: outcome.as_str().to_owned(),
            };
            append_generated_in_transaction(transaction, &event, now)?;
            Ok((project_id, ordinal))
        })?;
        let _ = project_id;
        Ok(AttemptRecord {
            id,
            task_id: task_id.clone(),
            ordinal,
            strategy_summary: strategy_summary.to_owned(),
            outcome,
            files: files.to_vec(),
            created_at_ms: now,
        })
    }

    /// Record provenance-bearing evidence.
    ///
    /// The artifact is redacted before any byte is persisted. A small redacted
    /// artifact stays inline in the canonical row; anything larger than the
    /// configured threshold is written to the content-addressed blob store and
    /// the row keeps a digest reference plus a bounded head excerpt. Identical
    /// artifacts deduplicate onto one refcounted blob.
    ///
    /// `artifact_digest` keeps its original meaning: the SHA-256 of the
    /// observed artifact bytes, unaffected by where the text is stored.
    ///
    /// # Errors
    ///
    /// Returns an error for inconsistent task/attempt scope, an unwritable blob
    /// store, or failed storage.
    #[allow(clippy::too_many_arguments)]
    pub fn record_evidence(
        &self,
        project_id: &ProjectId,
        task_id: Option<&TaskId>,
        attempt_id: Option<&AttemptId>,
        kind: &str,
        strength: EvidenceStrength,
        commit_hash: Option<&str>,
        command: Option<&str>,
        exit_code: Option<i32>,
        artifact: &[u8],
        metadata: &Value,
    ) -> Result<EvidenceRecord> {
        let kind = kind.trim();
        validate_evidence_kind(kind, command, exit_code)?;
        let metadata = self.redactor.redact_json(metadata);
        let metadata_json = serde_json::to_string(&metadata)?;
        if metadata_json.len() > 64 * 1_024 {
            return Err(Error::Config(
                "evidence metadata exceeds 64 KiB canonical limit".to_owned(),
            ));
        }
        let id = EvidenceId::new();
        let now = unix_time_ms();
        let artifact_digest = sha256(artifact);
        let command = command.map(|command| self.redactor.redact_text(command));
        // Redaction happens before the bytes reach either SQLite or the disk.
        let artifact_text = self
            .redactor
            .redact_text(&String::from_utf8_lossy(artifact));
        let payload = self.stage_artifact(&artifact_text)?;
        let event = DomainEvent::EvidenceObserved {
            project_id: project_id.clone(),
            task_id: task_id.cloned(),
            evidence_id: id.clone(),
            kind: kind.to_owned(),
            strength: strength.as_str().to_owned(),
            artifact_digest: artifact_digest.clone(),
        };
        let committed = self.database.write_immediate(|transaction| {
            validate_evidence_scope(transaction, project_id, task_id, attempt_id)?;
            if let ArtifactPayload::Blob {
                digest, size_bytes, ..
            } = &payload
            {
                BlobStore::retain_in_transaction(
                    transaction,
                    digest,
                    *size_bytes,
                    ARTIFACT_CONTENT_TYPE,
                    now,
                )?;
            }
            transaction.execute(
                "INSERT INTO evidence
                 (id, project_id, task_id, attempt_id, kind, strength, commit_hash,
                  command, exit_code, artifact_digest, artifact_text, artifact_excerpt,
                  artifact_blob_digest, artifact_size_bytes, metadata_json, observed_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    id.as_str(),
                    project_id.as_str(),
                    task_id.map(TaskId::as_str),
                    attempt_id.map(AttemptId::as_str),
                    kind,
                    strength.as_str(),
                    commit_hash,
                    command.as_deref(),
                    exit_code,
                    artifact_digest,
                    payload.inline_text(),
                    payload.excerpt(),
                    payload.blob_digest(),
                    i64::try_from(artifact_text.len()).unwrap_or(i64::MAX),
                    metadata_json,
                    now
                ],
            )?;
            append_generated_in_transaction(transaction, &event, now)
        });
        if committed.is_err() {
            self.discard_unreferenced_blob(&payload);
        }
        committed?;
        Ok(EvidenceRecord {
            id,
            project_id: project_id.clone(),
            task_id: task_id.cloned(),
            attempt_id: attempt_id.cloned(),
            kind: kind.to_owned(),
            strength,
            commit_hash: commit_hash.map(str::to_owned),
            command,
            exit_code,
            artifact_digest,
            artifact_text,
            artifact_blob_digest: payload.blob_digest().map(str::to_owned),
            metadata,
            observed_at_ms: now,
        })
    }

    /// Fetch one evidence record with its artifact text fully resolved.
    ///
    /// Blob-backed artifacts are read from the blob store transparently, so
    /// callers never see the storage decision.
    ///
    /// # Errors
    ///
    /// Returns an error for corrupt persisted IDs, storage failures, or a blob
    /// whose backing file is missing or corrupt.
    pub fn get_evidence(&self, id: &EvidenceId) -> Result<Option<EvidenceRecord>> {
        let Some(row) = self.database.with_reader(|connection| {
            connection
                .query_row(
                    "SELECT id, project_id, task_id, attempt_id, kind, strength, commit_hash,
                            command, exit_code, artifact_digest, artifact_text,
                            artifact_blob_digest, metadata_json, observed_at_ms
                     FROM evidence WHERE id = ?1",
                    [id.as_str()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, Option<String>>(6)?,
                            row.get::<_, Option<String>>(7)?,
                            row.get::<_, Option<i32>>(8)?,
                            row.get::<_, String>(9)?,
                            row.get::<_, Option<String>>(10)?,
                            row.get::<_, Option<String>>(11)?,
                            row.get::<_, String>(12)?,
                            row.get::<_, i64>(13)?,
                        ))
                    },
                )
                .optional()
                .map_err(Error::from)
        })?
        else {
            return Ok(None);
        };
        let artifact_text = match &row.11 {
            Some(digest) => self.blobs.read_text(digest)?,
            None => row.10.clone().unwrap_or_default(),
        };
        Ok(Some(EvidenceRecord {
            id: EvidenceId::parse(&row.0)?,
            project_id: ProjectId::parse(&row.1)?,
            task_id: row.2.as_deref().map(TaskId::parse).transpose()?,
            attempt_id: row.3.as_deref().map(AttemptId::parse).transpose()?,
            kind: row.4,
            strength: parse_strength(&row.5)?,
            commit_hash: row.6,
            command: row.7,
            exit_code: row.8,
            artifact_digest: row.9,
            artifact_text,
            artifact_blob_digest: row.11,
            metadata: serde_json::from_str(&row.12)?,
            observed_at_ms: row.13,
        }))
    }

    /// Irreversibly remove one evidence record and its stored artifact.
    ///
    /// Retries are safe: purging already-purged evidence succeeds without
    /// touching canonical state, and a blob file that is already gone is not an
    /// error. The blob file is unlinked only after the transaction commits.
    ///
    /// # Errors
    ///
    /// Returns an error for a failed canonical transaction or an unlink failure
    /// other than a missing file.
    pub fn purge_evidence(&self, id: &EvidenceId) -> Result<()> {
        let orphaned = self.database.write_immediate(|transaction| {
            let stored: Option<Option<String>> = transaction
                .query_row(
                    "SELECT artifact_blob_digest FROM evidence WHERE id = ?1",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(blob_digest) = stored else {
                // Already purged: idempotent success, never an error.
                return Ok(Vec::new());
            };
            transaction.execute("DELETE FROM evidence WHERE id = ?1", [id.as_str()])?;
            let mut orphaned = Vec::new();
            if let Some(digest) = blob_digest
                && BlobStore::release_in_transaction(transaction, &digest)?.removes_file()
            {
                orphaned.push(digest);
            }
            Ok(orphaned)
        })?;
        for digest in orphaned {
            self.blobs.remove_file(&digest)?;
        }
        Ok(())
    }

    /// Drop bytes staged for a write that never committed.
    ///
    /// Only a digest that no metadata row claims is removed, so a blob that
    /// deduplicated onto an earlier reference is never touched.
    fn discard_unreferenced_blob(&self, payload: &ArtifactPayload) {
        if let ArtifactPayload::Blob { digest, .. } = payload
            && matches!(self.blobs.refcount(digest), Ok(None))
        {
            let _ = self.blobs.remove_file(digest);
        }
    }

    /// Decide where a redacted artifact lives and materialize blob bytes.
    fn stage_artifact(&self, artifact_text: &str) -> Result<ArtifactPayload> {
        if artifact_text.len() <= self.blob_threshold_bytes {
            return Ok(ArtifactPayload::Inline(artifact_text.to_owned()));
        }
        let digest = self.blobs.put(artifact_text.as_bytes())?;
        Ok(ArtifactPayload::Blob {
            digest,
            // The excerpt never makes the row larger than an inline artifact.
            excerpt: head_excerpt(
                artifact_text,
                ARTIFACT_EXCERPT_BYTES.min(self.blob_threshold_bytes),
            ),
            size_bytes: artifact_text.len(),
        })
    }

    /// Finish an open task. Unknown completion is deliberately unsupported;
    /// leave the task open until a real signal arrives.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown/already-closed task or failed commit.
    pub fn finish_task(
        &self,
        task_id: &TaskId,
        outcome: TaskOutcome,
        accepted_by_user: Option<bool>,
        final_commit: Option<&str>,
    ) -> Result<TaskRecord> {
        let now = unix_time_ms();
        self.database.write_immediate(|transaction| {
            let project_id = open_task_project(transaction, task_id)?;
            transaction.execute(
                "UPDATE tasks
                 SET status = ?2, accepted_by_user = ?3, completed_at_ms = ?4, final_commit = ?5
                 WHERE id = ?1",
                params![
                    task_id.as_str(),
                    outcome.as_str(),
                    accepted_by_user.map(i64::from),
                    now,
                    final_commit
                ],
            )?;
            let event = DomainEvent::TaskFinished {
                project_id,
                task_id: task_id.clone(),
                outcome: outcome.as_str().to_owned(),
                accepted_by_user,
                final_commit: final_commit.map(str::to_owned),
            };
            append_generated_in_transaction(transaction, &event, now)
        })?;
        self.get_task(task_id)?.ok_or_else(|| {
            Error::Config("task projection missing after successful finish".to_owned())
        })
    }

    /// Fetch one task by typed ID.
    ///
    /// # Errors
    ///
    /// Returns an error for corrupt persisted IDs or storage failures.
    pub fn get_task(&self, id: &TaskId) -> Result<Option<TaskRecord>> {
        self.database.with_reader(|connection| {
            let raw = connection
                .query_row(
                    "SELECT id, project_id, session_id, prompt, status, accepted_by_user,
                            started_at_ms, completed_at_ms, final_commit
                     FROM tasks WHERE id = ?1",
                    [id.as_str()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, Option<i64>>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, Option<i64>>(7)?,
                            row.get::<_, Option<String>>(8)?,
                        ))
                    },
                )
                .optional()?;
            raw.map(|raw| {
                Ok(TaskRecord {
                    id: TaskId::parse(&raw.0)?,
                    project_id: ProjectId::parse(&raw.1)?,
                    session_id: raw.2,
                    prompt: raw.3,
                    status: raw.4,
                    accepted_by_user: raw.5.map(|value| value != 0),
                    started_at_ms: raw.6,
                    completed_at_ms: raw.7,
                    final_commit: raw.8,
                })
            })
            .transpose()
        })
    }

    /// Query bounded demonstrated failed attempts for explicit historical recall.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid bounds or corrupt/storage state.
    pub fn past_failures(
        &self,
        project_id: &ProjectId,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PastFailure>> {
        if limit == 0 || limit > 1_000 {
            return Err(Error::Config(
                "past failure limit must be between 1 and 1000".to_owned(),
            ));
        }
        let pattern = query
            .filter(|value| !value.trim().is_empty())
            .map(|value| format!("%{}%", value.replace('%', "\\%").replace('_', "\\_")));
        self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT t.id, a.id, t.prompt, a.strategy_summary, a.files_json, a.created_at_ms
                 FROM attempts a JOIN tasks t ON t.id = a.task_id
                 WHERE t.project_id = ?1 AND a.outcome = 'failure'
                   AND (?2 IS NULL OR t.prompt LIKE ?2 ESCAPE '\\'
                                    OR a.strategy_summary LIKE ?2 ESCAPE '\\')
                 ORDER BY a.created_at_ms DESC, a.id LIMIT ?3",
            )?;
            let rows = statement.query_map(
                params![
                    project_id.as_str(),
                    pattern,
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )?;
            rows.map(|row| {
                let row = row?;
                Ok(PastFailure {
                    task_id: TaskId::parse(&row.0)?,
                    attempt_id: AttemptId::parse(&row.1)?,
                    problem: row.2,
                    strategy_summary: row.3,
                    files: serde_json::from_str(&row.4)?,
                    created_at_ms: row.5,
                })
            })
            .collect()
        })
    }
}

impl ArtifactPayload {
    /// Inline artifact text, or `None` when the bytes live in a blob.
    const fn inline_text(&self) -> Option<&String> {
        match self {
            Self::Inline(text) => Some(text),
            Self::Blob { .. } => None,
        }
    }

    /// Bounded head excerpt kept beside a blob reference.
    const fn excerpt(&self) -> Option<&String> {
        match self {
            Self::Inline(_) => None,
            Self::Blob { excerpt, .. } => Some(excerpt),
        }
    }

    /// Blob address of the stored artifact, when it was offloaded.
    fn blob_digest(&self) -> Option<&str> {
        match self {
            Self::Inline(_) => None,
            Self::Blob { digest, .. } => Some(digest.as_str()),
        }
    }
}

/// Bounded head of an artifact, truncated on a UTF-8 character boundary.
fn head_excerpt(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Parse a persisted evidence strength back into its typed form.
fn parse_strength(value: &str) -> Result<EvidenceStrength> {
    match value {
        "deterministic" => Ok(EvidenceStrength::Deterministic),
        "observational" => Ok(EvidenceStrength::Observational),
        "inferred" => Ok(EvidenceStrength::Inferred),
        other => Err(Error::Config(format!(
            "unknown persisted evidence strength `{other}`"
        ))),
    }
}

/// Reject malformed evidence kinds and command results missing provenance.
fn validate_evidence_kind(kind: &str, command: Option<&str>, exit_code: Option<i32>) -> Result<()> {
    if kind.is_empty()
        || kind.len() > 64
        || !kind
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(Error::Config(
            "evidence kind must be 1..=64 lowercase ASCII letters, digits, or underscores"
                .to_owned(),
        ));
    }
    if is_command_result_kind(kind)
        && (exit_code.is_none() || command.is_none_or(|value| value.trim().is_empty()))
    {
        return Err(Error::Config(format!(
            "{kind} evidence requires both command and exit_code"
        )));
    }
    Ok(())
}

fn is_command_result_kind(kind: &str) -> bool {
    matches!(
        kind,
        "test_result" | "build_result" | "lint_result" | "typecheck_result" | "benchmark_result"
    )
}

fn open_task_project(
    transaction: &rusqlite::Transaction<'_>,
    task_id: &TaskId,
) -> Result<ProjectId> {
    let project = transaction
        .query_row(
            "SELECT project_id FROM tasks WHERE id = ?1 AND status = 'open'",
            [task_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| Error::Config(format!("task `{task_id}` is unknown or already closed")))?;
    ProjectId::parse(&project)
}

fn validate_evidence_scope(
    transaction: &rusqlite::Transaction<'_>,
    project_id: &ProjectId,
    task_id: Option<&TaskId>,
    attempt_id: Option<&AttemptId>,
) -> Result<()> {
    if let Some(task_id) = task_id {
        let stored_project = transaction
            .query_row(
                "SELECT project_id FROM tasks WHERE id = ?1",
                [task_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| Error::Config(format!("unknown task `{task_id}`")))?;
        if stored_project != project_id.as_str() {
            return Err(Error::Config(
                "evidence task belongs to another project".to_owned(),
            ));
        }
    }
    if let Some(attempt_id) = attempt_id {
        let stored_task = transaction
            .query_row(
                "SELECT task_id FROM attempts WHERE id = ?1",
                [attempt_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| Error::Config(format!("unknown attempt `{attempt_id}`")))?;
        if task_id.map(TaskId::as_str) != Some(stored_task.as_str()) {
            return Err(Error::Config(
                "evidence attempt does not belong to the supplied task".to_owned(),
            ));
        }
    }
    Ok(())
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
    use crate::projects::ProjectRegistry;

    fn store() -> (TempDir, ExperienceStore, ProjectId) {
        let temp = TempDir::new().expect("temp");
        let root = temp.path().join("project");
        std::fs::create_dir(&root).expect("project");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("register")
            .project
            .id;
        (temp, ExperienceStore::new(database), project)
    }

    #[test]
    fn episode_records_attempt_test_evidence_and_strong_completion() {
        let (_temp, store, project) = store();
        let task = store
            .start_task(&project, "fix cleanup", Some("session-1"))
            .expect("task");
        let attempt = store
            .record_attempt(
                &task.id,
                "add a finally path",
                AttemptOutcome::Success,
                &["src/lib.rs".to_owned()],
            )
            .expect("attempt");
        let evidence = store
            .record_evidence(
                &project,
                Some(&task.id),
                Some(&attempt.id),
                "test_result",
                EvidenceStrength::Deterministic,
                Some("abc"),
                Some("cargo test"),
                Some(0),
                b"test result bytes",
                &serde_json::json!({"passed": 4, "failed": 0}),
            )
            .expect("evidence");
        assert_eq!(evidence.strength, EvidenceStrength::Deterministic);
        let finished = store
            .finish_task(&task.id, TaskOutcome::Success, Some(true), Some("def"))
            .expect("finish");
        assert_eq!(finished.status, "success");
        assert_eq!(finished.accepted_by_user, Some(true));
    }

    fn log_evidence(
        store: &ExperienceStore,
        project: &ProjectId,
        artifact: &[u8],
    ) -> EvidenceRecord {
        store
            .record_evidence(
                project,
                None,
                None,
                "log",
                EvidenceStrength::Observational,
                None,
                None,
                None,
                artifact,
                &serde_json::json!({}),
            )
            .expect("evidence")
    }

    fn stored_artifact_columns(
        store: &ExperienceStore,
        id: &EvidenceId,
    ) -> (Option<String>, Option<String>, Option<String>, Option<i64>) {
        store
            .database
            .with_reader(|connection| {
                connection
                    .query_row(
                        "SELECT artifact_text, artifact_excerpt, artifact_blob_digest,
                                artifact_size_bytes
                         FROM evidence WHERE id = ?1",
                        [id.as_str()],
                        |row| {
                            Ok((
                                row.get::<_, Option<String>>(0)?,
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, Option<String>>(2)?,
                                row.get::<_, Option<i64>>(3)?,
                            ))
                        },
                    )
                    .map_err(Error::from)
            })
            .expect("artifact columns")
    }

    #[test]
    fn small_artifacts_stay_inline_and_large_artifacts_move_to_the_blob_store() {
        let (_temp, store, project) = store();
        let store = store.with_blob_threshold(64);

        let small = log_evidence(&store, &project, b"small artifact");
        assert!(small.artifact_blob_digest.is_none());
        assert_eq!(small.artifact_text, "small artifact");
        let (inline, excerpt, blob_digest, size) = stored_artifact_columns(&store, &small.id);
        assert_eq!(inline.as_deref(), Some("small artifact"));
        assert_eq!(excerpt, None);
        assert_eq!(blob_digest, None);
        assert_eq!(size, Some(14));

        let large_text = "large artifact line\n".repeat(64);
        let large = log_evidence(&store, &project, large_text.as_bytes());
        let digest = large.artifact_blob_digest.clone().expect("blob digest");
        assert!(
            store.blobs().path_for(&digest).expect("path").exists(),
            "blob file must exist on disk"
        );
        let (inline, excerpt, blob_digest, size) = stored_artifact_columns(&store, &large.id);
        assert_eq!(inline, None, "large artifacts must not bloat the row");
        let excerpt = excerpt.expect("excerpt");
        assert_eq!(
            excerpt.len(),
            64,
            "the excerpt stays bounded by the threshold"
        );
        assert!(large.artifact_text.starts_with(&excerpt));
        assert_eq!(blob_digest.as_deref(), Some(digest.as_str()));
        assert_eq!(size, Some(i64::try_from(large_text.len()).expect("size")));
        // `artifact_digest` still addresses the observed bytes. It coincides
        // with the blob address only because redaction changed nothing here.
        assert_eq!(large.artifact_digest, sha256(large_text.as_bytes()));
        assert_eq!(large.artifact_digest, digest);
    }

    #[test]
    fn blob_backed_artifacts_read_back_transparently_and_are_redacted_on_disk() {
        let (_temp, store, project) = store();
        let store = store.with_blob_threshold(64);
        let artifact = format!("{}\ntoken=hunter2\n", "audit trail line\n".repeat(64));

        let recorded = log_evidence(&store, &project, artifact.as_bytes());
        let digest = recorded.artifact_blob_digest.clone().expect("blob digest");
        let fetched = store
            .get_evidence(&recorded.id)
            .expect("fetch")
            .expect("evidence exists");
        assert_eq!(fetched.artifact_text, recorded.artifact_text);
        assert_eq!(
            fetched.artifact_blob_digest.as_deref(),
            Some(digest.as_str())
        );

        // The observed-artifact digest is unchanged by redaction, so it and the
        // blob address of the redacted bytes are deliberately different values.
        assert_eq!(recorded.artifact_digest, sha256(artifact.as_bytes()));
        assert_ne!(recorded.artifact_digest, digest);

        let on_disk =
            std::fs::read_to_string(store.blobs().path_for(&digest).expect("path")).expect("read");
        assert_eq!(on_disk, fetched.artifact_text);
        assert!(
            !on_disk.contains("hunter2"),
            "redaction must happen before bytes reach the disk"
        );
        assert!(on_disk.contains("[REDACTED]"));

        let inline = log_evidence(&store, &project, b"inline artifact");
        assert_eq!(
            store
                .get_evidence(&inline.id)
                .expect("fetch")
                .expect("evidence exists")
                .artifact_text,
            "inline artifact"
        );
        assert!(
            store
                .get_evidence(&EvidenceId::new())
                .expect("miss")
                .is_none()
        );
    }

    #[test]
    fn identical_artifacts_share_one_refcounted_blob_until_purge_empties_it() {
        let (_temp, store, project) = store();
        let store = store.with_blob_threshold(64);
        let artifact = "shared artifact line\n".repeat(64);

        let first = log_evidence(&store, &project, artifact.as_bytes());
        let second = log_evidence(&store, &project, artifact.as_bytes());
        let digest = first.artifact_blob_digest.clone().expect("blob digest");
        assert_eq!(
            second.artifact_blob_digest.as_deref(),
            Some(digest.as_str())
        );
        assert_eq!(store.blobs().refcount(&digest).expect("refcount"), Some(2));
        let path = store.blobs().path_for(&digest).expect("path");
        assert!(path.exists());

        store.purge_evidence(&first.id).expect("purge first");
        assert_eq!(store.blobs().refcount(&digest).expect("refcount"), Some(1));
        assert!(path.exists(), "a live reference must keep the file");
        assert!(store.get_evidence(&first.id).expect("fetch").is_none());
        assert_eq!(
            store
                .get_evidence(&second.id)
                .expect("fetch")
                .expect("evidence exists")
                .artifact_text,
            artifact
        );

        store.purge_evidence(&second.id).expect("purge second");
        assert_eq!(store.blobs().refcount(&digest).expect("refcount"), None);
        assert!(!path.exists(), "zero refcount must delete the file");
        assert_eq!(store.blobs().health().expect("health").blob_count, 0);

        // Purge retries must never error, with or without surviving state.
        store.purge_evidence(&first.id).expect("idempotent purge");
        store.purge_evidence(&second.id).expect("idempotent purge");
        assert!(!path.exists());
    }

    #[test]
    fn a_rejected_evidence_write_leaves_no_orphan_blob() {
        let (temp, store, project) = store();
        let store = store.with_blob_threshold(64);
        let other_root = temp.path().join("other");
        std::fs::create_dir(&other_root).expect("other");
        let other = ProjectRegistry::new(store.database.clone())
            .resolve_or_register(&other_root)
            .expect("other project")
            .project
            .id;
        let task = store.start_task(&project, "work", None).expect("task");
        let artifact = "rejected artifact line\n".repeat(64);

        let error = store
            .record_evidence(
                &other,
                Some(&task.id),
                None,
                "log",
                EvidenceStrength::Observational,
                None,
                None,
                None,
                artifact.as_bytes(),
                &serde_json::json!({}),
            )
            .expect_err("scope mismatch");
        assert!(error.to_string().contains("another project"));
        let health = store.blobs().health().expect("health");
        assert_eq!((health.blob_count, health.orphan_files), (0, 0));
    }

    #[test]
    fn unknown_outcomes_do_not_close_tasks_and_cross_scope_evidence_is_rejected() {
        let (temp, store, project) = store();
        let other_root = temp.path().join("other");
        std::fs::create_dir(&other_root).expect("other");
        let other = ProjectRegistry::new(store.database.clone())
            .resolve_or_register(&other_root)
            .expect("other project")
            .project
            .id;
        let task = store.start_task(&project, "work", None).expect("task");
        let error = store
            .record_evidence(
                &other,
                Some(&task.id),
                None,
                "test_result",
                EvidenceStrength::Deterministic,
                None,
                Some("cargo test"),
                Some(0),
                b"x",
                &serde_json::json!({}),
            )
            .expect_err("scope mismatch");
        assert!(error.to_string().contains("another project"));
        assert_eq!(
            store.get_task(&task.id).expect("get").expect("task").status,
            "open"
        );
    }
}
