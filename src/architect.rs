//! LLM-assisted workspace architecture synthesis.
//!
//! `leb architect` compiles a DETERMINISTIC dossier — every registered
//! project's facts, links (with their recorded evidence), module structure,
//! and route-like string literals extracted from source — and asks the
//! configured local runtime for a CLAUDE.md-style explanation of what
//! happens where and how the services coordinate. The output is a DERIVED
//! document written into the vault, clearly labeled as synthesized: it never
//! enters the evidence-gated memory store (spec P9 keeps LLM output out of
//! the canonical loop), and regenerating it is always safe. The dossier
//! builder is pure and testable; only the final call touches a model, and
//! the runtime URL is subject to the same local-only network policy as the
//! proxy.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ids::ProjectId;
use crate::projects::ProjectRegistry;
use crate::storage::Database;

const MAX_SOURCE_FILES: usize = 3_000;
const MAX_FILE_BYTES: u64 = 262_144;
const MAX_ROUTES_PER_PROJECT: usize = 40;
const MAX_MODULES_PER_PROJECT: usize = 16;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ArchitectReport {
    pub model: String,
    pub output_path: String,
    pub dossier_bytes: usize,
    pub synthesized_bytes: usize,
}

/// One project's deterministic slice of the dossier.
struct ProjectDossier {
    name: String,
    facts: Vec<(String, String, Option<String>)>,
    links: Vec<(String, String, Option<String>)>,
    modules: Vec<(String, i64)>,
    routes: Vec<String>,
    head: Option<String>,
}

