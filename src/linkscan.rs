//! Automatic cross-repository link discovery from the repos' own metadata.
//!
//! The repositories already declare most inter-repo relationships: a Cargo
//! `path` dependency, an npm `file:` dependency, an editable pip install, or
//! a git submodule URL literally names the other project. Those signals are
//! DEFINITIVE — the repo's own manifest asserts the dependency — so they are
//! safe to auto-link, each carrying its evidence in the link note.
//! Name-only matches (a dependency whose package name equals a registered
//! project's name) are merely suggestive and are returned as PROPOSALS for a
//! human to confirm; they are never auto-applied. Scanning is bounded, reads
//! only well-known manifest files at the repo root, and never touches the
//! network.
//!
//! A link makes every linked repository's lessons part of this project's
//! context, so automatic links stay accountable to evidence. Until 2026-09-16
//! a referenced hostname was applied as definitive. Such a link is STALE when
//! a complete scan of the checkout finds the host only in documentation or
//! nowhere. Only an explicit `engram link scan --apply` retracts stale links,
//! because a scan sees one checkout and a branch can lack evidence that the
//! default branch has; `engram doctor` lists the links that remain.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ids::ProjectId;
use crate::projects::{LinkedProject, ProjectRegistry};
use crate::storage::Database;

/// Manifests larger than this are skipped rather than parsed.
const MAX_MANIFEST_BYTES: u64 = 1_048_576;
/// Dependencies examined per manifest.
const MAX_DEPENDENCIES: usize = 500;
/// Note prefix that marks a link as applied by the scanner.
const AUTO_NOTE_PREFIX: &str = "auto: ";
/// Evidence of every signal that `apply_definitive` applies today.
const DEFINITIVE_EVIDENCE: [&str; 4] = [
    "Cargo path dependency `",
    "npm file dependency `",
    "git submodule `",
    "pip editable install `",
];
/// Evidence prefix of links the earlier hostname rule applied.
const HOSTNAME_EVIDENCE: &str = "references host `";
/// Prose files. They name services a repository does not call, for example
/// in handoff notes, so they cannot support a link on their own. Plain `.txt`
/// is excluded: `CMakeLists.txt` and `requirements.txt` are configuration.
const DOCUMENTATION_EXTENSIONS: [&str; 7] =
    ["md", "mdx", "markdown", "rst", "adoc", "asciidoc", "org"];

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LinkCandidate {
    pub target_project_id: ProjectId,
    pub target_name: String,
    pub relation: String,
    /// Human-readable provenance ("Cargo path dependency `../platform`").
    pub evidence: String,
    /// Definitive candidates come from the repo's own declaration of the
    /// dependency and are safe to auto-apply; the rest are name matches.
    pub definitive: bool,
    /// Every match is in documentation, which can name services the
    /// repository does not call.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub documentation_only: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LinkScanReport {
    pub candidates: Vec<LinkCandidate>,
    pub applied: Vec<LinkedProject>,
    /// Automatic links applied under an earlier policy whose evidence is gone
    /// or only in documentation. They are retracted when the scan applies its
    /// results.
    #[serde(default)]
    pub stale: Vec<LinkedProject>,
    /// The scan could not read every file, so no link was judged stale.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
}

/// One scan of a checkout and what it was able to check.
#[derive(Clone, Debug, Default)]
pub struct LinkScan {
    pub candidates: Vec<LinkCandidate>,
    /// Targets that declare a hosting fact, whose hostname evidence this scan
    /// checked. Links to other targets are never judged stale by it.
    pub hosted_targets: Vec<ProjectId>,
    /// The hostname walk read every file it was meant to: no file cap, walker
    /// error or unreadable file. An incomplete walk cannot show absence.
    pub complete: bool,
}

struct KnownProject {
    id: ProjectId,
    name: String,
    canonical_remote: Option<String>,
    roots: Vec<PathBuf>,
    /// Deployment hostnames declared as `hosting.*` facts. A repo whose
    /// source references one of these hosts is declaring an API dependency
    /// on the owning service.
    hosts: Vec<String>,
}

/// Scan one repository's manifests for links to other registered projects.
/// Already-linked targets are excluded from the result, except targets of
/// links applied under an earlier policy, whose current evidence decides
/// whether they are stale.
///
/// # Errors
///
/// Returns an error for storage failures and for a checkout that cannot be
/// listed, which would otherwise look like a repository without evidence.
/// Unreadable or oversized manifests are skipped, not fatal.
pub fn scan(
    database: &Database,
    project_id: &ProjectId,
    root: &Path,
) -> Result<Vec<LinkCandidate>> {
    Ok(scan_checkout(database, project_id, root)?.candidates)
}

