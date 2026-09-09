//! Deterministic technology detection: what a codebase is actually built
//! with, read from its own manifests and infra files — never inferred by a
//! model. Feeds the workspace browser (chips per service), the architect
//! dossier ("how it works" cites real technologies), and staleness (a stack
//! change is meaningful architecture drift).

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

const MAX_MANIFEST_BYTES: u64 = 1_048_576;
const MAX_WALK_ENTRIES: usize = 600;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Technology {
    pub name: String,
    /// `language` | `framework` | `database` | `infra` | `cloud` |
    /// `messaging` | `observability`
    pub category: String,
    /// Where it was detected ("Cargo.toml: sqlx", "infra/*.tf").
    pub evidence: String,
}

/// Curated dependency-name → technology table. Exact matches only; a
/// dependency the table does not know is simply not reported.
const DEPENDENCY_TECHS: &[(&str, &str, &str)] = &[
    // databases
    ("sqlx", "PostgreSQL", "database"),
    ("tokio-postgres", "PostgreSQL", "database"),
    ("postgres", "PostgreSQL", "database"),
    ("psycopg", "PostgreSQL", "database"),
    ("psycopg2", "PostgreSQL", "database"),
    ("psycopg2-binary", "PostgreSQL", "database"),
    ("asyncpg", "PostgreSQL", "database"),
    ("pg", "PostgreSQL", "database"),
    ("rusqlite", "SQLite", "database"),
    ("better-sqlite3", "SQLite", "database"),
    ("redis", "Redis", "database"),
    ("ioredis", "Redis", "database"),
    ("mongodb", "MongoDB", "database"),
    ("pymongo", "MongoDB", "database"),
    ("mysql2", "MySQL", "database"),
    ("prisma", "Prisma", "database"),
    ("@prisma/client", "Prisma", "database"),
    ("sqlalchemy", "SQLAlchemy", "database"),
    ("diesel", "Diesel ORM", "database"),
    // frameworks / runtimes
    ("axum", "axum", "framework"),
    ("actix-web", "Actix Web", "framework"),
    ("rocket", "Rocket", "framework"),
    ("tokio", "Tokio", "framework"),
    ("next", "Next.js", "framework"),
    ("react", "React", "framework"),
    ("react-native", "React Native", "framework"),
    ("expo", "Expo", "framework"),
    ("express", "Express", "framework"),
    ("fastify", "Fastify", "framework"),
    ("fastapi", "FastAPI", "framework"),
    ("django", "Django", "framework"),
    ("flask", "Flask", "framework"),
    ("svelte", "Svelte", "framework"),
    ("vue", "Vue", "framework"),
    ("tailwindcss", "Tailwind CSS", "framework"),
    // messaging / queues
    ("kafka-python", "Kafka", "messaging"),
    ("kafkajs", "Kafka", "messaging"),
    ("rdkafka", "Kafka", "messaging"),
    ("amqplib", "RabbitMQ", "messaging"),
    ("pika", "RabbitMQ", "messaging"),
    ("bullmq", "BullMQ", "messaging"),
    ("celery", "Celery", "messaging"),
    // cloud SDKs
    ("google-cloud-storage", "Google Cloud Storage", "cloud"),
    ("@google-cloud/storage", "Google Cloud Storage", "cloud"),
    ("google-cloud-pubsub", "Google Cloud Pub/Sub", "cloud"),
    ("boto3", "AWS", "cloud"),
    ("aws-sdk", "AWS", "cloud"),
    ("@aws-sdk/client-s3", "AWS S3", "cloud"),
    ("stripe", "Stripe", "cloud"),
    ("firebase-admin", "Firebase", "cloud"),
    // observability
    ("sentry-sdk", "Sentry", "observability"),
    ("@sentry/nextjs", "Sentry", "observability"),
    ("@sentry/node", "Sentry", "observability"),
    ("tracing", "tracing", "observability"),
    ("prometheus", "Prometheus", "observability"),
    ("prom-client", "Prometheus", "observability"),
];

