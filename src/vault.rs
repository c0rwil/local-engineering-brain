//! Obsidian-vault knowledge export built from the canonical world model.
//!
//! The vault is a DERIVED, rebuildable artifact: one folder per project with
//! an `INDEX.md` (identity, freshness, a bounded mermaid module diagram, and
//! links), one note per validated memory, and a workspace-level
//! `WORKSPACE.md` whose mermaid flowchart draws the explicit project links.
//! Content is deterministic (stable ordering, no wall-clock stamps — the only
//! times shown come from recorded scans), and files are rewritten only when
//! their bytes change so Obsidian sync/history stays quiet. Vault refreshes
//! ride the same freshness machinery as everything else: the daemon enqueues
//! a `vault_export` job after a structural scan changes the graph, so notes
//! and diagrams track the code without any manual step.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ids::ProjectId;
use crate::projects::ProjectRegistry;
use crate::storage::Database;

/// Bounded diagram sizes: Obsidian renders mermaid client-side and large
/// graphs become unreadable long before they become slow.
const MAX_DIAGRAM_GROUPS: usize = 24;
const MAX_DIAGRAM_EDGES: usize = 48;
const MAX_MEMORY_NOTES: usize = 500;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct VaultReport {
    pub root: String,
    pub projects: Vec<String>,
    pub files_written: u32,
    pub files_unchanged: u32,
}

#[derive(Clone, Debug)]
pub struct VaultBuilder {
    database: Database,
    root: PathBuf,
}

struct ModuleGroup {
    label: String,
    node_count: i64,
}

impl VaultBuilder {
    #[must_use]
    pub const fn new(database: Database, root: PathBuf) -> Self {
        Self { database, root }
    }

    /// Export one project's folder plus the shared workspace overview.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown projects, storage failures, or vault I/O.
    pub fn build_project(&self, project_id: &ProjectId) -> Result<VaultReport> {
        let registry = ProjectRegistry::new(self.database.clone());
        let project = registry
            .find_by_id(project_id)?
            .ok_or_else(|| Error::Config(format!("unknown project `{project_id}`")))?;
        let mut report = VaultReport::default();
        let dir = self.root.join(slug(&project.name));
        fs::create_dir_all(dir.join("memories"))
            .map_err(|error| Error::Config(format!("vault directory: {error}")))?;

        let index = self.render_project_index(&registry, project_id, &project.name)?;
        write_if_changed(&dir.join("INDEX.md"), &index, &mut report)?;
        for (file_name, body) in self.render_memory_notes(project_id, &project.name)? {
            write_if_changed(&dir.join("memories").join(file_name), &body, &mut report)?;
        }
        let workspace = self.render_workspace(&registry)?;
        write_if_changed(&self.root.join("WORKSPACE.md"), &workspace, &mut report)?;
        report.projects.push(project.name);
        report.root = self.root.to_string_lossy().into_owned();
        Ok(report)
    }

