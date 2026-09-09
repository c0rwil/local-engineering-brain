//! Stable project identity and current Git state discovery.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use crate::error::{Error, Result, io_error};
use crate::events::{DomainEvent, EventStore};
pub use crate::ids::ProjectId;
use crate::storage::Database;

const MAX_GIT_STATUS_BYTES: u64 = 32 * 1_024 * 1_024;
const MAX_CHANGED_PATHS: usize = 100_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectMetadataFile {
    project_id: ProjectId,
    name: Option<String>,
    parent_project_id: Option<ProjectId>,
    /// Marks a derived documentation view (an origin/main shadow). View
    /// projects register with NO remote or root-commit identity so they can
    /// coexist with the real project and never capture its resolutions.
    #[serde(default)]
    derived_view: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ComponentMetadataFile {
    component_id: String,
    name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ComponentRef {
    pub id: String,
    pub name: String,
    pub root: PathBuf,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct GitState {
    pub repository_root: PathBuf,
    pub canonical_remote: Option<String>,
    pub root_commit: Option<String>,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub dirty: bool,
    pub changed_paths: Vec<PathBuf>,
    pub diff_fingerprint: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProjectRecord {
    pub id: ProjectId,
    pub name: String,
    pub canonical_remote: Option<String>,
    pub root_commit: Option<String>,
    pub parent_project_id: Option<ProjectId>,
    pub created_at_ms: i64,
}

/// One curated architecture fact (`hosting.base_url` -> `api.example.com`).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProjectFact {
    pub key: String,
    pub value: String,
    pub note: Option<String>,
}

/// One directed project link, resolved to the target's display name so
/// injection labels and CLI listings never expose bare IDs alone.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LinkedProject {
    pub project_id: ProjectId,
    pub name: String,
    pub relation: String,
    pub note: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ResolvedProject {
    pub project: ProjectRecord,
    pub root: PathBuf,
    pub component: Option<ComponentRef>,
    pub git: Option<GitState>,
}

#[derive(Clone, Debug)]
pub(crate) struct ProjectObservation {
    root: PathBuf,
    metadata: Option<ProjectMetadataFile>,
    component: Option<ComponentRef>,
    git: Option<GitState>,
}

#[derive(Clone, Debug)]
pub struct ProjectRegistry {
    database: Database,
    events: EventStore,
}

impl ProjectRegistry {
    #[must_use]
    pub fn new(database: Database) -> Self {
        Self {
            events: EventStore::new(database.clone()),
            database,
        }
    }

    /// Resolve a project, registering and persisting a UUID when it is new.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid paths/metadata, failed Git discovery, or storage failures.
    pub fn resolve_or_register(&self, requested_path: &Path) -> Result<ResolvedProject> {
        self.resolve_or_register_with_parent(requested_path, None)
    }

    /// Resolve/register a project and link a newly created fork to an explicit parent.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid paths/metadata, failed Git discovery, or storage failures.
    pub fn resolve_or_register_with_parent(
        &self,
        requested_path: &Path,
        requested_parent: Option<&ProjectId>,
    ) -> Result<ResolvedProject> {
        let observation = Self::observe(requested_path)?;
        self.resolve_observation(observation, requested_parent)
    }

    /// Read-only identity lookup: which registered project (if any) owns
    /// `requested_path`. Never registers, never persists metadata, never
    /// records an alias — this is what universal (user-level) agent hooks
    /// use, so a global install cannot silently enroll repositories;
    /// `engram index <repo>` stays the explicit opt-in.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid paths, metadata, bounded Git discovery,
    /// or storage-read failures.
    pub fn lookup(&self, requested_path: &Path) -> Result<Option<ProjectRecord>> {
        let ProjectObservation {
            root,
            metadata,
            git,
            component: _,
        } = Self::observe(requested_path)?;
        if let Some(metadata) = &metadata {
            return self.find_by_id(&metadata.project_id);
        }
        if let Some(remote) = git
            .as_ref()
            .and_then(|state| state.canonical_remote.as_deref())
        {
            return self.find_by_remote(remote);
        }
        if let Some(root_commit) = git.as_ref().and_then(|state| state.root_commit.as_deref()) {
            return self.find_by_root_commit_without_remote(root_commit);
        }
        self.find_by_alias(&root)
    }

    /// Observe filesystem/Git identity without holding the canonical mutation
    /// scheduler. The returned value is immutable input to the small
    /// idempotent registration transaction.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid paths, metadata, or bounded Git discovery.
    pub(crate) fn observe(requested_path: &Path) -> Result<ProjectObservation> {
        let requested_path = if requested_path.is_file() {
            requested_path.parent().unwrap_or(requested_path)
        } else {
            requested_path
        };
        let requested_path = canonical_path(requested_path)?;
        let git = GitState::discover(&requested_path)?;
        let root = git.as_ref().map_or_else(
            || requested_path.clone(),
            |state| state.repository_root.clone(),
        );
        let metadata = read_project_metadata(&root)?;
        let component = discover_component(&requested_path, &root)?;

        Ok(ProjectObservation {
            root,
            metadata,
            component,
            git,
        })
    }

    /// Resolve an observed identity and commit only missing project/alias
    /// projections. Expensive repository inspection has already completed.
    ///
    /// # Errors
    ///
    /// Returns an error for storage or metadata-persistence failures.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn resolve_observation(
        &self,
        observation: ProjectObservation,
        requested_parent: Option<&ProjectId>,
    ) -> Result<ResolvedProject> {
        let ProjectObservation {
            root,
            metadata,
            component,
            git,
        } = observation;

        let mut project = if let Some(metadata) = &metadata {
            let derived_view = metadata.derived_view.unwrap_or(false);
            self.find_by_id(&metadata.project_id)?
                .unwrap_or_else(|| ProjectRecord {
                    id: metadata.project_id.clone(),
                    name: metadata.name.clone().unwrap_or_else(|| project_name(&root)),
                    canonical_remote: if derived_view {
                        None
                    } else {
                        git.as_ref()
                            .and_then(|state| state.canonical_remote.clone())
                    },
                    root_commit: if derived_view {
                        None
                    } else {
                        git.as_ref().and_then(|state| state.root_commit.clone())
                    },
                    parent_project_id: metadata
                        .parent_project_id
                        .clone()
                        .or_else(|| requested_parent.cloned()),
                    created_at_ms: 0,
                })
        } else if let Some(remote) = git
            .as_ref()
            .and_then(|state| state.canonical_remote.as_deref())
        {
            self.find_by_remote(remote)?
                .unwrap_or_else(|| ProjectRecord {
                    id: ProjectId::new(),
                    name: project_name(&root),
                    canonical_remote: Some(remote.to_owned()),
                    root_commit: git.as_ref().and_then(|state| state.root_commit.clone()),
                    parent_project_id: requested_parent.cloned(),
                    created_at_ms: 0,
                })
        } else if let Some(root_commit) =
            git.as_ref().and_then(|state| state.root_commit.as_deref())
        {
            self.find_by_root_commit_without_remote(root_commit)?
                .unwrap_or_else(|| ProjectRecord {
                    id: ProjectId::new(),
                    name: project_name(&root),
                    canonical_remote: None,
                    root_commit: Some(root_commit.to_owned()),
                    parent_project_id: requested_parent.cloned(),
                    created_at_ms: 0,
                })
        } else {
            self.find_by_alias(&root)?.unwrap_or_else(|| ProjectRecord {
                id: ProjectId::new(),
                name: project_name(&root),
                canonical_remote: None,
                root_commit: None,
                parent_project_id: requested_parent.cloned(),
                created_at_ms: 0,
            })
        };

        if self.find_by_id(&project.id)?.is_none() {
            self.events.append(&DomainEvent::ProjectRegistered {
                project_id: project.id.clone(),
                name: project.name.clone(),
                canonical_remote: project.canonical_remote.clone(),
                root_commit: project.root_commit.clone(),
                parent_project_id: project.parent_project_id.clone(),
            })?;
            project = self.find_by_id(&project.id)?.ok_or_else(|| {
                Error::Config("project projection missing after registration".to_owned())
            })?;
        }

        // Derived views (origin/main shadows) live in throwaway worktrees:
        // recording those paths would grow one dead alias per refresh, and a
        // shadow is only ever addressed by id anyway.
        let is_derived_view = metadata
            .as_ref()
            .is_some_and(|metadata| metadata.derived_view.unwrap_or(false));
        if !is_derived_view {
            let alias = root.to_string_lossy().into_owned();
            if !self.project_has_alias(&project.id, &alias)? {
                self.events.append(&DomainEvent::ProjectAliasAdded {
                    project_id: project.id.clone(),
                    path: alias,
                })?;
            }
        }

        let metadata_was_persisted = metadata.is_none();
        if metadata_was_persisted {
            persist_project_metadata(&root, &project)?;
        }
        // Persisting `.engram/project.toml` changes the working tree, so the
        // pre-write observation is stale for brand-new projects. Recompute so
        // callers never record a fingerprint the next observation contradicts.
        let git = if metadata_was_persisted && git.is_some() {
            GitState::discover(&root)?
        } else {
            git
        };

        Ok(ResolvedProject {
            project,
            root,
            component,
            git,
        })
    }

    /// Create or update a directed link that makes `linked_project_id`'s
    /// validated memories eligible for labeled cross-repository injection
    /// when compiling context for `project_id`.
    ///
    /// # Errors
    ///
    /// Returns an error for self-links, unknown or tombstoned projects,
    /// invalid relation/note bounds, or storage failures.
    pub fn link(
        &self,
        project_id: &ProjectId,
        linked_project_id: &ProjectId,
        relation: &str,
        note: Option<&str>,
    ) -> Result<LinkedProject> {
        if project_id == linked_project_id {
            return Err(Error::Config(
                "a project cannot be linked to itself".to_owned(),
            ));
        }
        let relation = relation.trim();
        if relation.is_empty() || relation.len() > 64 {
            return Err(Error::Config(
                "link relation must be 1-64 characters".to_owned(),
            ));
        }
        if note.is_some_and(|note| note.len() > 500) {
            return Err(Error::Config(
                "link note must be at most 500 bytes".to_owned(),
            ));
        }
        self.find_by_id(project_id)?
            .ok_or_else(|| Error::Config(format!("unknown project `{project_id}`")))?;
        let target = self
            .find_by_id(linked_project_id)?
            .ok_or_else(|| Error::Config(format!("unknown project `{linked_project_id}`")))?;
        self.events.append(&DomainEvent::ProjectLinked {
            project_id: project_id.clone(),
            linked_project_id: linked_project_id.clone(),
            relation: relation.to_owned(),
            note: note.map(str::to_owned),
        })?;
        Ok(LinkedProject {
            project_id: target.id,
            name: target.name,
            relation: relation.to_owned(),
            note: note.map(str::to_owned),
        })
    }

    /// Remove a directed project link. Returns `true` when a link existed.
    ///
    /// # Errors
    ///
    /// Returns an error for storage failures.
    pub fn unlink(&self, project_id: &ProjectId, linked_project_id: &ProjectId) -> Result<bool> {
        let existed = self
            .links(project_id)?
            .iter()
            .any(|link| &link.project_id == linked_project_id);
        if existed {
            self.events.append(&DomainEvent::ProjectUnlinked {
                project_id: project_id.clone(),
                linked_project_id: linked_project_id.clone(),
            })?;
        }
        Ok(existed)
    }

    /// List a project's outgoing links in deterministic (name, id) order,
    /// excluding tombstoned targets.
    ///
    /// # Errors
    ///
    /// Returns an error for storage failures.
    pub fn links(&self, project_id: &ProjectId) -> Result<Vec<LinkedProject>> {
        self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT p.id, p.name, l.relation, l.note
                 FROM project_links l
                 JOIN projects p ON p.id = l.linked_project_id
                 WHERE l.project_id = ?1 AND p.tombstoned_at_ms IS NULL
                 ORDER BY p.name, p.id",
            )?;
            let rows = statement.query_map([project_id.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?;
            let mut links = Vec::new();
            for row in rows {
                let (id, name, relation, note) = row?;
                links.push(LinkedProject {
                    project_id: ProjectId::parse(&id)?,
                    name,
                    relation,
                    note,
                });
            }
            Ok(links)
        })
    }

    /// Set (or update) one curated architecture fact for a project.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown projects, invalid bounds, or storage
    /// failures.
    pub fn set_fact(
        &self,
        project_id: &ProjectId,
        key: &str,
        value: &str,
        note: Option<&str>,
    ) -> Result<ProjectFact> {
        let key = key.trim();
        if key.is_empty() || key.len() > 64 {
            return Err(Error::Config("fact key must be 1-64 characters".to_owned()));
        }
        let value = value.trim();
        if value.is_empty() || value.len() > 500 {
            return Err(Error::Config(
                "fact value must be 1-500 characters".to_owned(),
            ));
        }
        if note.is_some_and(|note| note.len() > 500) {
            return Err(Error::Config(
                "fact note must be at most 500 bytes".to_owned(),
            ));
        }
        self.find_by_id(project_id)?
            .ok_or_else(|| Error::Config(format!("unknown project `{project_id}`")))?;
        self.events.append(&DomainEvent::ProjectFactSet {
            project_id: project_id.clone(),
            key: key.to_owned(),
            value: value.to_owned(),
            note: note.map(str::to_owned),
        })?;
        Ok(ProjectFact {
            key: key.to_owned(),
            value: value.to_owned(),
            note: note.map(str::to_owned),
        })
    }

    /// Remove one fact. Returns `true` when it existed.
    ///
    /// # Errors
    ///
    /// Returns storage errors.
    pub fn unset_fact(&self, project_id: &ProjectId, key: &str) -> Result<bool> {
        let existed = self.facts(project_id)?.iter().any(|fact| fact.key == key);
        if existed {
            self.events.append(&DomainEvent::ProjectFactUnset {
                project_id: project_id.clone(),
                key: key.to_owned(),
            })?;
        }
        Ok(existed)
    }

    /// List a project's facts in key order.
    ///
    /// # Errors
    ///
    /// Returns storage errors.
    pub fn facts(&self, project_id: &ProjectId) -> Result<Vec<ProjectFact>> {
        self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT key, value, note FROM project_facts
                 WHERE project_id = ?1 ORDER BY key",
            )?;
            let rows = statement
                .query_map([project_id.as_str()], |row| {
                    Ok(ProjectFact {
                        key: row.get(0)?,
                        value: row.get(1)?,
                        note: row.get(2)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Whether this project is a derived view (an origin/main shadow) of
    /// another project, marked by the internal `internal.view_of` fact.
    ///
    /// # Errors
    ///
    /// Returns storage errors.
    pub fn is_view_project(&self, project_id: &ProjectId) -> Result<bool> {
        self.database.with_reader(|connection| {
            connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM project_facts
                     WHERE project_id = ?1 AND key = 'internal.view_of')",
                    [project_id.as_str()],
                    |row| row.get(0),
                )
                .map_err(Error::from)
        })
    }

    /// Return a project's recorded checkout paths, most recently observed
    /// first. Paths may no longer exist; callers must check.
    ///
    /// # Errors
    ///
    /// Returns an error for storage failures.
    pub fn alias_paths(&self, project_id: &ProjectId) -> Result<Vec<PathBuf>> {
        self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT path FROM project_aliases WHERE project_id = ?1
                 ORDER BY observed_at_ms DESC",
            )?;
            let rows = statement
                .query_map([project_id.as_str()], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows.into_iter().map(PathBuf::from).collect())
        })
    }

    /// Find one active project by its stable ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the canonical database cannot be queried or contains an invalid ID.
    pub fn find_by_id(&self, id: &ProjectId) -> Result<Option<ProjectRecord>> {
        self.database.with_reader(|connection| {
            connection
                .query_row(
                "SELECT id, name, canonical_remote, root_commit, parent_project_id, created_at_ms
                 FROM projects WHERE id = ?1 AND tombstoned_at_ms IS NULL",
                [id.as_str()],
                project_from_row,
            )
            .optional()
            .map_err(Error::from)
        })
    }

    fn find_by_remote(&self, remote: &str) -> Result<Option<ProjectRecord>> {
        self.database.with_reader(|connection| {
            connection
                .query_row(
                "SELECT id, name, canonical_remote, root_commit, parent_project_id, created_at_ms
                 FROM projects
                 WHERE canonical_remote = ?1 AND tombstoned_at_ms IS NULL",
                [remote],
                project_from_row,
            )
            .optional()
            .map_err(Error::from)
        })
    }

    fn find_by_root_commit_without_remote(
        &self,
        root_commit: &str,
    ) -> Result<Option<ProjectRecord>> {
        self.database.with_reader(|connection| {
            connection
                .query_row(
                "SELECT id, name, canonical_remote, root_commit, parent_project_id, created_at_ms
                 FROM projects
                 WHERE root_commit = ?1 AND canonical_remote IS NULL
                   AND tombstoned_at_ms IS NULL
                 ORDER BY created_at_ms LIMIT 1",
                [root_commit],
                project_from_row,
            )
            .optional()
            .map_err(Error::from)
        })
    }

    fn find_by_alias(&self, path: &Path) -> Result<Option<ProjectRecord>> {
        self.database.with_reader(|connection| {
            connection
                .query_row(
                    "SELECT p.id, p.name, p.canonical_remote, p.root_commit,
                        p.parent_project_id, p.created_at_ms
                 FROM projects p
                 JOIN project_aliases a ON a.project_id = p.id
                 WHERE a.path = ?1 AND p.tombstoned_at_ms IS NULL",
                    [path.to_string_lossy().as_ref()],
                    project_from_row,
                )
                .optional()
                .map_err(Error::from)
        })
    }

    fn project_has_alias(&self, id: &ProjectId, path: &str) -> Result<bool> {
        self.database.with_reader(|connection| {
            connection
                .query_row(
                    "SELECT EXISTS(
                    SELECT 1 FROM project_aliases WHERE project_id = ?1 AND path = ?2
                 )",
                    params![id.as_str(), path],
                    |row| row.get(0),
                )
                .map_err(Error::from)
        })
    }
}

