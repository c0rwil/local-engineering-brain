//! Durable, bounded background jobs over the canonical `jobs` table.
//!
//! The daemon is the only executor. A single tokio task polls the queue on a
//! fixed interval, claims due jobs through the canonical mutation scheduler at
//! background priority, and executes at most `daemon.background_concurrency`
//! jobs concurrently under the daemon's existing background-slot semaphore.
//! Under hard RSS pressure no new job starts (spec section 24.8); jobs that
//! crash mid-run are recovered on the next daemon start by resetting every
//! `running` row to `pending` (spec section 27.5), which is safe precisely
//! because no second executor exists.
//!
//! Lifecycle: `pending -> running -> succeeded`, or `running -> failed` with
//! bounded retries and exponential backoff recorded in the row
//! (`attempts`, `last_error`, `not_before_ms`), and finally `dead` once
//! `max_attempts` is exhausted. Enqueue is idempotent by
//! `(job_type, dedupe_key)`.

use std::sync::Arc;
use std::time::Duration;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;

use crate::blobs::BlobStore;
use crate::config::JobsConfig;
use crate::error::{Error, Result};
use crate::ids::{JobId, MemoryId, ProjectId};
use crate::memory::{MemoryStatus, MemoryStore};
use crate::metrics::MetricsRegistry;
use crate::scheduler::{MutationPriority, MutationScheduler};
use crate::security::Redactor;
use crate::semantic::LocalEmbedderSettings;
use crate::storage::Database;

/// Base delay before the first retry; each further attempt doubles it.
const RETRY_BASE_BACKOFF_MS: i64 = 30_000;
/// Retry backoff never exceeds one hour.
const RETRY_MAX_BACKOFF_MS: i64 = 3_600_000;
/// Rows deleted per garbage-collection transaction.
const GC_BATCH_ROWS: usize = 500;
/// Bounded batches per `consolidation_gc` run; remaining rows wait for the
/// next daily run so one job can never monopolize the canonical writer.
const GC_MAX_BATCHES: usize = 20;
/// Orphaned blob files younger than this are skipped: a concurrent evidence
/// write materializes the file before the referencing transaction commits.
const ORPHAN_BLOB_GRACE_MS: u64 = 3_600_000;
/// Orphaned blob files removed per `consolidation_gc` run.
const ORPHAN_BLOB_LIMIT: usize = 1_000;
/// Evidence rows examined per `evidence_reverify` run.
const REVERIFY_EVIDENCE_LIMIT: usize = 500;

/// Allowlisted background job types. Anything else is rejected at enqueue.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobType {
    /// Run the FTS/database integrity check and record the report.
    FtsIntegrity,
    /// Rebuild the derived semantic vector sidecar for one project.
    SemanticRebuild,
    /// Bounded retention GC over `context_runs`, orphaned blob files, stale
    /// hook-session files, and long-open hook tasks (abandoned after 7 days).
    ConsolidationGc,
    /// Staleness sweep over stored evidence. Evidence rows that carry a stored
    /// command are **never re-executed automatically**: commands recorded from
    /// repository work are untrusted repository-supplied content, and running
    /// them from a background daemon would let a checked-out repo execute
    /// arbitrary code without a human in the loop (spec section 26.4). The
    /// sweep instead marks memories suspect when their supporting evidence
    /// references a commit no longer known to the project or when their linked
    /// symbols drifted, so dependent memories get re-checked explicitly.
    EvidenceReverify,
    /// Re-export the derived Obsidian vault for one project (plus the shared
    /// workspace overview). Read-only against the canonical database; writes
    /// only Markdown under the configured vault root.
    VaultExport,
    /// Transactionally consistent daily brain backup with bounded retention.
    BackupDaily,
}

impl JobType {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FtsIntegrity => "fts_integrity",
            Self::SemanticRebuild => "semantic_rebuild",
            Self::ConsolidationGc => "consolidation_gc",
            Self::EvidenceReverify => "evidence_reverify",
            Self::VaultExport => "vault_export",
            Self::BackupDaily => "backup_daily",
        }
    }

    /// Parse an allowlisted job type name.
    ///
    /// # Errors
    ///
    /// Returns an error for any name outside the fixed V1 allowlist.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "fts_integrity" => Ok(Self::FtsIntegrity),
            "semantic_rebuild" => Ok(Self::SemanticRebuild),
            "consolidation_gc" => Ok(Self::ConsolidationGc),
            "evidence_reverify" => Ok(Self::EvidenceReverify),
            "vault_export" => Ok(Self::VaultExport),
            "backup_daily" => Ok(Self::BackupDaily),
            other => Err(Error::Config(format!(
                "unsupported job type `{other}`; expected one of fts_integrity, \
                 semantic_rebuild, consolidation_gc, evidence_reverify, vault_export"
            ))),
        }
    }

    /// Queue priority derived from the spec section 24.8 ordering; a larger
    /// number is claimed sooner.
    #[must_use]
    pub const fn priority(self) -> i64 {
        match self {
            Self::EvidenceReverify => 40,
            Self::FtsIntegrity => 30,
            Self::SemanticRebuild => 20,
            Self::ConsolidationGc => 10,
            // Cosmetic derived output; always yields to correctness work.
            Self::VaultExport => 5,
            // Durability first: a daily backup outranks derived output.
            Self::BackupDaily => 35,
        }
    }

    /// Whether this job type requires an explicit project scope.
    #[must_use]
    pub const fn requires_project(self) -> bool {
        matches!(
            self,
            Self::SemanticRebuild | Self::EvidenceReverify | Self::VaultExport
        )
    }
}

/// Durable job states.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Dead,
}

impl JobStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Dead => "dead",
        }
    }

    /// Parse a stored status value.
    ///
    /// # Errors
    ///
    /// Returns an error for a value outside the fixed status vocabulary.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "dead" => Ok(Self::Dead),
            other => Err(Error::Config(format!("unknown job status `{other}`"))),
        }
    }
}

/// One durable queue row.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JobRecord {
    pub id: JobId,
    pub project_id: Option<ProjectId>,
    pub job_type: JobType,
    pub priority: i64,
    pub status: JobStatus,
    pub payload: serde_json::Value,
    pub dedupe_key: String,
    pub attempts: u32,
    pub max_attempts: u32,
    pub last_error: Option<String>,
    pub result: Option<serde_json::Value>,
    pub not_before_ms: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub completed_at_ms: Option<i64>,
}

/// Idempotent enqueue request.
#[derive(Clone, Debug)]
pub struct EnqueueJob {
    pub job_type: JobType,
    pub project_id: Option<ProjectId>,
    pub payload: serde_json::Value,
    pub dedupe_key: String,
    pub max_attempts: u32,
}

/// Whether a new background job may start right now. Hard RSS pressure sheds
/// new job starts entirely; already-running jobs finish (spec section 24.8).
/// An unreadable RSS admits work, matching the daemon's admission behavior.
#[must_use]
pub const fn can_start_job(rss_bytes: Option<u64>, rss_hard_limit_bytes: u64) -> bool {
    match rss_bytes {
        Some(rss) => rss < rss_hard_limit_bytes,
        None => true,
    }
}

/// Exponential retry backoff recorded into `not_before_ms`, capped at one
/// hour. `attempts` is the number of attempts already consumed (>= 1).
#[must_use]
pub fn retry_backoff_ms(attempts: u32) -> i64 {
    let exponent = attempts.saturating_sub(1).min(30);
    RETRY_BASE_BACKOFF_MS
        .saturating_mul(1_i64 << exponent)
        .min(RETRY_MAX_BACKOFF_MS)
}

/// Durable job queue over the canonical `jobs` table.
///
/// Methods take explicit timestamps so lifecycle behavior stays deterministic
/// under test. Inside the daemon every mutating call is submitted through the
/// canonical mutation scheduler; nothing here opens a second ad-hoc writer.
#[derive(Clone, Debug)]
pub struct JobQueue {
    database: Database,
}

impl JobQueue {
    #[must_use]
    pub const fn new(database: Database) -> Self {
        Self { database }
    }

    /// Idempotently enqueue one job, due immediately. An existing row with the
    /// same `(job_type, dedupe_key)` is returned unchanged, whatever its status.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid limits, a missing required project scope,
    /// or storage failure.
    pub fn enqueue(&self, request: &EnqueueJob, now_ms: i64) -> Result<JobRecord> {
        self.enqueue_due(request, now_ms, now_ms)
    }