/// [`scan`], keeping what the scan could check for [`stale_auto_links`].
///
/// # Errors
///
/// As for [`scan`].
pub fn scan_checkout(database: &Database, project_id: &ProjectId, root: &Path) -> Result<LinkScan> {
    fs::read_dir(root).map_err(|error| {
        Error::Config(format!(
            "cannot read checkout `{}` to scan links: {error}",
            root.display()
        ))
    })?;
    let registry = ProjectRegistry::new(database.clone());
    let known = known_projects(database, &registry, project_id)?;
    let existing = registry
        .links(project_id)?
        .into_iter()
        .filter(|link| !applied_under_old_policy(link))
        .map(|link| link.project_id)
        .collect::<Vec<_>>();
    let mut candidates = Vec::new();
    scan_cargo(root, &known, &mut candidates);
    scan_package_json(root, &known, &mut candidates);
    scan_gitmodules(root, &known, &mut candidates);
    scan_requirements(root, &known, &mut candidates);
    let complete = scan_hostnames(root, &known, &mut candidates);
    scan_route_overlap(root, &known, &mut candidates);
    candidates.retain(|candidate| !existing.contains(&candidate.target_project_id));
    // One candidate per target: keep the strongest (definitive first, then
    // evidence outside documentation).
    candidates.sort_by(|a, b| {
        a.target_project_id
            .as_str()
            .cmp(b.target_project_id.as_str())
            .then(b.definitive.cmp(&a.definitive))
            .then(a.documentation_only.cmp(&b.documentation_only))
    });
    candidates.dedup_by(|a, b| a.target_project_id == b.target_project_id);
    candidates.sort_by(|a, b| {
        b.definitive
            .cmp(&a.definitive)
            .then_with(|| a.target_name.cmp(&b.target_name))
    });
    let hosted_targets = known
        .iter()
        .filter(|project| !project.hosts.is_empty())
        .map(|project| project.id.clone())
        .collect();
    Ok(LinkScan {
        candidates,
        hosted_targets,
        complete,
    })
}

/// Apply the definitive candidates as links, stamping each note with its
/// discovery evidence. Name-only proposals are never applied here.
///
/// # Errors
///
/// Returns storage errors from link creation.
pub fn apply_definitive(
    database: &Database,
    project_id: &ProjectId,
    candidates: &[LinkCandidate],
) -> Result<Vec<LinkedProject>> {
    let registry = ProjectRegistry::new(database.clone());
    let mut applied = Vec::new();
    for candidate in candidates.iter().filter(|candidate| candidate.definitive) {
        applied.push(registry.link(
            project_id,
            &candidate.target_project_id,
            &candidate.relation,
            Some(&format!("{AUTO_NOTE_PREFIX}{}", candidate.evidence)),
        )?);
    }
    Ok(applied)
}

/// A link the scanner applied on evidence it no longer applies by itself.
/// Links a person added or edited carry no scanner note.
fn applied_under_old_policy(link: &LinkedProject) -> bool {
    link.note
        .as_deref()
        .and_then(|note| note.strip_prefix(AUTO_NOTE_PREFIX))
        .is_some_and(|evidence| {
            !DEFINITIVE_EVIDENCE
                .iter()
                .any(|prefix| evidence.starts_with(prefix))
        })
}

/// A link the earlier hostname rule applied.
fn hostname_link(link: &LinkedProject) -> bool {
    link.note
        .as_deref()
        .and_then(|note| note.strip_prefix(AUTO_NOTE_PREFIX))
        .is_some_and(|evidence| evidence.starts_with(HOSTNAME_EVIDENCE))
}

/// Links the earlier hostname rule applied that a complete `scan` of the
/// project's checkout no longer supports outside documentation. Nothing is
/// stale when the scan was incomplete or the target declares no host.
///
/// # Errors
///
/// Returns storage errors.
pub fn stale_auto_links(
    database: &Database,
    project_id: &ProjectId,
    scan: &LinkScan,
) -> Result<Vec<LinkedProject>> {
    if !scan.complete {
        return Ok(Vec::new());
    }
    Ok(ProjectRegistry::new(database.clone())
        .links(project_id)?
        .into_iter()
        .filter(|link| {
            hostname_link(link)
                && scan.hosted_targets.contains(&link.project_id)
                && !scan.candidates.iter().any(|candidate| {
                    candidate.target_project_id == link.project_id && !candidate.documentation_only
                })
        })
        .collect())
}

/// Retract stale automatic links. Run it on the writer: it rereads the
/// links, so a link a person edited in the meantime stays.
///
/// # Errors
///
/// Returns storage errors.
pub fn retract_stale(
    database: &Database,
    project_id: &ProjectId,
    scan: &LinkScan,
) -> Result<Vec<LinkedProject>> {
    let registry = ProjectRegistry::new(database.clone());
    let mut retracted = Vec::new();
    for link in stale_auto_links(database, project_id, scan)? {
        if registry.unlink(project_id, &link.project_id)? {
            retracted.push(link);
        }
    }
    Ok(retracted)
}

fn known_projects(
    database: &Database,
    registry: &ProjectRegistry,
    current: &ProjectId,
) -> Result<Vec<KnownProject>> {
    let rows = database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT id, name, canonical_remote FROM projects p
             WHERE p.tombstoned_at_ms IS NULL
               AND NOT EXISTS (SELECT 1 FROM project_facts pf WHERE pf.project_id = p.id
                       AND (pf.key = 'internal.view_of'
                            OR (pf.key = 'internal.hidden' AND pf.value = 'true')))
             ORDER BY p.name, p.id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    })?;
    let mut known = Vec::new();
    for (id, name, canonical_remote) in rows {
        let id = ProjectId::parse(&id)?;
        if &id == current {
            continue;
        }
        let roots = registry
            .alias_paths(&id)?
            .into_iter()
            .filter_map(|path| fs::canonicalize(path).ok())
            .collect();
        let hosts = registry
            .facts(&id)?
            .into_iter()
            .filter(|fact| fact.key.starts_with("hosting."))
            .filter_map(|fact| normalize_host(&fact.value))
            .collect();
        known.push(KnownProject {
            id,
            name,
            canonical_remote,
            roots,
            hosts,
        });
    }
    Ok(known)
}

