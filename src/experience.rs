//! Typed engineering episodes and evidence capture.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::events::{DomainEvent, append_generated_in_transaction};
use crate::ids::{AttemptId, EvidenceId, ProjectId, TaskId};
use crate::security::Redactor;
use crate::storage::Database;

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
    pub artifact_digest: String,
    pub metadata: Value,
    pub observed_at_ms: i64,
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
}

impl ExperienceStore {
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

    /// Record provenance-bearing evidence. Raw artifacts remain outside this
    /// row; only their digest and bounded metadata are canonical here.
    ///
    /// # Errors
    ///
    /// Returns an error for inconsistent task/attempt scope or failed storage.
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
        let event = DomainEvent::EvidenceObserved {
            project_id: project_id.clone(),
            task_id: task_id.cloned(),
            evidence_id: id.clone(),
            kind: kind.to_owned(),
            strength: strength.as_str().to_owned(),
            artifact_digest: artifact_digest.clone(),
        };
        self.database.write_immediate(|transaction| {
            validate_evidence_scope(transaction, project_id, task_id, attempt_id)?;
            transaction.execute(
                "INSERT INTO evidence
                 (id, project_id, task_id, attempt_id, kind, strength, commit_hash,
                  command, exit_code, artifact_digest, metadata_json, observed_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
                    metadata_json,
                    now
                ],
            )?;
            append_generated_in_transaction(transaction, &event, now)
        })?;
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
            metadata,
            observed_at_ms: now,
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
