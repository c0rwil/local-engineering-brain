//! Whether hook-observed verification runs carry their own exit status.
//!
//! A recognized test, lint, type-check or build run becomes evidence only
//! when its exit status is its own. A pipe without `pipefail`, a command
//! joined after it by `;` or a newline, `||`, a background run, a directory
//! change that may not apply, or shell syntax Engram does not read hides it.
//! Post-tool hooks note this to the agent once per conversation and cause,
//! and record every recognized run here, so reports can show the share of
//! runs Engram could record and how it moves after a note. Rows keep the
//! verification kind and the cause label only: never commands, output,
//! error text or paths.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::agenthooks::StatusHidden;
use crate::error::{Error, Result};
use crate::ids::{ProjectId, TaskId};
use crate::storage::Database;

use super::{EffectivenessStore, unix_time_ms};

/// Evidence kinds that hooks recognize as verification.
const KINDS: [&str; 4] = [
    "test_result",
    "lint_result",
    "typecheck_result",
    "build_result",
];

/// One recognized verification run that a post-tool hook observed.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationObservationRequest {
    pub project_id: ProjectId,
    /// The agent's session ID.
    pub session_id: String,
    /// The agent's tool-use ID. Only its digest with the session is stored.
    pub tool_use_id: String,
    /// The conversation a status note is limited to: the session ID, or a
    /// subagent thread's scope within the session.
    pub conversation_id: String,
    #[serde(default)]
    pub task_id: Option<TaskId>,
    /// Producer, for example `claude_code`.
    pub source: String,
    /// Evidence kind of the last recognized verification in the command.
    pub kind: String,
    /// Why the exit status could not be recorded; `None` when it could.
    #[serde(default)]
    pub hidden: Option<StatusHidden>,
    /// The call's hook output carried a status note.
    #[serde(default)]
    pub note_delivered: bool,
}

/// The daemon's acknowledgement of a validated, timestamped and queued
/// observation. The write commits later at the lowest priority; project and
/// task scope are checked when it runs.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct VerificationObservationReceipt {
    pub call_id: String,
    pub observed_at_ms: i64,
    pub queued: bool,
}

impl VerificationObservationRequest {
    /// Check the request shape and return the stored call digest.
    ///
    /// # Errors
    /// Rejects malformed observations. Scope is checked by the write.
    pub fn call_id(&self) -> Result<String> {
        validate(self)?;
        Ok(crate::agenthooks::tool_usage_sample_id(
            &self.session_id,
            &self.tool_use_id,
        ))
    }
}

/// Runs, and how many of them had an exit status Engram could record.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct StatusShare {
    pub runs: u64,
    /// Runs whose exit status was the verification's own.
    pub status_bearing: u64,
    /// `status_bearing / runs`; unknown without runs.
    pub share: Option<f64>,
}

/// Hook-observed verification runs in one period and their status notes.
/// Observational: a note is not a controlled treatment.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct VerificationStatus {
    /// Every recognized verification run.
    pub runs: StatusShare,
    /// Status notes delivered.
    pub notes: u64,
    /// Conversations that received a note in this period.
    pub noted_conversations: u64,
    /// Runs in conversations that received a note, up to and including the
    /// first note. The run that prompted the note is one of them.
    pub before_note: StatusShare,
    /// Runs after their conversation's first note.
    pub after_note: StatusShare,
    /// Runs in conversations without a note: every run there was
    /// recordable, notes were off (brain-off mode), or none was delivered.
    pub without_note: StatusShare,
    /// Runs whose exit status was hidden, by cause.
    pub hidden_by_reason: BTreeMap<String, u64>,
}

impl EffectivenessStore {
    /// Record one hook-observed verification run, timed now.
    ///
    /// # Errors
    /// Rejects invalid observations, unknown or inconsistent references and
    /// storage failures.
    pub fn record_verification_observation(
        &self,
        request: &VerificationObservationRequest,
    ) -> Result<bool> {
        self.record_verification_observation_at(request, unix_time_ms())
    }

