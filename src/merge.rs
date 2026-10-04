//! Merging one project into another (`engram project merge`).
//!
//! A repository registered twice has two project ids (`engram doctor` reports
//! it). A merge retires one id (the *retired* project) into the other (the
//! *survivor*):
//!
//! 1. Preconditions: both projects are active, not origin views, distinct,
//!    and a live checkout of each, readable by the daemon, reports the same
//!    canonical remote and root commit. There is no override.
//! 2. The daemon takes a transactionally consistent backup and, in the same
//!    scheduler job, runs ONE canonical transaction that re-scopes every
//!    project-keyed row by its [`TableRole`] and appends `PROJECTS_MERGED`.
//!    The tables come from the live schema: a project-keyed table without a
//!    role makes the merge refuse instead of leaving rows behind.
//! 3. Afterwards the client rewrites checkout pins, re-indexes the survivor
//!    over every checkout path and calls [`finalize`], which re-attaches
//!    memory anchors by stable symbol identity (or marks their memories for
//!    revalidation) and re-scopes rows written under the retired id since.
//!
//! The retired id stays as a tombstone naming its survivor
//! (`projects.merged_into`). The daemon redirects requests that still carry
//! it, and checkouts whose pin still holds it resolve to the survivor.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::events::{
    DomainEvent, DuplicateClaim, MergeCollision, MergedFact, MergedLink, ProjectMergeAudit,
    append_generated_in_transaction, append_generated_with_id_in_transaction,
};
use crate::ids::{MemoryId, ProjectId};
use crate::projects::{CheckoutState, ProjectRegistry, probe_checkout};
use crate::storage::Database;

pub mod client;
pub mod render;

/// Reason recorded when a memory loses a symbol anchor in a merge.
pub const ANCHOR_UNRESOLVED_REASON: &str = "merge_anchor_unresolved";

/// What a merge does with the rows of one project-keyed table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableRole {
    /// Project records changed by the `PROJECTS_MERGED` projection: checkout
    /// paths and links move, facts merge (survivor wins), the retired row
    /// becomes a tombstone naming the survivor.
    Projection,
    /// Rows move to the survivor. A unique key that includes the project id
    /// collides by the rule.
    Moved(CollisionRule),
    /// Derived from checkouts. The retired project's rows are dropped and the
    /// survivor is re-indexed over every checkout path after the merge.
    Rebuilt,
    /// Derived from moved rows and re-derived for the survivor inside the
    /// merge transaction.
    Rederived,
    /// Maintained by triggers on another table.
    Triggered,
    /// History that stays with the retired id.
    Kept,
    /// Owned through a project-owned parent row, which it follows.
    FollowsParent,
}

impl TableRole {
    /// The three outcomes a person reads: `moved`, `rebuilt` or `retired`.
    #[must_use]
    pub const fn class(self) -> &'static str {
        match self {
            Self::Projection | Self::Moved(_) | Self::FollowsParent => "moved",
            Self::Rebuilt | Self::Rederived | Self::Triggered => "rebuilt",
            Self::Kept => "retired",
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Projection => "project record",
            Self::Moved(_) => "moved",
            Self::Rebuilt => "dropped and re-indexed",
            Self::Rederived => "re-derived",
            Self::Triggered => "maintained by triggers",
            Self::Kept => "kept with the retired id",
            Self::FollowsParent => "follows its parent row",
        }
    }
}

/// How rows of both projects that share a unique key are resolved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CollisionRule {
    /// No unique key includes the project id, so rows cannot collide.
    Impossible,
    /// Keep both rows: the retired row's column gets a `~merged-…` suffix.
    RekeyRetired(&'static str),
    /// The same sample recorded under both projects: the survivor's row stays
    /// and the retired duplicate is dropped (recorded in the audit).
    KeepSurvivor,
    /// The same tool call observed under both projects: the survivor's row
    /// stays and takes the union of both rows' paths.
    UnionPaths,
}

impl CollisionRule {
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Impossible => "no unique key includes the project",
            Self::RekeyRetired(_) => "keep both; the retired row's key gets a merge suffix",
            Self::KeepSurvivor => "keep the survivor's row; drop the retired duplicate",
            Self::UnionPaths => "keep the survivor's row with the union of both rows' paths",
        }
    }
}

/// Every project-keyed table and its merge role. Rebuilt tables are listed
/// in deletion order, children first.
pub const TABLE_ROLES: &[(&str, TableRole)] = &[
    ("projects", TableRole::Projection),
    ("project_aliases", TableRole::Projection),
    ("project_links", TableRole::Projection),
    ("project_facts", TableRole::Projection),
    ("tasks", TableRole::Moved(CollisionRule::Impossible)),
    ("evidence", TableRole::Moved(CollisionRule::Impossible)),
    ("memories", TableRole::Moved(CollisionRule::Impossible)),
    (
        "lesson_reviews",
        TableRole::Moved(CollisionRule::Impossible),
    ),
    (
        "context_runs",
        TableRole::Moved(CollisionRule::RekeyRetired("request_id")),
    ),
    (
        "resource_usage",
        TableRole::Moved(CollisionRule::KeepSurvivor),
    ),
    (
        "tool_observations",
        TableRole::Moved(CollisionRule::UnionPaths),
    ),
    (
        "verification_observations",
        TableRole::Moved(CollisionRule::KeepSurvivor),
    ),
    (
        "project_merge_anchors",
        TableRole::Moved(CollisionRule::Impossible),
    ),
    ("edges", TableRole::Rebuilt),
    ("nodes", TableRole::Rebuilt),
    ("indexed_files", TableRole::Rebuilt),
    ("structural_scans", TableRole::Rebuilt),
    ("commits", TableRole::Rebuilt),
    ("source_node_fts", TableRole::Triggered),
    ("memories_fts", TableRole::Rederived),
    ("memory_vectors", TableRole::Rederived),
    ("events", TableRole::Kept),
    ("jobs", TableRole::Kept),
    ("attempts", TableRole::FollowsParent),
    ("memory_evidence", TableRole::FollowsParent),
    ("memory_sources", TableRole::FollowsParent),
    ("memory_nodes", TableRole::FollowsParent),
    ("context_items", TableRole::FollowsParent),
    ("usage_events", TableRole::FollowsParent),
    ("memory_compact_injections", TableRole::FollowsParent),
    ("lesson_judgments", TableRole::FollowsParent),
    ("task_signals", TableRole::FollowsParent),
    ("memory_expiry", TableRole::FollowsParent),
];

/// The role of `table`, when it has one.
#[must_use]
pub fn role_of(table: &str) -> Option<TableRole> {
    TABLE_ROLES
        .iter()
        .find(|(name, _)| *name == table)
        .map(|(_, role)| *role)
}

// ---------------------------------------------------------------------------
// Schema introspection
// ---------------------------------------------------------------------------