    /// Export every registered (non-tombstoned) project and the workspace.
    ///
    /// # Errors
    ///
    /// Returns an error for storage failures or vault I/O.
    pub fn build_all(&self) -> Result<VaultReport> {
        let ids = self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT id FROM projects WHERE tombstoned_at_ms IS NULL ORDER BY name, id",
            )?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        let mut total = VaultReport::default();
        for id in ids {
            let report = self.build_project(&ProjectId::parse(&id)?)?;
            total.files_written += report.files_written;
            total.files_unchanged += report.files_unchanged;
            total.projects.extend(report.projects);
        }
        total.projects.dedup();
        total.root = self.root.to_string_lossy().into_owned();
        Ok(total)
    }

    fn render_project_index(
        &self,
        registry: &ProjectRegistry,
        project_id: &ProjectId,
        name: &str,
    ) -> Result<String> {
        let (head, scanned_at, provider) = self.latest_scan(project_id)?;
        let kinds = self.node_kind_counts(project_id)?;
        let groups = self.module_groups(project_id)?;
        let edges = self.group_edges(project_id)?;
        let links = registry.links(project_id)?;
        let memory_count = self.validated_memory_count(project_id)?;

        let mut out = String::new();
        let _ = write!(out, "# {}\n\n", md_safe(name));
        let _ = writeln!(out, "- Project id: `{project_id}`");
        if let Some(head) = &head {
            let _ = writeln!(out, "- Indexed HEAD: `{head}`");
        }
        if let Some(at) = scanned_at {
            let _ = writeln!(out, "- Last structural scan: {}", iso_date(at));
        }
        if let Some(provider) = &provider {
            let _ = writeln!(out, "- Graph provider: {provider}");
        }
        let _ = writeln!(
            out,
            "- Validated memories: {memory_count} ([[{}/memories|browse]])",
            slug(name)
        );
        if !kinds.is_empty() {
            let summary = kinds
                .iter()
                .map(|(kind, count)| format!("{kind} {count}"))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(out, "- Graph: {summary}");
        }

        if !links.is_empty() {
            out.push_str("\n## Linked repositories\n\n");
            for link in &links {
                let note = link
                    .note
                    .as_deref()
                    .map(|note| format!(" — {}", md_safe(note)))
                    .unwrap_or_default();
                let _ = writeln!(
                    out,
                    "- [[{}/INDEX|{}]] ({}){note}",
                    slug(&link.name),
                    md_safe(&link.name),
                    md_safe(&link.relation)
                );
            }
        }

        if !groups.is_empty() {
            out.push_str("\n## Module map\n\n```mermaid\nflowchart LR\n");
            for (ordinal, group) in groups.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "    m{ordinal}[\"{} ({})\"]",
                    mermaid_safe(&group.label),
                    group.node_count
                );
            }
            let index_of: BTreeMap<&str, usize> = groups
                .iter()
                .enumerate()
                .map(|(ordinal, group)| (group.label.as_str(), ordinal))
                .collect();
            for (source, target, count) in &edges {
                if let (Some(from), Some(to)) =
                    (index_of.get(source.as_str()), index_of.get(target.as_str()))
                {
                    let _ = writeln!(out, "    m{from} -->|{count}| m{to}");
                }
            }
            out.push_str("```\n");
        }
        Ok(out)
    }

    fn render_memory_notes(
        &self,
        project_id: &ProjectId,
        project_name: &str,
    ) -> Result<Vec<(String, String)>> {
        let rows = self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, kind, claim, rationale, confidence, tags_json
                 FROM memories
                 WHERE project_id = ?1 AND status = 'active'
                   AND epistemic_state = 'validated'
                 ORDER BY known_from_ms, id LIMIT ?2",
            )?;
            let rows = statement
                .query_map(
                    params![
                        project_id.as_str(),
                        i64::try_from(MAX_MEMORY_NOTES).unwrap_or(i64::MAX)
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, f64>(4)?,
                            row.get::<_, String>(5)?,
                        ))
                    },
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        let mut notes = Vec::new();
        for (id, kind, claim, rationale, confidence, tags_json) in rows {
            let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
            let mut body = String::new();
            body.push_str("---\n");
            let _ = writeln!(body, "memory: {id}");
            let _ = writeln!(body, "kind: {kind}");
            body.push_str("state: validated\n");
            let _ = writeln!(body, "confidence: {confidence}");
            if !tags.is_empty() {
                let _ = writeln!(body, "tags: [{}]", tags.join(", "));
            }
            body.push_str("---\n\n");
            let _ = write!(body, "{}\n\n", md_safe(&claim));
            let _ = write!(body, "**Why:** {}\n\n", md_safe(&rationale));
            let _ = writeln!(
                body,
                "Project: [[{}/INDEX|{}]]",
                slug(project_name),
                md_safe(project_name)
            );
            notes.push((format!("{id}.md"), body));
        }
        Ok(notes)
    }

    fn render_workspace(&self, registry: &ProjectRegistry) -> Result<String> {
        let projects = self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, name FROM projects WHERE tombstoned_at_ms IS NULL
                 ORDER BY name, id",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        let mut out = String::new();
        out.push_str("# Workspace\n\nEvery registered project and its explicit knowledge links. Edges carry the declared relation; knowledge flows along them into compiled context.\n\n");
        out.push_str("```mermaid\nflowchart LR\n");
        let ordinal_of: BTreeMap<&str, usize> = projects
            .iter()
            .enumerate()
            .map(|(ordinal, (id, _))| (id.as_str(), ordinal))
            .collect();
        for (ordinal, (_, name)) in projects.iter().enumerate() {
            let _ = writeln!(out, "    p{ordinal}[\"{}\"]", mermaid_safe(name));
        }
        for (id, _) in &projects {
            let project_id = ProjectId::parse(id)?;
            for link in registry.links(&project_id)? {
                if let (Some(from), Some(to)) = (
                    ordinal_of.get(id.as_str()),
                    ordinal_of.get(link.project_id.as_str()),
                ) {
                    let _ = writeln!(
                        out,
                        "    p{from} -->|{}| p{to}",
                        mermaid_safe(&link.relation)
                    );
                }
            }
        }
        out.push_str("```\n\n");
        for (_, name) in &projects {
            let _ = writeln!(out, "- [[{}/INDEX|{}]]", slug(name), md_safe(name));
        }
        Ok(out)
    }

    fn latest_scan(
        &self,
        project_id: &ProjectId,
    ) -> Result<(Option<String>, Option<i64>, Option<String>)> {
        self.database.with_reader(|connection| {
            use rusqlite::OptionalExtension as _;
            connection
                .query_row(
                    "SELECT head, completed_at_ms, provider FROM structural_scans
                     WHERE project_id = ?1 ORDER BY completed_at_ms DESC LIMIT 1",
                    [project_id.as_str()],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(Error::from)
                .map(|row| row.unwrap_or((None, None, None)))
        })
    }

    fn node_kind_counts(&self, project_id: &ProjectId) -> Result<Vec<(String, i64)>> {
        self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT kind, COUNT(*) FROM nodes
                 WHERE project_id = ?1 AND tombstoned_at_ms IS NULL
                 GROUP BY kind ORDER BY kind",
            )?;
            let rows = statement
                .query_map([project_id.as_str()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    fn module_groups(&self, project_id: &ProjectId) -> Result<Vec<ModuleGroup>> {
        self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT file_path, COUNT(*) AS n FROM nodes
                 WHERE project_id = ?1 AND tombstoned_at_ms IS NULL
                   AND file_path IS NOT NULL
                 GROUP BY file_path",
            )?;
            let rows = statement
                .query_map([project_id.as_str()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut groups = BTreeMap::<String, i64>::new();
            for (path, count) in rows {
                *groups.entry(group_of(&path)).or_default() += count;
            }
            let mut groups = groups
                .into_iter()
                .map(|(label, node_count)| ModuleGroup { label, node_count })
                .collect::<Vec<_>>();
            groups.sort_by(|a, b| {
                b.node_count
                    .cmp(&a.node_count)
                    .then_with(|| a.label.cmp(&b.label))
            });
            groups.truncate(MAX_DIAGRAM_GROUPS);
            groups.sort_by(|a, b| a.label.cmp(&b.label));
            Ok(groups)
        })
    }

    fn group_edges(&self, project_id: &ProjectId) -> Result<Vec<(String, String, i64)>> {
        self.database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT s.file_path, t.file_path, COUNT(*) AS n
                 FROM edges e
                 JOIN nodes s ON s.id = e.source_id
                 JOIN nodes t ON t.id = e.target_id
                 WHERE e.project_id = ?1 AND e.tombstoned_at_ms IS NULL
                   AND s.file_path IS NOT NULL AND t.file_path IS NOT NULL
                 GROUP BY s.file_path, t.file_path",
            )?;
            let rows = statement
                .query_map([project_id.as_str()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut aggregated = BTreeMap::<(String, String), i64>::new();
            for (source, target, count) in rows {
                let source = group_of(&source);
                let target = group_of(&target);
                if source != target {
                    *aggregated.entry((source, target)).or_default() += count;
                }
            }
            let mut edges = aggregated
                .into_iter()
                .map(|((source, target), count)| (source, target, count))
                .collect::<Vec<_>>();
            edges.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| (&a.0, &a.1).cmp(&(&b.0, &b.1))));
            edges.truncate(MAX_DIAGRAM_EDGES);
            edges.sort();
            Ok(edges)
        })
    }

    fn validated_memory_count(&self, project_id: &ProjectId) -> Result<i64> {
        self.database.with_reader(|connection| {
            connection
                .query_row(
                    "SELECT COUNT(*) FROM memories
                     WHERE project_id = ?1 AND status = 'active'
                       AND epistemic_state = 'validated'",
                    [project_id.as_str()],
                    |row| row.get(0),
                )
                .map_err(Error::from)
        })
    }
}