    /// Request the automatic vault refresh after a graph change: one
    /// `vault_export` per project per `refresh_interval_secs` window. The job is
    /// due when its window closes, so one export covers every change made in
    /// the window. Each window has its own dedupe key; a succeeded or dead
    /// export never blocks a later window's refresh.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero interval, invalid limits or storage failure.
    pub fn enqueue_vault_refresh(
        &self,
        project_id: &ProjectId,
        refresh_interval_secs: u64,
        max_attempts: u32,
        now_ms: i64,
    ) -> Result<JobRecord> {
        let window_ms = i64::try_from(refresh_interval_secs.saturating_mul(1_000))
            .map_err(|_| Error::Config("vault refresh interval is too long".to_owned()))?;
        if window_ms == 0 {
            return Err(Error::Config(
                "vault refresh interval must be positive".to_owned(),
            ));
        }
        let window = now_ms.div_euclid(window_ms);
        let request = EnqueueJob {
            job_type: JobType::VaultExport,
            project_id: Some(project_id.clone()),
            payload: serde_json::json!({}),
            dedupe_key: format!("vault_export:{project_id}:{refresh_interval_secs}s-{window}"),
            max_attempts,
        };
        let window_end = window.saturating_add(1).saturating_mul(window_ms);
        self.enqueue_due(&request, now_ms, window_end)
    }

    /// [`Self::enqueue`] with a first-run time; a duplicate keeps its own.
    fn enqueue_due(
        &self,
        request: &EnqueueJob,
        now_ms: i64,
        not_before_ms: i64,
    ) -> Result<JobRecord> {
        if request.max_attempts == 0 {
            return Err(Error::Config(
                "jobs require at least one attempt".to_owned(),
            ));
        }
        if request.job_type.requires_project() && request.project_id.is_none() {
            return Err(Error::Config(format!(
                "job type `{}` requires a project scope",
                request.job_type.as_str()
            )));
        }
        if request.dedupe_key.is_empty() || request.dedupe_key.len() > 256 {
            return Err(Error::Config(
                "job dedupe keys must be 1..=256 bytes".to_owned(),
            ));
        }
        let id = JobId::new();
        let payload = serde_json::to_string(&request.payload)?;
        self.database.write_immediate(|transaction| {
            transaction.execute(
                "INSERT INTO jobs(id, project_id, job_type, priority, status, payload_json,
                                  dedupe_key, attempts, max_attempts, last_error, result_json,
                                  not_before_ms, created_at_ms, updated_at_ms, completed_at_ms)
                 VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, 0, ?7, NULL, NULL, ?9, ?8, ?8, NULL)
                 ON CONFLICT(job_type, dedupe_key) DO NOTHING",
                params![
                    id.as_str(),
                    request.project_id.as_ref().map(ProjectId::as_str),
                    request.job_type.as_str(),
                    request.job_type.priority(),
                    payload,
                    request.dedupe_key,
                    i64::from(request.max_attempts),
                    now_ms,
                    not_before_ms,
                ],
            )?;
            Ok(())
        })?;
        self.find_by_dedupe(request.job_type, &request.dedupe_key)?
            .ok_or_else(|| Error::Config("job row missing directly after enqueue".to_owned()))
    }

    /// Claim the highest-priority due job, marking it `running` and consuming
    /// one attempt. Both fresh `pending` jobs and `failed` jobs whose recorded
    /// backoff has elapsed are claimable.
    ///
    /// # Errors
    ///
    /// Returns an error on storage failure.
    pub fn claim_due(&self, now_ms: i64) -> Result<Option<JobRecord>> {
        let claimed = self.database.write_immediate(|transaction| {
            let candidate: Option<String> = transaction
                .query_row(
                    "SELECT id FROM jobs
                     WHERE status IN ('pending','failed') AND not_before_ms <= ?1
                     ORDER BY priority DESC, not_before_ms, id LIMIT 1",
                    [now_ms],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(id) = candidate else {
                return Ok(None);
            };
            transaction.execute(
                "UPDATE jobs SET status = 'running', attempts = attempts + 1,
                     updated_at_ms = ?2
                 WHERE id = ?1",
                params![id, now_ms],
            )?;
            Ok(Some(id))
        })?;
        match claimed {
            Some(id) => self.get(&JobId::parse(&id)?),
            None => Ok(None),
        }
    }

    /// Record a successful run and its result.
    ///
    /// # Errors
    ///
    /// Returns an error on storage failure.
    pub fn record_success(
        &self,
        id: &JobId,
        result: &serde_json::Value,
        now_ms: i64,
    ) -> Result<()> {
        let result = serde_json::to_string(result)?;
        self.database.write_immediate(|transaction| {
            transaction.execute(
                "UPDATE jobs SET status = 'succeeded', result_json = ?2, last_error = NULL,
                     updated_at_ms = ?3, completed_at_ms = ?3
                 WHERE id = ?1 AND status = 'running'",
                params![id.as_str(), result, now_ms],
            )?;
            Ok(())
        })
    }

    /// Record a failed run. With attempts remaining the job becomes `failed`
    /// with an exponential backoff recorded in `not_before_ms`; once
    /// `max_attempts` is exhausted it becomes `dead`.
    ///
    /// # Errors
    ///
    /// Returns an error on storage failure.
    pub fn record_failure(&self, id: &JobId, error: &str, now_ms: i64) -> Result<()> {
        let error = truncate_error(error);
        self.database.write_immediate(|transaction| {
            let row: Option<(i64, i64)> = transaction
                .query_row(
                    "SELECT attempts, max_attempts FROM jobs
                     WHERE id = ?1 AND status = 'running'",
                    [id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((attempts, max_attempts)) = row else {
                return Ok(());
            };
            if attempts >= max_attempts {
                transaction.execute(
                    "UPDATE jobs SET status = 'dead', last_error = ?2,
                         updated_at_ms = ?3, completed_at_ms = ?3
                     WHERE id = ?1",
                    params![id.as_str(), error, now_ms],
                )?;
            } else {
                let backoff = retry_backoff_ms(u32::try_from(attempts).unwrap_or(u32::MAX));
                transaction.execute(
                    "UPDATE jobs SET status = 'failed', last_error = ?2,
                         not_before_ms = ?3, updated_at_ms = ?4
                     WHERE id = ?1",
                    params![id.as_str(), error, now_ms.saturating_add(backoff), now_ms],
                )?;
            }
            Ok(())
        })
    }

    /// Reset every `running` job to `pending` after a daemon restart.
    ///
    /// The daemon is the only executor, so a `running` row observed at startup
    /// can only mean the previous process crashed mid-job (spec section 27.5).
    /// The consumed attempt is kept, so a job that repeatedly crashes the
    /// executor still converges to `dead`.
    ///
    /// # Errors
    ///
    /// Returns an error on storage failure.
    pub fn recover_running(&self, now_ms: i64) -> Result<u64> {
        self.database.write_immediate(|transaction| {
            let changed = transaction.execute(
                "UPDATE jobs SET status = 'pending', updated_at_ms = ?1
                 WHERE status = 'running'",
                [now_ms],
            )?;
            u64::try_from(changed)
                .map_err(|_| Error::Config("recovered job count exceeds u64".to_owned()))
        })
    }

    /// Fetch one job by ID.
    ///
    /// # Errors
    ///
    /// Returns an error on storage failure or a malformed stored row.
    pub fn get(&self, id: &JobId) -> Result<Option<JobRecord>> {
        self.database
            .with_reader(|connection| {
                connection
                    .query_row(
                        &format!("{JOB_SELECT} WHERE id = ?1"),
                        [id.as_str()],
                        row_to_stored_job,
                    )
                    .optional()
                    .map_err(Error::from)
            })?
            .map(StoredJob::into_record)
            .transpose()
    }

    /// List newest jobs, optionally filtered by status.
    ///
    /// # Errors
    ///
    /// Returns an error on storage failure or a malformed stored row.
    pub fn list(&self, status: Option<JobStatus>, limit: usize) -> Result<Vec<JobRecord>> {
        let limit = i64::try_from(limit.clamp(1, 1_000)).unwrap_or(1_000);
        let rows = self.database.with_reader(|connection| {
            let mut statement = connection.prepare(&format!(
                "{JOB_SELECT} WHERE ?1 IS NULL OR status = ?1
                 ORDER BY created_at_ms DESC, id LIMIT ?2"
            ))?;
            let rows = statement.query_map(
                params![status.map(JobStatus::as_str), limit],
                row_to_stored_job,
            )?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Error::from)
        })?;
        rows.into_iter().map(StoredJob::into_record).collect()
    }

    fn find_by_dedupe(&self, job_type: JobType, dedupe_key: &str) -> Result<Option<JobRecord>> {
        self.database
            .with_reader(|connection| {
                connection
                    .query_row(
                        &format!("{JOB_SELECT} WHERE job_type = ?1 AND dedupe_key = ?2"),
                        params![job_type.as_str(), dedupe_key],
                        row_to_stored_job,
                    )
                    .optional()
                    .map_err(Error::from)
            })?
            .map(StoredJob::into_record)
            .transpose()
    }
}

const JOB_SELECT: &str = "SELECT id, project_id, job_type, priority, status, payload_json,
        dedupe_key, attempts, max_attempts, last_error, result_json, not_before_ms,
        created_at_ms, updated_at_ms, completed_at_ms FROM jobs";

/// Raw row image decoded outside the `rusqlite` callback.
struct StoredJob {
    id: String,
    project_id: Option<String>,
    job_type: String,
    priority: i64,
    status: String,
    payload_json: String,
    dedupe_key: String,
    attempts: i64,
    max_attempts: i64,
    last_error: Option<String>,
    result_json: Option<String>,
    not_before_ms: i64,
    created_at_ms: i64,
    updated_at_ms: i64,
    completed_at_ms: Option<i64>,
}

fn row_to_stored_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredJob> {
    Ok(StoredJob {
        id: row.get(0)?,
        project_id: row.get(1)?,
        job_type: row.get(2)?,
        priority: row.get(3)?,
        status: row.get(4)?,
        payload_json: row.get(5)?,
        dedupe_key: row.get(6)?,
        attempts: row.get(7)?,
        max_attempts: row.get(8)?,
        last_error: row.get(9)?,
        result_json: row.get(10)?,
        not_before_ms: row.get(11)?,
        created_at_ms: row.get(12)?,
        updated_at_ms: row.get(13)?,
        completed_at_ms: row.get(14)?,
    })
}

impl StoredJob {
    fn into_record(self) -> Result<JobRecord> {
        Ok(JobRecord {
            id: JobId::parse(&self.id)?,
            project_id: self
                .project_id
                .as_deref()
                .map(ProjectId::parse)
                .transpose()?,
            job_type: JobType::parse(&self.job_type)?,
            priority: self.priority,
            status: JobStatus::parse(&self.status)?,
            payload: serde_json::from_str(&self.payload_json)?,
            dedupe_key: self.dedupe_key,
            attempts: u32::try_from(self.attempts).unwrap_or(u32::MAX),
            max_attempts: u32::try_from(self.max_attempts).unwrap_or(u32::MAX),
            last_error: self.last_error,
            result: self
                .result_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?,
            not_before_ms: self.not_before_ms,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            completed_at_ms: self.completed_at_ms,
        })
    }
}

fn truncate_error(error: &str) -> String {
    const LIMIT: usize = 2_048;
    if error.len() <= LIMIT {
        return error.to_owned();
    }
    let mut end = LIMIT;
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &error[..end])
}