fn read_manifest(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() > MAX_MANIFEST_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn project_owning<'known>(
    known: &'known [KnownProject],
    path: &Path,
) -> Option<&'known KnownProject> {
    let canonical = fs::canonicalize(path).ok()?;
    known
        .iter()
        .find(|project| project.roots.iter().any(|root| canonical.starts_with(root)))
}

fn project_named<'known>(
    known: &'known [KnownProject],
    dependency: &str,
) -> Option<&'known KnownProject> {
    let dependency = normalize_name(dependency);
    known
        .iter()
        .find(|project| normalize_name(&project.name) == dependency)
}

fn normalize_name(name: &str) -> String {
    name.to_ascii_lowercase().replace('_', "-")
}

fn push_path_candidate(
    root: &Path,
    known: &[KnownProject],
    relative: &str,
    evidence: String,
    candidates: &mut Vec<LinkCandidate>,
) {
    let absolute = root.join(relative);
    // A path inside this repo (vendored copy) is not a cross-repo link.
    if fs::canonicalize(&absolute)
        .ok()
        .zip(fs::canonicalize(root).ok())
        .is_some_and(|(target, root)| target.starts_with(root))
    {
        return;
    }
    if let Some(project) = project_owning(known, &absolute) {
        candidates.push(LinkCandidate {
            target_project_id: project.id.clone(),
            target_name: project.name.clone(),
            relation: "depends-on".to_owned(),
            evidence,
            definitive: true,
            documentation_only: false,
        });
    }
}

fn push_name_candidate(
    known: &[KnownProject],
    dependency: &str,
    evidence: String,
    candidates: &mut Vec<LinkCandidate>,
) {
    if let Some(project) = project_named(known, dependency) {
        candidates.push(LinkCandidate {
            target_project_id: project.id.clone(),
            target_name: project.name.clone(),
            relation: "uses".to_owned(),
            evidence,
            definitive: false,
            documentation_only: false,
        });
    }
}

fn scan_cargo(root: &Path, known: &[KnownProject], candidates: &mut Vec<LinkCandidate>) {
    let Some(manifest) = read_manifest(&root.join("Cargo.toml")) else {
        return;
    };
    let Ok(value) = manifest.parse::<toml::Table>() else {
        return;
    };
    let tables = ["dependencies", "dev-dependencies", "build-dependencies"];
    let mut sections = Vec::new();
    for table in tables {
        if let Some(section) = value.get(table) {
            sections.push(section);
        }
    }
    if let Some(section) = value.get("workspace").and_then(|w| w.get("dependencies")) {
        sections.push(section);
    }
    let mut seen = 0_usize;
    for section in sections {
        let Some(entries) = section.as_table() else {
            continue;
        };
        for (name, spec) in entries {
            seen += 1;
            if seen > MAX_DEPENDENCIES {
                return;
            }
            if let Some(path) = spec.get("path").and_then(toml::Value::as_str) {
                push_path_candidate(
                    root,
                    known,
                    path,
                    format!("Cargo path dependency `{path}`"),
                    candidates,
                );
            } else {
                push_name_candidate(
                    known,
                    name,
                    format!("Cargo dependency `{name}` matches project name"),
                    candidates,
                );
            }
        }
    }
}

fn scan_package_json(root: &Path, known: &[KnownProject], candidates: &mut Vec<LinkCandidate>) {
    let Some(manifest) = read_manifest(&root.join("package.json")) else {
        return;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&manifest) else {
        return;
    };
    let mut seen = 0_usize;
    for table in ["dependencies", "devDependencies"] {
        let Some(entries) = value.get(table).and_then(serde_json::Value::as_object) else {
            continue;
        };
        for (name, spec) in entries {
            seen += 1;
            if seen > MAX_DEPENDENCIES {
                return;
            }
            if let Some(path) = spec.as_str().and_then(|spec| spec.strip_prefix("file:")) {
                push_path_candidate(
                    root,
                    known,
                    path,
                    format!("npm file dependency `{path}`"),
                    candidates,
                );
            } else {
                push_name_candidate(
                    known,
                    name,
                    format!("npm dependency `{name}` matches project name"),
                    candidates,
                );
            }
        }
    }
}