/// Detect a repository's technology stack. Bounded, filesystem-only.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn detect(root: &Path) -> Vec<Technology> {
    let mut seen = BTreeSet::new();
    let mut found = Vec::new();
    let mut push = |name: &str, category: &str, evidence: String| {
        if seen.insert(name.to_owned()) {
            found.push(Technology {
                name: name.to_owned(),
                category: category.to_owned(),
                evidence,
            });
        }
    };

    // Languages + dependency-declared technologies from manifests.
    if let Some(manifest) = read(root, "Cargo.toml") {
        push("Rust", "language", "Cargo.toml".to_owned());
        for dependency in cargo_dependencies(&manifest) {
            lookup(&dependency, "Cargo.toml", &mut push);
        }
    }
    if let Some(manifest) = read(root, "package.json") {
        push(
            "TypeScript/JavaScript",
            "language",
            "package.json".to_owned(),
        );
        for dependency in package_json_dependencies(&manifest) {
            lookup(&dependency, "package.json", &mut push);
        }
    }
    let mut python_dependencies = Vec::new();
    for file in ["requirements.txt", "requirements-dev.txt"] {
        if let Some(manifest) = read(root, file) {
            python_dependencies.extend(requirements_dependencies(&manifest));
        }
    }
    if let Some(manifest) = read(root, "pyproject.toml") {
        python_dependencies.extend(pyproject_dependencies(&manifest));
    }
    if !python_dependencies.is_empty() || root.join("pyproject.toml").is_file() {
        push("Python", "language", "requirements/pyproject".to_owned());
    }
    for dependency in python_dependencies {
        lookup(&dependency, "requirements", &mut push);
    }
    if read(root, "go.mod").is_some() {
        push("Go", "language", "go.mod".to_owned());
    }
    if root.join("app/build.gradle.kts").is_file() || root.join("build.gradle.kts").is_file() {
        push("Kotlin/Android", "language", "build.gradle.kts".to_owned());
    }

    // Infra by file presence (bounded shallow walk for extensions).
    if root.join("Dockerfile").is_file() {
        push("Docker", "infra", "Dockerfile".to_owned());
    }
    for compose in ["docker-compose.yml", "docker-compose.yaml", "compose.yaml"] {
        if root.join(compose).is_file() {
            push("Docker Compose", "infra", compose.to_owned());
        }
    }
    if root.join(".github/workflows").is_dir() {
        push("GitHub Actions", "infra", ".github/workflows".to_owned());
    }
    let mut scanned = 0_usize;
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .follow_links(false)
        .max_depth(Some(4))
        .build();
    for entry in walker {
        let Ok(entry) = entry else { continue };
        scanned += 1;
        if scanned > MAX_WALK_ENTRIES {
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .display()
            .to_string();
        if std::path::Path::new(&name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("tf"))
        {
            push("Terraform", "infra", rel.clone());
        }
        if name == "cdk.json" || rel.contains("aws-cdk") {
            push("AWS CDK", "infra", rel.clone());
        }
        if name == "app.yaml" || name == "cloudbuild.yaml" {
            push("Google Cloud", "cloud", rel.clone());
        }
        if name == "Chart.yaml" {
            push("Helm/Kubernetes", "infra", rel.clone());
        }
        if name == "wrangler.toml" || name == "wrangler.jsonc" {
            push("Cloudflare Workers", "cloud", rel.clone());
        }
    }

    found.sort_by(|a, b| a.category.cmp(&b.category).then(a.name.cmp(&b.name)));
    found
}

fn lookup(dependency: &str, source: &str, push: &mut impl FnMut(&str, &str, String)) {
    let normalized = dependency.to_ascii_lowercase();
    for (needle, name, category) in DEPENDENCY_TECHS {
        if normalized == *needle {
            push(name, category, format!("{source}: {dependency}"));
        }
    }
}

fn read(root: &Path, file: &str) -> Option<String> {
    let path = root.join(file);
    let metadata = fs::metadata(&path).ok()?;
    if metadata.len() > MAX_MANIFEST_BYTES {
        return None;
    }
    fs::read_to_string(&path).ok()
}

fn cargo_dependencies(manifest: &str) -> Vec<String> {
    let Ok(table) = manifest.parse::<toml::Table>() else {
        return Vec::new();
    };
    let mut dependencies = Vec::new();
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(entries) = table.get(section).and_then(|value| value.as_table()) {
            dependencies.extend(entries.keys().cloned());
        }
    }
    if let Some(entries) = table
        .get("workspace")
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(|value| value.as_table())
    {
        dependencies.extend(entries.keys().cloned());
    }
    dependencies
}

fn package_json_dependencies(manifest: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(manifest) else {
        return Vec::new();
    };
    let mut dependencies = Vec::new();
    for section in ["dependencies", "devDependencies"] {
        if let Some(entries) = value.get(section).and_then(serde_json::Value::as_object) {
            dependencies.extend(entries.keys().cloned());
        }
    }
    dependencies
}

fn requirements_dependencies(manifest: &str) -> Vec<String> {
    manifest
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with(['#', '-']) {
                return None;
            }
            Some(
                line.split(['=', '<', '>', '[', ';', ' '])
                    .next()
                    .unwrap_or(line)
                    .trim()
                    .to_owned(),
            )
        })
        .filter(|name| !name.is_empty())
        .collect()
}

fn pyproject_dependencies(manifest: &str) -> Vec<String> {
    let Ok(table) = manifest.parse::<toml::Table>() else {
        return Vec::new();
    };
    let mut dependencies = Vec::new();
    if let Some(entries) = table
        .get("project")
        .and_then(|project| project.get("dependencies"))
        .and_then(|value| value.as_array())
    {
        for entry in entries {
            if let Some(spec) = entry.as_str() {
                let name = spec
                    .split(['=', '<', '>', '[', ';', ' '])
                    .next()
                    .unwrap_or(spec)
                    .trim();
                if !name.is_empty() {
                    dependencies.push(name.to_owned());
                }
            }
        }
    }
    dependencies
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn detects_stack_from_manifests_and_infra_files() {
        let temp = TempDir::new().expect("temp");
        let root = temp.path();
        fs::write(
            root.join("Cargo.toml"),
            "[dependencies]\nsqlx = \"0.7\"\naxum = \"0.8\"\ntracing = \"0.1\"\n",
        )
        .expect("cargo");
        fs::create_dir_all(root.join("infra")).expect("infra");
        fs::write(root.join("infra/main.tf"), "resource {}\n").expect("tf");
        fs::write(root.join("Dockerfile"), "FROM rust\n").expect("docker");

        let techs = detect(root);
        let names: Vec<&str> = techs.iter().map(|tech| tech.name.as_str()).collect();
        assert!(names.contains(&"Rust"));
        assert!(names.contains(&"PostgreSQL"), "{names:?}");
        assert!(names.contains(&"axum"));
        assert!(names.contains(&"Terraform"));
        assert!(names.contains(&"Docker"));
        assert!(names.contains(&"tracing"));
        let postgres = techs.iter().find(|tech| tech.name == "PostgreSQL").unwrap();
        assert_eq!(postgres.category, "database");
        assert!(postgres.evidence.contains("sqlx"));
        // Unknown dependencies are simply not reported.
        assert!(!names.contains(&"unknown"));
    }
}