/// Tables that hold project-scoped rows: a `project_id` column, a foreign
/// key to `projects`, or a foreign key to such a table.
///
/// # Errors
///
/// Returns storage errors.
pub fn project_keyed_tables(connection: &Connection) -> Result<Vec<String>> {
    let names = {
        let mut statement = connection.prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut keyed = BTreeSet::from(["projects".to_owned()]);
    let mut parents = BTreeMap::new();
    for name in &names {
        if table_columns(connection, name)?
            .iter()
            .any(|column| column == "project_id")
        {
            keyed.insert(name.clone());
        }
        parents.insert(name.clone(), foreign_parents(connection, name)?);
    }
    loop {
        let before = keyed.len();
        for (name, references) in &parents {
            if references.iter().any(|parent| keyed.contains(parent)) {
                keyed.insert(name.clone());
            }
        }
        if keyed.len() == before {
            break;
        }
    }
    Ok(keyed.into_iter().collect())
}

/// Refuse when the schema has a project-keyed table without a merge role:
/// a merge would leave its rows under the retired id.
///
/// # Errors
///
/// Names every unclassified table.
pub fn verify_roles(connection: &Connection) -> Result<()> {
    let unclassified = project_keyed_tables(connection)?
        .into_iter()
        .filter(|table| role_of(table).is_none())
        .collect::<Vec<_>>();
    if unclassified.is_empty() {
        return Ok(());
    }
    Err(Error::MergeRefused(format!(
        "this build does not know how to merge the project-keyed tables {}; \
         nothing was changed",
        unclassified.join(", ")
    )))
}

fn table_columns(connection: &Connection, table: &str) -> Result<Vec<String>> {
    let mut statement = connection.prepare("SELECT name FROM pragma_table_info(?1)")?;
    let columns = statement
        .query_map([table], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(columns)
}

fn foreign_parents(connection: &Connection, table: &str) -> Result<Vec<String>> {
    let mut statement =
        connection.prepare("SELECT DISTINCT \"table\" FROM pragma_foreign_key_list(?1)")?;
    let parents = statement
        .query_map([table], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(parents)
}

/// Unique keys of `table` that include `project_id`, as their other columns.
///
/// # Errors
///
/// Returns storage errors.
pub fn project_unique_keys(connection: &Connection, table: &str) -> Result<Vec<Vec<String>>> {
    let indexes = {
        let mut statement =
            connection.prepare("SELECT name FROM pragma_index_list(?1) WHERE \"unique\" = 1")?;
        statement
            .query_map([table], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut keys = BTreeSet::new();
    for index in indexes {
        let columns = {
            let mut statement =
                connection.prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")?;
            statement
                .query_map([&index], |row| row.get::<_, Option<String>>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        // Expression indexes have no column names; they are not keys here.
        let Some(columns) = columns.into_iter().collect::<Option<Vec<_>>>() else {
            continue;
        };
        if columns.iter().any(|column| column == "project_id") {
            keys.insert(
                columns
                    .into_iter()
                    .filter(|column| column != "project_id")
                    .collect::<Vec<_>>(),
            );
        }
    }
    Ok(keys.into_iter().collect())
}

fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

// ---------------------------------------------------------------------------
// Parties, selectors and identity evidence
// ---------------------------------------------------------------------------

/// One side of a merge.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MergeParty {
    pub id: ProjectId,
    pub name: String,
    /// Hidden from the workspace browser (`internal.hidden`).
    pub hidden: bool,
    pub canonical_remote: Option<String>,
    pub root_commit: Option<String>,
    /// Recorded checkout paths, oldest first.
    pub checkouts: Vec<String>,
}

/// Lifecycle of a project row as a merge sees it.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ProjectState {
    Active,
    Merged(ProjectId),
    Tombstoned,
}

fn project_state(connection: &Connection, id: &ProjectId) -> Result<Option<ProjectState>> {
    let row: Option<(bool, Option<String>)> = connection
        .query_row(
            "SELECT tombstoned_at_ms IS NULL, merged_into FROM projects WHERE id = ?1",
            [id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(active, merged_into)| match (active, merged_into) {
        (true, _) => Ok(ProjectState::Active),
        (false, Some(survivor)) => ProjectId::parse(&survivor).map(ProjectState::Merged),
        (false, None) => Ok(ProjectState::Tombstoned),
    })
    .transpose()
}

fn is_view(connection: &Connection, id: &ProjectId) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM project_facts
                       WHERE project_id = ?1 AND key = 'internal.view_of')",
        [id.as_str()],
        |row| row.get(0),
    )?)
}

/// Load one side of a merge, active or not.
///
/// # Errors
///
/// Returns an error for unknown projects or storage failures.
pub fn party(connection: &Connection, id: &ProjectId) -> Result<MergeParty> {
    let (name, canonical_remote, root_commit, hidden) = connection
        .query_row(
            "SELECT p.name, p.canonical_remote, p.root_commit,
                    EXISTS(SELECT 1 FROM project_facts f WHERE f.project_id = p.id
                           AND f.key = 'internal.hidden' AND f.value = 'true')
             FROM projects p WHERE p.id = ?1",
            [id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| Error::MergeRefused(format!("unknown project `{id}`")))?;
    Ok(MergeParty {
        id: id.clone(),
        name,
        hidden,
        canonical_remote,
        root_commit,
        checkouts: alias_paths(connection, id)?,
    })
}

fn alias_paths(connection: &Connection, id: &ProjectId) -> Result<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT path FROM project_aliases WHERE project_id = ?1
         ORDER BY observed_at_ms ASC, path ASC",
    )?;
    let paths = statement
        .query_map([id.as_str()], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(paths)
}

/// Resolve a `--into`/`--from` selector: a project id, a project name, or a
/// checkout path. A name or path may select a retired (merged) project so a
/// repeated merge can report "already merged".
///
/// # Errors
///
/// Returns an error for unknown or ambiguous selectors.
pub fn resolve_selector(registry: &ProjectRegistry, selector: &str) -> Result<ProjectId> {
    let selector = selector.trim();
    if selector.is_empty() {
        return Err(Error::MergeRefused(
            "a project selector cannot be empty".to_owned(),
        ));
    }
    let database = registry.database();
    if let Ok(id) = ProjectId::parse(selector) {
        let known = database.with_reader(|connection| project_state(connection, &id))?;
        return known
            .map(|_| id.clone())
            .ok_or_else(|| Error::MergeRefused(format!("unknown project `{id}`")));
    }
    // Clients send existing checkout paths absolute; the daemon's working
    // directory means nothing to them.
    let path = std::path::Path::new(selector);
    if path.is_absolute() {
        return resolve_path_selector(registry, path);
    }
    let candidates = database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT id, tombstoned_at_ms IS NULL FROM projects
             WHERE name = ?1 OR lower(name) = lower(?1)
             ORDER BY name = ?1 DESC, tombstoned_at_ms IS NULL DESC, id",
        )?;
        let rows = statement
            .query_map([selector], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    })?;
    let active = candidates
        .iter()
        .filter(|(_, active)| *active)
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>();
    match active.as_slice() {
        [one] => return ProjectId::parse(one),
        [] => {}
        many => {
            return Err(Error::MergeRefused(format!(
                "`{selector}` names {} active projects ({}); select one by id",
                many.len(),
                many.join(", ")
            )));
        }
    }
    match candidates.as_slice() {
        [(only, _)] => ProjectId::parse(only),
        [] => Err(Error::MergeRefused(format!(
            "no project is named `{selector}` and no such checkout path exists"
        ))),
        _ => Err(Error::MergeRefused(format!(
            "`{selector}` names several retired projects; select one by id"
        ))),
    }
}

fn resolve_path_selector(registry: &ProjectRegistry, path: &std::path::Path) -> Result<ProjectId> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let text = canonical.to_string_lossy().into_owned();
    // A recorded checkout path first: it names its project even when the
    // checkout is gone.
    let recorded = registry.database().with_reader(|connection| {
        connection
            .query_row(
                "SELECT project_id FROM project_aliases WHERE path = ?1",
                [&text],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Error::from)
    })?;
    if let Some(id) = recorded {
        return ProjectId::parse(&id);
    }
    registry
        .lookup(&canonical)?
        .map(|project| project.id)
        .ok_or_else(|| {
            Error::MergeRefused(format!(
                "no registered project owns the checkout {}",
                canonical.display()
            ))
        })
}

/// What one recorded checkout path told the daemon about its repository.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CheckoutEvidence {
    pub project_id: ProjectId,
    pub path: String,
    /// `readable`, `missing`, `denied`, `failed`, `not_git`, `incomplete`
    /// or `pinned_elsewhere`. Only `readable` checkouts are evidence.
    pub state: String,
    pub pinned_project_id: Option<ProjectId>,
    pub canonical_remote: Option<String>,
    pub root_commit: Option<String>,
    pub detail: Option<String>,
}

/// Whether both projects are provably the same repository.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct IdentityEvidence {
    pub same_repository: bool,
    pub canonical_remote: Option<String>,
    pub root_commit: Option<String>,
    pub checkouts: Vec<CheckoutEvidence>,
    /// Why the merge is refused, when it is.
    pub refusal: Option<String>,
}

/// Probe every recorded checkout of both projects from this process (the
/// daemon) and decide whether they are the same repository: each needs a
/// readable Git checkout, every such checkout must report one canonical
/// remote and root commit, and recorded identity must agree with it.
#[must_use]
pub fn identity_evidence(survivor: &MergeParty, retired: &MergeParty) -> IdentityEvidence {
    let mut checkouts = Vec::new();
    for party in [survivor, retired] {
        for path in &party.checkouts {
            checkouts.push(probe(party, path));
        }
    }
    let readable = |party: &MergeParty| {
        checkouts
            .iter()
            .filter(|probe| probe.project_id == party.id && probe.state == "readable")
            .collect::<Vec<_>>()
    };
    let mut evidence = IdentityEvidence {
        same_repository: false,
        canonical_remote: None,
        root_commit: None,
        refusal: None,
        checkouts: Vec::new(),
    };
    for party in [survivor, retired] {
        if readable(party).is_empty() {
            let states = checkouts
                .iter()
                .filter(|probe| probe.project_id == party.id)
                .map(|probe| format!("{} {}", probe.path, probe.state))
                .collect::<Vec<_>>();
            evidence.refusal = Some(format!(
                "no checkout of {} ({}) that the daemon can read reports a Git origin remote \
                 and root commit{}; the merge needs live evidence that both projects are the \
                 same repository",
                party.name,
                party.id,
                if states.is_empty() {
                    " (it has no recorded checkout paths)".to_owned()
                } else {
                    format!(" ({})", states.join("; "))
                }
            ));
        }
    }
    let identities = checkouts
        .iter()
        .filter(|probe| probe.state == "readable")
        .filter_map(|probe| Some((probe.canonical_remote.clone()?, probe.root_commit.clone()?)))
        .collect::<BTreeSet<_>>();
    if evidence.refusal.is_none() && identities.len() > 1 {
        let listed = checkouts
            .iter()
            .filter(|probe| probe.state == "readable")
            .map(|probe| {
                format!(
                    "{} reports {} at root {}",
                    probe.path,
                    probe.canonical_remote.as_deref().unwrap_or("?"),
                    short(probe.root_commit.as_deref().unwrap_or("?"))
                )
            })
            .collect::<Vec<_>>();
        evidence.refusal = Some(format!(
            "the checkouts are not one repository: {}",
            listed.join("; ")
        ));
    }
    if evidence.refusal.is_none()
        && let Some((remote, root)) = identities.into_iter().next()
    {
        for party in [survivor, retired] {
            let disagreement = match (&party.canonical_remote, &party.root_commit) {
                (Some(recorded), _) if *recorded != remote => {
                    Some(format!("records remote {recorded}"))
                }
                (_, Some(recorded)) if *recorded != root => {
                    Some(format!("records root commit {}", short(recorded)))
                }
                _ => None,
            };
            if let Some(disagreement) = disagreement {
                evidence.refusal = Some(format!(
                    "{} ({}) {disagreement}, but its checkouts report {remote} at root {}",
                    party.name,
                    party.id,
                    short(&root)
                ));
            }
        }
        evidence.same_repository = evidence.refusal.is_none();
        evidence.canonical_remote = Some(remote);
        evidence.root_commit = Some(root);
    }
    evidence.checkouts = checkouts;
    evidence
}

