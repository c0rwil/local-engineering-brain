//! Bounded related-use lookup for a task-relevant definition.
//!
//! The native indexer records every call as an `ExternalReference` node in the
//! calling file, labelled with the callee expression text; it does not resolve
//! imports or receivers. This module therefore reports two separate facts:
//!
//! - a caller, only when the call text names the definition exactly, in the
//!   same language family, and resolves to it: within the definition's file
//!   when no other definition there has the name; from another file only when
//!   the name is unique in the repository and the calling file imports it (or
//!   shares the directory in a language with package-wide visibility);
//! - a shared call: another definition in the same file that makes the same
//!   call expression as the definition, such as acquiring the same lock. This
//!   is a textual co-use, never presented as a dependency between the two.
//!
//! Direct graph edges into the definition (for example from an external graph
//! export) are reported as edges. Call text that does not resolve is ignored.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{OptionalExtension, params};

use crate::error::Result;
use crate::storage::Database;

/// Reference nodes (one per calling file) inspected per definition name.
const MAX_REFERENCE_FILES: i64 = 256;
/// Call edges inspected per definition.
const MAX_CALL_EDGES: i64 = 512;
/// Call expressions of the definition inspected for shared use.
const MAX_OWN_CALLS: i64 = 512;
/// A call expression shared by more definitions in one file is too common to
/// identify a specific shared resource.
const MAX_SHARED_USERS: i64 = 8;
/// A call expression present in more files is treated as generic.
const MAX_SHARED_FILES: i64 = 32;
/// Reference labels are cut at this many characters by the native indexer.
const IMPORT_LABEL_LIMIT: usize = 200;
/// Shared calls whose combined specificity is lower are not reported. One
/// expression shared with two other definitions and present in one file
/// scores 0.5; one shared with four others across eight files scores 0.08.
const MIN_SHARED_SPECIFICITY: f64 = 0.2;

/// How a related definition uses the anchor definition. A definition can
/// both call the anchor and share a call with it; the resolved relation wins.
#[derive(Clone, Debug, PartialEq)]
pub enum UseRelation {
    /// Calls the anchor through call text that resolves only to the anchor.
    Calls,
    /// A non-structural indexed graph edge that targets the anchor directly.
    Edge(String),
    /// Makes the same call expression as the anchor, in the anchor's file.
    SharedCall(String),
}

impl UseRelation {
    /// Whether this relation is a resolved dependency on the anchor.
    #[must_use]
    pub const fn is_resolved(&self) -> bool {
        matches!(self, Self::Calls | Self::Edge(_))
    }
}

/// One definition related to an anchor definition by use.
#[derive(Clone, Debug, PartialEq)]
pub struct RelatedUse {
    pub node_id: String,
    pub kind: String,
    pub label: String,
    pub file_path: String,
    pub relation: UseRelation,
    /// Whether the definition also shares a specific call with the anchor.
    pub shares_call: bool,
    /// Structural specificity, comparable only among one anchor's uses: higher
    /// for rarer shared expressions and for definitions with fewer callers.
    pub specificity: f64,
    /// Distinct resolved callers and graph users of the anchor.
    pub anchor_callers: usize,
}

struct Anchor {
    project_id: String,
    label: String,
    file_path: String,
    language: Option<String>,
}

#[derive(Default)]
struct Candidate {
    kind: String,
    label: String,
    file_path: String,
    calls: bool,
    edge: Option<String>,
    shared: Option<(f64, String)>,
    shared_specificity: f64,
}