/// Executes claimed jobs. Every canonical write goes through the daemon's
/// mutation scheduler at background priority; reads use the bounded pool.
#[derive(Clone)]
pub struct JobExecutor {
    database: Database,
    scheduler: MutationScheduler,
    redactor: Redactor,
    config: JobsConfig,
    /// Job-driven invalidations count toward the daemon's stale-injection
    /// signal; absent only in library/test contexts without a registry.
    metrics: Option<std::sync::Arc<MetricsRegistry>>,
    /// Vault root for `vault_export` jobs; `None` fails those jobs cleanly.
    vault_root: Option<std::path::PathBuf>,
    /// Local embedding model whose vectors `semantic_rebuild` maintains;
    /// `None` rebuilds only the default hashed vectors.
    semantic_provider: Option<LocalEmbedderSettings>,
}

impl JobExecutor {
    #[must_use]
    pub const fn new(
        database: Database,
        scheduler: MutationScheduler,
        redactor: Redactor,
        config: JobsConfig,
        metrics: Option<std::sync::Arc<MetricsRegistry>>,
    ) -> Self {
        Self {
            database,
            scheduler,
            redactor,
            config,
            metrics,
            vault_root: None,
            semantic_provider: None,
        }
    }

    /// Maintain a local embedding model's vectors in `semantic_rebuild` jobs.
    #[must_use]
    pub fn with_semantic_provider(mut self, provider: Option<LocalEmbedderSettings>) -> Self {
        self.semantic_provider = provider;
        self
    }

    /// Enable `vault_export` jobs by providing the vault root directory.
    #[must_use]
    pub fn with_vault_root(mut self, vault_root: Option<std::path::PathBuf>) -> Self {
        self.vault_root = vault_root;
        self
    }

    /// Execute one claimed job and durably record its outcome. Never panics
    /// the runner: failures land in the row's `last_error`/backoff fields.
    pub fn execute_and_record(&self, job: &JobRecord, now_ms: i64) {
        let outcome = self.execute(job, now_ms);
        let queue = JobQueue::new(self.database.clone());
        let id = job.id.clone();
        let recorded = match outcome {
            Ok(result) => self
                .scheduler
                .submit(MutationPriority::BackgroundLearning, move || {
                    queue.record_success(&id, &result, now_ms)
                }),
            Err(error) => {
                let message = error.to_string();
                self.scheduler
                    .submit(MutationPriority::BackgroundLearning, move || {
                        queue.record_failure(&id, &message, now_ms)
                    })
            }
        };
        if let Ok(handle) = recorded {
            let _ = handle.wait();
        }
    }

    /// Dispatch one job body.
    ///
    /// # Errors
    ///
    /// Returns the job's domain error; the caller records it into the row.
    pub fn execute(&self, job: &JobRecord, now_ms: i64) -> Result<serde_json::Value> {
        match job.job_type {
            JobType::FtsIntegrity => self.run_fts_integrity(),
            JobType::SemanticRebuild => self.run_semantic_rebuild(job),
            JobType::ConsolidationGc => self.run_consolidation_gc(now_ms),
            JobType::EvidenceReverify => self.run_evidence_reverify(job),
            JobType::VaultExport => self.run_vault_export(job),
            JobType::BackupDaily => self.run_backup_daily(now_ms),
        }
    }

    /// Run the FTS/database integrity check and record the report. A failing
    /// integrity check fails the job so the problem is visible in the row and
    /// retried on the recorded backoff schedule.
    fn run_fts_integrity(&self) -> Result<serde_json::Value> {
        let report = self.database.integrity_report()?;
        if report.integrity != "ok" {
            return Err(Error::Config(format!(
                "integrity check failed: {}",
                report.integrity
            )));
        }
        Ok(serde_json::to_value(&report)?)
    }

