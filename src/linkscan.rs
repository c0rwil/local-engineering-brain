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

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::ids::ProjectId;
use crate::projects::{LinkedProject, ProjectRegistry};
use crate::storage::Database;

/// Manifests larger than this are skipped rather than parsed.
const MAX_MANIFEST_BYTES: u64 = 1_048_576;
/// Dependencies examined per manifest.
const MAX_DEPENDENCIES: usize = 500;

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
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LinkScanReport {
    pub candidates: Vec<LinkCandidate>,
    pub applied: Vec<LinkedProject>,
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
/// Already-linked targets are excluded from the result.
///
/// # Errors
///
/// Returns an error for storage failures; unreadable or oversized manifests
/// are skipped, not fatal.
pub fn scan(
    database: &Database,
    project_id: &ProjectId,
    root: &Path,
) -> Result<Vec<LinkCandidate>> {
    let registry = ProjectRegistry::new(database.clone());
    let known = known_projects(database, &registry, project_id)?;
    let existing = registry
        .links(project_id)?
        .into_iter()
        .map(|link| link.project_id)
        .collect::<Vec<_>>();
    let mut candidates = Vec::new();
    scan_cargo(root, &known, &mut candidates);
    scan_package_json(root, &known, &mut candidates);
    scan_gitmodules(root, &known, &mut candidates);
    scan_requirements(root, &known, &mut candidates);
    scan_hostnames(root, &known, &mut candidates);
    scan_route_overlap(root, &known, &mut candidates);
    candidates.retain(|candidate| !existing.contains(&candidate.target_project_id));
    // One candidate per target: keep the strongest (definitive first).
    candidates.sort_by(|a, b| {
        a.target_project_id
            .as_str()
            .cmp(b.target_project_id.as_str())
            .then(b.definitive.cmp(&a.definitive))
    });
    candidates.dedup_by(|a, b| a.target_project_id == b.target_project_id);
    candidates.sort_by(|a, b| {
        b.definitive
            .cmp(&a.definitive)
            .then_with(|| a.target_name.cmp(&b.target_name))
    });
    Ok(candidates)
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
            Some(&format!("auto: {}", candidate.evidence)),
        )?);
    }
    Ok(applied)
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

/// Search the repo's tracked-looking text files for other services'
/// declared hostnames. A source literal naming another service's domain is
/// the repo declaring that it calls that service — definitive evidence.
fn scan_hostnames(root: &Path, known: &[KnownProject], candidates: &mut Vec<LinkCandidate>) {
    const MAX_FILES: usize = 3_000;
    const MAX_FILE_BYTES: u64 = 262_144;
    let hosted = known
        .iter()
        .filter(|project| !project.hosts.is_empty())
        .collect::<Vec<_>>();
    if hosted.is_empty() {
        return;
    }
    let mut hits: Vec<(usize, usize, String)> =
        hosted.iter().map(|_| (0, 0, String::new())).collect();
    let mut scanned = 0_usize;
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .follow_links(false)
        .build();
    for entry in walker {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if entry
            .metadata()
            .ok()
            .is_none_or(|metadata| metadata.len() > MAX_FILE_BYTES)
        {
            continue;
        }
        scanned += 1;
        if scanned > MAX_FILES {
            break;
        }
        let Ok(content) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let content = content.to_ascii_lowercase();
        for (index, project) in hosted.iter().enumerate() {
            if project.hosts.iter().any(|host| content.contains(host)) {
                hits[index].0 += 1;
                if hits[index].2.is_empty() {
                    hits[index].1 = 0;
                    hits[index].2 = entry
                        .path()
                        .strip_prefix(root)
                        .unwrap_or(entry.path())
                        .display()
                        .to_string();
                }
            }
        }
    }
    for (index, project) in hosted.iter().enumerate() {
        let (count, _, example) = &hits[index];
        if *count > 0 {
            candidates.push(LinkCandidate {
                target_project_id: project.id.clone(),
                target_name: project.name.clone(),
                relation: "calls-api-of".to_owned(),
                evidence: format!(
                    "references host `{}` in {count} file(s), e.g. `{example}`",
                    project.hosts.join("`/`")
                ),
                definitive: true,
            });
        }
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
    fn hostname_facts_yield_definitive_api_links() {
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
        assert!(candidates[0].definitive);
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
}