/// Related uses of `anchor_id`, unranked by task relevance and bounded by
/// the module limits. Unknown, whole-file and reference anchors yield none.
///
/// # Errors
///
/// Returns storage errors.
pub fn related_uses(database: &Database, anchor_id: &str) -> Result<Vec<RelatedUse>> {
    database.with_reader(|connection| {
        let anchor = connection
            .prepare_cached(
                "SELECT project_id,label,file_path,language FROM nodes
                 WHERE id=?1 AND tombstoned_at_ms IS NULL AND file_path IS NOT NULL
                   AND kind NOT IN ('File','Section','ExternalReference')",
            )?
            .query_row(params![anchor_id], |row| {
                Ok(Anchor {
                    project_id: row.get(0)?,
                    label: row.get(1)?,
                    file_path: row.get(2)?,
                    language: row.get(3)?,
                })
            })
            .optional()?;
        let Some(anchor) = anchor else {
            return Ok(Vec::new());
        };
        let mut candidates = BTreeMap::<String, Candidate>::new();
        collect_callers(connection, anchor_id, &anchor, &mut candidates)?;
        collect_edges(connection, anchor_id, &anchor, &mut candidates)?;
        collect_shared_calls(connection, anchor_id, &anchor, &mut candidates)?;
        let callers = candidates
            .values()
            .filter(|candidate| candidate.calls || candidate.edge.is_some())
            .count();
        #[allow(clippy::cast_precision_loss)] // Bounded by MAX_CALL_EDGES.
        let caller_specificity = 1.0 / (1.0 + (callers.max(1) as f64).ln());
        Ok(candidates
            .into_iter()
            .filter_map(|(node_id, candidate)| {
                let resolved = candidate.calls || candidate.edge.is_some();
                let shared = candidate.shared_specificity >= MIN_SHARED_SPECIFICITY;
                if !resolved && !shared {
                    return None;
                }
                let relation = if candidate.calls {
                    UseRelation::Calls
                } else if let Some(relation) = candidate.edge {
                    UseRelation::Edge(relation)
                } else {
                    UseRelation::SharedCall(candidate.shared.map(|(_, label)| label)?)
                };
                Some(RelatedUse {
                    node_id,
                    kind: candidate.kind,
                    label: candidate.label,
                    file_path: candidate.file_path,
                    relation,
                    shares_call: shared,
                    specificity: if resolved { caller_specificity } else { 0.0 }
                        + if shared {
                            candidate.shared_specificity
                        } else {
                            0.0
                        },
                    anchor_callers: callers,
                })
            })
            .collect())
    })
}

/// JavaScript and TypeScript call each other; other languages only themselves.
fn language_family(language: Option<&str>) -> Option<&str> {
    match language {
        Some("javascript" | "typescript" | "tsx") => Some("typescript"),
        other => other,
    }
}