    /// Daily transactional backup of the canonical brain, keeping the
    /// newest `DAILY_BACKUP_RETENTION` files. The backup itself runs as a
    /// scheduler mutation so it sees a consistent writer-quiesced database.
    fn run_backup_daily(&self, now_ms: i64) -> Result<serde_json::Value> {
        const DAILY_BACKUP_RETENTION: usize = 7;
        let directory = self
            .database
            .path()
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("backups");
        std::fs::create_dir_all(&directory)
            .map_err(|error| Error::Config(format!("backup directory: {error}")))?;
        let destination = directory.join(format!("daily-{}.sqlite", now_ms.div_euclid(86_400_000)));
        let backed_up = if destination.exists() {
            false // same-day rerun after a retry: the backup already exists
        } else {
            let database = self.database.clone();
            let target = destination.clone();
            let handle = self
                .scheduler
                .submit(MutationPriority::BackgroundLearning, move || {
                    database.backup(&target)
                })?;
            handle.wait()?;
            true
        };
        // Bounded retention over daily-* files only; manual backups stay.
        let mut dailies = std::fs::read_dir(&directory)
            .map_err(|error| Error::Config(format!("backup listing: {error}")))?
            .filter_map(std::result::Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .strip_prefix("daily-")
                    .is_some_and(|rest| rest.ends_with(".sqlite"))
            })
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        dailies.sort();
        let mut pruned = 0_usize;
        while dailies.len() > DAILY_BACKUP_RETENTION {
            let oldest = dailies.remove(0);
            if std::fs::remove_file(&oldest).is_ok() {
                pruned += 1;
            }
        }
        Ok(serde_json::json!({
            "destination": destination.display().to_string(),
            "backed_up": backed_up,
            "pruned": pruned,
        }))
    }

    /// Re-export the derived Obsidian vault for the job's project.
    fn run_vault_export(&self, job: &JobRecord) -> Result<serde_json::Value> {
        let project_id = required_project(job)?;
        let root = self.vault_root.clone().ok_or_else(|| {
            Error::Config("vault export requires vault.enabled with a root".to_owned())
        })?;
        let report = crate::vault::VaultBuilder::new(self.database.clone(), root)
            .build_project(&project_id)?;
        Ok(serde_json::to_value(&report)?)
    }

    /// Rebuild the derived semantic vector sidecar for the job's project:
    /// hashed vectors in the writer, then any local model's vectors embedded
    /// outside it.
    fn run_semantic_rebuild(&self, job: &JobRecord) -> Result<serde_json::Value> {
        let project_id = required_project(job)?;
        let report = crate::semantic::rebuild_project(
            &self.database,
            &self.scheduler,
            self.semantic_provider.as_ref(),
            &project_id,
            None,
        )?;
        Ok(serde_json::to_value(&report)?)
    }

    /// Bounded consolidation GC (spec section 16): delete `context_runs`
    /// (their `context_items` cascade) older than the configured retention in
    /// bounded batches, then unlink orphaned blob files that `engram doctor`
    /// would report. Each batch is its own scheduler mutation so interactive
    /// writes interleave freely.
    /// Hook hygiene folded into the daily GC: session files older than a
    /// week are deleted, and tasks left open that long are marked abandoned
    /// so episode stats stay meaningful.
    fn gc_hook_sessions(&self, now_ms: i64) -> (u64, u64) {
        const STALE_MS: i64 = 7 * 86_400_000;
        let mut files_removed = 0_u64;
        let directory = self
            .database
            .path()
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("hook-sessions");
        if let Ok(entries) = std::fs::read_dir(&directory) {
            for entry in entries.flatten() {
                let stale = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| {
                        i64::try_from(age.as_millis()).unwrap_or(i64::MAX) > STALE_MS
                    });
                if stale && std::fs::remove_file(entry.path()).is_ok() {
                    files_removed += 1;
                }
            }
        }
        // Relay inboxes age out with their sessions: stale per-session
        // directories (undelivered messages included) are swept whole.
        let relay = self
            .database
            .path()
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("relay");
        if let Ok(entries) = std::fs::read_dir(&relay) {
            for entry in entries.flatten() {
                let stale = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| {
                        i64::try_from(age.as_millis()).unwrap_or(i64::MAX) > STALE_MS
                    });
                if stale && std::fs::remove_dir_all(entry.path()).is_ok() {
                    files_removed += 1;
                }
            }
        }
        let cutoff = now_ms - STALE_MS;
        let database = self.database.clone();
        let abandoned = self
            .scheduler
            .submit(MutationPriority::BackgroundLearning, move || {
                database.write_immediate(|transaction| {
                    let changed = transaction.execute(
                        "UPDATE tasks SET status = 'abandoned', completed_at_ms = ?2
                         WHERE status = 'open' AND started_at_ms < ?1",
                        params![cutoff, unix_time_ms()],
                    )?;
                    Ok(u64::try_from(changed).unwrap_or(0))
                })
            })
            .ok()
            .and_then(|handle| handle.wait().ok())
            .unwrap_or(0);
        (files_removed, abandoned)
    }

    fn run_consolidation_gc(&self, now_ms: i64) -> Result<serde_json::Value> {
        let retention_ms =
            i64::from(self.config.context_run_retention_days).saturating_mul(86_400_000);
        let cutoff_ms = now_ms.saturating_sub(retention_ms);
        let mut deleted_runs = 0_u64;
        let mut truncated = true;
        for _ in 0..GC_MAX_BATCHES {
            let database = self.database.clone();
            let batch = self
                .scheduler
                .submit(MutationPriority::BackgroundLearning, move || {
                    delete_context_run_batch(&database, cutoff_ms, GC_BATCH_ROWS)
                })?
                .wait()?;
            deleted_runs = deleted_runs.saturating_add(batch);
            if usize::try_from(batch).unwrap_or(usize::MAX) < GC_BATCH_ROWS {
                truncated = false;
                break;
            }
        }
        // Hook tool observations follow the same retention window.
        let mut deleted_observations = 0_u64;
        let mut observations_truncated = true;
        for _ in 0..GC_MAX_BATCHES {
            let database = self.database.clone();
            let batch = self
                .scheduler
                .submit(MutationPriority::BackgroundLearning, move || {
                    crate::effectiveness::delete_tool_observation_batch(
                        &database,
                        cutoff_ms,
                        GC_BATCH_ROWS,
                    )
                })?
                .wait()?;
            deleted_observations = deleted_observations.saturating_add(batch);
            if usize::try_from(batch).unwrap_or(usize::MAX) < GC_BATCH_ROWS {
                observations_truncated = false;
                break;
            }
        }
        // So do hook-observed verification runs.
        let mut deleted_verifications = 0_u64;
        let mut verifications_truncated = true;
        for _ in 0..GC_MAX_BATCHES {
            let database = self.database.clone();
            let batch = self
                .scheduler
                .submit(MutationPriority::BackgroundLearning, move || {
                    crate::effectiveness::delete_verification_observation_batch(
                        &database,
                        cutoff_ms,
                        GC_BATCH_ROWS,
                    )
                })?
                .wait()?;
            deleted_verifications = deleted_verifications.saturating_add(batch);
            if usize::try_from(batch).unwrap_or(usize::MAX) < GC_BATCH_ROWS {
                verifications_truncated = false;
                break;
            }
        }
        let gc_database = self.database.clone();
        let orphans_removed = self
            .scheduler
            .submit(MutationPriority::BackgroundLearning, move || {
                remove_orphaned_blobs(
                    &BlobStore::new(gc_database),
                    ORPHAN_BLOB_GRACE_MS,
                    ORPHAN_BLOB_LIMIT,
                )
            })?
            .wait()?;
        let (hook_session_files_removed, stale_open_tasks_abandoned) =
            self.gc_hook_sessions(now_ms);
        Ok(serde_json::json!({
            "context_runs_deleted": deleted_runs,
            "tool_observations_deleted": deleted_observations,
            "verification_observations_deleted": deleted_verifications,
            "orphan_blobs_removed": orphans_removed,
            "hook_session_files_removed": hook_session_files_removed,
            "stale_open_tasks_abandoned": stale_open_tasks_abandoned,
            "truncated": truncated || observations_truncated || verifications_truncated,
        }))
    }

    /// Staleness sweep over evidence (see [`JobType::EvidenceReverify`] for
    /// why stored commands are deliberately never re-executed): memories whose
    /// supporting evidence references a commit unknown to the project are
    /// marked suspect, and the shared symbol-drift quarantine runs for the
    /// project so evidence tied to drifted symbols is re-checked too.
    fn run_evidence_reverify(&self, job: &JobRecord) -> Result<serde_json::Value> {
        let project_id = required_project(job)?;
        let limit = i64::try_from(REVERIFY_EVIDENCE_LIMIT).unwrap_or(i64::MAX);
        let scan_project = project_id.clone();
        let (stale_evidence, memory_ids) = self.database.with_reader(move |connection| {
            let known_commits: i64 = connection.query_row(
                "SELECT COUNT(*) FROM commits WHERE project_id = ?1",
                [scan_project.as_str()],
                |row| row.get(0),
            )?;
            if known_commits == 0 {
                // Without ingested history every commit hash would look
                // unreachable; skip rather than mass-quarantine.
                return Ok((0_u64, Vec::new()));
            }
            let mut statement = connection.prepare(
                "SELECT DISTINCT e.id, me.memory_id
                 FROM evidence e
                 JOIN memory_evidence me
                   ON me.evidence_id = e.id AND me.relation = 'supports'
                 JOIN memories m ON m.id = me.memory_id AND m.status = 'active'
                 WHERE e.project_id = ?1 AND e.commit_hash IS NOT NULL
                   AND NOT EXISTS (
                     SELECT 1 FROM commits c
                     WHERE c.project_id = e.project_id AND c.hash = e.commit_hash
                   )
                 ORDER BY e.id LIMIT ?2",
            )?;
            let rows = statement.query_map(params![scan_project.as_str(), limit], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut evidence = std::collections::BTreeSet::new();
            let mut memories = std::collections::BTreeSet::new();
            for row in rows {
                let (evidence_id, memory_id) = row?;
                evidence.insert(evidence_id);
                memories.insert(memory_id);
            }
            Ok((
                u64::try_from(evidence.len()).unwrap_or(u64::MAX),
                memories.into_iter().collect::<Vec<_>>(),
            ))
        })?;

        let mut marked = 0_u64;
        for raw_id in memory_ids {
            let memory_id = MemoryId::parse(&raw_id)?;
            let store = MemoryStore::with_redactor(self.database.clone(), self.redactor.clone());
            self.scheduler
                .submit(MutationPriority::BackgroundLearning, move || {
                    store.mark_status(
                        &memory_id,
                        MemoryStatus::Suspect,
                        "evidence_commit_unreachable",
                    )
                })?
                .wait()?;
            marked = marked.saturating_add(1);
        }

        let drift_store = MemoryStore::with_redactor(self.database.clone(), self.redactor.clone());
        let drift_project = project_id.clone();
        let drift_marked = self
            .scheduler
            .submit(MutationPriority::BackgroundLearning, move || {
                drift_store.mark_symbol_drift_suspect(&drift_project)
            })?
            .wait()?;

        if let Some(metrics) = &self.metrics {
            metrics.count_drift_invalidation(
                drift_marked.invalidated,
                drift_marked.previously_injected,
            );
            // Commit-unreachable markings carry no injection attribution;
            // count them as invalidations with a zero injected lower bound.
            metrics.count_drift_invalidation(marked, 0);
        }

        Ok(serde_json::json!({
            "stale_commit_evidence": stale_evidence,
            "memories_marked_suspect": marked,
            "drift_marked_suspect": drift_marked,
            "auto_reexecution": "disabled_by_design_spec_26_4",
        }))
    }
}

