//! Versioned product intent, contracts and living documentation.
//!
//! These records are cited knowledge, never supporting evidence for memory.
//! All writes enter through the daemon's single-writer scheduler.

mod context;
pub mod lessons;
pub use context::{Bundle, BundleRequest, Impact, ImpactRequest};

use std::collections::BTreeSet;
use std::path::{Component, Path};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::ids::{EvidenceId, NodeId, ProjectId};
use crate::security::Redactor;
use crate::storage::Database;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    #[default]
    Declared,
    Observed,
    Inferred,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Product,
    Capability,
    Flow,
    Service,
    Repository,
    Api,
    Queue,
    Database,
    Schema,
    Deployment,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    Implements,
    Calls,
    Publishes,
    Consumes,
    Reads,
    Writes,
    UsesSchema,
    Deploys,
    DependsOn,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Anchor {
    pub project_id: ProjectId,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub node_id: Option<NodeId>,
    #[serde(default)]
    pub evidence_id: Option<EvidenceId>,
    /// Optional revision assertion, checked against the source at capture.
    #[serde(default)]
    pub revision: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SourceStamp {
    pub anchor: Anchor,
    pub digest: Option<String>,
    pub revision: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FlowStep {
    pub entity_id: String,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub anchors: Vec<Anchor>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entity {
    pub kind: EntityKind,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub intent: String,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub basis: Basis,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub anchors: Vec<Anchor>,
    #[serde(default)]
    pub steps: Vec<FlowStep>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub source_id: String,
    pub target_id: String,
    pub relation: Relation,
    /// Endpoint, event or schema name. Multiple contracts per pair are allowed.
    pub contract: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub basis: Basis,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub anchors: Vec<Anchor>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub entity_id: String,
    pub title: String,
    #[serde(default)]
    pub curated: String,
    /// Derived output, kept separately from the human explanation.
    #[serde(default)]
    pub generated: String,
    #[serde(default)]
    pub basis: Basis,
    /// Explicit review attestation. No sources means freshness remains unknown.
    #[serde(default)]
    pub reviewed: bool,
    #[serde(default)]
    pub anchors: Vec<Anchor>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    pub entity_id: String,
    pub project_id: ProjectId,
    pub environment: String,
    pub revision: String,
    pub evidence_id: EvidenceId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Guide {
    pub content: String,
    pub dossier_digest: String,
    pub model: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Item {
    Entity(Entity),
    Dependency(Dependency),
    Section(Section),
    Deployment(Deployment),
    Guide(Guide),
}

impl Item {
    fn kind(&self) -> &'static str {
        match self {
            Self::Entity(_) => "entity",
            Self::Dependency(_) => "dependency",
            Self::Section(_) => "section",
            Self::Deployment(_) => "deployment",
            Self::Guide(_) => "guide",
        }
    }

    fn anchors(&self) -> Vec<Anchor> {
        match self {
            Self::Entity(e) => e
                .anchors
                .iter()
                .chain(e.steps.iter().flat_map(|s| &s.anchors))
                .cloned()
                .collect(),
            Self::Dependency(d) => d.anchors.clone(),
            Self::Section(s) => s.anchors.clone(),
            Self::Deployment(d) => vec![Anchor {
                project_id: d.project_id.clone(),
                path: None,
                node_id: None,
                evidence_id: Some(d.evidence_id.clone()),
                revision: Some(d.revision.clone()),
            }],
            Self::Guide(_) => Vec::new(),
        }
    }

    fn basis(&self) -> Basis {
        match self {
            Self::Entity(e) => e.basis,
            Self::Dependency(d) => d.basis,
            Self::Section(s) => s.basis,
            Self::Deployment(_) => Basis::Observed,
            Self::Guide(_) => Basis::Inferred,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Verified,
    NeedsReview,
    Inferred,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Record {
    pub id: String,
    pub version: u64,
    pub item: Item,
    pub stamps: Vec<SourceStamp>,
    pub archived: bool,
    pub updated_at_ms: i64,
    pub freshness: Freshness,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Mutation {
    Save {
        id: Option<String>,
        expected_version: u64,
        item: Box<Item>,
    },
    Archive {
        id: String,
        expected_version: u64,
    },
    /// Rebuild only a section's derived source summary, preserving curated text.
    RefreshSection {
        id: String,
        expected_version: u64,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Query {
    Snapshot,
    Schema,
    Reviews,
    TaskSignals { task_id: crate::ids::TaskId },
    Guide,
    Source { anchor: Anchor },
    History { id: String },
    Bundle(BundleRequest),
    Impact(ImpactRequest),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Snapshot {
    pub records: Vec<Record>,
    pub repositories: Vec<Repository>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Repository {
    pub id: String,
    pub project_id: ProjectId,
    pub name: String,
    pub head: Option<String>,
    pub derived: bool,
}

#[derive(Clone)]
pub struct ProductStore {
    pub(crate) database: Database,
    redactor: Redactor,
}

impl ProductStore {
    #[must_use]
    pub const fn new(database: Database, redactor: Redactor) -> Self {
        Self { database, redactor }
    }

    /// Execute the common query contract used by CLI, MCP and web.
    /// # Errors
    /// Returns validation or storage errors.
    pub fn query(&self, query: &Query) -> Result<serde_json::Value> {
        Ok(match query {
            Query::TaskSignals { task_id } => self.task_signals(task_id)?,
            Query::Schema => {
                serde_json::from_str(include_str!("../../assets/product-schema.json"))?
            }
            Query::Source { anchor } => self.source(anchor)?,
            Query::Guide => serde_json::to_value(
                self.database
                    .with_reader(|c| read_record(c, "workspace-guide"))?,
            )?,
            Query::Reviews => serde_json::to_value(self.reviews()?)?,
            Query::Snapshot => serde_json::to_value(self.snapshot()?)?,
            Query::History { id } => serde_json::to_value(self.history(id)?)?,
            Query::Bundle(request) => serde_json::to_value(self.bundle(request)?)?,
            Query::Impact(request) => serde_json::to_value(self.impact(request)?)?,
        })
    }

    /// Inspect a source anchor using canonical indexed data, never arbitrary files.
    /// # Errors
    /// Returns missing/scope validation or storage errors.
    pub fn source(&self, anchor: &Anchor) -> Result<serde_json::Value> {
        let stamp = self.database.with_reader(|c| capture(c, anchor, false))?;
        if let Some(id) = &anchor.evidence_id {
            let evidence = crate::experience::ExperienceStore::with_redactor(
                self.database.clone(),
                self.redactor.clone(),
            )
            .get_evidence(id)?;
            return Ok(serde_json::json!({"stamp":stamp,"evidence":evidence}));
        }
        let nodes=self.database.with_reader(|c| {
            let mut s=c.prepare("SELECT id,label,kind,file_path,start_byte,end_byte FROM nodes WHERE project_id=?1 AND tombstoned_at_ms IS NULL AND ((?2 IS NOT NULL AND file_path=?2) OR (?3 IS NOT NULL AND id=?3)) ORDER BY start_byte,id LIMIT 100")?;
            let rows=s.query_map(params![anchor.project_id.as_str(),anchor.path,anchor.node_id.as_ref().map(NodeId::as_str)],|r|Ok(serde_json::json!({"id":r.get::<_,String>(0)?,"label":r.get::<_,String>(1)?,"kind":r.get::<_,String>(2)?,"path":r.get::<_,Option<String>>(3)?,"start_byte":r.get::<_,Option<i64>>(4)?,"end_byte":r.get::<_,Option<i64>>(5)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
            Ok(rows)
        })?;
        Ok(serde_json::json!({"stamp":stamp,"symbols":nodes}))
    }

    /// Read one coherent knowledge snapshot, including per-source freshness.
    /// # Errors
    /// Returns storage or invalid persisted-data errors.
    pub fn snapshot(&self) -> Result<Snapshot> {
        self.database.with_reader(|connection| {
            let transaction = connection.unchecked_transaction()?;
            let snapshot = snapshot(&transaction)?;
            transaction.commit()?;
            Ok(snapshot)
        })
    }

    /// Save an optimistic, versioned change and its audit revision atomically.
    /// # Errors
    /// Returns validation, conflicting-version or storage errors.
    pub fn mutate(&self, mutation: Mutation) -> Result<Record> {
        self.database.write_immediate(|tx| {
            let (id, expected, mut item, archived) = match mutation {
                Mutation::Save { id, expected_version, item } => (id.unwrap_or_else(|| format!("cat_{}", Uuid::new_v4())), expected_version, *item, false),
                Mutation::Archive { id, expected_version } => {
                    let old = read_record(tx, &id)?.ok_or_else(|| invalid("record does not exist"))?;
                    (id, expected_version, old.item, true)
                },
                Mutation::RefreshSection { id, expected_version } => {
                    let old = read_record(tx, &id)?.ok_or_else(|| invalid("section does not exist"))?;
                    let Item::Section(mut section) = old.item else { return Err(invalid("record is not a section")); };
                    section.generated = source_summary(tx, &section.anchors)?;
                    section.reviewed = false;
                    for anchor in &mut section.anchors { anchor.revision = None; }
                    (id, expected_version, Item::Section(section), false)
                },
            };
            validate_id(&id)?;
            let old = read_record(tx, &id)?;
            if old.as_ref().map_or(0, |r| r.version) != expected {
                return Err(invalid("record changed since you loaded it; reload before saving"));
            }
            if old.as_ref().is_some_and(|r| r.item.kind() != item.kind()) {
                return Err(invalid("a stable record id cannot change its type"));
            }
            item = serde_json::from_value(self.redactor.redact_json(&serde_json::to_value(item)?))?;
            let serialized = serde_json::to_string(&item)?;
            if serialized.len() > 262_144 { return Err(invalid("record exceeds 256 KiB")); }
            if !archived { validate_item(tx, &id, &item)?; }
            let stamps: Vec<SourceStamp> = if archived { old.as_ref().map(|r| r.stamps.clone()).unwrap_or_default() } else { item.anchors().iter().map(|a| capture(tx, a, true)).collect::<Result<_>>()? };
            let version = expected.checked_add(1).filter(|v| i64::try_from(*v).is_ok()).ok_or_else(|| invalid("version overflow"))?;
            let sql_version = i64::try_from(version).map_err(|_| invalid("version overflow"))?;
            let now = now_ms();
            let stamps_json = serde_json::to_string(&stamps)?;
            tx.execute("INSERT INTO product_records(id,kind,version,document_json,stamps_json,archived,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(id) DO UPDATE SET version=excluded.version,document_json=excluded.document_json,stamps_json=excluded.stamps_json,archived=excluded.archived,updated_at_ms=excluded.updated_at_ms", params![id, item.kind(), sql_version, serialized, stamps_json, archived, now])?;
            tx.execute("INSERT INTO product_revisions(record_id,version,document_json,stamps_json,archived,recorded_at_ms) VALUES(?1,?2,?3,?4,?5,?6)", params![id,sql_version,serialized,stamps_json,archived,now])?;
            let mut record = Record { id, version, item, stamps, archived, updated_at_ms: now, freshness: Freshness::Unknown };
            record.freshness = freshness(tx, &record)?;
            Ok(record)
        })
    }

    /// Refresh only derived section inventories whose anchored bytes changed.
    /// Human explanations survive; refreshed sections require explicit review.
    /// # Errors
    /// Returns storage or validation errors.
    pub fn refresh_changed_sections(&self, project_id: &ProjectId) -> Result<usize> {
        let records = self.snapshot()?.records;
        let mut count = 0;
        for record in records {
            let Item::Section(section) = &record.item else {
                continue;
            };
            if section.generated.is_empty() {
                continue;
            }
            let changed = self.database.with_reader(|c| {
                for stamp in &record.stamps {
                    if &stamp.anchor.project_id == project_id
                        && capture(c, &stamp.anchor, false)?.digest != stamp.digest
                    {
                        return Ok(true);
                    }
                }
                Ok(false)
            })?;
            if changed {
                self.mutate(Mutation::RefreshSection {
                    id: record.id,
                    expected_version: record.version,
                })?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Return up to 100 immutable revisions (newest first).
    /// # Errors
    /// Returns storage or invalid data errors.
    pub fn history(&self, id: &str) -> Result<Vec<Record>> {
        self.database.with_reader(|c| {
            let mut statement = c.prepare("SELECT record_id,version,document_json,stamps_json,archived,recorded_at_ms FROM product_revisions WHERE record_id=?1 ORDER BY version DESC LIMIT 100")?;
            let rows = statement.query_map([id], row_record)?.collect::<std::result::Result<Vec<_>,_>>()?;
            rows.into_iter().map(decode_record).collect()
        })
    }
}

type RecordRow = (String, i64, String, String, bool, i64);
fn row_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<RecordRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}
fn decode_record(row: RecordRow) -> Result<Record> {
    Ok(Record {
        id: row.0,
        version: u64::try_from(row.1).map_err(|_| invalid("invalid record version"))?,
        item: serde_json::from_str(&row.2)?,
        stamps: serde_json::from_str(&row.3)?,
        archived: row.4,
        updated_at_ms: row.5,
        freshness: Freshness::Unknown,
    })
}
fn read_record(c: &Connection, id: &str) -> Result<Option<Record>> {
    c.query_row("SELECT id,version,document_json,stamps_json,archived,updated_at_ms FROM product_records WHERE id=?1", [id], row_record).optional()?.map(decode_record).transpose()
}
fn snapshot(c: &Connection) -> Result<Snapshot> {
    let mut statement = c.prepare("SELECT id,version,document_json,stamps_json,archived,updated_at_ms FROM product_records WHERE archived=0 ORDER BY id LIMIT 10001")?;
    let rows = statement
        .query_map([], row_record)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if rows.len() > 10_000 {
        return Err(invalid(
            "catalog exceeds 10000 active records; archive unused records",
        ));
    }
    let records = rows
        .into_iter()
        .map(|row| {
            let mut r = decode_record(row)?;
            r.freshness = freshness(c, &r)?;
            Ok(r)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut statement = c.prepare("SELECT p.id,p.name,(SELECT head FROM structural_scans s WHERE s.project_id=p.id ORDER BY completed_at_ms DESC,rowid DESC LIMIT 1),EXISTS(SELECT 1 FROM project_facts f WHERE f.project_id=p.id AND f.key='internal.view_of') FROM projects p WHERE tombstoned_at_ms IS NULL AND NOT EXISTS(SELECT 1 FROM project_facts f WHERE f.project_id=p.id AND f.key='internal.hidden' AND f.value='true') ORDER BY p.name,p.id")?;
    let rows = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, bool>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let repositories = rows
        .into_iter()
        .map(|(id, name, head, derived)| {
            Ok(Repository {
                id: format!("repo:{id}"),
                project_id: ProjectId::parse(&id)?,
                name,
                head,
                derived,
            })
        })
        .collect::<Result<_>>()?;
    Ok(Snapshot {
        records,
        repositories,
    })
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
        || id.starts_with("repo:")
    {
        return Err(invalid(
            "id must be 1–128 letters, digits, underscore, dot, colon or dash; repo: is reserved",
        ));
    }
    Ok(())
}
fn active_entity(c: &Connection, id: &str) -> Result<()> {
    if let Some(project) = id.strip_prefix("repo:") {
        return require_project(c, &ProjectId::parse(project)?);
    }
    if read_record(c, id)?.is_some_and(|r| !r.archived && matches!(r.item, Item::Entity(_))) {
        Ok(())
    } else {
        Err(invalid(&format!("unknown entity {id}")))
    }
}
fn require_project(c: &Connection, id: &ProjectId) -> Result<()> {
    let exists: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id=?1 AND tombstoned_at_ms IS NULL)",
        [id.as_str()],
        |r| r.get(0),
    )?;
    if exists {
        Ok(())
    } else {
        Err(invalid("unknown or archived project"))
    }
}
fn validate_item(c: &Connection, id: &str, item: &Item) -> Result<()> {
    let anchors = item.anchors();
    if anchors.len() > 128 {
        return Err(invalid("at most 128 source anchors are allowed"));
    }
    if item.basis() == Basis::Observed && !anchors.iter().any(|a| a.evidence_id.is_some()) {
        return Err(invalid(
            "observed records require an evidence anchor; use declared for intent or inferred for a hypothesis",
        ));
    }
    if item.basis() == Basis::Observed {
        let mut independent = false;
        for anchor in &anchors {
            if let Some(evidence) = &anchor.evidence_id {
                independent |= c.query_row("SELECT EXISTS(SELECT 1 FROM verified_supporting_evidence v JOIN evidence e ON e.id=v.evidence_id WHERE v.evidence_id=?1 AND v.project_id=?2 AND e.kind != 'user_acceptance')",params![evidence.as_str(),anchor.project_id.as_str()],|r|r.get::<_,bool>(0))?;
            }
        }
        if !independent {
            return Err(invalid(
                "observed records require independent verification from a successful source task; generated or inferred claims are not evidence",
            ));
        }
    }
    match item {
        Item::Entity(e) => {
            nonempty(&e.name, "name")?;
            if e.constraints.len() > 32 || e.steps.len() > 64 {
                return Err(invalid(
                    "at most 32 constraints and 64 flow steps are allowed",
                ));
            }
            if let Some(p) = &e.project_id {
                require_project(c, p)?;
            }
            if let Some(parent) = &e.parent_id {
                active_entity(c, parent)?;
                let mut seen = BTreeSet::from([id.to_owned()]);
                let mut next = Some(parent.clone());
                while let Some(parent) = next {
                    if !seen.insert(parent.clone()) {
                        return Err(invalid("product hierarchy cannot contain cycles"));
                    }
                    next = read_record(c, &parent)?.and_then(|r| match r.item {
                        Item::Entity(e) => e.parent_id,
                        _ => None,
                    });
                }
            }
            if e.kind != EntityKind::Flow && !e.steps.is_empty() {
                return Err(invalid("only flows can contain ordered steps"));
            }
            for step in &e.steps {
                active_entity(c, &step.entity_id)?;
                if step.entity_id == id {
                    return Err(invalid("flow cannot contain itself"));
                }
                nonempty(&step.title, "step title")?;
            }
        }
        Item::Dependency(d) => {
            active_entity(c, &d.source_id)?;
            active_entity(c, &d.target_id)?;
            nonempty(&d.contract, "contract")?;
        }
        Item::Section(s) => {
            active_entity(c, &s.entity_id)?;
            nonempty(&s.title, "title")?;
        }
        Item::Deployment(d) => {
            active_entity(c, &d.entity_id)?;
            require_project(c, &d.project_id)?;
            let bound_project = if let Some(project) = d.entity_id.strip_prefix("repo:") {
                Some(ProjectId::parse(project)?)
            } else {
                read_record(c, &d.entity_id)?.and_then(|r| match r.item {
                    Item::Entity(e) => e.project_id,
                    _ => None,
                })
            };
            if bound_project.as_ref().is_some_and(|p| p != &d.project_id) {
                return Err(invalid(
                    "deployment project must match the entity's repository",
                ));
            }
            nonempty(&d.environment, "environment")?;
            nonempty(&d.revision, "revision")?;
            let valid: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM evidence WHERE id=?1 AND project_id=?2 AND commit_hash=?3 AND strength='observational' AND kind IN ('runtime_observation','trace','metric') AND (exit_code=0 OR exit_code IS NULL))", params![d.evidence_id.as_str(),d.project_id.as_str(),d.revision],|r|r.get(0))?;
            if !valid {
                return Err(invalid(
                    "deployment requires successful independent evidence for this project and exact revision",
                ));
            }
        }
        Item::Guide(g) => {
            if g.content.trim().is_empty() || g.content.len() > 262_144 {
                return Err(invalid("guide content must contain 1–262144 bytes"));
            }
            nonempty(&g.dossier_digest, "dossier digest")?;
        }
    }
    Ok(())
}
fn nonempty(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 4096 {
        Err(invalid(&format!("{field} must contain 1–4096 bytes")))
    } else {
        Ok(())
    }
}

fn capture(c: &Connection, anchor: &Anchor, enforce_revision: bool) -> Result<SourceStamp> {
    require_project(c, &anchor.project_id)?;
    if [
        &anchor.path.as_ref().map(|_| ()),
        &anchor.node_id.as_ref().map(|_| ()),
        &anchor.evidence_id.as_ref().map(|_| ()),
    ]
    .iter()
    .filter(|v| v.is_some())
    .count()
        != 1
    {
        return Err(invalid(
            "each anchor needs exactly one path, node_id or evidence_id",
        ));
    }
    let revision: Option<String> = c.query_row("SELECT head FROM structural_scans WHERE project_id=?1 ORDER BY completed_at_ms DESC,rowid DESC LIMIT 1",[anchor.project_id.as_str()],|r|r.get(0)).optional()?.flatten();
    let (digest, revision) = if let Some(path) = &anchor.path {
        if path.is_empty()
            || path.len() > 4096
            || Path::new(path).is_absolute()
            || Path::new(path)
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(invalid(
                "anchor path must be repository relative without ..",
            ));
        }
        let digest = c
            .query_row(
                "SELECT content_digest FROM indexed_files WHERE project_id=?1 AND path=?2",
                params![anchor.project_id.as_str(), path],
                |r| r.get(0),
            )
            .optional()?;
        (digest, revision)
    } else if let Some(node) = &anchor.node_id {
        let digest = c.query_row("SELECT current_digest FROM nodes WHERE project_id=?1 AND id=?2 AND tombstoned_at_ms IS NULL",params![anchor.project_id.as_str(),node.as_str()],|r|r.get(0)).optional()?;
        (digest, revision)
    } else if let Some(evidence) = &anchor.evidence_id {
        let data: Option<(Option<String>, Option<String>)> = c
            .query_row(
                "SELECT CASE WHEN artifact_text IS NOT NULL OR artifact_blob_digest IS NOT NULL THEN artifact_digest ELSE NULL END,commit_hash FROM evidence WHERE project_id=?1 AND id=?2",
                params![anchor.project_id.as_str(), evidence.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (digest, revision) = data
            .ok_or_else(|| invalid("evidence anchor is missing or belongs to another project"))?;
        (digest, revision)
    } else {
        unreachable!()
    };
    if enforce_revision
        && anchor
            .revision
            .as_ref()
            .is_some_and(|r| Some(r) != revision.as_ref())
    {
        return Err(invalid(
            "source revision does not match the indexed/evidence revision; index the selected checkout or use its actual revision",
        ));
    }
    Ok(SourceStamp {
        anchor: anchor.clone(),
        digest,
        revision,
    })
}
fn freshness(c: &Connection, record: &Record) -> Result<Freshness> {
    let inferred = record.item.basis() == Basis::Inferred;
    if record.stamps.is_empty() {
        return Ok(if inferred {
            Freshness::Inferred
        } else {
            Freshness::Unknown
        });
    }
    let mut unknown = false;
    for stamp in &record.stamps {
        let current = match capture(c, &stamp.anchor, false) {
            Ok(s) => s,
            Err(Error::Config(_)) => return Ok(Freshness::NeedsReview),
            Err(error) => return Err(error),
        };
        if stamp.digest.is_some() && current.digest != stamp.digest {
            return Ok(Freshness::NeedsReview);
        }
        if stamp.digest.is_none() || current.digest.is_none() {
            unknown = true;
        }
    }
    if inferred {
        return Ok(Freshness::Inferred);
    }
    if unknown {
        return Ok(Freshness::Unknown);
    }
    if matches!(&record.item,Item::Section(s) if !s.reviewed) {
        return Ok(Freshness::NeedsReview);
    }
    Ok(Freshness::Verified)
}
fn source_summary(c: &Connection, anchors: &[Anchor]) -> Result<String> {
    let mut lines =
        vec!["Derived source inventory. Behavior and intent require review.".to_owned()];
    for anchor in anchors {
        let source = capture(c, anchor, false)?;
        let label = anchor
            .path
            .clone()
            .or_else(|| anchor.node_id.as_ref().map(ToString::to_string))
            .or_else(|| anchor.evidence_id.as_ref().map(ToString::to_string))
            .unwrap_or_default();
        lines.push(format!(
            "- {} / {} @ {}: {}",
            anchor.project_id,
            label,
            source.revision.as_deref().unwrap_or("unknown revision"),
            source.digest.as_deref().unwrap_or("not indexed")
        ));
    }
    Ok(lines.join("\n"))
}
pub(crate) fn invalid(message: &str) -> Error {
    Error::Config(message.to_owned())
}
pub(crate) fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}
pub(crate) fn digest(text: &str) -> String {
    crate::experience::sha256(text.as_bytes())
}

#[cfg(test)]
mod tests;
