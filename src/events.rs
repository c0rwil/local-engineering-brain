//! Typed, immutable events and transactional materialized projections.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::ids::{AttemptId, EvidenceId, MemoryId, ProjectId, TaskId};
use crate::storage::Database;

pub const CURRENT_EVENT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DomainEvent {
    ProjectRegistered {
        project_id: ProjectId,
        name: String,
        canonical_remote: Option<String>,
        root_commit: Option<String>,
        parent_project_id: Option<ProjectId>,
    },
    ProjectAliasAdded {
        project_id: ProjectId,
        path: String,
    },
    ProjectTombstoned {
        project_id: ProjectId,
    },
    ProjectLinked {
        project_id: ProjectId,
        linked_project_id: ProjectId,
        relation: String,
        note: Option<String>,
    },
    ProjectUnlinked {
        project_id: ProjectId,
        linked_project_id: ProjectId,
    },
    ProjectFactSet {
        project_id: ProjectId,
        key: String,
        value: String,
        note: Option<String>,
    },
    ProjectFactUnset {
        project_id: ProjectId,
        key: String,
    },
    /// Git identity observed for a project registered without it (for
    /// example while Git was unreadable). Fills only missing fields; never
    /// changes the project id or overwrites recorded identity.
    ProjectIdentityObserved {
        project_id: ProjectId,
        canonical_remote: Option<String>,
        root_commit: Option<String>,
    },
    StructuralScanApplied {
        project_id: ProjectId,
        scan_id: String,
        provider: String,
        head: Option<String>,
        diff_fingerprint: String,
        file_count: u64,
        node_count: u64,
        edge_count: u64,
        incremental: bool,
    },
    GitHistoryIngested {
        project_id: ProjectId,
        head: Option<String>,
        commit_count: u64,
        truncated: bool,
    },
    TaskStarted {
        project_id: ProjectId,
        task_id: TaskId,
        session_id: Option<String>,
        prompt_digest: String,
    },
    AttemptRecorded {
        project_id: ProjectId,
        task_id: TaskId,
        attempt_id: AttemptId,
        ordinal: u32,
        outcome: String,
    },
    EvidenceObserved {
        project_id: ProjectId,
        task_id: Option<TaskId>,
        evidence_id: EvidenceId,
        kind: String,
        strength: String,
        artifact_digest: String,
    },
    TaskPromptPurged {
        project_id: ProjectId,
        task_id: TaskId,
    },
    TaskFinished {
        project_id: ProjectId,
        task_id: TaskId,
        outcome: String,
        accepted_by_user: Option<bool>,
        final_commit: Option<String>,
    },
    MemoryCreated {
        project_id: ProjectId,
        memory_id: MemoryId,
        kind: String,
        claim_digest: String,
        epistemic_state: String,
    },
    MemoryStateChanged {
        project_id: ProjectId,
        memory_id: MemoryId,
        epistemic_state: String,
        status: String,
        reason: String,
    },
    MemoryPurged {
        project_id: ProjectId,
        memory_id: MemoryId,
    },
    LearningDistilled {
        project_id: ProjectId,
        memory_id: MemoryId,
        source_task_ids: Vec<TaskId>,
        reused_existing: bool,
    },
    ProcedureSynthesized {
        project_id: ProjectId,
        memory_id: MemoryId,
        signature: String,
        source_task_ids: Vec<TaskId>,
        cross_project_hypothesis: bool,
    },
    /// `retired_project_id` was merged into `project_id` (the survivor). The
    /// retired project stays as a tombstone that names the survivor; its
    /// checkout paths, links and facts move as the projection documents.
    ProjectsMerged {
        project_id: ProjectId,
        retired_project_id: ProjectId,
        /// Git identity recorded for the survivor where it had none.
        canonical_remote: Option<String>,
        root_commit: Option<String>,
        /// The retired project's origin view, now paired with the survivor.
        repaired_view: Option<ProjectId>,
        /// Origin views tombstoned with the retired project.
        retired_views: Vec<ProjectId>,
        audit: ProjectMergeAudit,
    },
}

/// Audit record of one project merge: what moved, what was dropped for a
/// rebuild, and how every key collision was resolved.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ProjectMergeAudit {
    /// Transactionally consistent backup taken before the merge.
    pub backup_path: Option<String>,
    /// Checkout paths moved from the retired project to the survivor.
    pub moved_aliases: Vec<String>,
    /// Rows re-scoped to the survivor, per table.
    pub moved: BTreeMap<String, u64>,
    /// Derived rows dropped and rebuilt for the survivor, per table.
    pub rebuilt: BTreeMap<String, u64>,
    /// Rows that stay with the retired id as history, per table.
    pub kept: BTreeMap<String, u64>,
    pub collisions: Vec<MergeCollision>,
    pub facts: Vec<MergedFact>,
    pub links: Vec<MergedLink>,
    /// Memories of both projects with the same normalized claim. Both stay.
    pub duplicate_claims: Vec<DuplicateClaim>,
    /// Memory anchors re-attached to the survivor's symbols in the merge.
    pub anchors_remapped: u64,
    /// Memory anchors waiting for the survivor's re-index.
    pub anchors_pending: u64,
    /// Product records re-anchored to the survivor (each got a revision).
    #[serde(default)]
    pub product_records: Vec<String>,
    /// Unfinished jobs of the retired project, cancelled by the merge.
    pub cancelled_jobs: Vec<String>,
}