fn required_project(job: &JobRecord) -> Result<ProjectId> {
    job.project_id.clone().ok_or_else(|| {
        Error::Config(format!(
            "job `{}` of type `{}` is missing its project scope",
            job.id,
            job.job_type.as_str()
        ))
    })
}

/// Delete one bounded batch of expired `context_runs` (their `context_items`
/// rows cascade) in one transaction. Runs referenced by `usage_events` are
/// exempt: nulling their `context_run_id` would silently erase the recorded
/// uses that gate procedure-to-skill promotion.
///
/// # Errors
///
/// Returns an error on storage failure.
pub fn delete_context_run_batch(database: &Database, cutoff_ms: i64, limit: usize) -> Result<u64> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    database.write_immediate(|transaction| {
        let deleted = transaction.execute(
            "DELETE FROM context_runs WHERE id IN (
                 SELECT id FROM context_runs WHERE created_at_ms < ?1
                   AND NOT EXISTS (
                     SELECT 1 FROM usage_events u WHERE u.context_run_id = context_runs.id
                   )
                 ORDER BY created_at_ms, id LIMIT ?2
             )",
            params![cutoff_ms, limit],
        )?;
        u64::try_from(deleted).map_err(|_| Error::Config("GC batch exceeds u64".to_owned()))
    })
}

/// Unlink blob files that have no metadata row (the `orphan_files` that
/// `engram doctor` reports). For every candidate, the row's absence is
/// re-confirmed and the unlink performed inside one canonical writer
/// transaction, and the mtime grace window is re-checked at unlink time —
/// an unreadable or future mtime counts as recent, because deletion is the
/// irreversible branch. Callers on the daemon path additionally submit this
/// through the mutation scheduler so it serializes with the evidence
/// stage-then-commit sequence.
///
/// # Errors
///
/// Returns an error when discovery, the row re-check, or an unlink (other
/// than already-missing) fails.
pub fn remove_orphaned_blobs(store: &BlobStore, grace_ms: u64, limit: usize) -> Result<u64> {
    let candidates = store.orphaned_digests(limit)?;
    if candidates.is_empty() {
        return Ok(0);
    }
    store.database().write_immediate(|transaction| {
        let mut removed = 0_u64;
        for digest in &candidates {
            let referenced: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM blobs WHERE digest = ?1)",
                [digest.as_str()],
                |row| row.get(0),
            )?;
            if referenced {
                continue;
            }
            if grace_ms > 0 {
                let path = store.path_for(digest)?;
                let Some(age) = std::fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                else {
                    continue;
                };
                if age < Duration::from_millis(grace_ms) {
                    continue;
                }
            }
            store.remove_file(digest)?;
            removed = removed.saturating_add(1);
        }
        Ok(removed)
    })
}

/// A `semantic_rebuild` job requested by retrieval, deduplicated per project
/// per UTC hour so repeated requests cannot churn the queue.
fn semantic_rebuild_request(project_id: ProjectId, now_ms: i64, max_attempts: u32) -> EnqueueJob {
    EnqueueJob {
        job_type: JobType::SemanticRebuild,
        dedupe_key: format!(
            "retrieval-{project_id}-hour-{}",
            now_ms.div_euclid(3_600_000)
        ),
        project_id: Some(project_id),
        payload: serde_json::json!({ "reason": "retrieval found missing or stale vectors" }),
        max_attempts,
    }
}

/// Daily dedupe key for self-enqueued maintenance jobs: one enqueue per UTC
/// day, derived arithmetically so no calendar dependency is needed.
#[must_use]
pub fn daily_dedupe_key(now_ms: i64) -> String {
    format!("day-{}", now_ms.div_euclid(86_400_000))
}

/// Everything the daemon-owned job loop needs. The loop is spawned once per
/// daemon and stops when `stop` observes `true` (hooked to the daemon's
/// shutdown notify), waiting for in-flight jobs before returning.
pub struct JobRunner {
    executor: JobExecutor,
    scheduler: MutationScheduler,
    background_slots: Arc<Semaphore>,
    config: JobsConfig,
    max_attempts: u32,
    rss_hard_limit_bytes: u64,
    rss_probe: Arc<dyn Fn() -> Option<u64> + Send + Sync>,
}

impl JobRunner {
    #[must_use]
    pub fn new(
        executor: JobExecutor,
        scheduler: MutationScheduler,
        background_slots: Arc<Semaphore>,
        config: JobsConfig,
        rss_hard_limit_bytes: u64,
        rss_probe: Arc<dyn Fn() -> Option<u64> + Send + Sync>,
    ) -> Self {
        let max_attempts = config.max_attempts;
        Self {
            executor,
            scheduler,
            background_slots,
            config,
            max_attempts,
            rss_hard_limit_bytes,
            rss_probe,
        }
    }