impl GitState {
    /// Discover the nearest Git repository and content-sensitive working-tree state.
    ///
    /// # Errors
    ///
    /// Returns an error when Git cannot execute or repository output cannot be read safely.
    pub fn discover(path: &Path) -> Result<Option<Self>> {
        let root_output = git_output(path, &["rev-parse", "--show-toplevel"])?;
        if !root_output.status.success() {
            return Ok(None);
        }
        let root_text = output_text(&root_output)?;
        let repository_root = canonical_path(Path::new(&root_text))?;

        let remote = git_optional_text(&repository_root, &["remote", "get-url", "origin"])?
            .map(|value| normalize_remote(&value, &repository_root));
        let mut roots =
            git_optional_text(&repository_root, &["rev-list", "--max-parents=0", "HEAD"])?
                .map(|value| value.lines().map(str::to_owned).collect::<Vec<_>>())
                .unwrap_or_default();
        roots.sort();
        let root_commit = roots.into_iter().next();
        let head = git_optional_text(&repository_root, &["rev-parse", "HEAD"])?;
        let branch = git_optional_text(
            &repository_root,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
        )?;

        let status = git_output_bounded(
            &repository_root,
            &["status", "--porcelain=v1", "-z", "--untracked-files=normal"],
            MAX_GIT_STATUS_BYTES,
        )?;
        if !status.status.success() {
            return Err(Error::Git(
                String::from_utf8_lossy(&status.stderr).into_owned(),
            ));
        }
        let dirty = !status.stdout.is_empty();
        let changed_paths = parse_changed_paths(&status.stdout)?;
        let diff_fingerprint = diff_fingerprint(&repository_root, &status.stdout, head.is_some())?;

        Ok(Some(Self {
            repository_root,
            canonical_remote: remote,
            root_commit,
            head,
            branch,
            dirty,
            changed_paths,
            diff_fingerprint,
        }))
    }