fn probe(party: &MergeParty, path: &str) -> CheckoutEvidence {
    let mut evidence = CheckoutEvidence {
        project_id: party.id.clone(),
        path: path.to_owned(),
        state: String::new(),
        pinned_project_id: None,
        canonical_remote: None,
        root_commit: None,
        detail: None,
    };
    let (state, detail) = match probe_checkout(std::path::Path::new(path), true) {
        CheckoutState::Missing => ("missing", None),
        CheckoutState::Denied { reason, .. } => ("denied", Some(reason)),
        CheckoutState::Failed { reason } => ("failed", Some(reason)),
        CheckoutState::Readable { identity: None } => ("not_git", None),
        CheckoutState::Readable {
            identity: Some(identity),
        } => {
            evidence
                .pinned_project_id
                .clone_from(&identity.pinned_project_id);
            evidence
                .canonical_remote
                .clone_from(&identity.canonical_remote);
            evidence.root_commit.clone_from(&identity.root_commit);
            match &identity.pinned_project_id {
                Some(pinned) if *pinned != party.id => (
                    "pinned_elsewhere",
                    Some(format!("its project.toml pins {pinned}")),
                ),
                _ if identity.canonical_remote.is_none() => {
                    ("incomplete", Some("no origin remote".to_owned()))
                }
                _ if identity.root_commit.is_none() => {
                    ("incomplete", Some("no root commit".to_owned()))
                }
                _ => ("readable", None),
            }
        }
    };
    state.clone_into(&mut evidence.state);
    evidence.detail = detail;
    evidence
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

/// Git identity the survivor records after the merge where it has none.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct IdentityFill {
    pub canonical_remote: Option<String>,
    pub root_commit: Option<String>,
}

/// The identity a merge records for the survivor: missing fields only, and
/// the remote only when no other active project holds it.
fn identity_fill(
    connection: &Connection,
    survivor: &MergeParty,
    retired: &ProjectId,
    evidence: &IdentityEvidence,
) -> Result<IdentityFill> {
    let canonical_remote = match (&survivor.canonical_remote, &evidence.canonical_remote) {
        (None, Some(remote)) => {
            let held: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM projects WHERE canonical_remote = ?1
                               AND tombstoned_at_ms IS NULL AND id NOT IN (?2, ?3))",
                params![remote, survivor.id.as_str(), retired.as_str()],
                |row| row.get(0),
            )?;
            (!held).then(|| remote.clone())
        }
        _ => None,
    };
    let root_commit = survivor
        .root_commit
        .is_none()
        .then(|| evidence.root_commit.clone())
        .flatten();
    Ok(IdentityFill {
        canonical_remote,
        root_commit,
    })
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

/// One project-keyed table in a merge plan.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TablePlan {
    pub table: String,
    /// `moved`, `rebuilt` or `retired`.
    pub class: String,
    pub role: String,
    /// Rows of the retired project this role acts on.
    pub rows: u64,
    pub collisions: u64,
    pub collision_rule: Option<String>,
}

/// What happens to the retired project's origin views.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ViewPlan {
    /// Paired with the survivor, which has no origin view of its own.
    pub repaired: Option<ProjectId>,
    /// Tombstoned: the survivor keeps its own origin view.
    pub retired: Vec<ProjectId>,
}

/// Memory anchors on the retired project's symbols.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct AnchorPlan {
    pub total: u64,
    /// The survivor already indexes a symbol with the same stable identity.
    pub remap_now: u64,
    /// Resolved after the survivor's re-index, or marked for revalidation.
    pub after_reindex: u64,
}

/// A read-only description of a merge: what moves, what is dropped and
/// rebuilt, every key collision and its resolution. Deterministic for a
/// given database state.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MergePlan {
    pub survivor: MergeParty,
    pub retired: MergeParty,
    pub identity: IdentityEvidence,
    pub identity_fill: IdentityFill,
    pub tables: Vec<TablePlan>,
    pub collisions: Vec<MergeCollision>,
    pub duplicate_claims: Vec<DuplicateClaim>,
    pub facts: Vec<MergedFact>,
    pub links: Vec<MergedLink>,
    pub views: ViewPlan,
    pub anchors: AnchorPlan,
    /// Product records whose anchors name the retired project.
    pub product_records: Vec<String>,
    /// Unfinished jobs of the retired project that the merge cancels.
    pub cancelled_jobs: Vec<String>,
    /// The retired project's checkout paths, whose pins are rewritten.
    pub checkouts_to_repin: Vec<String>,
    /// The one checkout re-indexed after the merge (see [`primary_checkout`]).
    pub reindex_path: Option<String>,
}

/// Plan a merge of `retired` into `survivor`. Read-only; call it with a
/// connection inside one read transaction for a consistent picture.
///
/// # Errors
///
/// Refuses unclassified project-keyed tables, and returns storage errors.
pub fn plan(
    connection: &Connection,
    survivor: &MergeParty,
    retired: &MergeParty,
    identity: IdentityEvidence,
) -> Result<MergePlan> {
    verify_roles(connection)?;
    let (s, r) = (&survivor.id, &retired.id);
    let mut tables = Vec::new();
    let mut collisions = Vec::new();
    for table in project_keyed_tables(connection)? {
        let Some(role) = role_of(&table) else {
            continue;
        };
        let mut found = Vec::new();
        if let TableRole::Moved(rule) = role {
            for key in project_unique_keys(connection, &table)? {
                for row in colliding_rows(connection, &table, &key, s, r)? {
                    found.push(MergeCollision {
                        table: table.clone(),
                        key: row.key,
                        resolution: rule.describe().to_owned(),
                    });
                }
            }
        }
        tables.push(TablePlan {
            rows: role_rows(connection, &table, role, r)?,
            collisions: u64::try_from(found.len()).unwrap_or(u64::MAX),
            collision_rule: match role {
                TableRole::Moved(collision) if collision != CollisionRule::Impossible => {
                    Some(collision.describe().to_owned())
                }
                _ => None,
            },
            class: role.class().to_owned(),
            role: role.name().to_owned(),
            table,
        });
        collisions.extend(found);
    }
    collisions.sort();
    let views = view_plan(connection, s, r)?;
    Ok(MergePlan {
        identity_fill: identity_fill(connection, survivor, r, &identity)?,
        identity,
        tables,
        collisions,
        duplicate_claims: duplicate_claims(connection, s, r)?,
        facts: fact_decisions(connection, s, r)?,
        links: link_decisions(connection, s, r)?,
        views,
        anchors: anchor_plan(connection, s, r)?,
        product_records: product_records_naming(connection, s, r)?,
        cancelled_jobs: unfinished_jobs(connection, r)?,
        checkouts_to_repin: retired.checkouts.clone(),
        reindex_path: primary_checkout(&survivor.checkouts, &retired.checkouts),
        survivor: survivor.clone(),
        retired: retired.clone(),
    })
}

/// The checkout the survivor is re-indexed from after a merge: its oldest
/// existing checkout that is not a linked worktree, else its oldest existing
/// checkout, else the retired project's oldest existing checkout. One scan
/// of one tree: scanning the other checkouts too would run drift detection
/// against trees that differ from the survivor's (another branch, an older
/// clone) and quarantine memories that are fine. The other checkouts stay
/// recorded paths of the survivor.
#[must_use]
pub fn primary_checkout(survivor_paths: &[String], retired_paths: &[String]) -> Option<String> {
    let exists = |path: &&String| std::path::Path::new(path.as_str()).exists();
    let linked_worktree =
        |path: &&String| std::path::Path::new(path.as_str()).join(".git").is_file();
    survivor_paths
        .iter()
        .filter(exists)
        .find(|path| !linked_worktree(path))
        .or_else(|| survivor_paths.iter().find(exists))
        .or_else(|| retired_paths.iter().find(exists))
        .cloned()
}

fn count(connection: &Connection, sql: &str, parameters: impl rusqlite::Params) -> Result<u64> {
    let value: i64 = connection.query_row(sql, parameters, |row| row.get(0))?;
    Ok(u64::try_from(value).unwrap_or(0))
}

fn role_rows(
    connection: &Connection,
    table: &str,
    role: TableRole,
    retired: &ProjectId,
) -> Result<u64> {
    let r = retired.as_str();
    match (table, role) {
        ("projects", _) => count(
            connection,
            "SELECT COUNT(*) FROM projects WHERE id = ?1 OR parent_project_id = ?1
                 OR merged_into = ?1",
            [r],
        ),
        ("project_links", _) => count(
            connection,
            "SELECT COUNT(*) FROM project_links WHERE project_id = ?1 OR linked_project_id = ?1",
            [r],
        ),
        ("source_node_fts", _) => count(
            connection,
            "SELECT COUNT(*) FROM nodes WHERE project_id = ?1 AND tombstoned_at_ms IS NULL
                 AND file_path IS NOT NULL AND kind <> 'ExternalReference'",
            [r],
        ),
        (_, TableRole::FollowsParent) => follower_rows(connection, table, retired),
        _ => count(
            connection,
            &format!(
                "SELECT COUNT(*) FROM {} WHERE project_id = ?1",
                quote(table)
            ),
            [r],
        ),
    }
}