    /// Spawn the polling loop onto the current tokio runtime.
    #[must_use]
    pub fn spawn(self, stop: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.run(stop))
    }

    async fn run(self, mut stop: watch::Receiver<bool>) {
        let poll = Duration::from_millis(self.config.poll_interval_ms.max(1));
        let mut in_flight: JoinSet<()> = JoinSet::new();
        let mut last_daily_day = i64::MIN;
        loop {
            if *stop.borrow() {
                break;
            }
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
                () = tokio::time::sleep(poll) => {
                    while in_flight.try_join_next().is_some() {}
                    self.tick(&mut in_flight, &mut last_daily_day).await;
                }
            }
        }
        // Clean shutdown: never abandon a running job mid-write.
        while in_flight.join_next().await.is_some() {}
    }

    async fn tick(&self, in_flight: &mut JoinSet<()>, last_daily_day: &mut i64) {
        let now_ms = unix_time_ms();
        let day = now_ms.div_euclid(86_400_000);
        if day != *last_daily_day && self.enqueue_daily(now_ms).await {
            *last_daily_day = day;
        }
        self.enqueue_requested_semantic_rebuilds(now_ms).await;
        loop {
            if !can_start_job((self.rss_probe)(), self.rss_hard_limit_bytes) {
                break;
            }
            let Ok(permit) = Arc::clone(&self.background_slots).try_acquire_owned() else {
                break;
            };
            let queue = JobQueue::new(self.executor.database.clone());
            let claim_now = unix_time_ms();
            let Ok(handle) = self
                .scheduler
                .submit(MutationPriority::BackgroundLearning, move || {
                    queue.claim_due(claim_now)
                })
            else {
                break;
            };
            let claimed = tokio::task::spawn_blocking(move || handle.wait()).await;
            let Ok(Ok(Some(job))) = claimed else {
                break;
            };
            let executor = self.executor.clone();
            in_flight.spawn_blocking(move || {
                let _permit = permit;
                executor.execute_and_record(&job, unix_time_ms());
            });
        }
    }

    /// Turn retrieval's requests for a semantic rebuild (missing, stale or
    /// unreadable vectors; spec section 27.2) into `semantic_rebuild` jobs,
    /// at most one per project per hour.
    async fn enqueue_requested_semantic_rebuilds(&self, now_ms: i64) {
        for project_id in crate::semantic::take_rebuild_requests(&self.executor.database) {
            let queue = JobQueue::new(self.executor.database.clone());
            let request = semantic_rebuild_request(project_id, now_ms, self.max_attempts);
            let Ok(handle) = self
                .scheduler
                .submit(MutationPriority::BackgroundLearning, move || {
                    queue.enqueue(&request, now_ms)
                })
            else {
                return;
            };
            let _ = tokio::task::spawn_blocking(move || handle.wait()).await;
        }
    }

    /// Self-enqueue the daily maintenance jobs, dedupe-keyed by UTC day so a
    /// restart within the same day never duplicates them. Returns whether both
    /// enqueues were durably accepted.
    async fn enqueue_daily(&self, now_ms: i64) -> bool {
        for job_type in [
            JobType::ConsolidationGc,
            JobType::FtsIntegrity,
            JobType::BackupDaily,
        ] {
            let queue = JobQueue::new(self.executor.database.clone());
            let request = EnqueueJob {
                job_type,
                project_id: None,
                payload: serde_json::json!({}),
                dedupe_key: daily_dedupe_key(now_ms),
                max_attempts: self.max_attempts,
            };
            let Ok(handle) = self
                .scheduler
                .submit(MutationPriority::BackgroundLearning, move || {
                    queue.enqueue(&request, now_ms)
                })
            else {
                return false;
            };
            let outcome = tokio::task::spawn_blocking(move || handle.wait()).await;
            if !matches!(outcome, Ok(Ok(_))) {
                return false;
            }
        }
        true
    }
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
    use tempfile::TempDir;

    use super::*;
    use crate::experience::{EvidenceStrength, ExperienceStore};
    use crate::memory::MemoryDraft;
    use crate::projects::ProjectRegistry;

    fn database() -> (TempDir, Database) {
        let temp = TempDir::new().expect("temp dir");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        (temp, database)
    }

    fn enqueue_request(job_type: JobType, dedupe_key: &str) -> EnqueueJob {
        EnqueueJob {
            job_type,
            project_id: None,
            payload: serde_json::json!({}),
            dedupe_key: dedupe_key.to_owned(),
            max_attempts: 3,
        }
    }

    #[test]
    fn enqueue_is_idempotent_by_type_and_dedupe_key() {
        let (_temp, database) = database();
        let queue = JobQueue::new(database);
        let first = queue
            .enqueue(&enqueue_request(JobType::FtsIntegrity, "day-1"), 1_000)
            .expect("first enqueue");
        let duplicate = queue
            .enqueue(&enqueue_request(JobType::FtsIntegrity, "day-1"), 2_000)
            .expect("duplicate enqueue");
        assert_eq!(first.id, duplicate.id);
        assert_eq!(duplicate.created_at_ms, 1_000, "row is unchanged");

        // A different type with the same key is a distinct job.
        let other = queue
            .enqueue(&enqueue_request(JobType::ConsolidationGc, "day-1"), 3_000)
            .expect("other type");
        assert_ne!(other.id, first.id);
        assert_eq!(queue.list(None, 10).expect("list").len(), 2);
    }

    #[test]
    fn lifecycle_retries_with_recorded_backoff_then_dies() {
        let (_temp, database) = database();
        let queue = JobQueue::new(database);
        let job = queue
            .enqueue(&enqueue_request(JobType::FtsIntegrity, "k"), 0)
            .expect("enqueue");
        assert_eq!(job.status, JobStatus::Pending);
        assert_eq!(job.max_attempts, 3);

        // Attempt 1: claim then fail -> failed with backoff recorded.
        let claimed = queue.claim_due(10).expect("claim").expect("due job");
        assert_eq!(claimed.id, job.id);
        assert_eq!(claimed.status, JobStatus::Running);
        assert_eq!(claimed.attempts, 1);
        queue
            .record_failure(&job.id, "probe failure", 20)
            .expect("failure");
        let failed = queue.get(&job.id).expect("get").expect("row");
        assert_eq!(failed.status, JobStatus::Failed);
        assert_eq!(failed.last_error.as_deref(), Some("probe failure"));
        assert_eq!(failed.not_before_ms, 20 + retry_backoff_ms(1));

        // Backoff gates the next claim deterministically.
        assert!(
            queue
                .claim_due(failed.not_before_ms - 1)
                .expect("early claim")
                .is_none(),
            "job must not be claimable before its recorded backoff"
        );

        // Attempt 2 fails with a doubled backoff.
        let second = queue
            .claim_due(failed.not_before_ms)
            .expect("claim")
            .expect("due after backoff");
        assert_eq!(second.attempts, 2);
        queue
            .record_failure(&job.id, "again", failed.not_before_ms)
            .expect("failure");
        let failed_again = queue.get(&job.id).expect("get").expect("row");
        assert_eq!(
            failed_again.not_before_ms,
            failed.not_before_ms + retry_backoff_ms(2)
        );

        // Attempt 3 exhausts max_attempts -> dead, never claimable again.
        let third = queue
            .claim_due(failed_again.not_before_ms)
            .expect("claim")
            .expect("final attempt");
        assert_eq!(third.attempts, 3);
        queue
            .record_failure(&job.id, "fatal", failed_again.not_before_ms)
            .expect("failure");
        let dead = queue.get(&job.id).expect("get").expect("row");
        assert_eq!(dead.status, JobStatus::Dead);
        assert_eq!(dead.last_error.as_deref(), Some("fatal"));
        assert!(dead.completed_at_ms.is_some());
        assert!(queue.claim_due(i64::MAX).expect("claim").is_none());
    }

    #[test]
    fn success_records_result_json() {
        let (_temp, database) = database();
        let queue = JobQueue::new(database);
        let job = queue
            .enqueue(&enqueue_request(JobType::FtsIntegrity, "ok"), 0)
            .expect("enqueue");
        queue.claim_due(1).expect("claim").expect("job");
        queue
            .record_success(&job.id, &serde_json::json!({"vectors": 7}), 2)
            .expect("success");
        let done = queue.get(&job.id).expect("get").expect("row");
        assert_eq!(done.status, JobStatus::Succeeded);
        assert_eq!(done.result, Some(serde_json::json!({"vectors": 7})));
        assert_eq!(done.completed_at_ms, Some(2));
        assert!(done.last_error.is_none());
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        assert_eq!(retry_backoff_ms(1), 30_000);
        assert_eq!(retry_backoff_ms(2), 60_000);
        assert_eq!(retry_backoff_ms(3), 120_000);
        assert_eq!(retry_backoff_ms(20), 3_600_000, "capped at one hour");
        assert_eq!(retry_backoff_ms(u32::MAX), 3_600_000);
    }

    #[test]
    fn crash_recovery_resets_running_jobs_to_pending() {
        let temp = TempDir::new().expect("temp dir");
        let path = temp.path().join("brain.sqlite");
        let job_id = {
            let database = Database::open(&path).expect("database");
            let queue = JobQueue::new(database);
            let job = queue
                .enqueue(&enqueue_request(JobType::FtsIntegrity, "crash"), 0)
                .expect("enqueue");
            queue.claim_due(1).expect("claim").expect("running job");
            job.id
        };

        // Reopen the store as a restarted daemon would and recover.
        let database = Database::open(&path).expect("reopen");
        let queue = JobQueue::new(database);
        assert_eq!(
            queue.get(&job_id).expect("get").expect("row").status,
            JobStatus::Running,
            "crashed executor left the row running"
        );
        assert_eq!(queue.recover_running(50).expect("recover"), 1);
        let recovered = queue.get(&job_id).expect("get").expect("row");
        assert_eq!(recovered.status, JobStatus::Pending);
        assert_eq!(recovered.attempts, 1, "the consumed attempt is kept");
        assert!(queue.claim_due(50).expect("claim").is_some());
    }

    #[test]
    fn hard_pressure_blocks_new_job_starts() {
        let hard = 900 * 1_024 * 1_024;
        assert!(can_start_job(Some(hard - 1), hard));
        assert!(!can_start_job(Some(hard), hard), "at the hard limit");
        assert!(!can_start_job(Some(hard + 1), hard), "over the hard limit");
        assert!(
            can_start_job(None, hard),
            "unknown RSS admits work, matching daemon admission"
        );
    }

    fn insert_context_run(database: &Database, project: &ProjectId, ordinal: u32, at_ms: i64) {
        database
            .write_immediate(|transaction| {
                transaction.execute(
                    "INSERT INTO context_runs(id, request_id, project_id, request_fingerprint,
                         mode, task_text_digest, token_budget, selected_tokens, candidate_count,
                         stable_prefix_hash, stable_prefix, dynamic_context, compile_ms,
                         created_at_ms)
                     VALUES (?1, ?2, ?3, ?2, 'brain', 'digest', 100, 10, 1, 'hash', '', '', 1, ?4)",
                    params![
                        crate::ids::ContextRunId::new().as_str(),
                        format!("gc-fixture-{ordinal}"),
                        project.as_str(),
                        at_ms,
                    ],
                )?;
                Ok(())
            })
            .expect("context run fixture");
    }

    fn count_context_runs(database: &Database) -> i64 {
        database
            .with_reader(|connection| {
                connection
                    .query_row("SELECT COUNT(*) FROM context_runs", [], |row| row.get(0))
                    .map_err(Error::from)
            })
            .expect("count")
    }

    #[test]
    fn gc_deletes_expired_runs_in_bounded_batches_and_stops_at_the_bound() {
        let temp = TempDir::new().expect("temp dir");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let root = temp.path().join("project");
        std::fs::create_dir_all(&root).expect("project dir");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("project")
            .project
            .id;
        for ordinal in 0..5 {
            insert_context_run(&database, &project, ordinal, 100); // expired
        }
        insert_context_run(&database, &project, 100, 10_000); // retained

        // Batch size 2: each call deletes at most one bounded batch.
        assert_eq!(
            delete_context_run_batch(&database, 1_000, 2).expect("batch"),
            2
        );
        assert_eq!(count_context_runs(&database), 4, "stops at the bound");
        assert_eq!(
            delete_context_run_batch(&database, 1_000, 2).expect("batch"),
            2
        );
        assert_eq!(
            delete_context_run_batch(&database, 1_000, 2).expect("batch"),
            1,
            "a short batch means the expired set is drained"
        );
        assert_eq!(count_context_runs(&database), 1);
        assert_eq!(
            delete_context_run_batch(&database, 1_000, 2).expect("batch"),
            0,
            "rows inside the retention window are never touched"
        );
    }

    #[test]
    fn gc_exempts_context_runs_referenced_by_usage_events() {
        let temp = TempDir::new().expect("temp dir");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let root = temp.path().join("project");
        std::fs::create_dir_all(&root).expect("project dir");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("project")
            .project
            .id;
        insert_context_run(&database, &project, 0, 100); // expired, unreferenced
        insert_context_run(&database, &project, 1, 100); // expired, but a use depends on it
        database
            .write_immediate(|transaction| {
                transaction.execute(
                    "INSERT INTO usage_events(id, source_type, source_id, context_run_id,
                         usage_kind, outcome, created_at_ms)
                     SELECT 'use_fixture', 'memory', 'mem_fixture', id, 'injected', NULL, 100
                     FROM context_runs WHERE request_id = 'gc-fixture-1'",
                    [],
                )?;
                Ok(())
            })
            .expect("usage fixture");

        assert_eq!(
            delete_context_run_batch(&database, 1_000, 10).expect("gc"),
            1,
            "only the unreferenced run is deleted"
        );
        assert_eq!(count_context_runs(&database), 1);
        let (kept_run, usage_link): (i64, i64) = database
            .with_reader(|connection| {
                connection
                    .query_row(
                        "SELECT
                             (SELECT COUNT(*) FROM context_runs WHERE request_id = 'gc-fixture-1'),
                             (SELECT COUNT(*) FROM usage_events WHERE context_run_id IS NOT NULL)",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(Error::from)
            })
            .expect("counts");
        assert_eq!(kept_run, 1, "the referenced run survives retention");
        assert_eq!(usage_link, 1, "use attribution is never nulled by GC");
    }

    #[test]
    fn orphaned_blob_files_are_removed_but_referenced_and_recent_files_stay() {
        let (_temp, database) = database();
        let store = BlobStore::new(database.clone());
        let orphan = store.put(b"orphan artifact").expect("orphan");
        let referenced = store.put(b"referenced artifact").expect("referenced");
        database
            .write_immediate(|transaction| {
                transaction.execute(
                    "INSERT INTO blobs(digest, size_bytes, content_type, created_at_ms, refcount)
                     VALUES (?1, 19, 'text/plain', 0, 1)",
                    [referenced.as_str()],
                )?;
                Ok(())
            })
            .expect("reference row");

        // A freshly written orphan survives the grace window.
        assert_eq!(
            remove_orphaned_blobs(&store, 3_600_000, 100).expect("graceful"),
            0
        );
        assert!(store.path_for(&orphan).expect("path").exists());

        // Without grace the orphan is unlinked and the referenced file stays.
        assert_eq!(remove_orphaned_blobs(&store, 0, 100).expect("gc"), 1);
        assert!(!store.path_for(&orphan).expect("path").exists());
        assert!(store.path_for(&referenced).expect("path").exists());
    }

    #[test]
    fn daily_dedupe_key_is_stable_within_a_day() {
        let day_ms = 86_400_000;
        assert_eq!(daily_dedupe_key(0), "day-0");
        assert_eq!(daily_dedupe_key(day_ms - 1), "day-0");
        assert_eq!(daily_dedupe_key(day_ms), "day-1");
    }

    /// Regression: the automatic vault refresh was keyed per project only, so
    /// the first export (or a dead one) blocked every later refresh.
    #[test]
    fn vault_refresh_recurs_once_per_window_after_the_window_closes() {
        let temp = TempDir::new().expect("temp dir");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let registry = ProjectRegistry::new(database.clone());
        let [project, other] = ["one", "two"].map(|name| {
            let root = temp.path().join(name);
            std::fs::create_dir_all(&root).expect("project dir");
            registry
                .resolve_or_register(&root)
                .expect("project")
                .project
                .id
        });
        let queue = JobQueue::new(database);
        let window = 300_000;

        let first = queue
            .enqueue_vault_refresh(&project, 300, 3, 10 * window + 1_000)
            .expect("first change");
        assert_eq!(first.job_type, JobType::VaultExport);
        assert_eq!(
            first.not_before_ms,
            11 * window,
            "due when the window closes"
        );
        assert!(
            first.dedupe_key.ends_with(":300s-10"),
            "{}",
            first.dedupe_key
        );
        let again = queue
            .enqueue_vault_refresh(&project, 300, 3, 11 * window - 1)
            .expect("later change in the same window");
        assert_eq!(again.id, first.id, "one export per window");
        assert_eq!(again.not_before_ms, first.not_before_ms);
        assert!(queue.claim_due(11 * window - 1).expect("claim").is_none());
        let claimed = queue.claim_due(11 * window).expect("claim").expect("due");
        assert_eq!(claimed.id, first.id);
        queue
            .record_success(&claimed.id, &serde_json::json!({}), 11 * window)
            .expect("success");

        // The next window gets its own job although the last one succeeded.
        let next = queue
            .enqueue_vault_refresh(&project, 300, 1, 11 * window + 5)
            .expect("change in the next window");
        assert_ne!(
            next.id, first.id,
            "a finished export does not block refreshes"
        );
        assert_eq!(
            (next.status, next.not_before_ms),
            (JobStatus::Pending, 12 * window)
        );
        let claimed = queue.claim_due(12 * window).expect("claim").expect("due");
        assert_eq!(claimed.id, next.id);
        queue
            .record_failure(&claimed.id, "vault unwritable", 12 * window)
            .expect("failure");
        let dead = queue.get(&next.id).expect("get").expect("row");
        assert_eq!(dead.status, JobStatus::Dead);
        let after_dead = queue
            .enqueue_vault_refresh(&project, 300, 3, 12 * window + 5)
            .expect("change after a dead export");
        assert_ne!(
            after_dead.id, next.id,
            "a dead export does not block refreshes"
        );
        assert_eq!(after_dead.status, JobStatus::Pending);
        let elsewhere = queue
            .enqueue_vault_refresh(&other, 300, 3, 12 * window + 5)
            .expect("another project");
        assert_ne!(elsewhere.id, after_dead.id, "windows are per project");

        assert!(queue.enqueue_vault_refresh(&project, 0, 3, 0).is_err());
    }

    fn executor(database: &Database) -> JobExecutor {
        JobExecutor::new(
            database.clone(),
            MutationScheduler::start(64),
            Redactor::default(),
            JobsConfig::default(),
            None,
        )
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn evidence_reverify_marks_unreachable_commit_memories_suspect_without_running_commands() {
        let temp = TempDir::new().expect("temp dir");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let root = temp.path().join("project");
        std::fs::create_dir_all(&root).expect("project dir");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("project")
            .project
            .id;
        database
            .write_immediate(|transaction| {
                transaction.execute(
                    "INSERT INTO commits(project_id, hash, parents_json)
                     VALUES (?1, 'reachable', '[]')",
                    [project.as_str()],
                )?;
                Ok(())
            })
            .expect("known commit");

        let experience = ExperienceStore::new(database.clone());
        let reachable = experience
            .record_evidence(
                &project,
                None,
                None,
                "command_result",
                EvidenceStrength::Deterministic,
                Some("reachable"),
                Some("cargo test"),
                Some(0),
                b"ok",
                &serde_json::json!({}),
            )
            .expect("reachable evidence");
        let unreachable = experience
            .record_evidence(
                &project,
                None,
                None,
                "command_result",
                EvidenceStrength::Deterministic,
                Some("rewritten-away"),
                Some("rm -rf / # hostile repo-supplied command, must never auto-run"),
                Some(0),
                b"stale",
                &serde_json::json!({}),
            )
            .expect("unreachable evidence");

        let memories = MemoryStore::new(database.clone());
        let mut draft = MemoryDraft {
            scope_type: "project".to_owned(),
            scope_id: project.to_string(),
            kind: "lesson".to_owned(),
            claim: "Backed by a reachable commit".to_owned(),
            rationale: "fixture".to_owned(),
            confidence: 0.5,
            importance: 0.5,
            tags: Vec::new(),
            valid_from_commit: None,
            valid_to_commit: None,
            trust_source: "human_instruction".to_owned(),
            evidence_ids: vec![reachable.id.clone()],
            source_task_ids: Vec::new(),
            node_ids: Vec::new(),
        };
        let healthy = memories
            .create_candidate(&project, &draft)
            .expect("healthy memory");
        draft.claim = "Backed by an unreachable commit".to_owned();
        draft.evidence_ids = vec![unreachable.id.clone()];
        let stale = memories
            .create_candidate(&project, &draft)
            .expect("stale memory");

        let queue = JobQueue::new(database.clone());
        let job = queue
            .enqueue(
                &EnqueueJob {
                    job_type: JobType::EvidenceReverify,
                    project_id: Some(project.clone()),
                    payload: serde_json::json!({}),
                    dedupe_key: "reverify".to_owned(),
                    max_attempts: 1,
                },
                0,
            )
            .expect("enqueue");
        let claimed = queue.claim_due(1).expect("claim").expect("job");
        let result = executor(&database)
            .execute(&claimed, 1)
            .expect("reverify run");
        assert_eq!(result["stale_commit_evidence"], 1);
        assert_eq!(result["memories_marked_suspect"], 1);
        assert_eq!(result["auto_reexecution"], "disabled_by_design_spec_26_4");
        queue.record_success(&job.id, &result, 2).expect("success");

        assert_eq!(
            memories.get(&stale.id).expect("get").expect("row").status,
            MemoryStatus::Suspect,
            "memory backed by an unreachable commit is quarantined"
        );
        assert_eq!(
            memories.get(&healthy.id).expect("get").expect("row").status,
            MemoryStatus::Active,
            "memory backed by a reachable commit is untouched"
        );
    }

    #[test]
    fn fts_integrity_job_records_the_report() {
        let (_temp, database) = database();
        let queue = JobQueue::new(database.clone());
        queue
            .enqueue(&enqueue_request(JobType::FtsIntegrity, "integrity"), 0)
            .expect("enqueue");
        let job = queue.claim_due(1).expect("claim").expect("job");
        let executor = executor(&database);
        executor.execute_and_record(&job, 2);
        let done = queue.get(&job.id).expect("get").expect("row");
        assert_eq!(done.status, JobStatus::Succeeded);
        let result = done.result.expect("recorded result");
        assert_eq!(result["integrity"], "ok");
        assert_eq!(result["journal_mode"], "wal");
    }

    #[test]
    fn project_scoped_types_require_a_project() {
        let (_temp, database) = database();
        let queue = JobQueue::new(database);
        let error = queue
            .enqueue(&enqueue_request(JobType::SemanticRebuild, "x"), 0)
            .expect_err("project required");
        assert!(error.to_string().contains("requires a project scope"));
    }

    #[test]
    fn unknown_job_types_are_rejected_by_the_allowlist() {
        let error = JobType::parse("shell_exec").expect_err("not allowlisted");
        assert!(error.to_string().contains("unsupported job type"));
    }

    #[test]
    fn requested_semantic_rebuilds_are_hourly_jobs_that_embed_with_the_local_model() {
        let temp = TempDir::new().expect("temp dir");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let root = temp.path().join("project");
        std::fs::create_dir_all(&root).expect("project dir");
        let project = ProjectRegistry::new(database.clone())
            .resolve_or_register(&root)
            .expect("project")
            .project
            .id;
        let store = MemoryStore::new(database.clone());
        let memory = store
            .create_candidate(
                &project,
                &MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: project.to_string(),
                    kind: "lesson".to_owned(),
                    claim: "Release reservations when abort cleanup runs".to_owned(),
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
            .expect("memory");
        store
            .promote(&memory.id, crate::memory::EpistemicState::Validated, true)
            .expect("validated");

        let queue = JobQueue::new(database.clone());
        let hour = 3_600_000;
        let first = queue
            .enqueue(&semantic_rebuild_request(project.clone(), hour, 5), hour)
            .expect("enqueue");
        let same_hour = queue
            .enqueue(
                &semantic_rebuild_request(project.clone(), 2 * hour - 1, 5),
                2 * hour - 1,
            )
            .expect("deduplicated enqueue");
        assert_eq!(first.id, same_hour.id, "one requested rebuild per hour");
        let next_hour = queue
            .enqueue(
                &semantic_rebuild_request(project.clone(), 2 * hour, 5),
                2 * hour,
            )
            .expect("next hour");
        assert_ne!(first.id, next_hour.id);

        let endpoint = crate::semantic::fake_endpoint::concept_model(4);
        let local = LocalEmbedderSettings {
            endpoint: endpoint.url.clone(),
            model: "embed-a".to_owned(),
            dimension: None,
            batch_size: 8,
            timeout: Duration::from_secs(5),
            query_timeout: Duration::from_secs(5),
            query_prefix: String::new(),
            document_prefix: String::new(),
        };
        let result = executor(&database)
            .with_semantic_provider(Some(local))
            .execute(&first, hour)
            .expect("semantic rebuild job");
        assert_eq!(result["vectors"], 1);
        assert_eq!(result["model"]["embedder"], "local:embed-a");
        assert_eq!(result["model"]["embedded"], 1);
        assert_eq!(result["model"]["dimension"], 4);
    }
}