fn scan_gitmodules(root: &Path, known: &[KnownProject], candidates: &mut Vec<LinkCandidate>) {
    let Some(manifest) = read_manifest(&root.join(".gitmodules")) else {
        return;
    };
    for line in manifest.lines().take(2_000) {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "url" {
            continue;
        }
        let url = normalize_git_url(value.trim());
        if let Some(project) = known.iter().find(|project| {
            project
                .canonical_remote
                .as_deref()
                .is_some_and(|remote| normalize_git_url(remote) == url)
        }) {
            candidates.push(LinkCandidate {
                target_project_id: project.id.clone(),
                target_name: project.name.clone(),
                relation: "submodule".to_owned(),
                evidence: format!("git submodule `{}`", value.trim()),
                definitive: true,
                documentation_only: false,
            });
        }
    }
}

fn scan_requirements(root: &Path, known: &[KnownProject], candidates: &mut Vec<LinkCandidate>) {
    for file in ["requirements.txt", "requirements-dev.txt"] {
        let Some(manifest) = read_manifest(&root.join(file)) else {
            continue;
        };
        for line in manifest.lines().take(MAX_DEPENDENCIES) {
            let line = line.trim();
            if let Some(path) = line
                .strip_prefix("-e ")
                .map(str::trim)
                .filter(|path| path.starts_with('.') || path.starts_with('/'))
            {
                push_path_candidate(
                    root,
                    known,
                    path,
                    format!("pip editable install `{path}` ({file})"),
                    candidates,
                );
            }
        }
    }
}

/// Extract a bare lowercase hostname from a fact value that may be a URL.
/// Values without a dot (or that are localhost) are not usable as
/// cross-repo evidence and are dropped.
fn normalize_host(value: &str) -> Option<String> {
    let value = value.trim().to_ascii_lowercase();
    let value = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .unwrap_or(&value);
    let host = value
        .split(['/', ':', '?'])
        .next()
        .unwrap_or_default()
        .trim();
    (host.contains('.') && !host.starts_with("localhost") && host.len() <= 253)
        .then(|| host.to_owned())
}

fn is_documentation(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            DOCUMENTATION_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        })
}

/// Search the repo's tracked-looking text files, dotfiles included, for other
/// services' declared hostnames. A source literal naming another service's
/// domain is only a discovery hint: documentation and examples can contain the
/// same host. Large files are searched in their first 256 KiB and binary files
/// are skipped. Returns whether the walk was complete: it is not when the file cap
/// stopped it or a directory or file could not be read.
fn scan_hostnames(
    root: &Path,
    known: &[KnownProject],
    candidates: &mut Vec<LinkCandidate>,
) -> bool {
    const MAX_FILES: usize = 3_000;
    struct Hits {
        files: usize,
        /// First matching file outside documentation, else the first match.
        example: Option<(bool, String)>,
    }
    let hosted = known
        .iter()
        .filter(|project| !project.hosts.is_empty())
        .collect::<Vec<_>>();
    if hosted.is_empty() {
        return true;
    }
    let mut hits = hosted
        .iter()
        .map(|_| Hits {
            files: 0,
            example: None,
        })
        .collect::<Vec<_>>();
    let mut complete = true;
    let mut scanned = 0_usize;
    // Sorted, so the evidence and the file cap do not vary between scans.
    // Tracked dotfiles such as CI workflows count; version-control stores do
    // not, and ignore files still apply.
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .follow_links(false)
        .filter_entry(|entry| {
            !matches!(
                entry.file_name().to_str(),
                Some(".git" | ".hg" | ".svn" | ".jj" | ".engram" | ".leb")
            )
        })
        .sort_by_file_name(std::cmp::Ord::cmp)
        .build();
    for entry in walker {
        let Ok(entry) = entry else {
            complete = false;
            continue;
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        scanned += 1;
        if scanned > MAX_FILES {
            complete = false;
            break;
        }
        let content = match searchable_text(entry.path()) {
            Ok(Some(content)) => content,
            Ok(None) => continue,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        let documentation = is_documentation(entry.path());
        for (index, project) in hosted.iter().enumerate() {
            if project.hosts.iter().any(|host| content.contains(host)) {
                let hit = &mut hits[index];
                hit.files += 1;
                if hit
                    .example
                    .as_ref()
                    .is_none_or(|(example_is_doc, _)| *example_is_doc && !documentation)
                {
                    let relative = entry
                        .path()
                        .strip_prefix(root)
                        .unwrap_or(entry.path())
                        .display()
                        .to_string();
                    hit.example = Some((documentation, relative));
                }
            }
        }
    }
    for (index, project) in hosted.iter().enumerate() {
        let Some((documentation_only, example)) = &hits[index].example else {
            continue;
        };
        let count = hits[index].files;
        candidates.push(LinkCandidate {
            target_project_id: project.id.clone(),
            target_name: project.name.clone(),
            relation: "calls-api-of".to_owned(),
            evidence: format!(
                "references host `{}` in {count} file(s), e.g. `{example}`{}",
                project.hosts.join("`/`"),
                if *documentation_only {
                    " (documentation only)"
                } else {
                    ""
                }
            ),
            definitive: false,
            documentation_only: *documentation_only,
        });
    }
    complete
}

/// A file's first 256 KiB as lowercase text, or `None` for a binary file.
fn searchable_text(path: &Path) -> std::io::Result<Option<String>> {
    use std::io::Read as _;
    const MAX_SEARCHED_BYTES: u64 = 262_144;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_SEARCHED_BYTES)
        .read_to_end(&mut bytes)?;
    if bytes.iter().take(8_192).any(|byte| *byte == 0) {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&bytes).to_ascii_lowercase()))
}