    /// Return the bounded set of repository-relative paths whose committed
    /// content differs between a prior indexed HEAD and this state. Both sides
    /// of renames/copies are returned so stale nodes can be tombstoned.
    ///
    /// # Errors
    ///
    /// Returns an error if either revision is unavailable, Git fails, or the
    /// byte/path bounds are exceeded.
    pub fn changed_paths_since(&self, previous_head: &str) -> Result<Vec<PathBuf>> {
        let current_head = self
            .head
            .as_deref()
            .ok_or_else(|| Error::Git("current repository has no HEAD".to_owned()))?;
        let output = git_output_bounded(
            &self.repository_root,
            &[
                "diff",
                "--name-status",
                "-z",
                "--find-renames",
                "--find-copies",
                previous_head,
                current_head,
                "--",
            ],
            MAX_GIT_STATUS_BYTES,
        )?;
        if !output.status.success() {
            return Err(Error::Git(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        parse_name_status_paths(&output.stdout)
    }
}

fn project_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectRecord> {
    let raw_id: String = row.get(0)?;
    let raw_parent: Option<String> = row.get(4)?;
    let id = ProjectId::parse(&raw_id).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let parent_project_id = raw_parent
        .map(|parent| ProjectId::parse(&parent))
        .transpose()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
    Ok(ProjectRecord {
        id,
        name: row.get(1)?,
        canonical_remote: row.get(2)?,
        root_commit: row.get(3)?,
        parent_project_id,
        created_at_ms: row.get(5)?,
    })
}

/// Metadata directory for a repository root: `.engram/`, or the legacy
/// `.leb/` when a checkout still carries one (identity must never fracture
/// across a rename — the pinned project id travels in whichever exists).
#[must_use]
pub fn metadata_dir(root: &Path) -> PathBuf {
    let current = root.join(".engram");
    if current.exists() {
        return current;
    }
    let legacy = root.join(".leb");
    if legacy.exists() { legacy } else { current }
}

fn read_project_metadata(root: &Path) -> Result<Option<ProjectMetadataFile>> {
    let path = metadata_dir(root).join("project.toml");
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path).map_err(|source| io_error(&path, source))?;
    toml::from_str(&contents)
        .map(Some)
        .map_err(|error| Error::InvalidProjectMetadata {
            path,
            message: error.to_string(),
        })
}

fn persist_project_metadata(root: &Path, project: &ProjectRecord) -> Result<()> {
    let directory = metadata_dir(root);
    let path = directory.join("project.toml");
    if path.exists() {
        return Ok(());
    }
    fs::create_dir_all(&directory).map_err(|source| io_error(&directory, source))?;
    let metadata = ProjectMetadataFile {
        project_id: project.id.clone(),
        name: Some(project.name.clone()),
        parent_project_id: project.parent_project_id.clone(),
        derived_view: None,
    };
    let contents = toml::to_string_pretty(&metadata)?;
    let temporary = directory.join(format!(".project.toml.{}.tmp", Uuid::new_v4()));
    fs::write(&temporary, contents).map_err(|source| io_error(&temporary, source))?;
    fs::rename(&temporary, &path).map_err(|source| io_error(&path, source))
}

fn discover_component(requested_path: &Path, project_root: &Path) -> Result<Option<ComponentRef>> {
    let mut current = Some(requested_path);
    while let Some(directory) = current {
        if directory == project_root {
            break;
        }
        let marker = directory.join(".engram/component.toml");
        if marker.exists() {
            let contents =
                fs::read_to_string(&marker).map_err(|source| io_error(&marker, source))?;
            let metadata: ComponentMetadataFile =
                toml::from_str(&contents).map_err(|error| Error::InvalidProjectMetadata {
                    path: marker,
                    message: error.to_string(),
                })?;
            return Ok(Some(ComponentRef {
                name: metadata
                    .name
                    .unwrap_or_else(|| metadata.component_id.clone()),
                id: metadata.component_id,
                root: directory.to_path_buf(),
            }));
        }
        current = directory
            .parent()
            .filter(|parent| parent.starts_with(project_root));
    }
    Ok(None)
}

fn diff_fingerprint(root: &Path, status: &[u8], has_head: bool) -> Result<String> {
    let untracked = git_output_bounded(
        root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        MAX_GIT_STATUS_BYTES,
    )?;
    if !untracked.status.success() {
        return Err(Error::Git(
            String::from_utf8_lossy(&untracked.stderr).into_owned(),
        ));
    }

    let mut hasher = Sha256::new();
    hasher.update(b"git-status\0");
    hasher.update(
        u64::try_from(status.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    hasher.update(status);
    hasher.update(b"git-diff\0");
    if has_head {
        hash_git_stdout(
            root,
            &["diff", "--binary", "--no-ext-diff", "HEAD"],
            &mut hasher,
        )?;
    }
    hasher.update(b"git-untracked\0");
    for raw_path in untracked
        .stdout
        .split(|byte| *byte == 0)
        .filter(|p| !p.is_empty())
    {
        hasher.update(
            u64::try_from(raw_path.len())
                .unwrap_or(u64::MAX)
                .to_le_bytes(),
        );
        hasher.update(raw_path);
        let full_path = root.join(path_from_git_bytes(raw_path));
        let metadata =
            fs::symlink_metadata(&full_path).map_err(|source| io_error(&full_path, source))?;
        if metadata.file_type().is_symlink() {
            hasher.update(b"symlink");
            let target =
                fs::read_link(&full_path).map_err(|source| io_error(&full_path, source))?;
            update_os_path(&mut hasher, &target);
        } else if metadata.is_file() {
            hasher.update(b"file");
            hasher.update(metadata.len().to_le_bytes());
            let mut file = File::open(&full_path).map_err(|source| io_error(&full_path, source))?;
            let mut buffer = [0_u8; 16 * 1024];
            loop {
                let read = file
                    .read(&mut buffer)
                    .map_err(|source| io_error(&full_path, source))?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
        } else {
            hasher.update(b"other");
        }
    }
    Ok(hex_digest(hasher.finalize().as_slice()))
}

/// Test seam exposing the private porcelain-z status parser to the
/// property-based suite in `tests/properties.rs` (same pattern as
/// [`crate::events::EventStore::hold_uncommitted_test_append`]).
///
/// # Errors
///
/// Propagates the parser's own validation errors.
#[doc(hidden)]
pub fn parse_changed_paths_for_tests(status: &[u8]) -> Result<Vec<PathBuf>> {
    parse_changed_paths(status)
}

/// Test seam exposing the private `--name-status -z` parser to the
/// property-based suite in `tests/properties.rs`.
///
/// # Errors
///
/// Propagates the parser's own validation errors.
#[doc(hidden)]
pub fn parse_name_status_paths_for_tests(output: &[u8]) -> Result<Vec<PathBuf>> {
    parse_name_status_paths(output)
}

fn parse_changed_paths(status: &[u8]) -> Result<Vec<PathBuf>> {
    let fields = status
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let mut paths = Vec::new();
    let mut index = 0_usize;
    while index < fields.len() {
        let entry = fields[index];
        if entry.len() < 4 || entry[2] != b' ' {
            return Err(Error::Git("malformed porcelain status record".to_owned()));
        }
        push_changed_path(&mut paths, &entry[3..])?;
        let is_rename_or_copy = matches!(entry[0], b'R' | b'C') || matches!(entry[1], b'R' | b'C');
        if is_rename_or_copy {
            index = index.saturating_add(1);
            let source = fields
                .get(index)
                .ok_or_else(|| Error::Git("truncated rename status record".to_owned()))?;
            push_changed_path(&mut paths, source)?;
        }
        index = index.saturating_add(1);
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn parse_name_status_paths(output: &[u8]) -> Result<Vec<PathBuf>> {
    if !output.is_empty() && output.last() != Some(&0) {
        return Err(Error::Git("truncated Git name-status output".to_owned()));
    }
    let fields = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let mut paths = Vec::new();
    let mut index = 0_usize;
    while index < fields.len() {
        let status = fields[index];
        if status.is_empty()
            || !status[0].is_ascii_alphabetic()
            || !status[1..].iter().all(u8::is_ascii_digit)
        {
            return Err(Error::Git("malformed Git name-status record".to_owned()));
        }
        index = index.saturating_add(1);
        let first = fields
            .get(index)
            .ok_or_else(|| Error::Git("truncated Git name-status path".to_owned()))?;
        push_changed_path(&mut paths, first)?;
        index = index.saturating_add(1);
        if matches!(status[0], b'R' | b'C') {
            let second = fields
                .get(index)
                .ok_or_else(|| Error::Git("truncated Git rename/copy path".to_owned()))?;
            push_changed_path(&mut paths, second)?;
            index = index.saturating_add(1);
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn push_changed_path(paths: &mut Vec<PathBuf>, raw: &[u8]) -> Result<()> {
    if paths.len() >= MAX_CHANGED_PATHS {
        return Err(Error::Config(format!(
            "Git working tree exceeds the {MAX_CHANGED_PATHS}-path freshness limit"
        )));
    }
    let path = path_from_git_bytes(raw);
    // Every component must be a normal segment: this rejects absolute paths,
    // `..` traversal, and `./`-prefixed paths. A leading `./` (which Git never
    // emits) would otherwise defeat the `.git`/`.engram` prefix filter below,
    // because `Path::starts_with(".git")` does not match `./.git/...`.
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(Error::Git("unsafe path in Git status output".to_owned()));
    }
    if path.starts_with(".git") || path.starts_with(".engram") || path.starts_with(".leb") {
        return Ok(());
    }
    paths.push(path);
    Ok(())
}

fn normalize_remote(remote: &str, repository_root: &Path) -> String {
    let remote = remote.trim();
    if let Some((user_host, path)) = remote.split_once(':')
        && !user_host.contains('/')
        && user_host.contains('@')
    {
        let host = user_host
            .rsplit_once('@')
            .map_or(user_host, |(_, host)| host);
        let path = path
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
            .trim_end_matches('/')
            .trim_end_matches(".git");
        return format!("{}/{path}", host.to_ascii_lowercase());
    }

    if remote.contains("://")
        && let Ok(mut parsed) = Url::parse(remote)
    {
        let _ = parsed.set_username("");
        let _ = parsed.set_password(None);
        parsed.set_query(None);
        parsed.set_fragment(None);
        if parsed.scheme() == "file"
            && let Ok(path) = parsed.to_file_path()
        {
            return filesystem_remote_identity(&path, repository_root);
        }
        if let Some(host) = parsed.host_str() {
            let authority = parsed.port().map_or_else(
                || host.to_ascii_lowercase(),
                |port| format!("{host}:{port}"),
            );
            let path = parsed
                .path()
                .trim_start_matches('/')
                .trim_end_matches('/')
                .trim_end_matches(".git");
            return format!("{authority}/{path}")
                .trim_end_matches('/')
                .to_owned();
        }
    }

    filesystem_remote_identity(Path::new(remote), repository_root)
}

fn filesystem_remote_identity(remote: &Path, repository_root: &Path) -> String {
    let absolute = if remote.is_absolute() {
        remote.to_path_buf()
    } else {
        repository_root.join(remote)
    };
    let normalized = fs::canonicalize(&absolute).unwrap_or_else(|_| lexical_normalize(&absolute));
    format!("file:{}", normalized.to_string_lossy())
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(unix)]
fn path_from_git_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;

    std::ffi::OsString::from_vec(bytes.to_vec()).into()
}

#[cfg(not(unix))]
fn path_from_git_bytes(bytes: &[u8]) -> PathBuf {
    String::from_utf8_lossy(bytes).as_ref().into()
}

#[cfg(unix)]
fn update_os_path(hasher: &mut Sha256, path: &Path) {
    use std::os::unix::ffi::OsStrExt;

    let bytes = path.as_os_str().as_bytes();
    hasher.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(not(unix))]
fn update_os_path(hasher: &mut Sha256, path: &Path) {
    let path = path.to_string_lossy();
    hasher.update(u64::try_from(path.len()).unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(path.as_bytes());
}

fn project_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("project")
        .to_owned()
}

fn canonical_path(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(|source| io_error(path, source))
}

fn git_optional_text(path: &Path, arguments: &[&str]) -> Result<Option<String>> {
    let output = git_output(path, arguments)?;
    if !output.status.success() {
        return Ok(None);
    }
    output_text(&output).map(Some)
}

fn git_output(path: &Path, arguments: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(arguments)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|source| Error::Git(source.to_string()))
}

fn git_output_bounded(path: &Path, arguments: &[&str], max_stdout: u64) -> Result<Output> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(arguments)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| Error::Git(source.to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Git("Git stdout pipe unavailable".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::Git("Git stderr pipe unavailable".to_owned()))?;
    let stderr_reader = std::thread::spawn(move || read_bounded(stderr, 1_024 * 1_024));
    let stdout = read_bounded(stdout, max_stdout);
    if stdout.is_err() {
        let _ = child.kill();
    }
    let status = child
        .wait()
        .map_err(|source| Error::Git(source.to_string()))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| Error::Git("Git stderr reader panicked".to_owned()))??;
    Ok(Output {
        status,
        stdout: stdout?,
        stderr,
    })
}

fn read_bounded(mut reader: impl Read, max_bytes: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| Error::Git(source.to_string()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(Error::Config(format!(
            "Git output exceeds the configured {max_bytes}-byte safety limit"
        )));
    }
    Ok(bytes)
}

fn hash_git_stdout(root: &Path, arguments: &[&str], hasher: &mut Sha256) -> Result<()> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| Error::Git(source.to_string()))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Git("Git stdout pipe unavailable".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::Git("Git stderr pipe unavailable".to_owned()))?;
    let stderr_reader = std::thread::spawn(move || read_bounded(stderr, 1_024 * 1_024));
    let mut buffer = vec![0_u8; 64 * 1_024];
    loop {
        let read = stdout
            .read(&mut buffer)
            .map_err(|source| Error::Git(source.to_string()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let status = child
        .wait()
        .map_err(|source| Error::Git(source.to_string()))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| Error::Git("Git stderr reader panicked".to_owned()))??;
    if !status.success() {
        return Err(Error::Git(String::from_utf8_lossy(&stderr).into_owned()));
    }
    Ok(())
}

fn output_text(output: &Output) -> Result<String> {
    let value = String::from_utf8(output.stdout.clone())
        .map_err(|error| Error::Git(format!("Git emitted non-UTF-8 output: {error}")))?;
    Ok(value.trim().to_owned())
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    fn registry(temp: &TempDir) -> ProjectRegistry {
        let database = Database::open(temp.path().join("registry.sqlite")).expect("database");
        ProjectRegistry::new(database)
    }

    #[test]
    fn legacy_leb_metadata_dir_keeps_identity_and_stays_the_write_target() {
        let temp = TempDir::new().expect("temp");
        let registry = registry(&temp);
        let repo = temp.path().join("legacy");
        fs::create_dir_all(repo.join(".leb")).expect("legacy dir");
        fs::write(
            repo.join(".leb/project.toml"),
            "project_id = \"prj_a11ce000-b31c-4f2e-9e01-000000000042\"\nname = \"legacy\"\n",
        )
        .expect("legacy metadata");

        let resolved = registry.resolve_or_register(&repo).expect("resolve");
        assert_eq!(
            resolved.project.id.as_str(),
            "prj_a11ce000-b31c-4f2e-9e01-000000000042",
            "pinned identity survives the rename"
        );
        assert!(
            !repo.join(".engram").exists(),
            "an existing legacy dir stays the metadata home; no parallel .engram"
        );
        assert_eq!(metadata_dir(&repo), repo.join(".leb"));

        let fresh = temp.path().join("fresh");
        fs::create_dir_all(&fresh).expect("fresh");
        registry
            .resolve_or_register(&fresh)
            .expect("register fresh");
        assert!(
            fresh.join(".engram/project.toml").exists(),
            "new repos use .engram"
        );
    }

    #[test]
    fn lookup_is_read_only_and_finds_registered_projects() {
        let temp = TempDir::new().expect("temp");
        let registry = registry(&temp);
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");

        let unknown = registry.lookup(&repo).expect("lookup unknown");
        assert!(unknown.is_none(), "unregistered path must resolve to None");
        assert!(
            !repo.join(".engram").exists(),
            "lookup must never persist metadata"
        );

        let resolved = registry.resolve_or_register(&repo).expect("register");
        let found = registry
            .lookup(&repo)
            .expect("lookup known")
            .expect("registered path must resolve");
        assert_eq!(found.id, resolved.project.id);
    }

    fn run_git(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo(path: &Path) {
        fs::create_dir_all(path).expect("repo dir");
        run_git(path, &["init", "-q"]);
        run_git(path, &["config", "user.email", "engram@example.invalid"]);
        run_git(path, &["config", "user.name", "Engram Tests"]);
        fs::write(path.join("README.md"), "initial\n").expect("fixture file");
        run_git(path, &["add", "README.md"]);
        run_git(path, &["commit", "-qm", "initial"]);
    }

    #[test]
    fn test_explicit_project_uuid_wins_over_path() {
        let temp = TempDir::new().expect("temp dir");
        let repo = temp.path().join("repo");
        fs::create_dir_all(repo.join(".engram")).expect("metadata dir");
        let explicit = "prj_11111111-1111-4111-8111-111111111111";
        fs::write(
            repo.join(".engram/project.toml"),
            format!("project_id = '{explicit}'\n"),
        )
        .expect("metadata");
        let resolved = registry(&temp)
            .resolve_or_register(&repo)
            .expect("resolved");
        assert_eq!(resolved.project.id.as_str(), explicit);
    }

    #[test]
    fn test_repo_move_preserves_identity() {
        let temp = TempDir::new().expect("temp dir");
        let original = temp.path().join("original");
        fs::create_dir_all(&original).expect("project");
        let registry = registry(&temp);
        let first = registry.resolve_or_register(&original).expect("first");
        let moved = temp.path().join("moved");
        fs::rename(&original, &moved).expect("move");
        let second = registry.resolve_or_register(&moved).expect("second");
        assert_eq!(first.project.id, second.project.id);
    }

    #[test]
    fn test_same_remote_different_worktree_same_project() {
        let temp = TempDir::new().expect("temp dir");
        let main = temp.path().join("main");
        let worktree = temp.path().join("worktree");
        init_repo(&main);
        run_git(
            &main,
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:example/engram.git",
            ],
        );
        run_git(
            &main,
            &[
                "worktree",
                "add",
                "-qb",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );
        let registry = registry(&temp);
        let first = registry.resolve_or_register(&main).expect("main");
        let second = registry.resolve_or_register(&worktree).expect("worktree");
        assert_eq!(first.project.id, second.project.id);
    }

    #[test]
    fn test_distinct_fork_can_be_linked_but_not_collapsed() {
        let temp = TempDir::new().expect("temp dir");
        let first_repo = temp.path().join("first");
        init_repo(&first_repo);
        run_git(
            &first_repo,
            &["remote", "add", "origin", "git@example.com:org/first.git"],
        );
        let second_repo = temp.path().join("second");
        run_git(
            temp.path(),
            &[
                "clone",
                "-q",
                first_repo.to_str().unwrap(),
                second_repo.to_str().unwrap(),
            ],
        );
        run_git(
            &second_repo,
            &[
                "remote",
                "set-url",
                "origin",
                "git@example.com:fork/second.git",
            ],
        );

        let registry = registry(&temp);
        let first = registry.resolve_or_register(&first_repo).expect("first");
        let second = registry
            .resolve_or_register_with_parent(&second_repo, Some(&first.project.id))
            .expect("second");
        assert_ne!(first.project.id, second.project.id);
        assert_eq!(second.project.parent_project_id, Some(first.project.id));
    }

    #[test]
    fn test_nested_repo_resolves_nearest_root() {
        let temp = TempDir::new().expect("temp dir");
        let outer = temp.path().join("outer");
        let inner = outer.join("vendor/inner");
        init_repo(&outer);
        init_repo(&inner);
        let resolved = registry(&temp)
            .resolve_or_register(&inner.join("src"))
            .or_else(|_| {
                fs::create_dir_all(inner.join("src")).expect("src");
                registry(&temp).resolve_or_register(&inner.join("src"))
            })
            .expect("resolved");
        assert_eq!(resolved.root, fs::canonicalize(inner).expect("canonical"));
    }

    #[test]
    fn test_monorepo_component_scope_resolves() {
        let temp = TempDir::new().expect("temp dir");
        let repo = temp.path().join("repo");
        init_repo(&repo);
        let component = repo.join("services/api");
        fs::create_dir_all(component.join(".engram")).expect("component dir");
        fs::write(
            component.join(".engram/component.toml"),
            "component_id = 'cmp_api'\nname = 'API'\n",
        )
        .expect("component marker");
        let resolved = registry(&temp)
            .resolve_or_register(&component)
            .expect("resolved");
        assert_eq!(resolved.component.expect("component").id, "cmp_api");
    }

    #[test]
    fn test_no_git_repo_gets_persisted_generated_uuid() {
        let temp = TempDir::new().expect("temp dir");
        let project = temp.path().join("plain");
        fs::create_dir_all(&project).expect("project");
        let resolved = registry(&temp)
            .resolve_or_register(&project)
            .expect("resolved");
        let metadata = fs::read_to_string(project.join(".engram/project.toml")).expect("metadata");
        assert!(metadata.contains(resolved.project.id.as_str()));
    }

    #[test]
    fn test_project_alias_path_normalization() {
        let temp = TempDir::new().expect("temp dir");
        let project = temp.path().join("project");
        fs::create_dir_all(project.join("nested")).expect("project");
        let registry = registry(&temp);
        let first = registry
            .resolve_or_register(&project.join("nested/.."))
            .expect("first");
        let second = registry.resolve_or_register(&project).expect("second");
        assert_eq!(first.project.id, second.project.id);
    }

    #[test]
    fn test_project_identity_does_not_depend_on_branch_name() {
        let temp = TempDir::new().expect("temp dir");
        let repo = temp.path().join("repo");
        init_repo(&repo);
        let registry = registry(&temp);
        let first = registry.resolve_or_register(&repo).expect("first");
        run_git(&repo, &["switch", "-qc", "different-name"]);
        let second = registry.resolve_or_register(&repo).expect("second");
        assert_eq!(first.project.id, second.project.id);
    }

    #[test]
    fn test_dirty_diff_hash_changes_on_edit() {
        let temp = TempDir::new().expect("temp dir");
        let repo = temp.path().join("repo");
        init_repo(&repo);
        fs::write(repo.join("README.md"), "first edit\n").expect("edit one");
        let first_state = GitState::discover(&repo)
            .expect("discover")
            .expect("git repo");
        assert_eq!(first_state.changed_paths, [PathBuf::from("README.md")]);
        let first = first_state.diff_fingerprint;
        fs::write(repo.join("README.md"), "second edit\n").expect("edit two");
        let second = GitState::discover(&repo)
            .expect("discover")
            .expect("git repo")
            .diff_fingerprint;
        assert_ne!(first, second);
    }

    #[test]
    fn unborn_git_repository_resolves() {
        let temp = TempDir::new().expect("temp dir");
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).expect("repo");
        run_git(&repo, &["init", "-q"]);
        fs::write(repo.join("untracked.txt"), "content\n").expect("untracked file");
        let state = GitState::discover(&repo)
            .expect("discover")
            .expect("git repository");
        assert!(state.head.is_none());
        assert!(state.dirty);
        registry(&temp)
            .resolve_or_register(&repo)
            .expect("register unborn repository");
    }

    #[test]
    fn remote_credentials_are_not_persisted() {
        let root = Path::new("/tmp/project");
        assert_eq!(
            normalize_remote("https://secret-token@GitHub.com/org/repo.git", root),
            "github.com/org/repo"
        );
        assert_eq!(
            normalize_remote("git@GitHub.com:org/repo.git", root),
            "github.com/org/repo"
        );
        assert_eq!(
            normalize_remote("https://github.com/org/repo.git?access_token=secret", root),
            "github.com/org/repo"
        );
    }

    #[test]
    fn relative_remotes_in_different_trees_do_not_collapse_projects() {
        let temp = TempDir::new().expect("temp dir");
        let first_repo = temp.path().join("first/work");
        let second_repo = temp.path().join("second/work");
        fs::create_dir_all(temp.path().join("first/origin")).expect("first origin");
        fs::create_dir_all(temp.path().join("second/origin")).expect("second origin");
        init_repo(&first_repo);
        init_repo(&second_repo);
        run_git(&first_repo, &["remote", "add", "origin", "../origin"]);
        run_git(&second_repo, &["remote", "add", "origin", "../origin"]);

        let registry = registry(&temp);
        let first = registry
            .resolve_or_register(&first_repo)
            .expect("first project");
        let second = registry
            .resolve_or_register(&second_repo)
            .expect("second project");
        assert_ne!(first.project.id, second.project.id);
        assert_ne!(
            first.project.canonical_remote,
            second.project.canonical_remote
        );
    }

    #[test]
    fn copies_carrying_project_metadata_share_identity_across_remotes() {
        // Benchmark-integrity contract: a `.engram/project.toml` traveling with
        // a checkout pins identity even when clones carry different
        // local-path remotes. Copies WITHOUT metadata and without a hosted
        // remote deliberately mint new identities (an independent repo with
        // an identical template history is indistinguishable from a clone).
        let temp = TempDir::new().expect("temp dir");
        let upstream = temp.path().join("upstream");
        init_repo(&upstream);
        run_git(temp.path(), &["clone", "-q", "upstream", "first"]);
        run_git(temp.path(), &["clone", "-q", "upstream", "second"]);
        let second = temp.path().join("second");
        run_git(
            &second,
            &["remote", "set-url", "origin", "../upstream/.git"],
        );

        let registry = registry(&temp);
        let seeded = registry
            .resolve_or_register(&temp.path().join("first"))
            .expect("first clone");
        // Carry the metadata the way the benchmark fixture does.
        fs::create_dir_all(second.join(".engram")).expect("metadata dir");
        fs::copy(
            temp.path().join("first/.engram/project.toml"),
            second.join(".engram/project.toml"),
        )
        .expect("carry metadata");
        let arm = registry.resolve_or_register(&second).expect("second clone");
        assert_eq!(
            seeded.project.id, arm.project.id,
            "metadata-pinned copies must resolve to one project"
        );
    }

    #[test]
    fn first_registration_returns_post_write_git_state() {
        let temp = TempDir::new().expect("temp dir");
        let repo = temp.path().join("repo");
        init_repo(&repo);
        let resolved = registry(&temp)
            .resolve_or_register(&repo)
            .expect("registered project");
        assert!(resolved.git.expect("git state").dirty);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_untracked_path_is_hashed_without_loss() {
        use std::os::unix::ffi::OsStringExt;

        let temp = TempDir::new().expect("temp dir");
        let repo = temp.path().join("repo");
        init_repo(&repo);
        let filename = std::ffi::OsString::from_vec(b"non-utf8-\xff".to_vec());
        let path = repo.join(filename);
        fs::write(&path, "first").expect("first content");
        let first = GitState::discover(&repo)
            .expect("discover first")
            .expect("git")
            .diff_fingerprint;
        fs::write(&path, "second").expect("second content");
        let second = GitState::discover(&repo)
            .expect("discover second")
            .expect("git")
            .diff_fingerprint;
        assert_ne!(first, second);
    }

    #[cfg(unix)]
    #[test]
    fn untracked_symlink_hash_does_not_follow_external_target() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp dir");
        let repo = temp.path().join("repo");
        init_repo(&repo);
        let external = temp.path().join("external.txt");
        fs::write(&external, "first secret").expect("external content");
        symlink(&external, repo.join("external-link")).expect("symlink");
        let first = GitState::discover(&repo)
            .expect("discover first")
            .expect("git")
            .diff_fingerprint;
        fs::write(&external, "different secret").expect("change external content");
        let second = GitState::discover(&repo)
            .expect("discover second")
            .expect("git")
            .diff_fingerprint;
        assert_eq!(first, second);
    }

    #[test]
    fn project_links_roundtrip_with_validation() {
        let temp = TempDir::new().expect("temp dir");
        let registry = registry(&temp);
        let root_a = temp.path().join("service-a");
        let root_b = temp.path().join("service-b");
        fs::create_dir_all(&root_a).expect("a dir");
        fs::create_dir_all(&root_b).expect("b dir");
        let a = registry.resolve_or_register(&root_a).expect("a").project.id;
        let b = registry.resolve_or_register(&root_b).expect("b").project.id;

        assert!(registry.link(&a, &a, "uses", None).is_err(), "self-link");
        assert!(
            registry.link(&a, &ProjectId::new(), "uses", None).is_err(),
            "unknown target"
        );
        assert!(registry.link(&a, &b, "", None).is_err(), "empty relation");

        let link = registry
            .link(&a, &b, "interface", Some("b is a's API surface"))
            .expect("link");
        assert_eq!(link.project_id, b);
        assert_eq!(link.relation, "interface");

        let links = registry.links(&a).expect("links");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].project_id, b);
        assert_eq!(links[0].note.as_deref(), Some("b is a's API surface"));
        assert!(registry.links(&b).expect("reverse").is_empty(), "directed");

        // Re-linking updates the relation instead of duplicating.
        registry.link(&a, &b, "uses", None).expect("relink");
        let links = registry.links(&a).expect("links after relink");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].relation, "uses");

        assert!(registry.unlink(&a, &b).expect("unlink"));
        assert!(!registry.unlink(&a, &b).expect("second unlink is a no-op"));
        assert!(registry.links(&a).expect("empty").is_empty());
    }

    #[test]
    fn project_facts_roundtrip_with_validation() {
        let temp = TempDir::new().expect("temp dir");
        let registry = registry(&temp);
        let root = temp.path().join("svc");
        fs::create_dir_all(&root).expect("dir");
        let id = registry.resolve_or_register(&root).expect("svc").project.id;

        assert!(registry.set_fact(&id, "", "x", None).is_err());
        assert!(registry.set_fact(&id, "k", "", None).is_err());
        assert!(
            registry
                .set_fact(&ProjectId::new(), "k", "v", None)
                .is_err()
        );

        registry
            .set_fact(&id, "hosting.base_url", "api.example.com", Some("prod"))
            .expect("set");
        registry
            .set_fact(&id, "hosting.provider", "gcp", None)
            .expect("set");
        let facts = registry.facts(&id).expect("facts");
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].key, "hosting.base_url");
        assert_eq!(facts[0].note.as_deref(), Some("prod"));

        // Upsert replaces the value in place.
        registry
            .set_fact(&id, "hosting.provider", "aws", None)
            .expect("update");
        let facts = registry.facts(&id).expect("facts");
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[1].value, "aws");

        assert!(registry.unset_fact(&id, "hosting.provider").expect("unset"));
        assert!(!registry.unset_fact(&id, "hosting.provider").expect("gone"));
        assert_eq!(registry.facts(&id).expect("facts").len(), 1);
    }
}