/// Rows of a child table whose parent row belongs to the retired project.
fn follower_rows(connection: &Connection, table: &str, retired: &ProjectId) -> Result<u64> {
    let references = {
        let mut statement = connection
            .prepare("SELECT \"table\", \"from\", \"to\" FROM pragma_foreign_key_list(?1)")?;
        statement
            .query_map([table], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut conditions = Vec::new();
    for (parent, from, to) in references {
        let Some(to) = to else { continue };
        if table_columns(connection, &parent)?
            .iter()
            .any(|column| column == "project_id")
        {
            conditions.push(format!(
                "{} IN (SELECT {} FROM {} WHERE project_id = ?1)",
                quote(&from),
                quote(&to),
                quote(&parent)
            ));
        }
    }
    if conditions.is_empty() {
        return Ok(0);
    }
    count(
        connection,
        &format!(
            "SELECT COUNT(*) FROM {} WHERE {}",
            quote(table),
            conditions.join(" OR ")
        ),
        [retired.as_str()],
    )
}

struct CollidingRow {
    rowid: i64,
    key: String,
}

fn colliding_rows(
    connection: &Connection,
    table: &str,
    key: &[String],
    survivor: &ProjectId,
    retired: &ProjectId,
) -> Result<Vec<CollidingRow>> {
    if key.is_empty() {
        // `UNIQUE(project_id)` alone: one row per project always collides.
        return Ok(Vec::new());
    }
    let matches = key
        .iter()
        .map(|column| format!("s.{0} = r.{0}", quote(column)))
        .collect::<Vec<_>>()
        .join(" AND ");
    let selected = key
        .iter()
        .map(|column| format!("r.{}", quote(column)))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT r.rowid, {selected} FROM {table} AS r
         WHERE r.project_id = ?2 AND EXISTS (
             SELECT 1 FROM {table} AS s WHERE s.project_id = ?1 AND {matches})
         ORDER BY {selected}",
        table = quote(table)
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement
        .query_map(params![survivor.as_str(), retired.as_str()], |row| {
            let mut parts = Vec::with_capacity(key.len());
            for (index, column) in key.iter().enumerate() {
                parts.push(format!("{column}={}", value_text(row.get_ref(index + 1)?)));
            }
            Ok(CollidingRow {
                rowid: row.get(0)?,
                key: parts.join(", "),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn value_text(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Null => "NULL".to_owned(),
        ValueRef::Integer(number) => number.to_string(),
        ValueRef::Real(number) => number.to_string(),
        ValueRef::Text(text) => String::from_utf8_lossy(text).into_owned(),
        ValueRef::Blob(bytes) => format!("<{} bytes>", bytes.len()),
    }
}

fn duplicate_claims(
    connection: &Connection,
    survivor: &ProjectId,
    retired: &ProjectId,
) -> Result<Vec<DuplicateClaim>> {
    let mut statement = connection.prepare(
        "SELECT s.id, r.id FROM memories r
         JOIN memories s ON s.project_id = ?1 AND s.normalized_claim = r.normalized_claim
         WHERE r.project_id = ?2 ORDER BY s.id, r.id",
    )?;
    let rows = statement
        .query_map(params![survivor.as_str(), retired.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|(survivor_memory, retired_memory)| {
            Ok(DuplicateClaim {
                survivor_memory_id: MemoryId::parse(&survivor_memory)?,
                retired_memory_id: MemoryId::parse(&retired_memory)?,
            })
        })
        .collect()
}

fn fact_decisions(
    connection: &Connection,
    survivor: &ProjectId,
    retired: &ProjectId,
) -> Result<Vec<MergedFact>> {
    let mut statement = connection.prepare(
        "SELECT r.key, r.value, s.value FROM project_facts r
         LEFT JOIN project_facts s ON s.project_id = ?1 AND s.key = r.key
         WHERE r.project_id = ?2 ORDER BY r.key",
    )?;
    let rows = statement
        .query_map(params![survivor.as_str(), retired.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .map(|(key, retired_value, survivor_value)| {
            let decision = if key == "internal.origin_view" {
                "replaced by the origin view rule"
            } else if key.starts_with("internal.") {
                "dropped: internal facts stay with their project"
            } else if survivor_value.is_some() {
                "survivor value kept; retired value dropped"
            } else {
                "moved to the survivor"
            };
            MergedFact {
                key,
                retired_value,
                survivor_value,
                decision: decision.to_owned(),
            }
        })
        .collect())
}

fn link_decisions(
    connection: &Connection,
    survivor: &ProjectId,
    retired: &ProjectId,
) -> Result<Vec<MergedLink>> {
    let (s, r) = (survivor.as_str(), retired.as_str());
    let mut statement = connection.prepare(
        "SELECT l.project_id, l.linked_project_id, l.relation,
                CASE
                  WHEN l.project_id IN (?1, ?2) AND l.linked_project_id IN (?1, ?2)
                    THEN 'dropped: a link between the merged projects'
                  WHEN l.project_id = ?2 AND EXISTS (SELECT 1 FROM project_links x
                       WHERE x.project_id = ?1 AND x.linked_project_id = l.linked_project_id)
                    THEN 'dropped: the survivor already links this project'
                  WHEN l.project_id = ?2 THEN 'moved to the survivor'
                  WHEN EXISTS (SELECT 1 FROM project_links x
                       WHERE x.project_id = l.project_id AND x.linked_project_id = ?1)
                    THEN 'dropped: this project already links the survivor'
                  ELSE 're-pointed at the survivor'
                END
         FROM project_links l
         WHERE l.project_id = ?2 OR l.linked_project_id = ?2
         ORDER BY l.project_id, l.linked_project_id",
    )?;
    let rows = statement
        .query_map(params![s, r], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|(project_id, linked_project_id, relation, decision)| {
            Ok(MergedLink {
                project_id: ProjectId::parse(&project_id)?,
                linked_project_id: ProjectId::parse(&linked_project_id)?,
                relation,
                decision,
            })
        })
        .collect()
}

/// Origin views of the retired project: re-paired with the survivor when it
/// has none of its own (keeping the origin index), otherwise tombstoned.
fn view_plan(
    connection: &Connection,
    survivor: &ProjectId,
    retired: &ProjectId,
) -> Result<ViewPlan> {
    let views_of = |project: &ProjectId| -> Result<BTreeSet<String>> {
        let mut statement = connection.prepare(
            "SELECT v.id FROM projects v
             WHERE v.tombstoned_at_ms IS NULL AND (
                 EXISTS (SELECT 1 FROM project_facts f WHERE f.project_id = v.id
                         AND f.key = 'internal.view_of' AND f.value = ?1)
                 OR EXISTS (SELECT 1 FROM project_facts f WHERE f.project_id = ?1
                            AND f.key = 'internal.origin_view' AND f.value = v.id))
             ORDER BY v.id",
        )?;
        let views = statement
            .query_map([project.as_str()], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<BTreeSet<_>, _>>()?;
        Ok(views)
    };
    let survivor_views = views_of(survivor)?;
    let retired_views = views_of(retired)?
        .into_iter()
        .map(|id| ProjectId::parse(&id))
        .collect::<Result<Vec<_>>>()?;
    if survivor_views.is_empty() && retired_views.len() == 1 {
        return Ok(ViewPlan {
            repaired: retired_views.into_iter().next(),
            retired: Vec::new(),
        });
    }
    Ok(ViewPlan {
        repaired: None,
        retired: retired_views,
    })
}

struct Anchor {
    memory_id: String,
    stable_key: String,
    kind: String,
    label: String,
    file_path: Option<String>,
    observed_digest: String,
    counterpart: Option<String>,
}

fn anchors_on(
    connection: &Connection,
    survivor: &ProjectId,
    retired: &ProjectId,
) -> Result<Vec<Anchor>> {
    let mut statement = connection.prepare(
        "SELECT mn.memory_id, n.stable_key, n.kind, n.label, n.file_path, mn.observed_digest,
                (SELECT s.id FROM nodes s WHERE s.project_id = ?1 AND s.stable_key = n.stable_key
                   AND s.tombstoned_at_ms IS NULL)
         FROM memory_nodes mn JOIN nodes n ON n.id = mn.node_id
         WHERE n.project_id = ?2 ORDER BY mn.memory_id, n.stable_key",
    )?;
    let anchors = statement
        .query_map(params![survivor.as_str(), retired.as_str()], |row| {
            Ok(Anchor {
                memory_id: row.get(0)?,
                stable_key: row.get(1)?,
                kind: row.get(2)?,
                label: row.get(3)?,
                file_path: row.get(4)?,
                observed_digest: row.get(5)?,
                counterpart: row.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(anchors)
}

fn anchor_plan(
    connection: &Connection,
    survivor: &ProjectId,
    retired: &ProjectId,
) -> Result<AnchorPlan> {
    let anchors = anchors_on(connection, survivor, retired)?;
    let remap_now = anchors
        .iter()
        .filter(|anchor| anchor.counterpart.is_some())
        .count();
    Ok(AnchorPlan {
        total: u64::try_from(anchors.len()).unwrap_or(u64::MAX),
        remap_now: u64::try_from(remap_now).unwrap_or(u64::MAX),
        after_reindex: u64::try_from(anchors.len() - remap_now).unwrap_or(u64::MAX),
    })
}

fn product_records_naming(
    connection: &Connection,
    survivor: &ProjectId,
    retired: &ProjectId,
) -> Result<Vec<String>> {
    Ok(reanchored_product_records(connection, survivor, retired)?
        .into_iter()
        .map(|record| record.id)
        .collect())
}

fn unfinished_jobs(connection: &Connection, retired: &ProjectId) -> Result<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT id FROM jobs WHERE project_id = ?1
           AND status IN ('pending', 'running', 'failed') ORDER BY id",
    )?;
    let ids = statement
        .query_map([retired.as_str()], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(ids)
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// Inputs of an executed merge, decided before the transaction.
#[derive(Clone, Debug)]
pub struct MergeExecution {
    pub survivor: ProjectId,
    pub retired: ProjectId,
    /// Live identity evidence: the survivor records missing fields from it.
    pub identity: IdentityEvidence,
    /// The backup taken immediately before the transaction.
    pub backup_path: Option<String>,
}

/// Result of an executed (or earlier) merge.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MergeOutcome {
    /// The `PROJECTS_MERGED` event.
    pub merge_event_id: String,
    pub survivor: ProjectId,
    pub survivor_name: String,
    pub retired: ProjectId,
    pub retired_name: String,
    pub canonical_remote: Option<String>,
    pub root_commit: Option<String>,
    pub repaired_view: Option<ProjectId>,
    pub retired_views: Vec<ProjectId>,
    pub audit: ProjectMergeAudit,
    /// The retired project's checkout paths, whose pins the client rewrites.
    pub checkouts_to_repin: Vec<String>,
    /// The one checkout the client re-indexes (see [`primary_checkout`]).
    pub reindex_path: Option<String>,
    /// Memory anchors still waiting for the re-index.
    pub pending_anchors: u64,
    /// The checkout a completed follow-up re-indexed after the merge, as
    /// recorded by its `PROJECT_MERGE_FINALIZED` event. A repeated merge
    /// skips the re-index then.
    #[serde(default)]
    pub reindexed: Option<String>,
}

/// Merge `retired` into `survivor` in one canonical transaction, or report
/// the earlier merge when it already happened. Run it through the daemon's
/// single writer, after the backup.
///
/// # Errors
///
/// Refuses invalid parties or unclassified tables and returns storage
/// errors. Nothing is changed when an error is returned.
pub fn execute(database: &Database, input: &MergeExecution) -> Result<MergeOutcome> {
    let event_id = format!("evt_{}", Uuid::new_v4());
    let now = now_ms();
    database.write_immediate(|transaction| {
        let (survivor, retired) = (&input.survivor, &input.retired);
        if let Some(outcome) = earlier_merge(transaction, survivor, retired)? {
            return Ok(outcome);
        }
        if !input.identity.same_repository || input.identity.refusal.is_some() {
            return Err(Error::MergeRefused(
                input.identity.refusal.clone().unwrap_or_else(|| {
                    "the two projects are not proven to be one repository".to_owned()
                }),
            ));
        }
        check_parties(transaction, survivor, retired)?;
        verify_roles(transaction)?;
        let survivor_party = party(transaction, survivor)?;
        let retired_party = party(transaction, retired)?;
        let fill = identity_fill(transaction, &survivor_party, retired, &input.identity)?;
        let views = view_plan(transaction, survivor, retired)?;
        let mut audit = ProjectMergeAudit {
            backup_path: input.backup_path.clone(),
            moved_aliases: retired_party.checkouts.clone(),
            facts: fact_decisions(transaction, survivor, retired)?,
            links: link_decisions(transaction, survivor, retired)?,
            duplicate_claims: duplicate_claims(transaction, survivor, retired)?,
            ..ProjectMergeAudit::default()
        };
        rescope(transaction, survivor, retired, &event_id, now, &mut audit)?;
        audit.collisions.sort();
        let event = DomainEvent::ProjectsMerged {
            project_id: survivor.clone(),
            retired_project_id: retired.clone(),
            canonical_remote: fill.canonical_remote.clone(),
            root_commit: fill.root_commit.clone(),
            repaired_view: views.repaired.clone(),
            retired_views: views.retired.clone(),
            audit: Box::new(audit.clone()),
        };
        append_generated_with_id_in_transaction(transaction, &event_id, &event, now)?;
        let survivor_after = party(transaction, survivor)?;
        Ok(MergeOutcome {
            merge_event_id: event_id.clone(),
            survivor: survivor.clone(),
            survivor_name: survivor_after.name.clone(),
            retired: retired.clone(),
            retired_name: retired_party.name.clone(),
            canonical_remote: fill.canonical_remote,
            root_commit: fill.root_commit,
            repaired_view: views.repaired,
            retired_views: views.retired,
            checkouts_to_repin: retired_party.checkouts.clone(),
            reindex_path: primary_checkout(&survivor_party.checkouts, &retired_party.checkouts),
            pending_anchors: pending_anchor_count(transaction, survivor)?,
            reindexed: None,
            audit,
        })
    })
}

/// Refuse a merge whose parties are not two distinct, active projects that
/// are not origin views.
///
/// # Errors
///
/// Returns [`Error::MergeRefused`] with the reason.
pub fn check_parties(
    connection: &Connection,
    survivor: &ProjectId,
    retired: &ProjectId,
) -> Result<()> {
    if survivor == retired {
        return Err(Error::MergeRefused(format!(
            "--into and --from select the same project ({survivor})"
        )));
    }
    for (side, id) in [("--into", survivor), ("--from", retired)] {
        match project_state(connection, id)? {
            None => return Err(Error::MergeRefused(format!("unknown project `{id}`"))),
            Some(ProjectState::Merged(into)) => {
                return Err(Error::MergeRefused(format!(
                    "{side} project {id} was already merged into {into}"
                )));
            }
            Some(ProjectState::Tombstoned) => {
                return Err(Error::MergeRefused(format!(
                    "{side} project {id} is retired"
                )));
            }
            Some(ProjectState::Active) => {}
        }
        if is_view(connection, id)? {
            return Err(Error::MergeRefused(format!(
                "{side} project {id} is an origin view (a derived documentation snapshot); \
                 merge the projects it belongs to instead"
            )));
        }
    }
    Ok(())
}

/// The outcome of an earlier merge of `retired` into `survivor`, if any.
///
/// # Errors
///
/// Returns storage or decode errors.
pub fn earlier_merge(
    connection: &Connection,
    survivor: &ProjectId,
    retired: &ProjectId,
) -> Result<Option<MergeOutcome>> {
    if project_state(connection, retired)? != Some(ProjectState::Merged(survivor.clone())) {
        return Ok(None);
    }
    let Some((event_id, payload, sequence)) = connection
        .query_row(
            "SELECT id, payload_json, sequence FROM events
             WHERE event_type = 'PROJECTS_MERGED'
               AND json_extract(payload_json, '$.data.retired_project_id') = ?1
             ORDER BY sequence DESC LIMIT 1",
            [retired.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?
    else {
        return Ok(None);
    };
    let DomainEvent::ProjectsMerged {
        canonical_remote,
        root_commit,
        repaired_view,
        retired_views,
        audit,
        ..
    } = serde_json::from_str(&payload)?
    else {
        return Err(Error::Config(format!(
            "event {event_id} is not a PROJECTS_MERGED event"
        )));
    };
    let survivor_party = party(connection, survivor)?;
    let retired_party = party(connection, retired)?;
    let survivor_others = survivor_party
        .checkouts
        .iter()
        .filter(|path| !audit.moved_aliases.contains(path))
        .cloned()
        .collect::<Vec<_>>();
    Ok(Some(MergeOutcome {
        merge_event_id: event_id,
        survivor: survivor.clone(),
        survivor_name: survivor_party.name,
        retired: retired.clone(),
        retired_name: retired_party.name,
        canonical_remote,
        root_commit,
        repaired_view,
        retired_views,
        checkouts_to_repin: audit.moved_aliases.clone(),
        reindex_path: primary_checkout(&survivor_others, &audit.moved_aliases),
        pending_anchors: pending_anchor_count(connection, survivor)?,
        reindexed: reindexed_after(connection, survivor, sequence)?,
        audit: *audit,
    }))
}

/// The latest merge that moved `path` into `survivor`, for a repeated merge
/// whose `--from` names a checkout path that now belongs to the survivor.
///
/// # Errors
///
/// Returns storage or decode errors.
pub fn merge_that_moved(
    connection: &Connection,
    survivor: &ProjectId,
    path: &str,
) -> Result<Option<ProjectId>> {
    let mut statement = connection.prepare(
        "SELECT json_extract(payload_json, '$.data.retired_project_id') FROM events
         WHERE event_type = 'PROJECTS_MERGED' AND project_id = ?1
           AND EXISTS (SELECT 1 FROM json_each(payload_json, '$.data.audit.moved_aliases')
                       WHERE value = ?2)
         ORDER BY sequence DESC LIMIT 1",
    )?;
    let retired = statement
        .query_row(params![survivor.as_str(), path], |row| {
            row.get::<_, String>(0)
        })
        .optional()?;
    retired.map(|id| ProjectId::parse(&id)).transpose()
}

fn pending_anchor_count(connection: &Connection, survivor: &ProjectId) -> Result<u64> {
    count(
        connection,
        "SELECT COUNT(*) FROM project_merge_anchors WHERE project_id = ?1",
        [survivor.as_str()],
    )
}

/// Re-scope every row still under `retired` by its table role. The merge
/// transaction runs it once; [`finalize`] runs it again for rows written
/// under the retired id after the merge.
fn rescope(
    transaction: &Transaction<'_>,
    survivor: &ProjectId,
    retired: &ProjectId,
    event_id: &str,
    now: i64,
    audit: &mut ProjectMergeAudit,
) -> Result<()> {
    verify_roles(transaction)?;
    let present = project_keyed_tables(transaction)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let roles = TABLE_ROLES
        .iter()
        .filter(|(table, _)| present.contains(*table))
        .copied()
        .collect::<Vec<_>>();

    // Anchors and product records name symbols of the retired index, which
    // is dropped below: carry them first.
    let (remapped, pending) = carry_anchors(transaction, survivor, retired, event_id, now)?;
    audit.anchors_remapped += remapped;
    audit.anchors_pending += pending;
    reanchor_product_records(transaction, survivor, retired, now, audit)?;

    let moved_memories = {
        let mut statement =
            transaction.prepare("SELECT id FROM memories WHERE project_id = ?1 ORDER BY id")?;
        statement
            .query_map([retired.as_str()], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for (table, role) in &roles {
        if let TableRole::Moved(rule) = role {
            resolve_collisions(transaction, table, *rule, survivor, retired, audit)?;
            move_rows(transaction, table, survivor, retired, audit)?;
        }
    }
    rederive_memories(transaction, retired, &moved_memories, now, audit)?;
    for (table, role) in &roles {
        if *role == TableRole::Rebuilt {
            let dropped = transaction.execute(
                &format!("DELETE FROM {} WHERE project_id = ?1", quote(table)),
                [retired.as_str()],
            )?;
            add(&mut audit.rebuilt, table, dropped);
        }
    }
    let cancelled = unfinished_jobs(transaction, retired)?;
    for job in &cancelled {
        transaction.execute(
            "UPDATE jobs SET status = 'dead', last_error = ?2, updated_at_ms = ?3,
                 completed_at_ms = ?3
             WHERE id = ?1",
            params![
                job,
                format!("cancelled: project merged into {survivor}"),
                now
            ],
        )?;
    }
    audit.cancelled_jobs.extend(cancelled);
    for (table, role) in &roles {
        if *role == TableRole::Kept {
            let kept = count(
                transaction,
                &format!(
                    "SELECT COUNT(*) FROM {} WHERE project_id = ?1",
                    quote(table)
                ),
                [retired.as_str()],
            )?;
            audit.kept.insert((*table).to_owned(), kept);
        }
    }
    Ok(())
}

fn add(counts: &mut BTreeMap<String, u64>, table: &str, rows: usize) {
    let entry = counts.entry(table.to_owned()).or_default();
    *entry = entry.saturating_add(u64::try_from(rows).unwrap_or(u64::MAX));
}

fn move_rows(
    transaction: &Transaction<'_>,
    table: &str,
    survivor: &ProjectId,
    retired: &ProjectId,
    audit: &mut ProjectMergeAudit,
) -> Result<()> {
    let (s, r) = (survivor.as_str(), retired.as_str());
    match table {
        // Project-scope memories name their project in `scope_id` as well.
        "memories" => {
            transaction.execute(
                "UPDATE memories SET scope_id = ?1
                 WHERE project_id = ?2 AND scope_type = 'project' AND scope_id = ?2",
                params![s, r],
            )?;
        }
        "resource_usage" => rescope_usage_requests(transaction, survivor, retired)?,
        _ => {}
    }
    let moved = transaction.execute(
        &format!(
            "UPDATE {} SET project_id = ?1 WHERE project_id = ?2",
            quote(table)
        ),
        params![s, r],
    )?;
    add(&mut audit.moved, table, moved);
    Ok(())
}

/// A stored usage request is compared byte for byte with a repeated sample,
/// which now carries the survivor's id. Only its `project_id` field changes:
/// it is re-serialized exactly as the recorder serializes requests, so a
/// resent sample still matches.
fn rescope_usage_requests(
    transaction: &Transaction<'_>,
    survivor: &ProjectId,
    retired: &ProjectId,
) -> Result<()> {
    let rows = {
        let mut statement = transaction
            .prepare("SELECT rowid, request_json FROM resource_usage WHERE project_id = ?1")?;
        statement
            .query_map([retired.as_str()], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for (rowid, request_json) in rows {
        match serde_json::from_str::<crate::effectiveness::UsageRecordRequest>(&request_json) {
            Ok(mut request) if request.project_id == *retired => {
                request.project_id = survivor.clone();
                transaction.execute(
                    "UPDATE resource_usage SET request_json = ?2 WHERE rowid = ?1",
                    params![rowid, serde_json::to_string(&request)?],
                )?;
            }
            Ok(_) => {}
            // A request stored by another build: rewrite the field in place.
            Err(_) => {
                transaction.execute(
                    "UPDATE resource_usage SET request_json = json_set(request_json,
                         '$.project_id', ?2)
                     WHERE rowid = ?1 AND json_valid(request_json)
                       AND json_extract(request_json, '$.project_id') = ?3",
                    params![rowid, survivor.as_str(), retired.as_str()],
                )?;
            }
        }
    }
    Ok(())
}

fn resolve_collisions(
    transaction: &Transaction<'_>,
    table: &str,
    rule: CollisionRule,
    survivor: &ProjectId,
    retired: &ProjectId,
    audit: &mut ProjectMergeAudit,
) -> Result<()> {
    for key in project_unique_keys(transaction, table)? {
        let rows = colliding_rows(transaction, table, &key, survivor, retired)?;
        if rows.is_empty() {
            continue;
        }
        for row in &rows {
            match rule {
                CollisionRule::Impossible => {
                    return Err(Error::MergeRefused(format!(
                        "{table} has rows of both projects with the same {} and no collision \
                         rule; nothing was changed",
                        row.key
                    )));
                }
                CollisionRule::RekeyRetired(column) => {
                    if !key.iter().any(|name| name == column) {
                        return Err(Error::MergeRefused(format!(
                            "{table} collides on ({}) but its rule re-keys {column}",
                            key.join(", ")
                        )));
                    }
                    let suffix = format!("~merged-{}", &retired.as_str()[4..12]);
                    transaction.execute(
                        &format!(
                            "UPDATE {table} SET {column} = {column} || ?2 WHERE rowid = ?1",
                            table = quote(table),
                            column = quote(column)
                        ),
                        params![row.rowid, suffix],
                    )?;
                }
                CollisionRule::KeepSurvivor => {
                    transaction.execute(
                        &format!("DELETE FROM {} WHERE rowid = ?1", quote(table)),
                        [row.rowid],
                    )?;
                }
                CollisionRule::UnionPaths => union_paths(transaction, table, &key, row, survivor)?,
            }
            audit.collisions.push(MergeCollision {
                table: table.to_owned(),
                key: row.key.clone(),
                resolution: rule.describe().to_owned(),
            });
        }
    }
    Ok(())
}

/// Fold the retired row's `paths_json` into the survivor row with the same
/// key, then drop the retired row.
fn union_paths(
    transaction: &Transaction<'_>,
    table: &str,
    key: &[String],
    row: &CollidingRow,
    survivor: &ProjectId,
) -> Result<()> {
    let matches = key
        .iter()
        .map(|column| format!("s.{0} = r.{0}", quote(column)))
        .collect::<Vec<_>>()
        .join(" AND ");
    let (survivor_rowid, survivor_paths, retired_paths): (i64, String, String) = transaction
        .query_row(
            &format!(
                "SELECT s.rowid, s.paths_json, r.paths_json FROM {table} AS r
                 JOIN {table} AS s ON s.project_id = ?2 AND {matches}
                 WHERE r.rowid = ?1",
                table = quote(table)
            ),
            params![row.rowid, survivor.as_str()],
            |result| Ok((result.get(0)?, result.get(1)?, result.get(2)?)),
        )?;
    let mut paths = serde_json::from_str::<Vec<String>>(&survivor_paths)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    paths.extend(serde_json::from_str::<Vec<String>>(&retired_paths)?);
    transaction.execute(
        &format!(
            "UPDATE {} SET paths_json = ?2 WHERE rowid = ?1",
            quote(table)
        ),
        params![
            survivor_rowid,
            serde_json::to_string(&paths.into_iter().collect::<Vec<_>>())?
        ],
    )?;
    transaction.execute(
        &format!("DELETE FROM {} WHERE rowid = ?1", quote(table)),
        [row.rowid],
    )?;
    Ok(())
}

/// Re-derive search text and vectors of memories that moved: rows of the
/// retired project are dropped and rebuilt from the canonical memory rows.
/// Model vectors are dropped too; a semantic rebuild embeds them again.
fn rederive_memories(
    transaction: &Transaction<'_>,
    retired: &ProjectId,
    moved: &[String],
    now: i64,
    audit: &mut ProjectMergeAudit,
) -> Result<()> {
    let text_rows = transaction.execute(
        "DELETE FROM memories_fts WHERE project_id = ?1",
        [retired.as_str()],
    )?;
    add(&mut audit.rebuilt, "memories_fts", text_rows);
    let vector_rows = transaction.execute(
        "DELETE FROM memory_vectors WHERE project_id = ?1",
        [retired.as_str()],
    )?;
    add(&mut audit.rebuilt, "memory_vectors", vector_rows);
    for raw_id in moved {
        let memory_id = MemoryId::parse(raw_id)?;
        let (project_id, claim, rationale, tags_json, active): (
            String,
            String,
            String,
            String,
            bool,
        ) = transaction.query_row(
            "SELECT project_id, claim, rationale, tags_json, status = 'active'
                 FROM memories WHERE id = ?1",
            [raw_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        let tags: Vec<String> = serde_json::from_str(&tags_json)?;
        transaction.execute("DELETE FROM memories_fts WHERE memory_id = ?1", [raw_id])?;
        transaction.execute(
            "INSERT INTO memories_fts(memory_id, project_id, claim, rationale, tags)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![raw_id, project_id, claim, rationale, tags.join(" ")],
        )?;
        crate::semantic::delete_memory_vector(transaction, &memory_id)?;
        crate::semantic::sync_memory_vector(transaction, &memory_id, active, now)?;
    }
    Ok(())
}

/// Carry memory anchors on the retired project's symbols: re-attach each to
/// the survivor's symbol with the same stable identity (path and qualified
/// name) when it is already indexed, and record the rest for [`finalize`].
fn carry_anchors(
    transaction: &Transaction<'_>,
    survivor: &ProjectId,
    retired: &ProjectId,
    event_id: &str,
    now: i64,
) -> Result<(u64, u64)> {
    let (mut remapped, mut pending) = (0_u64, 0_u64);
    for anchor in anchors_on(transaction, survivor, retired)? {
        if let Some(node) = &anchor.counterpart {
            transaction.execute(
                "INSERT OR IGNORE INTO memory_nodes(memory_id, node_id, observed_digest)
                 VALUES (?1, ?2, ?3)",
                params![anchor.memory_id, node, anchor.observed_digest],
            )?;
            remapped += 1;
        } else {
            transaction.execute(
                "INSERT OR IGNORE INTO project_merge_anchors
                     (project_id, memory_id, stable_key, kind, label, file_path,
                      observed_digest, merge_event_id, recorded_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    survivor.as_str(),
                    anchor.memory_id,
                    anchor.stable_key,
                    anchor.kind,
                    anchor.label,
                    anchor.file_path,
                    anchor.observed_digest,
                    event_id,
                    now
                ],
            )?;
            pending += 1;
        }
    }
    Ok((remapped, pending))
}

/// A product record whose typed references a merge re-points.
struct ReanchoredRecord {
    id: String,
    version: i64,
    archived: bool,
    document: String,
    stamps: String,
}

/// Product records with typed references to the retired project (see
/// [`redirect_project_references`]) and their re-pointed documents. A symbol
/// anchor (`node_id`) on the retired project's index moves to the survivor's
/// active symbol with the same stable identity; without one it stays and the
/// record reads as needing review. Ids that appear only in text are left
/// alone, and such records are not touched.
fn reanchored_product_records(
    connection: &Connection,
    survivor: &ProjectId,
    retired: &ProjectId,
) -> Result<Vec<ReanchoredRecord>> {
    let candidates = {
        let mut statement = connection.prepare(
            "SELECT id, version, document_json, stamps_json, archived FROM product_records
             WHERE instr(document_json, ?1) > 0 OR instr(stamps_json, ?1) > 0 ORDER BY id",
        )?;
        statement
            .query_map([retired.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, bool>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    let projects = BTreeMap::from([(retired.to_string(), survivor.to_string())]);
    let mut records = Vec::new();
    for (id, version, document, stamps, archived) in candidates {
        let mut document: serde_json::Value = serde_json::from_str(&document)?;
        let mut stamps: serde_json::Value = serde_json::from_str(&stamps)?;
        let mut changed = redirect_project_references(&mut document, &projects);
        changed |= redirect_project_references(&mut stamps, &projects);
        let mut anchored = BTreeSet::new();
        collect_node_ids(&document, &mut anchored);
        collect_node_ids(&stamps, &mut anchored);
        let mut nodes = BTreeMap::new();
        for node in anchored {
            let counterpart: Option<String> = connection
                .query_row(
                    "SELECT s.id FROM nodes r
                     JOIN nodes s ON s.project_id = ?3 AND s.stable_key = r.stable_key
                                 AND s.tombstoned_at_ms IS NULL
                     WHERE r.id = ?1 AND r.project_id = ?2",
                    params![node, retired.as_str(), survivor.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(counterpart) = counterpart {
                nodes.insert(node, counterpart);
            }
        }
        changed |= remap_node_ids(&mut document, &nodes);
        changed |= remap_node_ids(&mut stamps, &nodes);
        if changed {
            records.push(ReanchoredRecord {
                id,
                version,
                archived,
                document: serde_json::to_string(&document)?,
                stamps: serde_json::to_string(&stamps)?,
            });
        }
    }
    Ok(records)
}

/// Write re-anchored product records, each as a new revision.
fn reanchor_product_records(
    transaction: &Transaction<'_>,
    survivor: &ProjectId,
    retired: &ProjectId,
    now: i64,
    audit: &mut ProjectMergeAudit,
) -> Result<()> {
    for record in reanchored_product_records(transaction, survivor, retired)? {
        let next = record.version.saturating_add(1);
        transaction.execute(
            "UPDATE product_records SET version = ?2, document_json = ?3, stamps_json = ?4,
                 updated_at_ms = ?5 WHERE id = ?1",
            params![record.id, next, record.document, record.stamps, now],
        )?;
        transaction.execute(
            "INSERT INTO product_revisions
                 (record_id, version, document_json, stamps_json, archived, recorded_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.id,
                next,
                record.document,
                record.stamps,
                record.archived,
                now
            ],
        )?;
        audit.product_records.push(record.id);
    }
    Ok(())
}

fn collect_node_ids(value: &serde_json::Value, nodes: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                match child {
                    serde_json::Value::String(text) if key == "node_id" => {
                        nodes.insert(text.clone());
                    }
                    _ => collect_node_ids(child, nodes),
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_node_ids(item, nodes);
            }
        }
        _ => {}
    }
}

fn remap_node_ids(value: &mut serde_json::Value, nodes: &BTreeMap<String, String>) -> bool {
    let mut changed = false;
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                match child {
                    serde_json::Value::String(text) if key == "node_id" => {
                        if let Some(node) = nodes.get(text.as_str()) {
                            text.clone_from(node);
                            changed = true;
                        }
                    }
                    _ => changed |= remap_node_ids(child, nodes),
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                changed |= remap_node_ids(item, nodes);
            }
        }
        _ => {}
    }
    changed
}

// ---------------------------------------------------------------------------
// Finalize
// ---------------------------------------------------------------------------

/// What a [`finalize`] call is for.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct FinalizeOptions {
    /// Attach waiting memory anchors, or mark their memories for
    /// revalidation. Leave false while the survivor's re-index has not run.
    pub resolve_anchors: bool,
    /// The checkout the client re-indexed for this follow-up, recorded so a
    /// repeated merge can skip the re-index.
    pub reindexed: Option<String>,
    /// `after_merge` or `client`, recorded in the audit event.
    pub trigger: String,
}

/// Result of [`finalize`].
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct FinalizeOutcome {
    pub survivor: Option<ProjectId>,
    /// Rows written under a retired id after its merge, re-scoped now.
    pub stragglers: BTreeMap<String, u64>,
    /// All such rows since the latest merge into the survivor, including
    /// those the daemon re-scoped on its own right after the merge.
    #[serde(default)]
    pub stragglers_since_merge: BTreeMap<String, u64>,
    pub anchors_remapped: u64,
    /// Memories whose anchors did not survive, marked suspect (revalidate).
    pub revalidate: Vec<MemoryId>,
    /// Anchors still waiting (anchor resolution was not requested).
    pub pending_anchors: u64,
}

/// Finish merges into `survivor`: re-scope rows written under a retired id
/// since its merge and, with `resolve_anchors`, attach each waiting memory
/// anchor to the survivor's active symbol with the same stable identity or
/// mark its memory suspect for revalidation (never promoting it). What it did
/// is recorded in a `PROJECT_MERGE_FINALIZED` event.
///
/// # Errors
///
/// Returns storage errors; nothing is changed then.
pub fn finalize(
    database: &Database,
    survivor: &ProjectId,
    options: &FinalizeOptions,
) -> Result<FinalizeOutcome> {
    let now = now_ms();
    database.write_immediate(|transaction| {
        let mut outcome = FinalizeOutcome {
            survivor: Some(survivor.clone()),
            ..FinalizeOutcome::default()
        };
        let retired_ids = {
            let mut statement = transaction
                .prepare("SELECT id FROM projects WHERE merged_into = ?1 ORDER BY id")?;
            statement
                .query_map([survivor.as_str()], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for raw in retired_ids {
            let retired = ProjectId::parse(&raw)?;
            let event_id = earlier_merge(transaction, survivor, &retired)?
                .map_or_else(|| "evt_unknown".to_owned(), |merge| merge.merge_event_id);
            let mut audit = ProjectMergeAudit::default();
            rescope(transaction, survivor, &retired, &event_id, now, &mut audit)?;
            for (table, rows) in audit.moved.iter().chain(&audit.rebuilt) {
                if *rows > 0 {
                    *outcome.stragglers.entry(table.clone()).or_default() += rows;
                }
            }
        }
        if options.resolve_anchors {
            resolve_pending_anchors(transaction, survivor, now, &mut outcome)?;
        }
        outcome.pending_anchors = pending_anchor_count(transaction, survivor)?;
        let changed = !outcome.stragglers.is_empty()
            || outcome.anchors_remapped > 0
            || !outcome.revalidate.is_empty();
        if changed || options.reindexed.is_some() {
            append_generated_in_transaction(
                transaction,
                &DomainEvent::ProjectMergeFinalized {
                    project_id: survivor.clone(),
                    trigger: options.trigger.clone(),
                    stragglers: outcome.stragglers.clone(),
                    anchors_remapped: outcome.anchors_remapped,
                    revalidated: outcome.revalidate.clone(),
                    reindexed: options.reindexed.clone(),
                },
                now,
            )?;
        }
        outcome.stragglers_since_merge = stragglers_since_merge(transaction, survivor)?;
        Ok(outcome)
    })
}

/// `PROJECT_MERGE_FINALIZED` events since the latest merge into `survivor`.
fn finalized_since_merge(
    connection: &Connection,
    survivor: &ProjectId,
) -> Result<Vec<(i64, DomainEvent)>> {
    let mut statement = connection.prepare(
        "SELECT sequence, payload_json FROM events
         WHERE event_type = 'PROJECT_MERGE_FINALIZED' AND project_id = ?1
           AND sequence > coalesce((SELECT max(sequence) FROM events
                                    WHERE event_type = 'PROJECTS_MERGED' AND project_id = ?1), 0)
         ORDER BY sequence",
    )?;
    let rows = statement
        .query_map([survivor.as_str()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|(sequence, payload)| Ok((sequence, serde_json::from_str(&payload)?)))
        .collect()
}

fn stragglers_since_merge(
    connection: &Connection,
    survivor: &ProjectId,
) -> Result<BTreeMap<String, u64>> {
    let mut totals = BTreeMap::new();
    for (_, event) in finalized_since_merge(connection, survivor)? {
        if let DomainEvent::ProjectMergeFinalized { stragglers, .. } = event {
            for (table, rows) in stragglers {
                *totals.entry(table).or_default() += rows;
            }
        }
    }
    Ok(totals)
}

/// The checkout a follow-up re-indexed after the merge at `merge_sequence`,
/// when a recorded follow-up finished it and no anchor waits any more.
fn reindexed_after(
    connection: &Connection,
    survivor: &ProjectId,
    merge_sequence: i64,
) -> Result<Option<String>> {
    if pending_anchor_count(connection, survivor)? > 0 {
        return Ok(None);
    }
    Ok(finalized_since_merge(connection, survivor)?
        .into_iter()
        .filter(|(sequence, _)| *sequence > merge_sequence)
        .filter_map(|(_, event)| match event {
            DomainEvent::ProjectMergeFinalized { reindexed, .. } => reindexed,
            _ => None,
        })
        .next_back())
}

fn resolve_pending_anchors(
    transaction: &Transaction<'_>,
    survivor: &ProjectId,
    now: i64,
    outcome: &mut FinalizeOutcome,
) -> Result<()> {
    let pending = {
        let mut statement = transaction.prepare(
            "SELECT a.memory_id, a.stable_key, a.observed_digest,
                    (SELECT n.id FROM nodes n WHERE n.project_id = a.project_id
                     AND n.stable_key = a.stable_key AND n.tombstoned_at_ms IS NULL)
             FROM project_merge_anchors a WHERE a.project_id = ?1
             ORDER BY a.memory_id, a.stable_key",
        )?;
        statement
            .query_map([survivor.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut unresolved = BTreeSet::new();
    for (memory_id, stable_key, observed_digest, node) in pending {
        if let Some(node) = node {
            transaction.execute(
                "INSERT OR IGNORE INTO memory_nodes(memory_id, node_id, observed_digest)
                 VALUES (?1, ?2, ?3)",
                params![memory_id, node, observed_digest],
            )?;
            outcome.anchors_remapped += 1;
        } else {
            unresolved.insert(memory_id.clone());
        }
        transaction.execute(
            "DELETE FROM project_merge_anchors WHERE memory_id = ?1 AND stable_key = ?2",
            params![memory_id, stable_key],
        )?;
    }
    for raw in unresolved {
        let memory_id = MemoryId::parse(&raw)?;
        let state: Option<(String, String)> = transaction
            .query_row(
                "SELECT epistemic_state, status FROM memories WHERE id = ?1",
                [raw.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((epistemic_state, status)) = state else {
            continue;
        };
        // Only active memories can be quarantined; others are already out
        // of injection and keep their state.
        if status != "active" {
            continue;
        }
        transaction.execute(
            "UPDATE memories SET status = 'suspect', known_to_ms = ?2, updated_at_ms = ?2
             WHERE id = ?1",
            params![raw, now],
        )?;
        crate::semantic::delete_memory_vector(transaction, &memory_id)?;
        append_generated_in_transaction(
            transaction,
            &DomainEvent::MemoryStateChanged {
                project_id: survivor.clone(),
                memory_id: memory_id.clone(),
                epistemic_state,
                status: "suspect".to_owned(),
                reason: ANCHOR_UNRESOLVED_REASON.to_owned(),
            },
            now,
        )?;
        outcome.revalidate.push(memory_id);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Redirects
// ---------------------------------------------------------------------------

/// The survivor of every merged project id, following chains of merges.
///
/// # Errors
///
/// Returns storage errors.
pub fn redirects(connection: &Connection) -> Result<BTreeMap<String, String>> {
    let pairs = {
        let mut statement = connection
            .prepare("SELECT id, merged_into FROM projects WHERE merged_into IS NOT NULL")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<BTreeMap<_, _>, _>>()?
    };
    let mut resolved = BTreeMap::new();
    for retired in pairs.keys() {
        let mut survivor = retired;
        let mut hops = 0;
        while let Some(next) = pairs.get(survivor) {
            survivor = next;
            hops += 1;
            if hops > pairs.len() {
                break;
            }
        }
        resolved.insert(retired.clone(), survivor.clone());
    }
    Ok(resolved)
}

/// Keys whose string value is a project id, in requests and product records.
const PROJECT_ID_KEYS: &[&str] = &[
    "project_id",
    "linked_project_id",
    "target_project_id",
    "parent_project_id",
    "source_project_id",
];

/// Keys whose string value may be a product entity reference to a
/// repository, `repo:<project id>`.
const ENTITY_REFERENCE_KEYS: &[&str] = &["entity_id", "parent_id", "source_id", "target_id"];

/// Point the typed project references in `value` that name a retired project
/// at its survivor: string values of the [`PROJECT_ID_KEYS`], elements of
/// `project_ids`, a `scope_id` beside `"scope_type": "project"`, and
/// `repo:<id>` references in the [`ENTITY_REFERENCE_KEYS`]. Every other
/// string is left alone, whatever it contains: prompts, commands, artifacts,
/// metadata, fact values and notes keep their text. Returns whether anything
/// changed.
pub fn redirect_project_references(
    value: &mut serde_json::Value,
    redirects: &BTreeMap<String, String>,
) -> bool {
    let mut changed = false;
    redirect_value(value, redirects, &mut changed);
    changed
}

fn redirect_value(
    value: &mut serde_json::Value,
    redirects: &BTreeMap<String, String>,
    changed: &mut bool,
) {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let project_scope = map.get("scope_type").and_then(Value::as_str) == Some("project");
            for (key, child) in map.iter_mut() {
                let key = key.as_str();
                match child {
                    Value::String(text)
                        if PROJECT_ID_KEYS.contains(&key)
                            || (key == "scope_id" && project_scope) =>
                    {
                        redirect_id(text, redirects, changed);
                    }
                    Value::String(text) if ENTITY_REFERENCE_KEYS.contains(&key) => {
                        let replacement = text
                            .strip_prefix("repo:")
                            .and_then(|id| redirects.get(id))
                            .map(|survivor| format!("repo:{survivor}"));
                        if let Some(replacement) = replacement {
                            *text = replacement;
                            *changed = true;
                        }
                    }
                    Value::Array(items) if key == "project_ids" => {
                        for item in items {
                            if let Value::String(text) = item {
                                redirect_id(text, redirects, changed);
                            }
                        }
                    }
                    _ => redirect_value(child, redirects, changed),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redirect_value(item, redirects, changed);
            }
        }
        _ => {}
    }
}

fn redirect_id(text: &mut String, redirects: &BTreeMap<String, String>, changed: &mut bool) {
    if let Some(survivor) = redirects.get(text.as_str()) {
        text.clone_from(survivor);
        *changed = true;
    }
}

/// A JSON request body with its typed project references redirected (see
/// [`redirect_project_references`]); `None` when nothing changed or the body
/// is not JSON. Bodies that do not contain a retired id at all are not
/// parsed.
#[must_use]
pub fn redirect_request_body(body: &[u8], redirects: &BTreeMap<String, String>) -> Option<Vec<u8>> {
    let mentions_retired = redirects.keys().any(|retired| {
        let needle = retired.as_bytes();
        !needle.is_empty() && body.windows(needle.len()).any(|window| window == needle)
    });
    if !mentions_retired {
        return None;
    }
    let mut value: serde_json::Value = serde_json::from_slice(body).ok()?;
    if !redirect_project_references(&mut value, redirects) {
        return None;
    }
    serde_json::to_vec(&value).ok()
}

/// The active survivor that `id` was merged into, following later merges;
/// `None` when `id` was never merged.
///
/// # Errors
///
/// Returns storage errors.
pub fn survivor_of(connection: &Connection, id: &ProjectId) -> Result<Option<ProjectId>> {
    let mut current = id.clone();
    for hops in 0..16 {
        let next: Option<Option<String>> = connection
            .query_row(
                "SELECT merged_into FROM projects WHERE id = ?1",
                [current.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        match next.flatten() {
            Some(survivor) => current = ProjectId::parse(&survivor)?,
            None => return Ok((hops > 0).then_some(current)),
        }
    }
    Ok(None)
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests;