/// Doctor check: links the earlier hostname rule applied stay until a person
/// confirms or retracts them, because only an explicit scan retracts links.
#[must_use]
pub fn legacy_link_check(database: &Database) -> crate::doctor::DoctorCheck {
    use crate::doctor::{CheckStatus, DoctorCheck};
    const NAME: &str = "legacy_links";
    let rows = database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT s.name, t.name, l.note FROM project_links l
             JOIN projects s ON s.id = l.project_id
             JOIN projects t ON t.id = l.linked_project_id
             WHERE s.tombstoned_at_ms IS NULL AND t.tombstoned_at_ms IS NULL
               AND NOT EXISTS (SELECT 1 FROM project_facts f
                               WHERE f.project_id = s.id AND f.key = 'internal.view_of')
             ORDER BY s.name, t.name",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    });
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => {
            return DoctorCheck {
                name: NAME.to_owned(),
                status: CheckStatus::Warning,
                detail: format!("could not list repository links: {error}"),
            };
        }
    };
    let legacy = rows
        .into_iter()
        .filter(|(_, _, note)| {
            note.as_deref()
                .and_then(|note| note.strip_prefix(AUTO_NOTE_PREFIX))
                .is_some_and(|evidence| evidence.starts_with(HOSTNAME_EVIDENCE))
        })
        .map(|(source, target, _)| format!("{source} → {target}"))
        .collect::<Vec<_>>();
    if legacy.is_empty() {
        return DoctorCheck {
            name: NAME.to_owned(),
            status: CheckStatus::Pass,
            detail: "no links from the hostname rule used before 2026-09-16".to_owned(),
        };
    }
    DoctorCheck {
        name: NAME.to_owned(),
        status: CheckStatus::Warning,
        detail: format!(
            "{} link(s) came from the hostname rule used before 2026-09-16, which linked a repository to any service whose host it mentioned, including in documentation: {}. Each makes the linked repository's lessons and source part of the project's context. Confirm a link with `engram link add <target> --path <repo>`, or run `engram link scan <repo>` to list the ones that only documentation supports and `--apply` to retract them",
            legacy.len(),
            legacy.join(", ")
        ),
    }
}

/// Repos whose sources share several multi-segment route literals are
/// probably two sides of one API. Lexical and heuristic, so this only ever
/// produces PROPOSALS for a human to confirm — never auto-applied links.
fn scan_route_overlap(root: &Path, known: &[KnownProject], candidates: &mut Vec<LinkCandidate>) {
    const MIN_SHARED_ROUTES: usize = 3;
    let mine: std::collections::BTreeSet<String> =
        crate::architect::extract_routes(root).into_iter().collect();
    if mine.len() < MIN_SHARED_ROUTES {
        return;
    }
    for project in known {
        let Some(other_root) = project.roots.iter().find(|path| path.is_dir()) else {
            continue;
        };
        let theirs = crate::architect::extract_routes(other_root);
        let shared = theirs
            .iter()
            .filter(|route| mine.contains(*route))
            .take(MIN_SHARED_ROUTES + 2)
            .cloned()
            .collect::<Vec<_>>();
        if shared.len() >= MIN_SHARED_ROUTES {
            candidates.push(LinkCandidate {
                target_project_id: project.id.clone(),
                target_name: project.name.clone(),
                relation: "shares-api-surface".to_owned(),
                evidence: format!(
                    "{} shared route literals, e.g. `{}`",
                    shared.len(),
                    shared[0]
                ),
                definitive: false,
                documentation_only: false,
            });
        }
    }
}