    /// Record one hook-observed verification run observed at `observed_at_ms`.
    /// A repeated call merges into its first record and keeps a delivered
    /// note. Returns whether a new record was inserted.
    pub(crate) fn record_verification_observation_at(
        &self,
        request: &VerificationObservationRequest,
        observed_at_ms: i64,
    ) -> Result<bool> {
        let call_id = request.call_id()?;
        if observed_at_ms < 0 {
            return Err(Error::Config(
                "verification observation time cannot be negative".to_owned(),
            ));
        }
        let now = unix_time_ms();
        self.database.write_immediate(|tx| {
            // Acknowledged once queued: an observation queued before its
            // project was merged belongs to the survivor when it is written.
            let resolved;
            let request = match crate::merge::survivor_of(tx, &request.project_id)? {
                Some(survivor) => {
                    resolved = VerificationObservationRequest {
                        project_id: survivor,
                        ..request.clone()
                    };
                    &resolved
                }
                None => request,
            };
            validate_scope(tx, request)?;
            let existing: Option<String> = tx
                .query_row(
                    "SELECT id FROM verification_observations
                     WHERE project_id = ?1 AND call_id = ?2",
                    params![request.project_id.as_str(), call_id],
                    |row| row.get(0),
                )
                .optional()?;
            tx.execute(
                "INSERT INTO verification_observations
                 (id, project_id, call_id, session_id, conversation_id, task_id, source, kind,
                  hidden_reason, note_delivered, observed_at_ms, recorded_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(project_id, call_id) DO UPDATE SET
                   task_id = coalesce(verification_observations.task_id, excluded.task_id),
                   hidden_reason = coalesce(verification_observations.hidden_reason,
                     excluded.hidden_reason),
                   note_delivered = max(verification_observations.note_delivered,
                     excluded.note_delivered),
                   observed_at_ms = min(verification_observations.observed_at_ms,
                     excluded.observed_at_ms)",
                params![
                    existing
                        .clone()
                        .unwrap_or_else(|| format!("vob_{}", uuid::Uuid::new_v4())),
                    request.project_id.as_str(),
                    call_id,
                    request.session_id,
                    request.conversation_id,
                    request.task_id.as_ref().map(TaskId::as_str),
                    request.source,
                    request.kind,
                    request.hidden.map(StatusHidden::as_str),
                    request.note_delivered,
                    observed_at_ms,
                    now,
                ],
            )?;
            Ok(existing.is_none())
        })
    }
}

fn validate(request: &VerificationObservationRequest) -> Result<()> {
    for (label, value, limit) in [
        ("session_id", request.session_id.as_str(), 256),
        ("tool_use_id", request.tool_use_id.as_str(), 256),
        ("conversation_id", request.conversation_id.as_str(), 320),
        ("source", request.source.as_str(), 128),
    ] {
        if value.trim().is_empty() || value.len() > limit || value.chars().any(char::is_control) {
            return Err(Error::Config(format!(
                "verification observation {label} must be nonempty, bounded text without control characters"
            )));
        }
    }
    if !KINDS.contains(&request.kind.as_str()) {
        return Err(Error::Config(format!(
            "verification observation kind must be one of {}",
            KINDS.join(", ")
        )));
    }
    if request.note_delivered && request.hidden.is_none() {
        return Err(Error::Config(
            "a status note applies only to a run whose exit status was hidden".to_owned(),
        ));
    }
    Ok(())
}

fn validate_scope(connection: &Connection, request: &VerificationObservationRequest) -> Result<()> {
    let project_exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1 AND tombstoned_at_ms IS NULL)",
        [request.project_id.as_str()],
        |row| row.get(0),
    )?;
    if !project_exists {
        return Err(Error::Config(
            "verification observation project is unknown or tombstoned".to_owned(),
        ));
    }
    if let Some(task_id) = &request.task_id {
        let task: Option<(String, Option<String>)> = connection
            .query_row(
                "SELECT project_id, session_id FROM tasks WHERE id = ?1",
                [task_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((project, session)) = task else {
            return Err(Error::Config(
                "verification observation task is unknown".to_owned(),
            ));
        };
        if project != request.project_id.as_str()
            || session.is_some_and(|session| session != request.session_id)
        {
            return Err(Error::Config(
                "verification observation task does not match the project or session".to_owned(),
            ));
        }
    }
    Ok(())
}

/// Delete one bounded batch of observations older than the retention cutoff.
///
/// # Errors
/// Returns storage errors.
pub fn delete_verification_observation_batch(
    database: &Database,
    cutoff_ms: i64,
    limit: usize,
) -> Result<u64> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    database.write_immediate(|transaction| {
        let deleted = transaction.execute(
            "DELETE FROM verification_observations WHERE id IN (
                 SELECT id FROM verification_observations WHERE observed_at_ms < ?1
                 ORDER BY observed_at_ms, id LIMIT ?2)",
            params![cutoff_ms, limit],
        )?;
        u64::try_from(deleted).map_err(|_| Error::Config("GC batch exceeds u64".to_owned()))
    })
}