/// Build the complete deterministic dossier for every registered project.
///
/// # Errors
///
/// Returns storage errors; unreadable repos degrade to graph-only slices.
pub fn build_dossier(database: &Database) -> Result<String> {
    let registry = ProjectRegistry::new(database.clone());
    let projects = database.with_reader(|connection| {
        let mut statement = connection.prepare(
            "SELECT id, name FROM projects WHERE tombstoned_at_ms IS NULL ORDER BY name, id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    })?;
    let mut out = String::new();
    out.push_str("WORKSPACE DOSSIER (deterministic facts; every line below is recorded or extracted, none is inferred)\n\n");
    for (id, name) in projects {
        let project_id = ProjectId::parse(&id)?;
        let dossier = project_dossier(database, &registry, &project_id, &name)?;
        render_project(&mut out, &dossier);
    }
    Ok(out)
}

fn project_dossier(
    database: &Database,
    registry: &ProjectRegistry,
    project_id: &ProjectId,
    name: &str,
) -> Result<ProjectDossier> {
    let facts = registry
        .facts(project_id)?
        .into_iter()
        .map(|fact| (fact.key, fact.value, fact.note))
        .collect();
    let links = registry
        .links(project_id)?
        .into_iter()
        .map(|link| (link.name, link.relation, link.note))
        .collect();
    let (modules, head) = database.with_reader(|connection| {
        use rusqlite::OptionalExtension as _;
        let mut statement = connection.prepare(
            "SELECT file_path, COUNT(*) FROM nodes
             WHERE project_id = ?1 AND tombstoned_at_ms IS NULL AND file_path IS NOT NULL
             GROUP BY file_path",
        )?;
        let rows = statement
            .query_map([project_id.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut groups = std::collections::BTreeMap::<String, i64>::new();
        for (path, count) in rows {
            let group = path.split('/').next().unwrap_or("(root)").to_owned();
            *groups.entry(group).or_default() += count;
        }
        let mut modules = groups.into_iter().collect::<Vec<_>>();
        modules.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        modules.truncate(MAX_MODULES_PER_PROJECT);
        let head = connection
            .query_row(
                "SELECT head FROM structural_scans WHERE project_id = ?1
                 ORDER BY completed_at_ms DESC LIMIT 1",
                [project_id.as_str()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok((modules, head))
    })?;
    let routes = registry
        .alias_paths(project_id)?
        .into_iter()
        .find(|path| path.is_dir())
        .map(|root| extract_routes(&root))
        .unwrap_or_default();
    Ok(ProjectDossier {
        name: name.to_owned(),
        facts,
        links,
        modules,
        routes,
        head,
    })
}

fn render_project(out: &mut String, dossier: &ProjectDossier) {
    let _ = writeln!(out, "## Project: {}", dossier.name);
    if let Some(head) = &dossier.head {
        let _ = writeln!(out, "indexed-head: {head}");
    }
    for (key, value, note) in &dossier.facts {
        let note = note
            .as_deref()
            .map(|note| format!(" ({note})"))
            .unwrap_or_default();
        let _ = writeln!(out, "fact {key}: {value}{note}");
    }
    for (name, relation, note) in &dossier.links {
        let note = note
            .as_deref()
            .map(|note| format!(" ({note})"))
            .unwrap_or_default();
        let _ = writeln!(out, "link -> {name} [{relation}]{note}");
    }
    if !dossier.modules.is_empty() {
        let summary = dossier
            .modules
            .iter()
            .map(|(name, count)| format!("{name}({count})"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "modules: {summary}");
    }
    for route in &dossier.routes {
        let _ = writeln!(out, "route-literal: {route}");
    }
    out.push('\n');
}

/// Extract route-like string literals from source files: absolute paths that
/// appear on lines with routing/HTTP vocabulary. Purely lexical and bounded;
/// results are dossier inputs, never link evidence by themselves.
fn extract_routes(root: &Path) -> Vec<String> {
    const HTTP_WORDS: [&str; 12] = [
        "route", "get(", "post(", "put(", "delete(", "patch(", "fetch", "axum", "axios", "request",
        "endpoint", "url",
    ];
    let mut routes = std::collections::BTreeSet::new();
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
        if scanned > MAX_SOURCE_FILES || routes.len() >= MAX_ROUTES_PER_PROJECT {
            break;
        }
        let Ok(content) = fs::read_to_string(entry.path()) else {
            continue;
        };
        for line in content.lines() {
            let lower = line.to_ascii_lowercase();
            if !HTTP_WORDS.iter().any(|word| lower.contains(word)) {
                continue;
            }
            for literal in line.split('"').skip(1).step_by(2) {
                if looks_like_route(literal) {
                    routes.insert(literal.to_owned());
                    if routes.len() >= MAX_ROUTES_PER_PROJECT {
                        break;
                    }
                }
            }
        }
    }
    routes.into_iter().collect()
}

fn looks_like_route(literal: &str) -> bool {
    literal.len() >= 4
        && literal.len() <= 120
        && literal.starts_with('/')
        && !literal.starts_with("//")
        && literal.split('/').count() >= 3
        && literal
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/_-{}:.$".contains(c))
}

/// The fixed synthesis prompt. The model writes prose; the dossier supplies
/// every fact it is allowed to rely on.
#[must_use]
pub fn synthesis_prompt(dossier: &str) -> String {
    format!(
        "You are documenting a multi-repository engineering workspace for an engineer \
         onboarding onto it. Using ONLY the dossier below (do not invent services, \
         hosts, or dependencies that are not listed), write a clear CLAUDE.md-style \
         architecture guide in Markdown with these sections:\n\
         1. `# Workspace overview` - one paragraph on what the system does as a whole.\n\
         2. `## Services` - per project: its role, where it is hosted (facts), its \
         main modules, and its notable API surface (route literals).\n\
         3. `## How the services coordinate` - who calls whom, over which declared \
         hostnames/relations, and what that implies when changing an API.\n\
         4. `## Cross-repo change checklist` - concrete steps an engineer should take \
         when a change in one repo affects a linked repo.\n\
         Be specific, cite the repos by name, and keep it under 900 words.\n\n{dossier}"
    )
}

/// Call an OpenAI-compatible chat-completions runtime and return the text.
///
/// # Errors
///
/// Returns transport, HTTP, or response-shape errors.
pub async fn synthesize(runtime_url: &str, model: &str, prompt: &str) -> Result<String> {
    #[derive(Serialize)]
    struct Message<'a> {
        role: &'a str,
        content: &'a str,
    }
    #[derive(Serialize)]
    struct Request<'a> {
        model: &'a str,
        messages: Vec<Message<'a>>,
        stream: bool,
    }
    #[derive(Deserialize)]
    struct Choice {
        message: ResponseMessage,
    }
    #[derive(Deserialize)]
    struct ResponseMessage {
        content: String,
    }
    #[derive(Deserialize)]
    struct Response {
        choices: Vec<Choice>,
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|error| Error::Config(format!("architect http client: {error}")))?;
    let response = client
        .post(runtime_url)
        .json(&Request {
            model,
            messages: vec![Message {
                role: "user",
                content: prompt,
            }],
            stream: false,
        })
        .send()
        .await
        .map_err(|error| Error::Config(format!("architect runtime request: {error}")))?;
    if !response.status().is_success() {
        return Err(Error::Config(format!(
            "architect runtime returned {}",
            response.status()
        )));
    }
    let body: Response = response
        .json()
        .await
        .map_err(|error| Error::Config(format!("architect runtime response: {error}")))?;
    body.choices
        .into_iter()
        .next()
        .map(|choice| choice.message.content)
        .ok_or_else(|| Error::Config("architect runtime returned no choices".to_owned()))
}

/// Enforce the same network stance as the proxy: a non-loopback runtime is
/// only reachable when `network.local_only` was explicitly disabled.
///
/// # Errors
///
/// Returns a config error for non-loopback URLs under local-only policy.
pub fn enforce_local_only(runtime_url: &str, local_only: bool) -> Result<()> {
    if !local_only {
        return Ok(());
    }
    let parsed = url::Url::parse(runtime_url)
        .map_err(|error| Error::Config(format!("architect runtime url: {error}")))?;
    let host = parsed.host_str().unwrap_or_default();
    let loopback = host == "localhost"
        || host == "127.0.0.1"
        || host == "[::1]"
        || host == "::1"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if loopback {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "architect runtime `{runtime_url}` is not loopback; set network.local_only = false to allow it"
        )))
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn dossier_lists_facts_links_and_routes_deterministically() {
        let temp = TempDir::new().expect("temp");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("db");
        let registry = ProjectRegistry::new(database.clone());
        let root_api = temp.path().join("api-svc");
        let root_web = temp.path().join("web-app");
        fs::create_dir_all(root_api.join("src")).expect("dirs");
        fs::create_dir_all(&root_web).expect("dirs");
        let api = registry
            .resolve_or_register(&root_api)
            .expect("api")
            .project
            .id;
        let web = registry
            .resolve_or_register(&root_web)
            .expect("web")
            .project
            .id;
        registry
            .set_fact(&api, "hosting.base_url", "api.example.com", None)
            .expect("fact");
        registry
            .link(&web, &api, "calls-api-of", None)
            .expect("link");
        fs::write(
            root_api.join("src/routes.rs"),
            ".route(\"/api/v1/jobs/list\", post(list_jobs))\nlet x = \"not a route\";\n",
        )
        .expect("src");

        let dossier = build_dossier(&database).expect("dossier");
        assert!(dossier.contains("## Project: api-svc"));
        assert!(dossier.contains("fact hosting.base_url: api.example.com"));
        assert!(dossier.contains("link -> api-svc [calls-api-of]"));
        assert!(dossier.contains("route-literal: /api/v1/jobs/list"));
        assert!(!dossier.contains("not a route"));
        // Deterministic: same inputs, same bytes.
        assert_eq!(dossier, build_dossier(&database).expect("again"));
    }

    #[test]
    fn local_only_policy_gates_remote_runtimes() {
        assert!(enforce_local_only("http://127.0.0.1:8010/v1/chat/completions", true).is_ok());
        assert!(enforce_local_only("http://localhost:8010/x", true).is_ok());
        assert!(enforce_local_only("https://api.openai.com/v1/chat/completions", true).is_err());
        assert!(enforce_local_only("https://api.openai.com/v1/chat/completions", false).is_ok());
    }
}