/// A unique key held by rows of both projects, and what the merge did.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct MergeCollision {
    pub table: String,
    /// The colliding key columns as `column=value`, comma separated.
    pub key: String,
    pub resolution: String,
}

/// A fact of the retired project and what the merge did with it.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct MergedFact {
    pub key: String,
    pub retired_value: String,
    pub survivor_value: Option<String>,
    pub decision: String,
}

/// A link of the retired project (either direction) and what the merge did.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct MergedLink {
    pub project_id: ProjectId,
    pub linked_project_id: ProjectId,
    pub relation: String,
    pub decision: String,
}

/// Two memories, one per project, with the same normalized claim.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct DuplicateClaim {
    pub survivor_memory_id: MemoryId,
    pub retired_memory_id: MemoryId,
}

impl DomainEvent {
    #[must_use]
    pub const fn event_type(&self) -> &'static str {
        match self {
            Self::ProjectRegistered { .. } => "PROJECT_REGISTERED",
            Self::ProjectAliasAdded { .. } => "PROJECT_ALIAS_ADDED",
            Self::ProjectTombstoned { .. } => "PROJECT_TOMBSTONED",
            Self::ProjectLinked { .. } => "PROJECT_LINKED",
            Self::ProjectUnlinked { .. } => "PROJECT_UNLINKED",
            Self::ProjectFactSet { .. } => "PROJECT_FACT_SET",
            Self::ProjectFactUnset { .. } => "PROJECT_FACT_UNSET",
            Self::ProjectIdentityObserved { .. } => "PROJECT_IDENTITY_OBSERVED",
            Self::StructuralScanApplied { .. } => "STRUCTURAL_SCAN_APPLIED",
            Self::GitHistoryIngested { .. } => "GIT_HISTORY_INGESTED",
            Self::TaskStarted { .. } => "TASK_STARTED",
            Self::AttemptRecorded { .. } => "ATTEMPT_RECORDED",
            Self::EvidenceObserved { .. } => "EVIDENCE_OBSERVED",
            Self::TaskPromptPurged { .. } => "TASK_PROMPT_PURGED",
            Self::TaskFinished { .. } => "TASK_FINISHED",
            Self::MemoryCreated { .. } => "MEMORY_CREATED",
            Self::MemoryStateChanged { .. } => "MEMORY_STATE_CHANGED",
            Self::MemoryPurged { .. } => "MEMORY_PURGED",
            Self::LearningDistilled { .. } => "LEARNING_DISTILLED",
            Self::ProcedureSynthesized { .. } => "PROCEDURE_SYNTHESIZED",
            Self::ProjectsMerged { .. } => "PROJECTS_MERGED",
        }
    }

    #[must_use]
    pub fn project_id(&self) -> &ProjectId {
        match self {
            Self::ProjectRegistered { project_id, .. }
            | Self::ProjectAliasAdded { project_id, .. }
            | Self::ProjectTombstoned { project_id }
            | Self::ProjectLinked { project_id, .. }
            | Self::ProjectUnlinked { project_id, .. }
            | Self::ProjectFactSet { project_id, .. }
            | Self::ProjectFactUnset { project_id, .. }
            | Self::ProjectIdentityObserved { project_id, .. }
            | Self::StructuralScanApplied { project_id, .. }
            | Self::GitHistoryIngested { project_id, .. }
            | Self::TaskStarted { project_id, .. }
            | Self::AttemptRecorded { project_id, .. }
            | Self::EvidenceObserved { project_id, .. }
            | Self::TaskPromptPurged { project_id, .. }
            | Self::TaskFinished { project_id, .. }
            | Self::MemoryCreated { project_id, .. }
            | Self::MemoryStateChanged { project_id, .. }
            | Self::MemoryPurged { project_id, .. }
            | Self::LearningDistilled { project_id, .. }
            | Self::ProcedureSynthesized { project_id, .. }
            | Self::ProjectsMerged { project_id, .. } => project_id,
        }
    }

    fn decode(event_type: &str, schema_version: u32, payload: &str) -> Result<Self> {
        if schema_version == 0 {
            return Err(Error::MissingEventVersion);
        }
        if schema_version != CURRENT_EVENT_SCHEMA_VERSION {
            return Err(Error::UnknownEventVersion {
                event_type: event_type.to_owned(),
                version: schema_version,
            });
        }
        match event_type {
            "PROJECT_REGISTERED"
            | "PROJECT_ALIAS_ADDED"
            | "PROJECT_TOMBSTONED"
            | "PROJECT_LINKED"
            | "PROJECT_UNLINKED"
            | "PROJECT_FACT_SET"
            | "PROJECT_FACT_UNSET"
            | "PROJECT_IDENTITY_OBSERVED"
            | "STRUCTURAL_SCAN_APPLIED"
            | "GIT_HISTORY_INGESTED"
            | "TASK_STARTED"
            | "ATTEMPT_RECORDED"
            | "EVIDENCE_OBSERVED"
            | "TASK_PROMPT_PURGED"
            | "TASK_FINISHED"
            | "MEMORY_CREATED"
            | "MEMORY_STATE_CHANGED"
            | "MEMORY_PURGED"
            | "LEARNING_DISTILLED"
            | "PROCEDURE_SYNTHESIZED"
            | "PROJECTS_MERGED" => {
                let event: Self = serde_json::from_str(payload)?;
                if event.event_type() != event_type {
                    return Err(Error::UnknownEventType {
                        event_type: format!(
                            "stored type {event_type} disagrees with payload type {}",
                            event.event_type()
                        ),
                    });
                }
                Ok(event)
            }
            _ => Err(Error::UnknownEventType {
                event_type: event_type.to_owned(),
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EventEnvelope {
    pub sequence: i64,
    pub id: String,
    pub project_id: ProjectId,
    pub event_type: String,
    pub schema_version: u32,
    pub event: DomainEvent,
    pub created_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppendOutcome {
    Appended { sequence: i64 },
    AlreadyPresent { sequence: i64 },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectProjection {
    pub projects: BTreeMap<ProjectId, ProjectProjectionEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectProjectionEntry {
    pub name: String,
    pub canonical_remote: Option<String>,
    pub root_commit: Option<String>,
    pub parent_project_id: Option<ProjectId>,
    pub aliases: Vec<String>,
    pub tombstoned: bool,
    /// The survivor this project was merged into.
    pub merged_into: Option<ProjectId>,
}

#[derive(Clone, Debug)]
pub struct EventStore {
    database: Database,
}

impl EventStore {
    #[must_use]
    pub const fn new(database: Database) -> Self {
        Self { database }
    }

    #[must_use]
    pub const fn database(&self) -> &Database {
        &self.database
    }

    /// Append a typed event and update its projection atomically.
    ///
    /// # Errors
    ///
    /// Returns an error when serialization, storage, or projection validation fails.
    pub fn append(&self, event: &DomainEvent) -> Result<EventEnvelope> {
        let id = format!("evt_{}", Uuid::new_v4());
        self.append_with_id(&id, CURRENT_EVENT_SCHEMA_VERSION, event)?;
        self.get(&id)?.ok_or_else(|| {
            Error::Config(format!(
                "event `{id}` disappeared after a successful commit"
            ))
        })
    }

    /// Append with a caller-provided ID, allowing safe idempotent retries.
    ///
    /// # Errors
    ///
    /// Returns an error for missing/unknown versions, ID conflicts, or failed projections.
    pub fn append_with_id(
        &self,
        id: &str,
        schema_version: u32,
        event: &DomainEvent,
    ) -> Result<AppendOutcome> {
        self.append_with_id_before_commit(id, schema_version, event, || Ok(()))
    }

    fn append_with_id_before_commit(
        &self,
        id: &str,
        schema_version: u32,
        event: &DomainEvent,
        before_commit: impl FnOnce() -> Result<()>,
    ) -> Result<AppendOutcome> {
        if schema_version == 0 {
            return Err(Error::MissingEventVersion);
        }
        if schema_version != CURRENT_EVENT_SCHEMA_VERSION {
            return Err(Error::UnknownEventVersion {
                event_type: event.event_type().to_owned(),
                version: schema_version,
            });
        }

        let payload = serde_json::to_string(&event)?;
        let event_type = event.event_type();
        let project_id = event.project_id();
        let created_at_ms = unix_time_ms();
        self.database.write_immediate(|transaction| {
            let existing = transaction
                .query_row(
                    "SELECT sequence, project_id, event_type, schema_version, payload_json
                     FROM events WHERE id = ?1",
                    [id],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, u32>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    },
                )
                .optional()?;

            if let Some((sequence, old_project, old_type, old_version, old_payload)) = existing {
                if old_project == project_id.as_str()
                    && old_type == event_type
                    && old_version == schema_version
                    && old_payload == payload
                {
                    return Ok(AppendOutcome::AlreadyPresent { sequence });
                }
                return Err(Error::DuplicateEvent(id.to_owned()));
            }

            transaction.execute(
                "INSERT INTO events
                 (id, project_id, event_type, schema_version, payload_json, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id,
                    project_id.as_str(),
                    event_type,
                    schema_version,
                    payload,
                    created_at_ms
                ],
            )?;
            let sequence = transaction.last_insert_rowid();
            apply_project_projection(transaction, event, created_at_ms)?;
            before_commit()?;
            Ok(AppendOutcome::Appended { sequence })
        })
    }

    /// Hold the real append path immediately before commit for crash testing.
    ///
    /// This hidden test seam never returns; a parent process kills it after the
    /// readiness marker is durable.
    ///
    /// # Errors
    ///
    /// Returns an error if the append path or readiness marker fails before the wait loop.
    #[doc(hidden)]
    pub fn hold_uncommitted_test_append(&self, ready_path: &Path) -> Result<()> {
        let project_id = ProjectId::parse("prj_00000000-0000-4000-8000-000000000000")?;
        let event = DomainEvent::ProjectRegistered {
            project_id,
            name: "crash-probe".to_owned(),
            canonical_remote: None,
            root_commit: None,
            parent_project_id: None,
        };
        self.append_with_id_before_commit(
            "evt_00000000-0000-4000-8000-000000000000",
            CURRENT_EVENT_SCHEMA_VERSION,
            &event,
            || {
                let ready = fs::File::create(ready_path)
                    .map_err(|source| crate::error::io_error(ready_path, source))?;
                ready
                    .sync_all()
                    .map_err(|source| crate::error::io_error(ready_path, source))?;
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            },
        )?;
        Ok(())
    }

    /// Fetch and validate one event.
    ///
    /// # Errors
    ///
    /// Returns an error for storage failures or an unsupported persisted event schema.
    pub fn get(&self, id: &str) -> Result<Option<EventEnvelope>> {
        self.database.with_reader(|connection| {
            let raw = connection
                .query_row(
                    "SELECT sequence, id, project_id, event_type, schema_version,
                        payload_json, created_at_ms
                 FROM events WHERE id = ?1",
                    [id],
                    raw_event_from_row,
                )
                .optional()?;
            raw.map(RawEvent::decode).transpose()
        })
    }

    /// Read all events in canonical sequence order.
    ///
    /// # Errors
    ///
    /// Returns an error for storage failures or an unsupported persisted event schema.
    pub fn replay(&self) -> Result<Vec<EventEnvelope>> {
        self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT sequence, id, project_id, event_type, schema_version,
                        payload_json, created_at_ms
                 FROM events ORDER BY sequence",
            )?;
            let rows = statement.query_map([], raw_event_from_row)?;
            let mut events = Vec::new();
            for row in rows {
                events.push(row?.decode()?);
            }
            Ok(events)
        })
    }

    /// Rebuild the pure project projection from immutable history.
    ///
    /// # Errors
    ///
    /// Returns an error when the event stream cannot be read or decoded.
    pub fn replay_project_projection(&self) -> Result<ProjectProjection> {
        let mut projection = ProjectProjection::default();
        for envelope in self.replay()? {
            match envelope.event {
                DomainEvent::ProjectRegistered {
                    project_id,
                    name,
                    canonical_remote,
                    root_commit,
                    parent_project_id,
                } => {
                    projection.projects.insert(
                        project_id,
                        ProjectProjectionEntry {
                            name,
                            canonical_remote,
                            root_commit,
                            parent_project_id,
                            aliases: Vec::new(),
                            tombstoned: false,
                            merged_into: None,
                        },
                    );
                }
                DomainEvent::ProjectAliasAdded { project_id, path } => {
                    if let Some(project) = projection.projects.get_mut(&project_id) {
                        if !project.aliases.contains(&path) {
                            project.aliases.push(path);
                            project.aliases.sort();
                        }
                    }
                }
                DomainEvent::ProjectTombstoned { project_id } => {
                    if let Some(project) = projection.projects.get_mut(&project_id) {
                        project.tombstoned = true;
                    }
                }
                DomainEvent::ProjectIdentityObserved {
                    project_id,
                    canonical_remote,
                    root_commit,
                } => {
                    if let Some(project) = projection.projects.get_mut(&project_id) {
                        if project.canonical_remote.is_none() {
                            project.canonical_remote = canonical_remote;
                        }
                        if project.root_commit.is_none() {
                            project.root_commit = root_commit;
                        }
                    }
                }
                DomainEvent::ProjectsMerged {
                    project_id,
                    retired_project_id,
                    canonical_remote,
                    root_commit,
                    repaired_view: _,
                    retired_views,
                    audit: _,
                } => projection.apply_merge(
                    &project_id,
                    &retired_project_id,
                    canonical_remote,
                    root_commit,
                    &retired_views,
                ),
                DomainEvent::ProjectLinked { .. }
                | DomainEvent::ProjectUnlinked { .. }
                | DomainEvent::ProjectFactSet { .. }
                | DomainEvent::ProjectFactUnset { .. }
                | DomainEvent::StructuralScanApplied { .. }
                | DomainEvent::GitHistoryIngested { .. }
                | DomainEvent::TaskStarted { .. }
                | DomainEvent::AttemptRecorded { .. }
                | DomainEvent::EvidenceObserved { .. }
                | DomainEvent::TaskPromptPurged { .. }
                | DomainEvent::TaskFinished { .. }
                | DomainEvent::MemoryCreated { .. }
                | DomainEvent::MemoryStateChanged { .. }
                | DomainEvent::MemoryPurged { .. }
                | DomainEvent::LearningDistilled { .. }
                | DomainEvent::ProcedureSynthesized { .. } => {}
            }
        }
        Ok(projection)
    }
}

impl ProjectProjection {
    /// The pure counterpart of the PROJECTS_MERGED projection: the retired
    /// project becomes a tombstone naming the survivor, its checkout paths
    /// move, its origin views are tombstoned, forks and earlier merges point
    /// at the survivor, and missing survivor identity is filled.
    fn apply_merge(
        &mut self,
        survivor: &ProjectId,
        retired: &ProjectId,
        canonical_remote: Option<String>,
        root_commit: Option<String>,
        retired_views: &[ProjectId],
    ) {
        let active = |projection: &Self, id: &ProjectId| {
            projection
                .projects
                .get(id)
                .is_some_and(|entry| !entry.tombstoned)
        };
        // The materialized projection refuses such an event, so it is never
        // stored; mirror that instead of applying half of it.
        if survivor == retired || !active(self, survivor) || !active(self, retired) {
            return;
        }
        let moved = self.projects.get_mut(retired).map(|entry| {
            entry.tombstoned = true;
            entry.merged_into = Some(survivor.clone());
            std::mem::take(&mut entry.aliases)
        });
        for view in retired_views {
            if let Some(entry) = self.projects.get_mut(view) {
                entry.tombstoned = true;
            }
        }
        for (id, entry) in &mut self.projects {
            if entry.merged_into.as_ref() == Some(retired) {
                entry.merged_into = Some(survivor.clone());
            }
            if entry.parent_project_id.as_ref() == Some(retired) {
                entry.parent_project_id = (id != survivor).then(|| survivor.clone());
            }
        }
        if let Some(entry) = self.projects.get_mut(survivor) {
            entry.aliases.extend(moved.unwrap_or_default());
            entry.aliases.sort();
            entry.aliases.dedup();
            if entry.canonical_remote.is_none() {
                entry.canonical_remote = canonical_remote;
            }
            if entry.root_commit.is_none() {
                entry.root_commit = root_commit;
            }
        }
    }
}

#[derive(Debug)]
struct RawEvent {
    sequence: i64,
    id: String,
    project_id: String,
    event_type: String,
    schema_version: u32,
    payload: String,
    created_at_ms: i64,
}

impl RawEvent {
    fn decode(self) -> Result<EventEnvelope> {
        let event = DomainEvent::decode(&self.event_type, self.schema_version, &self.payload)?;
        let project_id = ProjectId::parse(&self.project_id)?;
        if event.project_id() != &project_id {
            return Err(Error::EventScopeMismatch {
                envelope_project_id: project_id.to_string(),
                payload_project_id: event.project_id().to_string(),
            });
        }
        Ok(EventEnvelope {
            sequence: self.sequence,
            id: self.id,
            project_id,
            event_type: self.event_type,
            schema_version: self.schema_version,
            event,
            created_at_ms: self.created_at_ms,
        })
    }
}

fn raw_event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawEvent> {
    Ok(RawEvent {
        sequence: row.get(0)?,
        id: row.get(1)?,
        project_id: row.get(2)?,
        event_type: row.get(3)?,
        schema_version: row.get(4)?,
        payload: row.get(5)?,
        created_at_ms: row.get(6)?,
    })
}

#[allow(clippy::too_many_lines)]
fn apply_project_projection(
    transaction: &Transaction<'_>,
    event: &DomainEvent,
    created_at_ms: i64,
) -> Result<()> {
    match event {
        DomainEvent::ProjectRegistered {
            project_id,
            name,
            canonical_remote,
            root_commit,
            parent_project_id,
        } => {
            transaction.execute(
                "INSERT INTO projects
                 (id, name, canonical_remote, root_commit, parent_project_id, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    project_id.as_str(),
                    name,
                    canonical_remote,
                    root_commit,
                    parent_project_id.as_ref().map(ProjectId::as_str),
                    created_at_ms
                ],
            )?;
        }
        DomainEvent::ProjectAliasAdded { project_id, path } => {
            transaction.execute(
                "INSERT INTO project_aliases(project_id, path, observed_at_ms)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(project_id, path) DO UPDATE SET observed_at_ms = excluded.observed_at_ms",
                params![project_id.as_str(), path, created_at_ms],
            )?;
        }
        DomainEvent::ProjectTombstoned { project_id } => {
            transaction.execute(
                "UPDATE projects SET tombstoned_at_ms = ?2
                 WHERE id = ?1 AND tombstoned_at_ms IS NULL",
                params![project_id.as_str(), created_at_ms],
            )?;
        }
        DomainEvent::ProjectIdentityObserved {
            project_id,
            canonical_remote,
            root_commit,
        } => {
            transaction.execute(
                "UPDATE projects
                 SET canonical_remote = COALESCE(canonical_remote, ?2),
                     root_commit = COALESCE(root_commit, ?3)
                 WHERE id = ?1",
                params![project_id.as_str(), canonical_remote, root_commit],
            )?;
        }
        DomainEvent::ProjectLinked {
            project_id,
            linked_project_id,
            relation,
            note,
        } => {
            transaction.execute(
                "INSERT INTO project_links(project_id, linked_project_id, relation, note, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(project_id, linked_project_id)
                 DO UPDATE SET relation = excluded.relation, note = excluded.note",
                params![
                    project_id.as_str(),
                    linked_project_id.as_str(),
                    relation,
                    note,
                    created_at_ms
                ],
            )?;
        }
        DomainEvent::ProjectUnlinked {
            project_id,
            linked_project_id,
        } => {
            transaction.execute(
                "DELETE FROM project_links WHERE project_id = ?1 AND linked_project_id = ?2",
                params![project_id.as_str(), linked_project_id.as_str()],
            )?;
        }
        DomainEvent::ProjectFactSet {
            project_id,
            key,
            value,
            note,
        } => {
            transaction.execute(
                "INSERT INTO project_facts(project_id, key, value, note, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(project_id, key)
                 DO UPDATE SET value = excluded.value, note = excluded.note,
                               updated_at_ms = excluded.updated_at_ms",
                params![project_id.as_str(), key, value, note, created_at_ms],
            )?;
        }
        DomainEvent::ProjectFactUnset { project_id, key } => {
            transaction.execute(
                "DELETE FROM project_facts WHERE project_id = ?1 AND key = ?2",
                params![project_id.as_str(), key],
            )?;
        }
        DomainEvent::ProjectsMerged {
            project_id,
            retired_project_id,
            canonical_remote,
            root_commit,
            repaired_view,
            retired_views,
            audit: _,
        } => apply_merge_projection(
            transaction,
            &MergeProjection {
                survivor: project_id,
                retired: retired_project_id,
                canonical_remote: canonical_remote.as_deref(),
                root_commit: root_commit.as_deref(),
                repaired_view: repaired_view.as_ref(),
                retired_views,
            },
            created_at_ms,
        )?,
        DomainEvent::StructuralScanApplied { .. }
        | DomainEvent::GitHistoryIngested { .. }
        | DomainEvent::TaskStarted { .. }
        | DomainEvent::AttemptRecorded { .. }
        | DomainEvent::EvidenceObserved { .. }
        | DomainEvent::TaskPromptPurged { .. }
        | DomainEvent::TaskFinished { .. }
        | DomainEvent::MemoryCreated { .. }
        | DomainEvent::MemoryStateChanged { .. }
        | DomainEvent::MemoryPurged { .. }
        | DomainEvent::LearningDistilled { .. }
        | DomainEvent::ProcedureSynthesized { .. } => {}
    }
    Ok(())
}

/// The project-projection part of a PROJECTS_MERGED event.
struct MergeProjection<'a> {
    survivor: &'a ProjectId,
    retired: &'a ProjectId,
    canonical_remote: Option<&'a str>,
    root_commit: Option<&'a str>,
    repaired_view: Option<&'a ProjectId>,
    retired_views: &'a [ProjectId],
}

/// Materialize a merge in the project projection. Every rule depends only on
/// the projection state the earlier events produced, so replaying the log
/// reproduces it:
///
/// - checkout paths move to the survivor (a path belongs to one project);
/// - links: links between the two are dropped, the retired project's links
///   in either direction are re-pointed at the survivor, and a link the
///   survivor already has wins over the retired project's copy;
/// - facts: the survivor's value wins; `internal.*` facts never move;
/// - origin views: `repaired_view` is paired with the survivor and every view
///   in `retired_views` is tombstoned;
/// - forks and earlier merges of the retired project point at the survivor;
/// - the retired project is tombstoned with `merged_into` naming the
///   survivor, then missing survivor identity is filled.
fn apply_merge_projection(
    transaction: &Transaction<'_>,
    merge: &MergeProjection<'_>,
    now: i64,
) -> Result<()> {
    let survivor = merge.survivor.as_str();
    let retired = merge.retired.as_str();
    if survivor == retired {
        return Err(Error::Config(format!(
            "project `{survivor}` cannot be merged into itself"
        )));
    }
    for id in [survivor, retired] {
        let active: Option<bool> = transaction
            .query_row(
                "SELECT tombstoned_at_ms IS NULL FROM projects WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        match active {
            Some(true) => {}
            Some(false) => {
                return Err(Error::Config(format!(
                    "project `{id}` is retired and cannot take part in a merge"
                )));
            }
            None => return Err(Error::Config(format!("unknown project `{id}`"))),
        }
    }
    transaction.execute(
        "UPDATE project_aliases SET project_id = ?1 WHERE project_id = ?2",
        params![survivor, retired],
    )?;
    transaction.execute(
        "DELETE FROM project_links
         WHERE (project_id = ?1 AND linked_project_id = ?2)
            OR (project_id = ?2 AND linked_project_id = ?1)",
        params![survivor, retired],
    )?;
    transaction.execute(
        "INSERT OR IGNORE INTO project_links
             (project_id, linked_project_id, relation, note, created_at_ms)
         SELECT ?1, linked_project_id, relation, note, created_at_ms
         FROM project_links WHERE project_id = ?2",
        params![survivor, retired],
    )?;
    transaction.execute(
        "DELETE FROM project_links WHERE project_id = ?1",
        [retired],
    )?;
    transaction.execute(
        "INSERT OR IGNORE INTO project_links
             (project_id, linked_project_id, relation, note, created_at_ms)
         SELECT project_id, ?1, relation, note, created_at_ms
         FROM project_links WHERE linked_project_id = ?2",
        params![survivor, retired],
    )?;
    transaction.execute(
        "DELETE FROM project_links WHERE linked_project_id = ?1",
        [retired],
    )?;
    transaction.execute(
        "INSERT OR IGNORE INTO project_facts(project_id, key, value, note, updated_at_ms)
         SELECT ?1, key, value, note, updated_at_ms
         FROM project_facts WHERE project_id = ?2 AND substr(key, 1, 9) <> 'internal.'",
        params![survivor, retired],
    )?;
    transaction.execute(
        "DELETE FROM project_facts WHERE project_id = ?1",
        [retired],
    )?;
    if let Some(view) = merge.repaired_view {
        transaction.execute(
            "INSERT INTO project_facts(project_id, key, value, note, updated_at_ms)
             VALUES (?1, 'internal.origin_view', ?2, NULL, ?3)
             ON CONFLICT(project_id, key)
             DO UPDATE SET value = excluded.value, updated_at_ms = excluded.updated_at_ms",
            params![survivor, view.as_str(), now],
        )?;
        transaction.execute(
            "UPDATE project_facts SET value = ?1, updated_at_ms = ?3
             WHERE project_id = ?2 AND key = 'internal.view_of'",
            params![survivor, view.as_str(), now],
        )?;
    }
    for view in merge.retired_views {
        transaction.execute(
            "UPDATE projects SET tombstoned_at_ms = ?2
             WHERE id = ?1 AND tombstoned_at_ms IS NULL",
            params![view.as_str(), now],
        )?;
    }
    transaction.execute(
        "UPDATE projects SET parent_project_id = CASE WHEN id = ?1 THEN NULL ELSE ?1 END
         WHERE parent_project_id = ?2",
        params![survivor, retired],
    )?;
    transaction.execute(
        "UPDATE projects SET merged_into = ?1 WHERE merged_into = ?2",
        params![survivor, retired],
    )?;
    transaction.execute(
        "UPDATE projects SET tombstoned_at_ms = ?3, merged_into = ?1 WHERE id = ?2",
        params![survivor, retired, now],
    )?;
    transaction.execute(
        "UPDATE projects
         SET canonical_remote = COALESCE(canonical_remote, ?2),
             root_commit = COALESCE(root_commit, ?3)
         WHERE id = ?1",
        params![survivor, merge.canonical_remote, merge.root_commit],
    )?;
    Ok(())
}

/// Append a generated event inside a domain service's existing transaction.
///
/// This keeps non-project projections and their immutable audit event atomic.
pub(crate) fn append_generated_in_transaction(
    transaction: &Transaction<'_>,
    event: &DomainEvent,
    created_at_ms: i64,
) -> Result<()> {
    append_generated_with_id_in_transaction(
        transaction,
        &format!("evt_{}", Uuid::new_v4()),
        event,
        created_at_ms,
    )
}

/// [`append_generated_in_transaction`] with an id the caller chose, so rows
/// written in the same transaction can refer to the event.
pub(crate) fn append_generated_with_id_in_transaction(
    transaction: &Transaction<'_>,
    id: &str,
    event: &DomainEvent,
    created_at_ms: i64,
) -> Result<()> {
    let payload = serde_json::to_string(event)?;
    transaction.execute(
        "INSERT INTO events
         (id, project_id, event_type, schema_version, payload_json, created_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id,
            event.project_id().as_str(),
            event.event_type(),
            CURRENT_EVENT_SCHEMA_VERSION,
            payload,
            created_at_ms
        ],
    )?;
    apply_project_projection(transaction, event, created_at_ms)
}

fn unix_time_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn store() -> (TempDir, EventStore) {
        let temp = TempDir::new().expect("temp dir");
        let db = Database::open(temp.path().join("brain.sqlite")).expect("database");
        (temp, EventStore::new(db))
    }

    fn registered(project_id: &str) -> DomainEvent {
        DomainEvent::ProjectRegistered {
            project_id: ProjectId::parse(project_id).expect("project id"),
            name: "sample".to_owned(),
            canonical_remote: None,
            root_commit: None,
            parent_project_id: None,
        }
    }

    #[test]
    fn test_event_schema_version_required() {
        let (_temp, store) = store();
        let error = store
            .append_with_id(
                "evt_missing",
                0,
                &registered("prj_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
            )
            .expect_err("version zero rejected");
        assert!(matches!(error, Error::MissingEventVersion));
    }

    #[test]
    fn test_event_ids_unique() {
        let (_temp, store) = store();
        let first = store
            .append(&registered("prj_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"))
            .expect("first");
        let second = store
            .append(&registered("prj_bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"))
            .expect("second");
        assert_ne!(first.id, second.id);
    }

    #[test]
    fn test_append_order_monotonic() {
        let (_temp, store) = store();
        let first = store
            .append(&registered("prj_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"))
            .expect("first");
        let second = store
            .append(&registered("prj_bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"))
            .expect("second");
        assert!(second.sequence > first.sequence);
    }

    #[test]
    fn test_event_payload_immutable() {
        let (_temp, store) = store();
        let event = store
            .append(&registered("prj_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"))
            .expect("event");
        let connection = store.database().connect().expect("connection");
        let result = connection.execute(
            "UPDATE events SET payload_json = '{}' WHERE id = ?1",
            [&event.id],
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_replay_reconstructs_projection() {
        let (_temp, store) = store();
        let project_id =
            ProjectId::parse("prj_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").expect("project id");
        store
            .append(&registered(project_id.as_str()))
            .expect("registered");
        store
            .append(&DomainEvent::ProjectAliasAdded {
                project_id: project_id.clone(),
                path: "/tmp/a".to_owned(),
            })
            .expect("alias");
        let replayed = store.replay_project_projection().expect("projection");
        let project = replayed.projects.get(&project_id).expect("project");
        assert_eq!(project.aliases, ["/tmp/a"]);
    }

    #[test]
    fn test_duplicate_event_id_is_idempotent_or_rejected() {
        let (_temp, store) = store();
        let event = registered("prj_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        let first = store.append_with_id("evt_fixed", 1, &event).expect("first");
        let duplicate = store
            .append_with_id("evt_fixed", 1, &event)
            .expect("idempotent");
        assert_eq!(first, AppendOutcome::Appended { sequence: 1 });
        assert_eq!(duplicate, AppendOutcome::AlreadyPresent { sequence: 1 });

        let error = store
            .append_with_id(
                "evt_fixed",
                1,
                &registered("prj_bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"),
            )
            .expect_err("different content rejected");
        assert!(matches!(error, Error::DuplicateEvent(_)));
    }

    #[test]
    fn test_unknown_event_version_fails_explicitly() {
        let (_temp, store) = store();
        let error = store
            .append_with_id(
                "evt_future",
                2,
                &registered("prj_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
            )
            .expect_err("future version rejected");
        assert!(matches!(error, Error::UnknownEventVersion { .. }));
    }

    #[test]
    fn test_transaction_rolls_back_event_and_projection_together() {
        let (_temp, store) = store();
        let project_a =
            ProjectId::parse("prj_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").expect("project a");
        let project_b =
            ProjectId::parse("prj_bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").expect("project b");
        store
            .append(&registered(project_a.as_str()))
            .expect("registered");
        store
            .append(&DomainEvent::ProjectAliasAdded {
                project_id: project_a,
                path: "/same/path".to_owned(),
            })
            .expect("first alias");
        store
            .append(&registered(project_b.as_str()))
            .expect("registered b");
        let before = store.replay().expect("before").len();
        let result = store.append(&DomainEvent::ProjectAliasAdded {
            project_id: project_b,
            path: "/same/path".to_owned(),
        });
        assert!(result.is_err());
        assert_eq!(store.replay().expect("after").len(), before);
    }

    #[test]
    fn test_tombstone_event_preserves_audit_history() {
        let (_temp, store) = store();
        let project_id =
            ProjectId::parse("prj_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").expect("project id");
        store
            .append(&registered(project_id.as_str()))
            .expect("registered");
        store
            .append(&DomainEvent::ProjectTombstoned {
                project_id: project_id.clone(),
            })
            .expect("tombstone");
        let events = store.replay().expect("events");
        assert_eq!(events.len(), 2);
        assert!(
            store
                .replay_project_projection()
                .expect("projection")
                .projects[&project_id]
                .tombstoned
        );
    }

    #[test]
    fn event_envelope_scope_must_match_typed_payload() {
        let (_temp, store) = store();
        let payload_project =
            ProjectId::parse("prj_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").expect("payload project");
        let envelope_project =
            ProjectId::parse("prj_bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").expect("envelope project");
        let event = DomainEvent::ProjectRegistered {
            project_id: payload_project,
            name: "mismatched".to_owned(),
            canonical_remote: None,
            root_commit: None,
            parent_project_id: None,
        };
        store
            .database()
            .connect()
            .expect("connection")
            .execute(
                "INSERT INTO events
                 (id, project_id, event_type, schema_version, payload_json, created_at_ms)
                 VALUES ('evt_scope_mismatch', ?1, 'PROJECT_REGISTERED', 1, ?2, 0)",
                params![
                    envelope_project.as_str(),
                    serde_json::to_string(&event).expect("payload")
                ],
            )
            .expect("raw mismatched event");
        let error = store.replay().expect_err("scope mismatch rejected");
        assert!(matches!(error, Error::EventScopeMismatch { .. }));
    }
}