/// One observed run in the report period.
pub(super) struct Run {
    pub(super) project_id: String,
    conversation_id: String,
    hidden_reason: Option<String>,
    note_delivered: bool,
    observed_at_ms: i64,
    /// When the run's conversation received its first note, in any period.
    first_note_ms: Option<i64>,
}

/// The runs observed in `[since, until)`, each with its conversation's first
/// note time. A note before the period still makes later runs "after".
pub(super) fn load(connection: &Connection, since: i64, until: i64) -> Result<Vec<Run>> {
    let mut statement = connection.prepare(
        "SELECT v.project_id, v.conversation_id, v.hidden_reason, v.note_delivered,
                v.observed_at_ms,
                (SELECT min(n.observed_at_ms) FROM verification_observations n
                 WHERE n.conversation_id = v.conversation_id AND n.note_delivered = 1)
         FROM verification_observations v
         WHERE v.observed_at_ms >= ?1 AND v.observed_at_ms < ?2
         ORDER BY v.observed_at_ms, v.id",
    )?;
    let runs = statement
        .query_map(params![since, until], |row| {
            Ok(Run {
                project_id: row.get(0)?,
                conversation_id: row.get(1)?,
                hidden_reason: row.get(2)?,
                note_delivered: row.get(3)?,
                observed_at_ms: row.get(4)?,
                first_note_ms: row.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(runs)
}

/// Summarize the given runs.
pub(super) fn summarize<'a>(runs: impl IntoIterator<Item = &'a Run>) -> VerificationStatus {
    let mut status = VerificationStatus::default();
    let mut noted = std::collections::BTreeSet::new();
    for run in runs {
        let recordable = run.hidden_reason.is_none();
        let group = match run.first_note_ms {
            None => &mut status.without_note,
            Some(first) if run.observed_at_ms <= first => &mut status.before_note,
            Some(_) => &mut status.after_note,
        };
        for share in [group, &mut status.runs] {
            share.runs += 1;
            share.status_bearing += u64::from(recordable);
        }
        if let Some(reason) = &run.hidden_reason {
            *status.hidden_by_reason.entry(reason.clone()).or_default() += 1;
        }
        if run.note_delivered {
            status.notes += 1;
            noted.insert(run.conversation_id.as_str());
        }
    }
    status.noted_conversations = u64::try_from(noted.len()).unwrap_or(u64::MAX);
    for share in [
        &mut status.runs,
        &mut status.before_note,
        &mut status.after_note,
        &mut status.without_note,
    ] {
        share.share = ratio(share.status_bearing, share.runs);
    }
    status
}

impl VerificationStatus {
    /// Terminal summary lines, empty without observed runs. Unknown shares
    /// print as unknown, never zero.
    #[must_use]
    pub fn summary_lines(&self) -> Vec<String> {
        if self.runs.runs == 0 {
            return Vec::new();
        }
        let share = |share: &StatusShare| {
            format!(
                "{} of {} ({})",
                share.status_bearing,
                share.runs,
                percent(share.share)
            )
        };
        let mut lines = vec![format!(
            "Verification runs with a recordable exit status: {}; status notes {} in {} conversations",
            share(&self.runs),
            self.notes,
            self.noted_conversations
        )];
        if self.before_note.runs > 0 {
            lines.push(format!(
                "  in noted conversations, up to the first note: {}; after it: {}",
                share(&self.before_note),
                share(&self.after_note)
            ));
        }
        if !self.hidden_by_reason.is_empty() {
            lines.push(format!(
                "  hidden by: {}",
                self.hidden_by_reason
                    .iter()
                    .map(|(reason, runs)| format!("{reason} {runs}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        lines
    }
}

#[allow(clippy::cast_precision_loss)]
fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
    (denominator > 0).then(|| numerator as f64 / denominator as f64)
}

fn percent(value: Option<f64>) -> String {
    value.map_or_else(
        || "unknown".to_owned(),
        |value| format!("{:.1}%", value * 100.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _temp: tempfile::TempDir,
        database: Database,
        store: EffectivenessStore,
        app: ProjectId,
        lib: ProjectId,
        task: TaskId,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().expect("tempdir");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let (app, lib, task) = (ProjectId::new(), ProjectId::new(), TaskId::new());
        database
            .write_immediate(|tx| {
                for (project, name) in [(&app, "app"), (&lib, "lib")] {
                    tx.execute(
                        "INSERT INTO projects (id,name,created_at_ms) VALUES (?1,?2,1)",
                        params![project.as_str(), name],
                    )?;
                }
                tx.execute(
                    "INSERT INTO tasks (id,project_id,session_id,prompt,prompt_digest,status,started_at_ms)
                     VALUES (?1,?2,'s1','synthetic task','digest','open',1)",
                    params![task.as_str(), app.as_str()],
                )?;
                Ok(())
            })
            .expect("fixtures");
        Fixture {
            _temp: temp,
            store: EffectivenessStore::new(database.clone()),
            database,
            app,
            lib,
            task,
        }
    }

    fn run(
        project: &ProjectId,
        conversation: &str,
        call: &str,
        hidden: Option<StatusHidden>,
        note_delivered: bool,
    ) -> VerificationObservationRequest {
        VerificationObservationRequest {
            project_id: project.clone(),
            session_id: conversation
                .split('-')
                .next()
                .unwrap_or(conversation)
                .to_owned(),
            tool_use_id: call.to_owned(),
            conversation_id: conversation.to_owned(),
            task_id: None,
            source: "claude_code".to_owned(),
            kind: "test_result".to_owned(),
            hidden,
            note_delivered,
        }
    }

    fn report(fixture: &Fixture, project: Option<&ProjectId>, since: i64) -> VerificationStatus {
        fixture
            .database
            .with_reader(|connection| {
                let runs = load(connection, since, 1_000)?;
                Ok(summarize(runs.iter().filter(|run| {
                    project.is_none_or(|project| run.project_id == project.as_str())
                })))
            })
            .expect("report")
    }

    #[test]
    fn shares_split_each_conversation_at_its_first_note() {
        use StatusHidden::{Or, Pipe, Sequence};
        let f = fixture();
        for (at, project, conversation, call, hidden, noted) in [
            // s1 pipes twice before its note, then mostly records.
            (10, &f.app, "s1", "c1", Some(Pipe), false),
            (20, &f.app, "s1", "c2", Some(Pipe), true),
            (30, &f.app, "s1", "c3", None, false),
            (40, &f.app, "s1", "c4", Some(Sequence), true),
            (50, &f.lib, "s1", "c5", None, false),
            // A subagent thread of s1 has its own conversation and note.
            (35, &f.app, "s1-worker", "w1", Some(Or), true),
            // s2 never needed a note; s3 ran with notes off.
            (15, &f.app, "s2", "d1", None, false),
            (25, &f.app, "s3", "e1", Some(Pipe), false),
        ] {
            f.store
                .record_verification_observation_at(
                    &run(project, conversation, call, hidden, noted),
                    at,
                )
                .expect("record");
        }
        let all = report(&f, None, 0);
        assert_eq!((all.runs.runs, all.runs.status_bearing), (8, 3));
        assert_eq!((all.notes, all.noted_conversations), (3, 2));
        assert_eq!(
            (all.before_note.runs, all.before_note.status_bearing),
            (3, 0)
        );
        assert_eq!((all.after_note.runs, all.after_note.status_bearing), (3, 2));
        assert_eq!(
            (all.without_note.runs, all.without_note.status_bearing),
            (2, 1)
        );
        assert_eq!(all.after_note.share, Some(2.0 / 3.0));
        assert_eq!(
            all.hidden_by_reason,
            BTreeMap::from([
                ("or".to_owned(), 1),
                ("pipe".to_owned(), 3),
                ("sequence".to_owned(), 1)
            ])
        );
        // A conversation's note applies across repositories and before the
        // period: lib's run after s1's note is "after".
        let lib = report(&f, Some(&f.lib), 0);
        assert_eq!((lib.after_note.runs, lib.notes), (1, 0));
        let late = report(&f, Some(&f.app), 30);
        assert_eq!((late.before_note.runs, late.after_note.runs), (1, 2));
        assert_eq!(late.before_note.status_bearing, 0);
        let lines = all.summary_lines();
        assert!(lines[0].contains("3 of 8 (37.5%)"), "{lines:?}");
        assert!(lines[1].contains("after it: 2 of 3 (66.7%)"), "{lines:?}");
        assert!(lines[2].contains("pipe 3"), "{lines:?}");
        assert!(report(&f, None, 900).summary_lines().is_empty());
    }

    #[test]
    fn observations_merge_per_call_and_check_their_scope() {
        let f = fixture();
        let mut first = run(&f.app, "s1", "c1", Some(StatusHidden::Pipe), false);
        first.task_id = Some(f.task.clone());
        assert!(
            f.store
                .record_verification_observation_at(&first, 20)
                .expect("first")
        );
        let mut again = first.clone();
        again.note_delivered = true;
        assert!(
            !f.store
                .record_verification_observation_at(&again, 10)
                .expect("merge")
        );
        let (notes, observed): (i64, i64) = f
            .database
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT note_delivered, observed_at_ms FROM verification_observations",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            })
            .expect("row");
        assert_eq!((notes, observed), (1, 10));
        // A note needs a hidden status; kinds are the recognized ones.
        let mut rejected = vec![run(&f.app, "s1", "c2", None, true)];
        let mut unknown_kind = run(&f.app, "s1", "c3", None, false);
        unknown_kind.kind = "format_result".to_owned();
        rejected.push(unknown_kind);
        // The task must belong to the project and the session.
        let mut foreign_task = run(&f.lib, "s1", "c4", None, false);
        foreign_task.task_id = Some(f.task.clone());
        rejected.push(foreign_task);
        let mut other_session = run(&f.app, "s2", "c5", None, false);
        other_session.task_id = Some(f.task.clone());
        rejected.push(other_session);
        rejected.push(run(&ProjectId::new(), "s1", "c6", None, false));
        rejected.push(run(&f.app, "s1", "", None, false));
        for request in rejected {
            assert!(
                f.store
                    .record_verification_observation_at(&request, 30)
                    .is_err(),
                "{request:?}"
            );
        }
        let deleted = delete_verification_observation_batch(&f.database, 15, 10).expect("delete");
        assert_eq!(deleted, 1);
    }
}