/// The directory prefix used to aggregate files into diagram nodes: the first
/// two path components (`src/world`), or one for top-level files.
fn group_of(path: &str) -> String {
    let mut components = path.split('/').filter(|part| !part.is_empty());
    match (components.next(), components.next()) {
        (Some(first), Some(second)) if second.contains('.') => first.to_owned(),
        (Some(first), Some(second)) => format!("{first}/{second}"),
        (Some(first), None) => first.to_owned(),
        _ => "(root)".to_owned(),
    }
}

fn slug(name: &str) -> String {
    let slug = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    if slug.is_empty() {
        "project".to_owned()
    } else {
        slug
    }
}

fn md_safe(text: &str) -> String {
    text.replace(['\n', '\r'], " ").replace("]]", "] ]")
}

fn mermaid_safe(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric()
                || matches!(character, ' ' | '-' | '_' | '.' | '/' | ':')
            {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn iso_date(ms: i64) -> String {
    // Deterministic derived-from-data date without a chrono dependency: days
    // since epoch converted via the civil-from-days algorithm.
    let days = ms.div_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

#[allow(clippy::many_single_char_names)]
const fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (y, m as u32, d as u32)
}

fn write_if_changed(path: &Path, content: &str, report: &mut VaultReport) -> Result<()> {
    if fs::read_to_string(path).is_ok_and(|existing| existing == content) {
        report.files_unchanged += 1;
        return Ok(());
    }
    fs::write(path, content).map_err(|error| Error::Config(format!("vault write: {error}")))?;
    report.files_written += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::experience::{EvidenceStrength, ExperienceStore, TaskOutcome};
    use crate::memory::{EpistemicState, MemoryDraft, MemoryStore};

    #[test]
    fn vault_exports_workspace_diagram_project_index_and_memory_notes() {
        let temp = TempDir::new().expect("temp");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let registry = ProjectRegistry::new(database.clone());
        let root_a = temp.path().join("api-service");
        let root_b = temp.path().join("platform");
        fs::create_dir(&root_a).expect("a");
        fs::create_dir(&root_b).expect("b");
        let a = registry.resolve_or_register(&root_a).expect("a").project.id;
        let b = registry.resolve_or_register(&root_b).expect("b").project.id;
        registry.link(&a, &b, "interface", None).expect("link");

        let experience = ExperienceStore::new(database.clone());
        let memories = MemoryStore::new(database.clone());
        let task = experience.start_task(&a, "seed", None).expect("task");
        let evidence = experience
            .record_evidence(
                &a,
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
                &a,
                &MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: a.to_string(),
                    kind: "lesson".to_owned(),
                    claim: "Cursor pagination tokens are opaque".to_owned(),
                    rationale: "Validated".to_owned(),
                    confidence: 0.5,
                    importance: 0.9,
                    tags: vec!["pagination".to_owned()],
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

        let vault_root = temp.path().join("vault");
        let builder = VaultBuilder::new(database, vault_root.clone());
        let report = builder.build_project(&a).expect("build");
        assert!(report.files_written >= 3, "index, memory note, workspace");

        let index = fs::read_to_string(vault_root.join("api-service/INDEX.md")).expect("index");
        assert!(index.contains("# api-service"));
        assert!(index.contains("[[platform/INDEX|platform]] (interface)"));
        assert!(index.contains("Validated memories: 1"));

        let workspace = fs::read_to_string(vault_root.join("WORKSPACE.md")).expect("workspace");
        assert!(workspace.contains("```mermaid"));
        assert!(workspace.contains("-->|interface|"));
        assert!(workspace.contains("[[api-service/INDEX|api-service]]"));

        let note = fs::read_to_string(
            vault_root
                .join("api-service/memories")
                .join(format!("{}.md", memory.id)),
        )
        .expect("note");
        assert!(note.contains("Cursor pagination tokens are opaque"));
        assert!(note.contains("kind: lesson"));

        // Idempotency: a second build rewrites nothing.
        let second = builder.build_project(&a).expect("rebuild");
        assert_eq!(second.files_written, 0, "byte-stable rebuild");
        assert!(second.files_unchanged >= 3);
    }
}