fn collect_callers(
    connection: &rusqlite::Connection,
    anchor_id: &str,
    anchor: &Anchor,
    candidates: &mut BTreeMap<String, Candidate>,
) -> Result<()> {
    if anchor.label.chars().count() < 3 {
        return Ok(());
    }
    // Name resolution is the only resolution the native index has. A name
    // shared by several definitions resolves nowhere, except that a call in a
    // definition's own file resolves to that file's single definition.
    let (in_project, in_file): (i64, i64) = connection
        .prepare_cached(
            "SELECT count(*),coalesce(sum(file_path=?3),0) FROM nodes INDEXED BY nodes_project_label
             WHERE project_id=?1 AND label=?2 AND tombstoned_at_ms IS NULL
               AND kind NOT IN ('File','Section','ExternalReference')",
        )?
        .query_row(
            params![anchor.project_id, anchor.label, anchor.file_path],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
    if in_file != 1 {
        return Ok(());
    }
    let mut references = connection.prepare_cached(
        "SELECT id,file_path,language FROM nodes INDEXED BY nodes_project_label
         WHERE project_id=?1 AND label=?2 AND kind='ExternalReference'
           AND tombstoned_at_ms IS NULL AND file_path IS NOT NULL
         ORDER BY file_path,id LIMIT ?3",
    )?;
    let references = references
        .query_map(
            params![anchor.project_id, anchor.label, MAX_REFERENCE_FILES],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut callers = connection.prepare_cached(
        "SELECT u.id,u.kind,u.label,u.file_path
         FROM edges e INDEXED BY edges_target_active CROSS JOIN nodes u ON u.id=e.source_id
         WHERE e.project_id=?1 AND e.target_id=?2 AND e.relation='CALLS'
           AND e.tombstoned_at_ms IS NULL AND u.tombstoned_at_ms IS NULL
           AND u.id<>?3 AND u.kind<>'ExternalReference' AND u.file_path IS NOT NULL
         ORDER BY u.id LIMIT ?4",
    )?;
    let mut imports = connection.prepare_cached(
        "SELECT label FROM nodes INDEXED BY nodes_project_file
         WHERE project_id=?1 AND file_path=?2 AND kind='ExternalReference'
           AND tombstoned_at_ms IS NULL AND substr(stable_key,1,length(?3))=?3
         ORDER BY id LIMIT 256",
    )?;
    let mut inspected = 0_i64;
    for (reference, path, language) in references {
        if language_family(language.as_deref()) != language_family(anchor.language.as_deref())
            || (path != anchor.file_path
                && (in_project != 1 || !imports_anchor(&mut imports, anchor, &path)?))
        {
            continue;
        }
        for row in callers.query_map(
            params![anchor.project_id, reference, anchor_id, MAX_CALL_EDGES],
            read_user,
        )? {
            let (id, kind, label, file_path) = row?;
            candidates
                .entry(id)
                .or_insert_with(|| Candidate::new(kind, label, file_path))
                .calls = true;
            inspected += 1;
        }
        if inspected >= MAX_CALL_EDGES {
            break;
        }
    }
    Ok(())
}

/// A call from another file names the anchor only when that file imports the
/// call's leading name, or sits in the anchor's directory in a language whose
/// package members are visible without an import. A name alone is not enough:
/// it may come from a dependency the index does not contain.
fn imports_anchor(
    imports: &mut rusqlite::CachedStatement<'_>,
    anchor: &Anchor,
    path: &str,
) -> Result<bool> {
    let same_directory = Path::new(path).parent() == Path::new(&anchor.file_path).parent();
    if same_directory
        && matches!(
            anchor.language.as_deref(),
            Some("kotlin" | "java" | "scala" | "csharp" | "go")
        )
    {
        return Ok(true);
    }
    let name = anchor.label.split("::").next().unwrap_or(&anchor.label);
    let prefix = format!("reference:{path}:import:");
    for label in imports.query_map(params![anchor.project_id, path, prefix], |row| {
        row.get::<_, String>(0)
    })? {
        // The indexer keeps the first 200 characters of an import statement;
        // a longer grouped import may name the anchor past that point, so it
        // counts as possible evidence rather than proof of absence.
        let label = label?;
        if names_identifier(&label, name) || label.chars().count() >= IMPORT_LABEL_LIMIT {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `text` contains `name` as a whole identifier.
fn names_identifier(text: &str, name: &str) -> bool {
    let identifier = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    !name.is_empty()
        && text.match_indices(name).any(|(start, _)| {
            !text[..start].chars().next_back().is_some_and(identifier)
                && !text[start + name.len()..]
                    .chars()
                    .next()
                    .is_some_and(identifier)
        })
}

fn collect_edges(
    connection: &rusqlite::Connection,
    anchor_id: &str,
    anchor: &Anchor,
    candidates: &mut BTreeMap<String, Candidate>,
) -> Result<()> {
    let mut statement = connection.prepare_cached(
        "SELECT u.id,u.kind,u.label,u.file_path,e.relation
         FROM edges e INDEXED BY edges_target_active CROSS JOIN nodes u ON u.id=e.source_id
         WHERE e.project_id=?1 AND e.target_id=?2 AND e.tombstoned_at_ms IS NULL
           AND upper(e.relation) NOT IN ('DEFINES','CONTAINS')
           AND u.tombstoned_at_ms IS NULL AND u.kind<>'ExternalReference'
           AND u.file_path IS NOT NULL AND u.project_id=?1
         ORDER BY e.relation,u.id LIMIT ?3",
    )?;
    for row in statement.query_map(
        params![anchor.project_id, anchor_id, MAX_CALL_EDGES],
        |row| Ok((read_user(row)?, row.get::<_, String>(4)?)),
    )? {
        let ((id, kind, label, file_path), relation) = row?;
        candidates
            .entry(id)
            .or_insert_with(|| Candidate::new(kind, label, file_path))
            .edge
            .get_or_insert(relation);
    }
    Ok(())
}

fn collect_shared_calls(
    connection: &rusqlite::Connection,
    anchor_id: &str,
    anchor: &Anchor,
    candidates: &mut BTreeMap<String, Candidate>,
) -> Result<()> {
    // The indexer keeps one reference node per call text and file, so other
    // users of that node are the other definitions in this file making the
    // same call.
    let mut own = connection.prepare_cached(
        "SELECT r.id,r.label,
           (SELECT count(*) FROM edges u INDEXED BY edges_target_active
            WHERE u.project_id=?1 AND u.target_id=r.id AND u.relation='CALLS'
              AND u.tombstoned_at_ms IS NULL)
         FROM edges e INDEXED BY edges_source_active CROSS JOIN nodes r ON r.id=e.target_id
         WHERE e.project_id=?1 AND e.source_id=?2 AND e.relation='CALLS'
           AND e.tombstoned_at_ms IS NULL AND r.kind='ExternalReference'
           AND r.tombstoned_at_ms IS NULL AND r.file_path=?3
         ORDER BY r.label,r.id LIMIT ?4",
    )?;
    let calls = own
        .query_map(
            params![
                anchor.project_id,
                anchor_id,
                anchor.file_path,
                MAX_OWN_CALLS
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut files = connection.prepare_cached(
        "SELECT count(*) FROM nodes INDEXED BY nodes_project_label
         WHERE project_id=?1 AND label=?2 AND kind='ExternalReference'
           AND tombstoned_at_ms IS NULL",
    )?;
    let mut users = connection.prepare_cached(
        "SELECT u.id,u.kind,u.label,u.file_path
         FROM edges e INDEXED BY edges_target_active CROSS JOIN nodes u ON u.id=e.source_id
         WHERE e.project_id=?1 AND e.target_id=?2 AND e.relation='CALLS'
           AND e.tombstoned_at_ms IS NULL AND u.tombstoned_at_ms IS NULL
           AND u.id<>?3 AND u.kind<>'ExternalReference' AND u.file_path IS NOT NULL
         ORDER BY u.id LIMIT ?4",
    )?;
    for (reference, label, user_count) in calls {
        let others = user_count - 1;
        if !(1..=MAX_SHARED_USERS).contains(&others) || !names_shared_target(&label) {
            continue;
        }
        let file_count: i64 =
            files.query_row(params![anchor.project_id, label], |row| row.get(0))?;
        if file_count > MAX_SHARED_FILES {
            continue;
        }
        #[allow(clippy::cast_precision_loss)] // Both counts are small and bounded.
        let specificity = 1.0 / (others as f64 * (1.0 + (file_count.max(1) as f64).ln()));
        for row in users.query_map(
            params![anchor.project_id, reference, anchor_id, MAX_SHARED_USERS],
            read_user,
        )? {
            let (id, kind, user_label, file_path) = row?;
            let candidate = candidates
                .entry(id)
                .or_insert_with(|| Candidate::new(kind, user_label, file_path));
            candidate.shared_specificity += specificity;
            // Show the most specific shared expression; ties keep the first
            // in deterministic label order.
            if candidate
                .shared
                .as_ref()
                .is_none_or(|(best, _)| specificity > *best)
            {
                candidate.shared = Some((specificity, label.clone()));
            }
        }
    }
    Ok(())
}

/// Call text that can name a shared target: a free function or path call, or
/// a member reached through a static, a type, or `self`/`this`. A method on a
/// lowercase receiver, usually a local variable (`entry.is_empty`), and
/// multi-line chains or closures name nothing shared and are skipped.
fn names_shared_target(expression: &str) -> bool {
    if expression.chars().count() < 3
        || expression.len() > 96
        || expression
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '|' | '{' | '}' | '"' | '\'' | ',' | ';'))
    {
        return false;
    }
    let Some((receiver, _)) = expression.split_once('.') else {
        return true;
    };
    let receiver = receiver.trim_start_matches(['&', '*']);
    let last = receiver.rsplit("::").next().unwrap_or(receiver);
    matches!(last, "self" | "this" | "Self")
        || (last.chars().next().is_some_and(char::is_uppercase)
            && last.chars().all(|c| c.is_alphanumeric() || c == '_'))
}

impl Candidate {
    fn new(kind: String, label: String, file_path: String) -> Self {
        Self {
            kind,
            label,
            file_path,
            ..Self::default()
        }
    }
}

fn read_user(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, String, String, String)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;
    use crate::world::fixtures::{CONTRACT_SUITE, IndexedRepo, SETTINGS};

    fn uses(repo: &IndexedRepo, label: &str) -> BTreeMap<String, RelatedUse> {
        related_uses(&repo.database, &repo.node(label))
            .unwrap()
            .into_iter()
            .map(|related| (related.label.clone(), related))
            .collect()
    }

    #[test]
    fn same_file_lock_users_are_shared_uses_and_helper_users_are_callers() {
        let repo = IndexedRepo::new(&[
            ("tests/contract.rs", CONTRACT_SUITE),
            ("src/settings.rs", SETTINGS),
        ]);
        let related = uses(&repo, "settings_for_test");
        let labels: Vec<_> = related.keys().map(String::as_str).collect();
        assert_eq!(
            labels,
            [
                "cluster_mode_keeps_its_own_url",
                "defaults_load_through_the_helper",
                "hosted_mode_keeps_its_own_url"
            ],
            "callees and unrelated definitions are not uses"
        );
        for test in [
            "cluster_mode_keeps_its_own_url",
            "hosted_mode_keeps_its_own_url",
        ] {
            let shared = &related[test];
            assert!(
                matches!(&shared.relation, UseRelation::SharedCall(call) if call.starts_with("TEST_ENV_LOCK.lock")),
                "{shared:?}"
            );
            assert!(!shared.relation.is_resolved());
            assert!(shared.shares_call);
        }
        let caller = &related["defaults_load_through_the_helper"];
        assert_eq!(caller.relation, UseRelation::Calls);
        assert_eq!(caller.anchor_callers, 1);
        // A callee's other callers are not uses of the helper.
        assert!(uses(&repo, "load_settings").contains_key("settings_for_test"));
        assert_eq!(uses(&repo, "load_settings").len(), 1);
    }

    #[test]
    fn ambiguous_names_and_other_languages_never_resolve_as_callers() {
        // The indexer stores only a 200-character prefix of this import.
        let long_import = format!(
            "use crate::{{{}, unique::rebuild_index}};\n\nfn long_import_user() {{\n    rebuild_index();\n}}\n",
            (0..40)
                .map(|index| format!("module_{index}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let repo = IndexedRepo::new(&[
            ("src/long_import.rs", long_import.as_str()),
            (
                "src/alpha.rs",
                "pub fn refresh_cache() {}\n\nfn alpha_user() {\n    refresh_cache();\n}\n",
            ),
            ("src/beta.rs", "pub fn refresh_cache() {}\n"),
            (
                "src/gamma.rs",
                "fn gamma_user() {\n    refresh_cache();\n}\n",
            ),
            ("src/unique.rs", "pub fn rebuild_index() {}\n"),
            (
                "src/caller.rs",
                "use crate::unique::rebuild_index;\n\nfn rust_user() {\n    rebuild_index();\n}\n",
            ),
            // Same name, no import: possibly another crate's function.
            (
                "src/unimported.rs",
                "use crate::unique::rebuild_index_later;\n\nfn guessing_user() {\n    rebuild_index();\n}\n",
            ),
            (
                "scripts/tool.ts",
                "import { rebuild_index } from './x';\nfunction scriptUser() {\n  rebuild_index();\n}\n",
            ),
            // Kotlin package members are visible in their directory unimported.
            ("app/Auth.kt", "fun refreshToken() {}\n"),
            ("app/Session.kt", "fun restore() {\n  refreshToken()\n}\n"),
            ("other/Login.kt", "fun login() {\n  refreshToken()\n}\n"),
        ]);
        let kotlin = uses(&repo, "refreshToken");
        assert_eq!(kotlin.keys().collect::<Vec<_>>(), ["restore"]);
        let alpha = related_uses(
            &repo.database,
            &repo.node_in("refresh_cache", "src/alpha.rs"),
        )
        .unwrap();
        assert_eq!(
            alpha
                .iter()
                .map(|used| used.label.as_str())
                .collect::<Vec<_>>(),
            ["alpha_user"],
            "only the same-file call resolves to a duplicated name"
        );
        assert!(
            related_uses(
                &repo.database,
                &repo.node_in("refresh_cache", "src/beta.rs")
            )
            .unwrap()
            .is_empty()
        );
        let unique = uses(&repo, "rebuild_index");
        assert_eq!(
            unique.keys().collect::<Vec<_>>(),
            ["long_import_user", "rust_user"],
            "unimported and cross-language calls are not callers"
        );
        assert_eq!(unique["rust_user"].relation, UseRelation::Calls);
        assert!(names_identifier("use crate::{a, Settings};", "Settings"));
        assert!(!names_identifier("use crate::SettingsCache;", "Settings"));
        assert!(!names_identifier("import { rebuild } from 'x';", ""));
    }

    #[test]
    fn common_or_crowded_shared_calls_are_not_reported() {
        let mut crowded = String::from(
            "fn anchor_step() {\n    BUS.publish();\n    tracing::warn();\n}\n\nfn quiet_log() {\n    tracing::warn();\n}\n",
        );
        for index in 0..9 {
            writeln!(crowded, "fn crowded_{index}() {{\n    BUS.publish();\n}}").unwrap();
        }
        let mut files = vec![("src/bus.rs".to_owned(), crowded)];
        for index in 0..40 {
            files.push((
                format!("src/log_{index}.rs"),
                format!("fn logs_{index}() {{\n    tracing::warn();\n}}\n"),
            ));
        }
        let borrowed: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, content)| (path.as_str(), content.as_str()))
            .collect();
        let repo = IndexedRepo::new(&borrowed);
        assert!(
            uses(&repo, "anchor_step").is_empty(),
            "nine other users in one file, or a call made in 41 files, is generic"
        );
    }
}