fn normalize_git_url(url: &str) -> String {
    let url = url
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("ssh://");
    let url = url.strip_prefix("git@").unwrap_or(url);
    url.replacen(':', "/", 1).to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn register(registry: &ProjectRegistry, root: &Path) -> ProjectId {
        fs::create_dir_all(root).expect("dir");
        registry
            .resolve_or_register(root)
            .expect("register")
            .project
            .id
    }

    #[test]
    fn cargo_path_dependency_is_definitive_and_applies_with_provenance() {
        let temp = TempDir::new().expect("temp");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("db");
        let registry = ProjectRegistry::new(database.clone());
        let root_a = temp.path().join("consumer");
        let root_b = temp.path().join("provider");
        let a = register(&registry, &root_a);
        let _b = register(&registry, &root_b);
        fs::write(
            root_a.join("Cargo.toml"),
            "[package]\nname = \"consumer\"\n[dependencies]\nprovider = { path = \"../provider\" }\n",
        )
        .expect("manifest");

        let candidates = scan(&database, &a, &root_a).expect("scan");
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].definitive);
        assert_eq!(candidates[0].target_name, "provider");
        assert_eq!(candidates[0].relation, "depends-on");

        let applied = apply_definitive(&database, &a, &candidates).expect("apply");
        assert_eq!(applied.len(), 1);
        let links = registry.links(&a).expect("links");
        assert_eq!(links.len(), 1);
        assert!(
            links[0]
                .note
                .as_deref()
                .is_some_and(|note| note.starts_with("auto: Cargo path dependency")),
            "{:?}",
            links[0].note
        );

        // Idempotent: the target is linked now, so rescans propose nothing.
        assert!(scan(&database, &a, &root_a).expect("rescan").is_empty());
    }

    #[test]
    fn name_matches_are_proposals_and_never_auto_applied() {
        let temp = TempDir::new().expect("temp");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("db");
        let registry = ProjectRegistry::new(database.clone());
        let root_a = temp.path().join("webapp");
        let root_b = temp.path().join("platform");
        let a = register(&registry, &root_a);
        let _b = register(&registry, &root_b);
        fs::write(
            root_a.join("package.json"),
            "{\"dependencies\": {\"platform\": \"^1.0.0\"}}",
        )
        .expect("manifest");

        let candidates = scan(&database, &a, &root_a).expect("scan");
        assert_eq!(candidates.len(), 1);
        assert!(!candidates[0].definitive);
        assert_eq!(candidates[0].target_name, "platform");

        let applied = apply_definitive(&database, &a, &candidates).expect("apply");
        assert!(applied.is_empty(), "name matches must not auto-link");
        assert!(registry.links(&a).expect("links").is_empty());
    }

    #[test]
    fn vendored_in_repo_paths_are_not_cross_repo_links() {
        let temp = TempDir::new().expect("temp");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("db");
        let registry = ProjectRegistry::new(database.clone());
        let root_a = temp.path().join("app");
        let a = register(&registry, &root_a);
        fs::create_dir_all(root_a.join("vendor/lib")).expect("vendor");
        fs::write(
            root_a.join("Cargo.toml"),
            "[dependencies]\nlib = { path = \"vendor/lib\" }\n",
        )
        .expect("manifest");
        assert!(scan(&database, &a, &root_a).expect("scan").is_empty());
    }

    #[test]
    fn hostname_facts_propose_api_links_without_auto_applying() {
        let temp = TempDir::new().expect("temp");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("db");
        let registry = ProjectRegistry::new(database.clone());
        let root_client = temp.path().join("mobile-app");
        let root_backend = temp.path().join("backend");
        let client = register(&registry, &root_client);
        let backend = register(&registry, &root_backend);
        registry
            .set_fact(
                &backend,
                "hosting.base_url",
                "https://androidagent.example.tech",
                None,
            )
            .expect("fact");
        fs::create_dir_all(root_client.join("src")).expect("src");
        fs::write(
            root_client.join("src/api.ts"),
            "const BASE = \"https://androidagent.example.tech/v1\";\n",
        )
        .expect("source");

        let candidates = scan(&database, &client, &root_client).expect("scan");
        assert_eq!(candidates.len(), 1, "{candidates:?}");
        assert!(!candidates[0].definitive);
        assert!(
            apply_definitive(&database, &client, &candidates)
                .expect("apply")
                .is_empty()
        );
        assert_eq!(candidates[0].relation, "calls-api-of");
        assert_eq!(candidates[0].target_project_id, backend);
        assert!(candidates[0].evidence.contains("androidagent.example.tech"));
        assert!(candidates[0].evidence.contains("src/api.ts"));

        // The backend does not reference its own host as a dependency, and
        // the client's own facts never self-link.
        assert!(
            scan(&database, &backend, &root_backend)
                .expect("reverse")
                .is_empty()
        );
    }

    #[test]
    fn shared_route_literals_propose_but_never_auto_link() {
        let temp = TempDir::new().expect("temp");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("db");
        let registry = ProjectRegistry::new(database.clone());
        let root_a = temp.path().join("service-a");
        let root_b = temp.path().join("service-b");
        let a = register(&registry, &root_a);
        let _b = register(&registry, &root_b);
        let routes = [
            "/api/v1/widgets/list",
            "/api/v1/widgets/{id}/rotate",
            "/api/v1/widgets/{id}/discard",
        ];
        for (root, style) in [(&root_a, "server"), (&root_b, "client")] {
            fs::create_dir_all(root.join("src")).expect("src");
            let body = routes
                .iter()
                .map(|route| {
                    if style == "server" {
                        format!(".route(\"{route}\", post(handler))\n")
                    } else {
                        format!("fetch(\"{route}\")\n")
                    }
                })
                .collect::<String>();
            fs::write(root.join("src/api.rs"), body).expect("source");
        }
        let candidates = scan(&database, &a, &root_a).expect("scan");
        assert_eq!(candidates.len(), 1, "{candidates:?}");
        assert!(
            !candidates[0].definitive,
            "route overlap is a proposal only"
        );
        assert_eq!(candidates[0].relation, "shares-api-surface");
        assert!(candidates[0].evidence.contains("3 shared route literals"));
        assert!(
            apply_definitive(&database, &a, &candidates)
                .expect("apply")
                .is_empty(),
            "proposals must never auto-apply"
        );
    }

    /// Register a backend that declares `api.example.tech`, and a client
    /// linked to it the way scans did before 2026-09-16.
    fn old_policy_hostname_link(
        temp: &TempDir,
    ) -> (Database, ProjectRegistry, PathBuf, ProjectId, ProjectId) {
        let database = Database::open(temp.path().join("brain.sqlite")).expect("db");
        let registry = ProjectRegistry::new(database.clone());
        let root_client = temp.path().join("tooling");
        let client = register(&registry, &root_client);
        let backend = register(&registry, &temp.path().join("backend"));
        registry
            .set_fact(&backend, "hosting.base_url", "api.example.tech", None)
            .expect("fact");
        registry
            .link(
                &client,
                &backend,
                "calls-api-of",
                Some(
                    "auto: references host `api.example.tech` in 1 file(s), e.g. `docs/handoff.md`",
                ),
            )
            .expect("old auto link");
        (database, registry, root_client, client, backend)
    }

    #[test]
    fn old_hostname_links_backed_only_by_documentation_are_stale() {
        let temp = TempDir::new().expect("temp");
        let (database, registry, root, client, backend) = old_policy_hostname_link(&temp);
        fs::create_dir_all(root.join("docs")).expect("docs");
        fs::write(
            root.join("docs/handoff.md"),
            "The backend serves api.example.tech.\n",
        )
        .expect("doc");

        // The target is proposed again on current evidence, as a hint.
        let scan = scan_checkout(&database, &client, &root).expect("scan");
        assert!(scan.complete);
        assert_eq!(scan.candidates.len(), 1, "{:?}", scan.candidates);
        assert_eq!(scan.candidates[0].target_project_id, backend);
        assert!(!scan.candidates[0].definitive);
        assert!(scan.candidates[0].documentation_only);
        assert!(
            scan.candidates[0]
                .evidence
                .ends_with("(documentation only)")
        );
        let stale = stale_auto_links(&database, &client, &scan).expect("stale");
        assert_eq!(stale.len(), 1, "{stale:?}");

        let retracted = retract_stale(&database, &client, &scan).expect("retract");
        assert_eq!(retracted.len(), 1);
        assert!(registry.links(&client).expect("links").is_empty());
        assert!(
            apply_definitive(&database, &client, &scan.candidates)
                .expect("apply")
                .is_empty(),
            "a hostname hint is never applied again"
        );
    }

    #[test]
    fn old_hostname_links_whose_evidence_is_gone_are_stale() {
        let temp = TempDir::new().expect("temp");
        let (database, registry, root, client, _backend) = old_policy_hostname_link(&temp);
        let scan = scan_checkout(&database, &client, &root).expect("scan");
        assert!(scan.candidates.is_empty(), "{:?}", scan.candidates);
        assert_eq!(
            retract_stale(&database, &client, &scan)
                .expect("retract")
                .len(),
            1
        );
        assert!(registry.links(&client).expect("links").is_empty());
    }

    #[test]
    fn source_configuration_and_dotfiles_keep_old_hostname_links() {
        for (path, content) in [
            (
                "config/env.ts",
                "export const API = \"https://api.example.tech\";\n",
            ),
            ("CMakeLists.txt", "set(API_HOST api.example.tech)\n"),
            (
                ".github/workflows/ci.yml",
                "env:\n  API: https://api.example.tech\n",
            ),
        ] {
            let temp = TempDir::new().expect("temp");
            let (database, registry, root, client, backend) = old_policy_hostname_link(&temp);
            // Sorted first, so the example must switch to the evidence file.
            fs::write(root.join("README.md"), "See api.example.tech.\n").expect("doc");
            fs::create_dir_all(root.join(path).parent().expect("parent")).expect("dir");
            fs::write(root.join(path), content).expect("evidence");

            let scan = scan_checkout(&database, &client, &root).expect("scan");
            assert_eq!(scan.candidates.len(), 1, "{path}: {:?}", scan.candidates);
            assert!(!scan.candidates[0].documentation_only, "{path}");
            assert!(
                scan.candidates[0].evidence.contains(path),
                "{path}: evidence outside documentation is the example: {}",
                scan.candidates[0].evidence
            );
            assert!(
                retract_stale(&database, &client, &scan)
                    .expect("retract")
                    .is_empty(),
                "{path}"
            );
            let links = registry.links(&client).expect("links");
            assert_eq!(links.len(), 1, "{path}");
            assert_eq!(links[0].project_id, backend);
        }
    }

    #[test]
    fn version_control_stores_are_not_evidence() {
        let temp = TempDir::new().expect("temp");
        let (database, _registry, root, client, _backend) = old_policy_hostname_link(&temp);
        fs::create_dir_all(root.join(".git")).expect("git");
        fs::write(
            root.join(".git/config"),
            "url = https://api.example.tech/repo\n",
        )
        .expect("config");
        let scan = scan_checkout(&database, &client, &root).expect("scan");
        assert!(scan.candidates.is_empty(), "{:?}", scan.candidates);
    }

    #[test]
    fn an_incomplete_scan_judges_no_link_stale() {
        let temp = TempDir::new().expect("temp");
        let (database, registry, root, client, backend) = old_policy_hostname_link(&temp);
        let scan = LinkScan {
            candidates: Vec::new(),
            hosted_targets: vec![backend],
            complete: false,
        };
        assert!(
            stale_auto_links(&database, &client, &scan)
                .expect("stale")
                .is_empty()
        );
        assert!(
            retract_stale(&database, &client, &scan)
                .expect("retract")
                .is_empty()
        );
        assert_eq!(registry.links(&client).expect("links").len(), 1);
        // A directory the walk cannot read makes the scan incomplete.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let locked = root.join("locked");
            fs::create_dir_all(&locked).expect("locked");
            fs::write(locked.join("env.ts"), "api.example.tech").expect("hidden evidence");
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
            let readable = fs::read_dir(&locked).is_ok();
            let scan = scan_checkout(&database, &client, &root).expect("scan");
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("restore");
            if !readable {
                assert!(!scan.complete);
                assert!(
                    stale_auto_links(&database, &client, &scan)
                        .expect("stale")
                        .is_empty()
                );
            }
        }
    }

    #[test]
    fn links_to_targets_without_a_declared_host_are_never_stale() {
        let temp = TempDir::new().expect("temp");
        let (database, registry, root, client, backend) = old_policy_hostname_link(&temp);
        registry
            .unset_fact(&backend, "hosting.base_url")
            .expect("unset");
        let scan = scan_checkout(&database, &client, &root).expect("scan");
        assert!(scan.complete);
        assert!(!scan.hosted_targets.contains(&backend));
        assert!(
            stale_auto_links(&database, &client, &scan)
                .expect("stale")
                .is_empty()
        );
        assert_eq!(registry.links(&client).expect("links").len(), 1);
    }

    #[test]
    fn an_unreadable_checkout_is_an_error_not_missing_evidence() {
        let temp = TempDir::new().expect("temp");
        let (database, registry, root, client, _backend) = old_policy_hostname_link(&temp);
        fs::remove_dir_all(&root).expect("remove checkout");
        assert!(scan(&database, &client, &root).is_err());
        assert_eq!(registry.links(&client).expect("links").len(), 1);
    }

    #[test]
    fn manual_and_definitive_links_are_never_stale() {
        let temp = TempDir::new().expect("temp");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("db");
        let registry = ProjectRegistry::new(database.clone());
        let consumer = register(&registry, &temp.path().join("consumer"));
        let notes = [
            None,
            Some("confirmed by hand: calls api.example.tech"),
            Some("auto: Cargo path dependency `../provider`"),
            Some("auto: npm file dependency `../web`"),
            Some("auto: git submodule `git@example.com:org/lib.git`"),
            Some("auto: pip editable install `../agents` (requirements.txt)"),
        ];
        let mut targets = Vec::new();
        for (index, note) in notes.into_iter().enumerate() {
            let target = register(&registry, &temp.path().join(format!("target-{index}")));
            registry
                .link(&consumer, &target, "uses", note)
                .expect("link");
            targets.push(target);
        }

        // Even a complete scan with no evidence at all for any target leaves
        // them: only links from the earlier hostname rule can be stale.
        let scan = LinkScan {
            candidates: Vec::new(),
            hosted_targets: targets,
            complete: true,
        };
        assert!(
            stale_auto_links(&database, &consumer, &scan)
                .expect("stale")
                .is_empty()
        );
        assert!(
            retract_stale(&database, &consumer, &scan)
                .expect("retract")
                .is_empty()
        );
        assert_eq!(registry.links(&consumer).expect("links").len(), 6);
    }

    #[test]
    fn doctor_lists_links_from_the_earlier_hostname_rule() {
        use crate::doctor::CheckStatus;
        let temp = TempDir::new().expect("temp");
        let (database, registry, _root, client, backend) = old_policy_hostname_link(&temp);
        let check = legacy_link_check(&database);
        assert_eq!(check.status, CheckStatus::Warning, "{}", check.detail);
        assert!(
            check.detail.contains("tooling → backend"),
            "{}",
            check.detail
        );
        // Confirming the link by hand clears the warning.
        registry
            .link(&client, &backend, "calls-api-of", None)
            .expect("confirm");
        assert_eq!(legacy_link_check(&database).status, CheckStatus::Pass);
    }

    #[test]
    fn definitive_evidence_prefixes_match_what_scans_record() {
        let temp = TempDir::new().expect("temp");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("db");
        let registry = ProjectRegistry::new(database.clone());
        let root_a = temp.path().join("consumer");
        let a = register(&registry, &root_a);
        for name in ["provider", "web", "agents"] {
            register(&registry, &temp.path().join(name));
        }
        fs::write(
            root_a.join("Cargo.toml"),
            "[dependencies]\nprovider = { path = \"../provider\" }\n",
        )
        .expect("cargo manifest");
        fs::write(
            root_a.join("package.json"),
            "{\"dependencies\": {\"web\": \"file:../web\"}}",
        )
        .expect("npm manifest");
        fs::write(root_a.join("requirements.txt"), "-e ../agents\n").expect("pip manifest");
        let candidates = scan(&database, &a, &root_a).expect("scan");
        assert_eq!(candidates.len(), 3, "{candidates:?}");
        apply_definitive(&database, &a, &candidates).expect("apply");
        let links = registry.links(&a).expect("links");
        assert_eq!(links.len(), 3);
        for link in &links {
            assert!(!applied_under_old_policy(link), "{:?}", link.note);
        }
    }
}
